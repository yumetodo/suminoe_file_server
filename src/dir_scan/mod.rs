//! スキャンサービス（要求キュー、スキャン実行スレッド）と、外部に公開する操作
//! （設計書 6.1, 6.2, 6.3, 6.6）。
//!
//! - 要求は [`ScanService::request_scan`] でキューに積まれ（同じ設定の要求は統合される）、
//!   専用のスキャン実行スレッドが 1 件ずつ実行する。tokio や async は使わない。
//! - スキャン実行スレッド自身はファイルシステムに触れない。走査は [`walk::run_walk`] が
//!   スキャンごとに作るスレッドプールの中で行う。
//! - ツリーは `std::sync::RwLock` で包んで公開する。書き込みロックの中では、
//!   [`tree::Tree::apply_full`]・[`tree::Tree::apply_partial`]・[`tree::Tree::record_failed_scan`]
//!   だけを行う。メモリ確保が必要な処理と、旧ツリーの解放はロックの外で行う。
//! - 反映処理（ロックを持っている間）でのパニックと、回復不能な [`tree::TreeError`] は、
//!   ツリーの整合性を保証できないので `std::process::abort()` でプロセスを終了する。
//!   そのため、書き込みロックが poisoned になることは実際には起きない。起きた場合
//!   （到達しない想定）、スキャン実行スレッドは終了し、読み取り操作は `Err` を返す。

pub mod ioprio;
pub mod report;
pub mod request;
pub mod settings;
pub mod tree;
pub mod walk;

/// キューと停止状態。`Mutex` で守り、`Condvar` で待つ。
struct QueueState {
    queue: request::ScanQueue,
    /// 停止が始まった。以後、新しい要求を受け付けず、キューの要素を実行しない
    stopping: bool,
}

/// 公開するツリーの更新を、スキャン実行スレッドと共有するための状態。
struct Shared {
    root: std::path::PathBuf,
    state: std::sync::Mutex<QueueState>,
    queue_changed: std::sync::Condvar,
    tree: std::sync::RwLock<tree::Tree>,
    last_full_report: std::sync::Mutex<Option<std::sync::Arc<report::ScanReport>>>,
    last_partial_report: std::sync::Mutex<Option<std::sync::Arc<report::ScanReport>>>,
    next_request_id: std::sync::atomic::AtomicU64,
    next_execution_id: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    test_gate: TestGate,
}

/// スキャンサービス。ルートは 1 つ。
///
/// 停止は [`ScanService::shutdown`]。`Drop` でも同じ停止手順を行う（二重に停止しても安全）。
pub struct ScanService {
    shared: std::sync::Arc<Shared>,
    /// スキャン実行スレッド。停止で取り出して `join` する（`join` の間は `Mutex` を持つので、
    /// 同時に呼ばれた別の `shutdown` は停止の完了まで待つ）
    thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl ScanService {
    /// サービスを立ち上げる。ルートが開けるかは、ここでは確かめない
    /// （開けなければ、そのスキャンが失敗になる。設計書 6.6）。
    /// スキャン実行スレッドを作れなければ `Err`。
    pub fn start(root: std::path::PathBuf) -> anyhow::Result<Self> {
        use anyhow::Context;
        let shared = std::sync::Arc::new(Shared {
            root,
            state: std::sync::Mutex::new(QueueState {
                queue: request::ScanQueue::new(),
                stopping: false,
            }),
            queue_changed: std::sync::Condvar::new(),
            tree: std::sync::RwLock::new(tree::Tree::new()),
            last_full_report: std::sync::Mutex::new(None),
            last_partial_report: std::sync::Mutex::new(None),
            next_request_id: std::sync::atomic::AtomicU64::new(1),
            next_execution_id: std::sync::atomic::AtomicU64::new(1),
            #[cfg(test)]
            test_gate: TestGate::new(),
        });
        let thread_shared = std::sync::Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("dir-scan".to_string())
            .spawn(move || thread_shared.run_scan_thread())
            .context("failed to spawn the scan thread")?;
        Ok(Self {
            shared,
            thread: std::sync::Mutex::new(Some(handle)),
        })
    }

    pub fn root(&self) -> &std::path::Path {
        &self.shared.root
    }

    /// スキャンを要求する。キューに積み（同じ設定の要求は統合される）、チケットを返してすぐ戻る。
    /// 停止が始まっていれば `Err`。完了は [`request::Ticket::wait`] で待つ。
    ///
    /// 部分スキャンのパスの検証は [`request::ScanTarget::partial`]（または
    /// [`ScanService::request_partial_scan`]）が行う。
    pub fn request_scan(
        &self,
        target: request::ScanTarget,
        settings: settings::ScanSettings,
    ) -> anyhow::Result<request::Ticket> {
        let mut state = self.shared.lock_state();
        if state.stopping {
            anyhow::bail!("the scan service is stopped; the request is rejected");
        }
        let request_id = self
            .shared
            .next_request_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let description = target.to_string();
        let (ticket, waiter) = request::Ticket::new(request_id, target);
        let push_outcome = state.queue.push(settings, waiter);
        let queue_len = state.queue.len();
        self.shared.queue_changed.notify_all();
        drop(state);
        log::debug!(
            "scan requested: request_id={request_id} target={description} push={push_outcome:?} queue_len={queue_len}"
        );
        Ok(ticket)
    }

    /// フルスキャンを要求する（[`ScanService::request_scan`]）。
    pub fn request_full_scan(
        &self,
        settings: settings::ScanSettings,
    ) -> anyhow::Result<request::Ticket> {
        self.request_scan(request::ScanTarget::full(), settings)
    }

    /// 部分スキャンを要求する。パス（ルートからの相対パス）の検証に失敗したらその場で `Err`。
    pub fn request_partial_scan(
        &self,
        relative_path: &std::path::Path,
        settings: settings::ScanSettings,
    ) -> anyhow::Result<request::Ticket> {
        use anyhow::Context;
        let target =
            request::ScanTarget::partial(relative_path).context("invalid partial scan request")?;
        self.request_scan(target, settings)
    }

    /// ツリーの状態と、指定したディレクトリ（ルートからの相対パス。ルートは空のパス）の
    /// 合計サイズ・子ディレクトリごとの合計サイズを、それぞれ「完全か」と一緒に読む。
    /// 読み取りロックを取るので、反映の瞬間だけ待つことがある。
    ///
    /// ロックが poisoned の場合は `Err`（整合性を保証できないデータを返さない）。
    /// 反映中のパニックはプロセスの終了なので、通常は起きない。
    pub fn read_directory(
        &self,
        relative_path: &std::path::Path,
    ) -> anyhow::Result<tree::TreeRead> {
        let guard = self
            .shared
            .tree
            .read()
            .map_err(|_| anyhow::anyhow!("the tree lock is poisoned"))?;
        Ok(tree::read_directory(&guard, relative_path))
    }

    /// 直近のフルスキャン（実際に走ったスキャンの種類がフル。格上げされた部分スキャンと、
    /// ルートまで遡った部分スキャンを含む）のレポート。まだなければ `None`。
    pub fn last_full_report(&self) -> Option<std::sync::Arc<report::ScanReport>> {
        lock_ignoring_poison(&self.shared.last_full_report).clone()
    }

    /// 直近の部分スキャンのレポート。まだなければ `None`。
    pub fn last_partial_report(&self) -> Option<std::sync::Arc<report::ScanReport>> {
        lock_ignoring_poison(&self.shared.last_partial_report).clone()
    }

    /// 停止する（設計書 6.2）。(1) 新しい要求の受け付けを止める、(2) 実行中のスキャンが
    /// あれば、その 1 件だけを完了させる、(3) キューに残っている未実行の要求をすべて取り出し、
    /// 待ち手全員に「停止」を通知する、(4) スキャン実行スレッドを終了する（`join`）、
    /// (5) 戻る。何度呼んでも安全。
    pub fn shutdown(&self) {
        {
            let mut state = self.shared.lock_state();
            state.stopping = true;
            self.shared.queue_changed.notify_all();
        }
        let mut thread = lock_ignoring_poison(&self.thread);
        if let Some(handle) = thread.take() {
            if handle.join().is_err() {
                log::error!("the scan thread terminated by a panic");
            }
        }
        // スキャン実行スレッドが終了時に通知するので、通常は何も残っていない。
        // スレッドが異常終了していた場合に備えて、待ち手が取り残されないようにする
        self.shared.notify_remaining_stopped();
    }
}

impl Drop for ScanService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `Mutex` を、poisoned でもそのまま使って取る。守っているのが単純な値（キューと停止フラグ、
/// 直近のレポート、スレッドのハンドル）で、ロックを持ったままパニックする処理がないため。
fn lock_ignoring_poison<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 回復不能な状態を `log::error!` に出してプロセスを終了する。
fn abort_process(reason: &str) -> ! {
    log::error!("fatal error in the directory scan service, aborting the process: {reason}");
    std::process::abort()
}

/// 反映処理が返した [`tree::TreeError`] への対処。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyErrorHandling {
    /// ツリーの整合性を保証できない。プロセスを終了する
    AbortProcess,
    /// ツリーは変更されていない。そのスキャンを失敗として扱い、続ける
    FailScan,
}

fn handle_apply_error(error: &tree::TreeError) -> ApplyErrorHandling {
    if error.is_fatal() {
        ApplyErrorHandling::AbortProcess
    } else {
        ApplyErrorHandling::FailScan
    }
}

/// 走査が組み立てた、公開前のツリーまたは部分木。
enum ScannedTree {
    Full(tree::FullScanTree),
    Partial {
        effective_path: std::path::PathBuf,
        subtree: tree::DirNode,
    },
}

/// 反映で外した、旧ツリーまたは旧部分木。書き込みロックを手放してから解放する。
enum RetiredTree {
    Full(tree::RetiredFullTree),
    Partial(tree::DirNode),
}

impl RetiredTree {
    fn release(self) {
        match self {
            Self::Full(old) => drop(old),
            Self::Partial(old) => drop(old),
        }
    }
}

fn apply_scanned(
    tree: &mut tree::Tree,
    scanned: ScannedTree,
    scan_had_errors: bool,
    finished_at: std::time::SystemTime,
) -> Result<RetiredTree, tree::TreeError> {
    match scanned {
        ScannedTree::Full(full) => {
            let applied = tree.apply_full(full, scan_had_errors, finished_at)?;
            Ok(RetiredTree::Full(applied.retired))
        }
        ScannedTree::Partial {
            effective_path,
            subtree,
        } => {
            let applied =
                tree.apply_partial(&effective_path, subtree, scan_had_errors, finished_at)?;
            Ok(RetiredTree::Partial(applied.retired_subtree))
        }
    }
}

/// 実行の直前に決めた、走査の計画。
struct ScanPlan {
    /// スキャン開始時の `generation`
    generation_at_start: u64,
    /// 走査する対象（ルートからの相対パス。ルート全体なら空）
    walk_path: std::path::PathBuf,
    /// フルスキャンは新しいもの、部分スキャンはツリーのものの複製
    name_hasher: tree::NameHasher,
}

/// 書き込みロックを使った処理の記録。
struct LockedRecord {
    lock_wait: std::time::Duration,
    apply: std::time::Duration,
    finished_at: std::time::SystemTime,
    /// ロックの中で読んだ、反映前の `generation`
    generation_before: u64,
    generation_after: u64,
}

impl Shared {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, QueueState> {
        lock_ignoring_poison(&self.state)
    }

    /// キューに残っている未実行の要求をすべて取り出し、待ち手全員に「停止」を通知する。
    fn notify_remaining_stopped(&self) {
        let items = self.lock_state().queue.drain_all();
        if !items.is_empty() {
            log::info!(
                "the scan service is stopping; {} queued scan(s) are not executed",
                items.len()
            );
        }
        for item in items {
            item.notify_stopped();
        }
    }

    /// スキャン実行スレッドの本体。停止が始まるまで、キューの先頭を 1 件ずつ実行する。
    fn run_scan_thread(&self) {
        loop {
            let item = {
                let mut state = self.lock_state();
                loop {
                    if state.stopping {
                        break None;
                    }
                    if let Some(item) = state.queue.pop_front() {
                        break Some(item);
                    }
                    state = self
                        .queue_changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            match item {
                Some(item) => self.process_item(item),
                None => break,
            }
        }
        // 実行中だった 1 件は完了している。残りは実行せず、停止を通知する
        self.notify_remaining_stopped();
    }

    /// キューから取り出した 1 件を処理する。パニックしても、その要求を失敗として通知して
    /// 次に進む（反映処理中のパニックは [`Shared::publish`] が `abort` する）。
    fn process_item(&self, item: request::QueueItem) {
        let execution_id = self
            .next_execution_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let started_at = std::time::SystemTime::now();
        let started = std::time::Instant::now();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute(execution_id, &item, started_at, started)
        }));
        let (executed, report) = match caught {
            Ok(result) => result,
            Err(payload) => {
                let reason = format!(
                    "the scan thread panicked while processing the scan: {}",
                    walk::panic_message(payload.as_ref())
                );
                log::error!("{reason}");
                self.failure_after_panic(execution_id, &item, started_at, started, reason)
            }
        };
        let report = std::sync::Arc::new(report);
        report.log_summary();
        let slot = match report.kind {
            report::ScanKind::Full => &self.last_full_report,
            report::ScanKind::Partial => &self.last_partial_report,
        };
        *lock_ignoring_poison(slot) = Some(std::sync::Arc::clone(&report));
        item.notify_executed(&executed, &report);
    }

    /// 1 件のスキャンを実行して、実際に走った対象とレポートを返す（設計書 6.3, 6.5, 6.6）。
    fn execute(
        &self,
        execution_id: u64,
        item: &request::QueueItem,
        started_at: std::time::SystemTime,
        started: std::time::Instant,
    ) -> (request::ScanTarget, report::ScanReport) {
        #[cfg(test)]
        self.test_gate.checkpoint();

        let plan = self.plan_scan(item);
        let walk_result = walk::run_walk(
            &self.root,
            &plan.walk_path,
            &item.settings,
            &plan.name_hasher,
            std::sync::Arc::new(report::ErrorRecorder::new()),
        );
        let walk::WalkResult {
            outcome,
            effective_path,
            parent_retries,
            worker_ioprio,
            errors,
            duration: walk_duration,
        } = walk_result;
        // 実際に走った対象。ルートまで遡った（または最初からルートの）なら、フルスキャン相当
        let executed = if effective_path.as_os_str().is_empty() {
            request::ScanTarget::full()
        } else {
            request::ScanTarget::partial(&effective_path).unwrap()
        };

        let mut dir_count: u64 = 0;
        let mut file_count: u64 = 0;
        let mut root_total_size: u128 = 0;
        let mut hash_collision_count: u64 = 0;
        let mut drop_old_tree = std::time::Duration::ZERO;
        let (outcome, locked) = match outcome {
            Ok(success) => {
                dir_count = success.counts.dir_count;
                file_count = success.counts.file_count;
                hash_collision_count = success.counts.hash_collision_count;
                root_total_size = success.node.total_size();
                let had_errors = errors.total_excluding_races() > 0;
                // メモリ確保が必要な処理（複製など）は、ロックを取る前に済ませる
                let scanned = if effective_path.as_os_str().is_empty() {
                    ScannedTree::Full(tree::FullScanTree {
                        root: success.node,
                        exclude_patterns: item.settings.exclude_patterns.clone(),
                        name_hasher: plan.name_hasher.clone(),
                    })
                } else {
                    ScannedTree::Partial {
                        effective_path: effective_path.clone(),
                        subtree: success.node,
                    }
                };
                let (locked, applied) = self.publish(scanned, had_errors);
                match applied {
                    Ok(retired) => {
                        let drop_started = std::time::Instant::now();
                        retired.release();
                        drop_old_tree = drop_started.elapsed();
                        let outcome = if had_errors {
                            report::ScanOutcome::SuccessWithErrors
                        } else {
                            report::ScanOutcome::Success
                        };
                        (outcome, locked)
                    }
                    Err(reason) => (report::ScanOutcome::Failed(reason), locked),
                }
            }
            Err(failure) => {
                let reason = failure.to_string();
                let locked = self.publish_failure(reason.clone());
                (report::ScanOutcome::Failed(reason), locked)
            }
        };

        let report = report::ScanReport {
            execution_id,
            request_ids: item.request_ids(),
            kind: executed.kind(),
            requested_path: item.target.path().map(std::path::Path::to_path_buf),
            effective_path,
            parent_retries,
            settings: item.settings.clone(),
            started_at,
            finished_at: locked.finished_at,
            durations: report::ScanDurations {
                total: started.elapsed(),
                walk: walk_duration,
                lock_wait: locked.lock_wait,
                apply: locked.apply,
                drop_old_tree,
            },
            worker_ioprio,
            dir_count,
            file_count,
            root_total_size,
            hash_collision_count,
            errors,
            generations: report::ScanGenerations {
                at_start: plan.generation_at_start,
                before_apply: locked.generation_before,
                after_apply: locked.generation_after,
            },
            outcome,
        };
        (executed, report)
    }

    /// 読み取りロックの中で、開始時の `generation`、格上げの判定、部分スキャンの走査対象と
    /// `NameHasher` の複製を取り、すぐにロックを手放す（設計書 6.3, 6.4, 6.6）。
    fn plan_scan(&self, item: &request::QueueItem) -> ScanPlan {
        let fresh_hasher = tree::NameHasher::new();
        let guard = self
            .tree
            .read()
            .unwrap_or_else(|_| abort_process("the tree lock is poisoned"));
        let generation_at_start = guard.generation();
        let executed =
            request::effective_target(&item.target, &item.settings, guard.exclude_patterns());
        let partial_plan = match &executed {
            request::ScanTarget::Full => None,
            request::ScanTarget::Partial(path) => guard
                .resolve_partial_target(path.as_path())
                .map(|resolved| (resolved.effective_path, guard.name_hasher().clone())),
        };
        drop(guard);
        match partial_plan {
            Some((walk_path, name_hasher)) => ScanPlan {
                generation_at_start,
                walk_path,
                name_hasher,
            },
            // フルスキャン。部分スキャンなのにツリーが未スキャンで解決できなかった場合も、
            // フルスキャンにする（通常は effective_target が先に格上げしている）
            None => ScanPlan {
                generation_at_start,
                walk_path: std::path::PathBuf::new(),
                name_hasher: fresh_hasher,
            },
        }
    }

    /// 走査の結果を、書き込みロックの中でツリーに反映する。
    ///
    /// ロックの中は [`apply_scanned`] だけで、パニックしたら `abort` する。
    /// 回復不能な [`tree::TreeError`] も `abort`。それ以外の `TreeError` はツリーが変更
    /// されていないので、失敗として記録し、理由を `Err` で返す。
    fn publish(
        &self,
        scanned: ScannedTree,
        scan_had_errors: bool,
    ) -> (LockedRecord, Result<RetiredTree, String>) {
        let wait_started = std::time::Instant::now();
        let mut guard = self
            .tree
            .write()
            .unwrap_or_else(|_| abort_process("the tree lock is poisoned"));
        let lock_wait = wait_started.elapsed();
        let apply_started = std::time::Instant::now();
        let generation_before = guard.generation();
        let finished_at = std::time::SystemTime::now();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let applied = apply_scanned(&mut guard, scanned, scan_had_errors, finished_at);
            if let Err(error) = &applied {
                if handle_apply_error(error) == ApplyErrorHandling::FailScan {
                    guard.record_failed_scan(
                        format!("failed to apply the scan result to the tree: {error}"),
                        finished_at,
                    );
                }
            }
            applied
        }));
        let generation_after = guard.generation();
        let apply = apply_started.elapsed();
        drop(guard);
        let locked = LockedRecord {
            lock_wait,
            apply,
            finished_at,
            generation_before,
            generation_after,
        };
        match caught {
            Err(payload) => abort_process(&format!(
                "panicked while applying the scan result to the tree: {}",
                walk::panic_message(payload.as_ref())
            )),
            Ok(Ok(retired)) => (locked, Ok(retired)),
            Ok(Err(error)) => match handle_apply_error(&error) {
                ApplyErrorHandling::AbortProcess => abort_process(&error.to_string()),
                ApplyErrorHandling::FailScan => (
                    locked,
                    Err(format!(
                        "failed to apply the scan result to the tree: {error}"
                    )),
                ),
            },
        }
    }

    /// ツリーに反映されなかったスキャン（失敗）を、書き込みロックの中で記録する。
    /// ツリーと `generation` は変わらない（未スキャンなら未スキャンのまま）。
    fn publish_failure(&self, reason: String) -> LockedRecord {
        let wait_started = std::time::Instant::now();
        let mut guard = self
            .tree
            .write()
            .unwrap_or_else(|_| abort_process("the tree lock is poisoned"));
        let lock_wait = wait_started.elapsed();
        let apply_started = std::time::Instant::now();
        let generation_before = guard.generation();
        let finished_at = std::time::SystemTime::now();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.record_failed_scan(reason, finished_at);
        }));
        let generation_after = guard.generation();
        let apply = apply_started.elapsed();
        drop(guard);
        if let Err(payload) = caught {
            abort_process(&format!(
                "panicked while recording a failed scan: {}",
                walk::panic_message(payload.as_ref())
            ));
        }
        LockedRecord {
            lock_wait,
            apply,
            finished_at,
            generation_before,
            generation_after,
        }
    }

    /// 処理中にスキャン実行スレッドがパニックしたときの、失敗のレポート。
    /// どこまで進んだかは分からないので、要求の対象をそのまま「実行された対象」とする。
    fn failure_after_panic(
        &self,
        execution_id: u64,
        item: &request::QueueItem,
        started_at: std::time::SystemTime,
        started: std::time::Instant,
        reason: String,
    ) -> (request::ScanTarget, report::ScanReport) {
        let locked = self.publish_failure(reason.clone());
        let executed = item.target.clone();
        let report = report::ScanReport {
            execution_id,
            request_ids: item.request_ids(),
            kind: executed.kind(),
            requested_path: item.target.path().map(std::path::Path::to_path_buf),
            effective_path: item
                .target
                .path()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default(),
            parent_retries: Vec::new(),
            settings: item.settings.clone(),
            started_at,
            finished_at: locked.finished_at,
            durations: report::ScanDurations {
                total: started.elapsed(),
                walk: std::time::Duration::ZERO,
                lock_wait: locked.lock_wait,
                apply: locked.apply,
                drop_old_tree: std::time::Duration::ZERO,
            },
            worker_ioprio: Vec::new(),
            dir_count: 0,
            file_count: 0,
            root_total_size: 0,
            hash_collision_count: 0,
            errors: report::ErrorCounts::new(),
            generations: report::ScanGenerations {
                at_start: locked.generation_before,
                before_apply: locked.generation_before,
                after_apply: locked.generation_after,
            },
            outcome: report::ScanOutcome::Failed(reason),
        };
        (executed, report)
    }
}

/// テスト用の関門。スキャン実行スレッドを、1 件の処理の開始時に止めたり、パニックさせたり
/// できる（要求の統合や停止を、時間に頼らず決定的にテストするため）。
#[cfg(test)]
struct TestGate {
    state: std::sync::Mutex<TestGateState>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct TestGateState {
    /// 次の処理を止める
    armed: bool,
    /// スキャン実行スレッドが止まっている
    reached: bool,
    /// 次の処理でパニックする
    panic_next: bool,
}

#[cfg(test)]
impl TestGate {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(TestGateState::default()),
            changed: std::sync::Condvar::new(),
        }
    }

    /// 次にスキャン実行スレッドが 1 件の処理を始めるときに止める。
    fn arm(&self) {
        let mut state = lock_ignoring_poison(&self.state);
        state.armed = true;
        state.reached = false;
    }

    fn arm_panic(&self) {
        lock_ignoring_poison(&self.state).panic_next = true;
    }

    /// スキャン実行スレッドが止まるまで待つ。
    fn wait_reached(&self) {
        let state = lock_ignoring_poison(&self.state);
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, std::time::Duration::from_secs(60), |s| !s.reached)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !timeout.timed_out() && state.reached,
            "the scan thread did not reach the gate"
        );
    }

    /// 止めたスキャン実行スレッドを再開する。
    fn release(&self) {
        let mut state = lock_ignoring_poison(&self.state);
        state.armed = false;
        state.reached = false;
        self.changed.notify_all();
    }

    /// スキャン実行スレッドから、1 件の処理の開始時に呼ぶ。
    fn checkpoint(&self) {
        let mut state = lock_ignoring_poison(&self.state);
        if state.panic_next {
            state.panic_next = false;
            drop(state);
            panic!("injected panic in the scan thread");
        }
        if state.armed {
            state.reached = true;
            self.changed.notify_all();
            while state.armed {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_logger() {
        env_logger::builder().is_test(true).try_init().ok();
    }

    fn is_root() -> bool {
        // SAFETY: geteuid は引数を取らず、失敗しない。
        unsafe { libc::geteuid() == 0 }
    }

    fn settings(parallelism: usize, patterns: &[&str]) -> settings::ScanSettings {
        settings_with_io(
            parallelism,
            settings::IoPriority::best_effort(4).unwrap(),
            patterns,
        )
    }

    fn settings_with_io(
        parallelism: usize,
        io_priority: settings::IoPriority,
        patterns: &[&str],
    ) -> settings::ScanSettings {
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        settings::ScanSettings::new(
            settings::Parallelism::new(parallelism).unwrap(),
            io_priority,
            settings::ExcludePatterns::new(&owned).unwrap(),
        )
    }

    /// 除外パターン `**/.*` の設定（並列度 2）。
    fn dot_excluded() -> settings::ScanSettings {
        settings(2, &["**/.*"])
    }

    fn make_file(root: &std::path::Path, relative: &str, size: u64) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(size).unwrap();
    }

    /// 一時ディレクトリにツリーを作る。
    ///
    /// ```text
    /// f1 (10)
    /// a/f2 (20) a/f3 (30) a/.hidden (7)
    /// a/b/f4 (40) a/b/file.txt (5)
    /// c/f5 (60)
    /// empty/
    /// ```
    fn sample_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        make_file(dir.path(), "f1", 10);
        make_file(dir.path(), "a/f2", 20);
        make_file(dir.path(), "a/f3", 30);
        make_file(dir.path(), "a/.hidden", 7);
        make_file(dir.path(), "a/b/f4", 40);
        make_file(dir.path(), "a/b/file.txt", 5);
        make_file(dir.path(), "c/f5", 60);
        std::fs::create_dir_all(dir.path().join("empty")).unwrap();
        dir
    }

    fn wait_completion(ticket: request::Ticket) -> request::Completion {
        ticket
            .wait_timeout(std::time::Duration::from_secs(60))
            .unwrap()
            .expect("timed out while waiting for the completion")
    }

    fn run_scan(
        service: &ScanService,
        target: request::ScanTarget,
        settings: settings::ScanSettings,
    ) -> request::Completion {
        wait_completion(service.request_scan(target, settings).unwrap())
    }

    fn run_full(service: &ScanService, settings: settings::ScanSettings) -> request::Completion {
        run_scan(service, request::ScanTarget::full(), settings)
    }

    fn run_partial(
        service: &ScanService,
        path: &str,
        settings: settings::ScanSettings,
    ) -> request::Completion {
        run_scan(
            service,
            request::ScanTarget::partial(std::path::Path::new(path)).unwrap(),
            settings,
        )
    }

    /// ディレクトリごとの (own_size, total_size, complete)。
    type Snapshot = std::collections::BTreeMap<std::path::PathBuf, (u128, u128, bool)>;

    fn snapshot(service: &ScanService) -> Snapshot {
        let mut result = Snapshot::new();
        let mut pending = vec![std::path::PathBuf::new()];
        while let Some(path) = pending.pop() {
            match service.read_directory(&path).unwrap().directory {
                tree::DirectoryLookup::Found(read) => {
                    for child in &read.children {
                        pending.push(path.join(&child.name));
                    }
                    result.insert(path, (read.own_size, read.total_size, read.complete));
                }
                tree::DirectoryLookup::NotFound => panic!("directory not found: {path:?}"),
            }
        }
        result
    }

    /// 別のサービスで同じ状態をフルスキャンし直した結果。
    fn fresh_full_snapshot(root: &std::path::Path, settings: settings::ScanSettings) -> Snapshot {
        let service = ScanService::start(root.to_path_buf()).unwrap();
        let completion = run_full(&service, settings);
        assert_eq!(completion.disposition, request::Disposition::Executed);
        let result = snapshot(&service);
        service.shutdown();
        result
    }

    fn report_of(completion: &request::Completion) -> &report::ScanReport {
        completion.report.as_ref().unwrap()
    }

    #[test]
    fn full_scan_builds_the_tree_and_the_report() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        let before = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(before.status.snapshot, tree::SnapshotState::Unscanned);
        assert_eq!(before.status.generation, 0);
        assert_eq!(before.directory, tree::DirectoryLookup::NotFound);
        assert!(service.last_full_report().is_none());
        assert!(service.last_partial_report().is_none());

        let ticket = service.request_full_scan(dot_excluded()).unwrap();
        let request_id = ticket.request_id();
        let completion = wait_completion(ticket);
        assert_eq!(completion.request_id, request_id);
        assert_eq!(completion.disposition, request::Disposition::Executed);
        assert_eq!(completion.requested, request::ScanTarget::full());
        assert_eq!(completion.executed, Some(request::ScanTarget::full()));
        let report = report_of(&completion);
        assert_eq!(Some(report.execution_id), completion.execution_id);
        assert_eq!(report.request_ids, vec![request_id]);
        assert_eq!(report.kind, report::ScanKind::Full);
        assert_eq!(report.requested_path, None);
        assert!(report.effective_path.as_os_str().is_empty());
        assert!(report.parent_retries.is_empty());
        assert_eq!(report.settings, dot_excluded());
        assert_eq!(report.outcome, report::ScanOutcome::Success);
        // root, a, a/b, c, empty
        assert_eq!(report.dir_count, 5);
        // f1, f2, f3, f4, file.txt, f5（.hidden は除外）
        assert_eq!(report.file_count, 6);
        assert_eq!(report.root_total_size, 10 + 20 + 30 + 40 + 5 + 60);
        assert_eq!(report.hash_collision_count, 0);
        assert_eq!(report.errors.total(), 0);
        assert_eq!(
            report.generations,
            report::ScanGenerations {
                at_start: 0,
                before_apply: 0,
                after_apply: 1
            }
        );
        assert!(report.started_at <= report.finished_at);
        assert!(report.durations.total >= report.durations.walk);
        assert!(report.durations.total >= report.durations.apply);

        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.snapshot, tree::SnapshotState::Complete);
        assert_eq!(read.status.generation, 1);
        assert_eq!(
            read.status.last_scan.as_ref().unwrap().outcome,
            report::ScanOutcome::Success
        );
        let tree::DirectoryLookup::Found(root) = read.directory else {
            panic!("root not found");
        };
        assert_eq!(root.own_size, 10);
        assert_eq!(root.total_size, 165);
        assert!(root.complete);
        let names: Vec<&std::ffi::OsStr> =
            root.children.iter().map(|c| c.name.as_os_str()).collect();
        assert_eq!(
            names,
            vec![
                std::ffi::OsStr::new("a"),
                std::ffi::OsStr::new("c"),
                std::ffi::OsStr::new("empty")
            ]
        );
        let sizes: Vec<u128> = root.children.iter().map(|c| c.total_size).collect();
        assert_eq!(sizes, vec![95, 60, 0]);
        let a = service.read_directory(std::path::Path::new("a")).unwrap();
        let tree::DirectoryLookup::Found(a) = a.directory else {
            panic!("a not found");
        };
        assert_eq!((a.own_size, a.total_size), (50, 95));
        let missing = service
            .read_directory(std::path::Path::new("nothing"))
            .unwrap();
        assert_eq!(missing.directory, tree::DirectoryLookup::NotFound);

        let last = service.last_full_report().unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &last,
            completion.report.as_ref().unwrap()
        ));
        assert!(service.last_partial_report().is_none());
        service.shutdown();
    }

    #[test]
    fn invalid_partial_path_is_rejected_immediately() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        for path in ["", "/abs", "../x", "a/../b"] {
            assert!(
                service
                    .request_partial_scan(std::path::Path::new(path), dot_excluded())
                    .is_err(),
                "{path:?}"
            );
        }
        assert!(service.shared.lock_state().queue.is_empty());
        service.shutdown();
    }

    #[test]
    fn partial_scan_equals_full_rescan_after_changes() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());

        // a の配下を変更する：サイズ変更、削除、追加、新しいディレクトリ
        make_file(dir.path(), "a/f2", 2000);
        std::fs::remove_file(dir.path().join("a/f3")).unwrap();
        make_file(dir.path(), "a/b/new_file", 123);
        make_file(dir.path(), "a/newdir/n1", 1);
        make_file(dir.path(), "a/newdir/deep/n2", 2);
        // a の外の変更は、部分スキャンでは反映されない
        make_file(dir.path(), "c/f5", 6000);

        let completion = run_partial(&service, "a", dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Executed);
        assert_eq!(
            completion.executed,
            Some(request::ScanTarget::partial(std::path::Path::new("a")).unwrap())
        );
        let report = report_of(&completion);
        assert_eq!(report.kind, report::ScanKind::Partial);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new("a"))
        );
        assert_eq!(report.effective_path, std::path::PathBuf::from("a"));
        assert_eq!(
            report.generations,
            report::ScanGenerations {
                at_start: 1,
                before_apply: 1,
                after_apply: 2
            }
        );
        let partial_total = snapshot(&service)[std::path::Path::new("")].1;
        // c は古いまま：10 + (2000 + 40 + 5 + 123 + 1 + 2) + 60
        assert_eq!(partial_total, 10 + 2171 + 60);
        assert!(std::sync::Arc::ptr_eq(
            &service.last_partial_report().unwrap(),
            completion.report.as_ref().unwrap()
        ));

        // c も反映してから、フルスキャンし直した結果と一致する
        run_partial(&service, "c", dot_excluded());
        assert_eq!(
            snapshot(&service),
            fresh_full_snapshot(dir.path(), dot_excluded())
        );
        service.shutdown();
    }

    /// 6.6 の表の各ケース：変更してから `requested` を部分スキャンし、実効パスと、
    /// フルスキャンし直した結果との一致を確かめる。
    fn check_partial_case(
        prepare_after_full: impl Fn(&std::path::Path),
        requested: &str,
        expected_effective: &str,
    ) {
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        prepare_after_full(dir.path());
        let completion = run_partial(&service, requested, dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Executed);
        let report = report_of(&completion);
        assert_eq!(report.outcome, report::ScanOutcome::Success);
        assert_eq!(report.kind, report::ScanKind::Partial);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new(requested))
        );
        assert_eq!(
            report.effective_path,
            std::path::PathBuf::from(expected_effective)
        );
        assert!(report.parent_retries.is_empty());
        assert_eq!(
            completion.executed,
            Some(request::ScanTarget::partial(std::path::Path::new(expected_effective)).unwrap())
        );
        assert_eq!(
            snapshot(&service),
            fresh_full_snapshot(dir.path(), dot_excluded())
        );
        service.shutdown();
    }

    #[test]
    fn partial_case_existing_directory() {
        init_logger();
        check_partial_case(|root| make_file(root, "a/b/f4", 4000), "a/b", "a/b");
    }

    #[test]
    fn partial_case_path_under_a_new_directory_resolves_to_the_parent() {
        init_logger();
        check_partial_case(|root| make_file(root, "a/new/x/file", 99), "a/new/x", "a");
    }

    #[test]
    fn partial_case_file_resolves_to_its_directory() {
        init_logger();
        check_partial_case(
            |root| make_file(root, "a/b/file.txt", 5555),
            "a/b/file.txt",
            "a/b",
        );
    }

    #[test]
    fn partial_case_excluded_path_resolves_to_the_parent() {
        init_logger();
        check_partial_case(
            |root| {
                make_file(root, "a/.cache/x", 1000);
                make_file(root, "a/f2", 2);
            },
            "a/.cache/x",
            "a",
        );
    }

    #[test]
    fn partial_case_symlink_resolves_to_the_parent() {
        init_logger();
        check_partial_case(
            |root| {
                std::os::unix::fs::symlink(root.join("c"), root.join("a/link")).unwrap();
                make_file(root, "a/f2", 3);
            },
            "a/link/f5",
            "a",
        );
    }

    #[test]
    fn partial_case_directory_replaced_by_a_file_retries_with_the_parent() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        std::fs::remove_dir_all(dir.path().join("a/b")).unwrap();
        make_file(dir.path(), "a/b", 777);
        let completion = run_partial(&service, "a/b", dot_excluded());
        let report = report_of(&completion);
        assert_eq!(report.outcome, report::ScanOutcome::Success);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new("a/b"))
        );
        assert_eq!(report.effective_path, std::path::PathBuf::from("a"));
        assert_eq!(report.parent_retries.len(), 1);
        assert_eq!(
            report.parent_retries[0].failed_path,
            std::path::PathBuf::from("a/b")
        );
        assert_eq!(report.parent_retries[0].errno, libc::ENOTDIR);
        assert_eq!(
            report.parent_retries[0].retry_path,
            std::path::PathBuf::from("a")
        );
        assert_eq!(
            completion.executed,
            Some(request::ScanTarget::partial(std::path::Path::new("a")).unwrap())
        );
        assert_eq!(
            snapshot(&service),
            fresh_full_snapshot(dir.path(), dot_excluded())
        );
        service.shutdown();
    }

    #[test]
    fn partial_case_vanished_directory_retries_with_the_parent() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        std::fs::remove_dir_all(dir.path().join("a/b")).unwrap();
        let completion = run_partial(&service, "a/b", dot_excluded());
        let report = report_of(&completion);
        assert_eq!(report.outcome, report::ScanOutcome::Success);
        assert_eq!(report.effective_path, std::path::PathBuf::from("a"));
        assert_eq!(report.parent_retries.len(), 1);
        assert_eq!(report.parent_retries[0].errno, libc::ENOENT);
        assert_eq!(
            snapshot(&service),
            fresh_full_snapshot(dir.path(), dot_excluded())
        );
        service.shutdown();
    }

    #[test]
    fn partial_scan_retrying_up_to_the_root_is_applied_as_a_full_scan() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        std::fs::remove_dir_all(dir.path().join("c")).unwrap();
        make_file(dir.path(), "g/new", 8);
        let completion = run_partial(&service, "c", dot_excluded());
        let report = report_of(&completion);
        assert_eq!(report.outcome, report::ScanOutcome::Success);
        // 実行された種類はフル、要求されたパスは残り、経緯が分かる
        assert_eq!(report.kind, report::ScanKind::Full);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new("c"))
        );
        assert!(report.effective_path.as_os_str().is_empty());
        assert_eq!(report.parent_retries.len(), 1);
        assert_eq!(
            report.parent_retries[0].failed_path,
            std::path::PathBuf::from("c")
        );
        assert_eq!(completion.executed, Some(request::ScanTarget::full()));
        assert_eq!(
            completion.requested,
            request::ScanTarget::partial(std::path::Path::new("c")).unwrap()
        );
        assert!(std::sync::Arc::ptr_eq(
            &service.last_full_report().unwrap(),
            completion.report.as_ref().unwrap()
        ));
        assert_eq!(
            report.generations,
            report::ScanGenerations {
                at_start: 1,
                before_apply: 1,
                after_apply: 2
            }
        );
        // ルートのノードが入れ替わり、g も載る
        assert_eq!(
            snapshot(&service),
            fresh_full_snapshot(dir.path(), dot_excluded())
        );
        service.shutdown();
    }

    #[test]
    fn unreadable_partial_target_fails_and_keeps_the_existing_subtree() {
        init_logger();
        if is_root() {
            return;
        }
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        let before = snapshot(&service);
        let unreadable = dir.path().join("a/b");
        std::fs::set_permissions(
            &unreadable,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o000),
        )
        .unwrap();
        // 先に変更しても、反映されない
        let completion = run_partial(&service, "a/b", dot_excluded());
        std::fs::set_permissions(
            &unreadable,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(completion.disposition, request::Disposition::Failed);
        let report = report_of(&completion);
        assert!(matches!(report.outcome, report::ScanOutcome::Failed(_)));
        assert_eq!(report.errors.errno_count(libc::EACCES), 1);
        assert!(report.parent_retries.is_empty());
        assert_eq!(report.generations.after_apply, 1);
        assert_eq!(snapshot(&service), before);
        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.generation, 1);
        assert_eq!(read.status.snapshot, tree::SnapshotState::Complete);
        assert!(matches!(
            read.status.last_scan.unwrap().outcome,
            report::ScanOutcome::Failed(_)
        ));
        service.shutdown();
    }

    #[test]
    fn unreadable_directory_makes_ancestors_incomplete() {
        init_logger();
        if is_root() {
            return;
        }
        let dir = sample_tree();
        let unreadable = dir.path().join("a/b");
        std::fs::set_permissions(
            &unreadable,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o000),
        )
        .unwrap();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        let completion = run_full(&service, dot_excluded());
        std::fs::set_permissions(
            &unreadable,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
        )
        .unwrap();
        let report = report_of(&completion);
        assert_eq!(report.outcome, report::ScanOutcome::SuccessWithErrors);
        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.snapshot, tree::SnapshotState::Incomplete);
        let tree::DirectoryLookup::Found(root) = read.directory else {
            panic!("root not found");
        };
        assert!(!root.complete);
        let complete_by_name: Vec<(String, bool)> = root
            .children
            .iter()
            .map(|c| (c.name.to_string_lossy().into_owned(), c.complete))
            .collect();
        assert_eq!(
            complete_by_name,
            vec![
                ("a".to_string(), false),
                ("c".to_string(), true),
                ("empty".to_string(), true)
            ]
        );
        service.shutdown();
    }

    #[test]
    fn partial_scan_on_an_unscanned_tree_is_upgraded_to_full() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        let completion = run_partial(&service, "a/b", dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Executed);
        assert_eq!(
            completion.requested,
            request::ScanTarget::partial(std::path::Path::new("a/b")).unwrap()
        );
        assert_eq!(completion.executed, Some(request::ScanTarget::full()));
        let report = report_of(&completion);
        assert_eq!(report.kind, report::ScanKind::Full);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new("a/b"))
        );
        assert!(report.effective_path.as_os_str().is_empty());
        assert!(report.parent_retries.is_empty());
        assert_eq!(report.root_total_size, 165);
        assert_eq!(report.generations.after_apply, 1);
        assert_eq!(
            snapshot(&service),
            fresh_full_snapshot(dir.path(), dot_excluded())
        );
        service.shutdown();
    }

    #[test]
    fn partial_scan_with_different_exclude_patterns_is_upgraded_to_full() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, settings(2, &[]));
        // 除外なしのツリーには .hidden が載っている
        assert_eq!(snapshot(&service)[std::path::Path::new("a")].0, 57);
        let completion = run_partial(&service, "a", dot_excluded());
        assert_eq!(completion.executed, Some(request::ScanTarget::full()));
        let report = report_of(&completion);
        assert_eq!(report.kind, report::ScanKind::Full);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new("a"))
        );
        assert_eq!(report.generations.after_apply, 2);
        // ツリーの除外パターンが新しいものに変わり、.hidden は消えた
        assert_eq!(snapshot(&service)[std::path::Path::new("a")].0, 50);
        {
            let guard = service.shared.tree.read().unwrap();
            assert_eq!(
                guard.exclude_patterns(),
                Some(&dot_excluded().exclude_patterns)
            );
        }
        // 以後、同じ除外パターンの部分スキャンは格上げされない
        let completion = run_partial(&service, "a", dot_excluded());
        assert_eq!(report_of(&completion).kind, report::ScanKind::Partial);
        service.shutdown();
    }

    #[test]
    fn requests_are_merged_while_a_scan_is_running_and_upgraded_together() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        let x = settings(2, &[]);
        let y = settings(1, &["**/.*"]);

        // 先行のフルスキャン F（設定 Y）をスキャン実行スレッドで止めておく
        service.shared.test_gate.arm();
        let ticket_f = service.request_full_scan(y).unwrap();
        service.shared.test_gate.wait_reached();
        // F は実行中なのでキューにはない。以後の要求は F に統合されない
        assert_eq!(service.shared.lock_state().queue.len(), 0);

        let ticket_a = service
            .request_partial_scan(std::path::Path::new("a/b"), x.clone())
            .unwrap();
        let ticket_b = service
            .request_partial_scan(std::path::Path::new("a"), x.clone())
            .unwrap();
        let ticket_c = service
            .request_partial_scan(std::path::Path::new("a/b"), x.clone())
            .unwrap();
        // A・B・C は 1 つの要素に統合されている
        assert_eq!(service.shared.lock_state().queue.len(), 1);
        let (id_a, id_b, id_c) = (
            ticket_a.request_id(),
            ticket_b.request_id(),
            ticket_c.request_id(),
        );
        service.shared.test_gate.release();

        let completion_f = wait_completion(ticket_f);
        let completion_a = wait_completion(ticket_a);
        let completion_b = wait_completion(ticket_b);
        let completion_c = wait_completion(ticket_c);

        assert_eq!(completion_f.disposition, request::Disposition::Executed);
        assert_eq!(completion_f.executed, Some(request::ScanTarget::full()));
        let execution_f = completion_f.execution_id.unwrap();

        // 3 つとも同じ実行（同じ execution_id、同じレポート）
        let execution_abc = completion_a.execution_id.unwrap();
        assert_ne!(execution_abc, execution_f);
        assert_eq!(completion_b.execution_id, Some(execution_abc));
        assert_eq!(completion_c.execution_id, Some(execution_abc));
        assert!(std::sync::Arc::ptr_eq(
            completion_a.report.as_ref().unwrap(),
            completion_b.report.as_ref().unwrap()
        ));
        assert!(std::sync::Arc::ptr_eq(
            completion_a.report.as_ref().unwrap(),
            completion_c.report.as_ref().unwrap()
        ));
        // B が A を飲み込んだ要素の代表。A と C は B に統合された
        assert_eq!(completion_b.disposition, request::Disposition::Executed);
        assert_eq!(
            completion_a.disposition,
            request::Disposition::Merged {
                into_request_id: id_b
            }
        );
        assert_eq!(
            completion_c.disposition,
            request::Disposition::Merged {
                into_request_id: id_b
            }
        );
        // 要求された種類はそれぞれの要求のもの
        let partial =
            |path: &str| request::ScanTarget::partial(std::path::Path::new(path)).unwrap();
        assert_eq!(completion_a.requested, partial("a/b"));
        assert_eq!(completion_b.requested, partial("a"));
        assert_eq!(completion_c.requested, partial("a/b"));
        // ツリーは F の除外パターン（Y）で、X の部分スキャンは格上げされてフルとして実行された
        for completion in [&completion_a, &completion_b, &completion_c] {
            assert_eq!(completion.executed, Some(request::ScanTarget::full()));
        }
        let report = report_of(&completion_b);
        assert_eq!(report.request_ids, vec![id_a, id_b, id_c]);
        assert_eq!(report.kind, report::ScanKind::Full);
        assert_eq!(
            report.requested_path.as_deref(),
            Some(std::path::Path::new("a"))
        );
        assert_eq!(report.settings, x);
        assert_eq!(report.generations.at_start, 1);
        assert_eq!(report.generations.after_apply, 2);
        // X は除外なしなので .hidden を含む
        assert_eq!(report.root_total_size, 172);
        service.shutdown();
    }

    #[test]
    fn merged_partial_requests_run_as_one_partial_scan_without_upgrade() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        make_file(dir.path(), "a/b/f4", 400);

        service.shared.test_gate.arm();
        let ticket_blocker = service
            .request_partial_scan(std::path::Path::new("c"), dot_excluded())
            .unwrap();
        service.shared.test_gate.wait_reached();
        // 設定が違う要求は統合されない
        let other = settings(3, &["**/.*"]);
        let ticket_a = service
            .request_partial_scan(std::path::Path::new("a/b"), dot_excluded())
            .unwrap();
        let ticket_other = service
            .request_partial_scan(std::path::Path::new("a/b"), other)
            .unwrap();
        let ticket_b = service
            .request_partial_scan(std::path::Path::new("a"), dot_excluded())
            .unwrap();
        assert_eq!(service.shared.lock_state().queue.len(), 2);
        service.shared.test_gate.release();

        wait_completion(ticket_blocker);
        let completion_a = wait_completion(ticket_a);
        let completion_other = wait_completion(ticket_other);
        let completion_b = wait_completion(ticket_b);
        // A は B に飲み込まれて、A の位置で（other より先に）実行された
        assert_eq!(completion_a.execution_id, completion_b.execution_id);
        assert!(completion_a.execution_id.unwrap() < completion_other.execution_id.unwrap());
        let a_path = request::ScanTarget::partial(std::path::Path::new("a")).unwrap();
        assert_eq!(completion_a.executed, Some(a_path.clone()));
        assert_eq!(completion_b.executed, Some(a_path));
        assert_eq!(
            completion_a.disposition,
            request::Disposition::Merged {
                into_request_id: completion_b.request_id
            }
        );
        assert_eq!(completion_b.disposition, request::Disposition::Executed);
        assert_eq!(completion_other.disposition, request::Disposition::Executed);
        assert_eq!(
            completion_other.executed,
            Some(request::ScanTarget::partial(std::path::Path::new("a/b")).unwrap())
        );
        assert_eq!(report_of(&completion_b).kind, report::ScanKind::Partial);
        service.shutdown();
    }

    #[test]
    fn unopenable_root_fails_and_leaves_an_unscanned_tree_unscanned() {
        init_logger();
        let dir = tempfile::tempdir().unwrap();
        let service = ScanService::start(dir.path().join("missing")).unwrap();
        let completion = run_full(&service, dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Failed);
        assert_eq!(
            completion.execution_id,
            Some(report_of(&completion).execution_id)
        );
        let report = report_of(&completion);
        assert!(matches!(report.outcome, report::ScanOutcome::Failed(_)));
        assert_eq!(report.errors.errno_count(libc::ENOENT), 1);
        assert_eq!(
            report.generations,
            report::ScanGenerations {
                at_start: 0,
                before_apply: 0,
                after_apply: 0
            }
        );
        assert_eq!(report.dir_count, 0);
        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.snapshot, tree::SnapshotState::Unscanned);
        assert_eq!(read.status.generation, 0);
        assert!(matches!(
            read.status.last_scan.unwrap().outcome,
            report::ScanOutcome::Failed(_)
        ));
        assert_eq!(read.directory, tree::DirectoryLookup::NotFound);
        assert!(service.last_full_report().is_some());
        service.shutdown();
    }

    #[test]
    fn root_that_becomes_unopenable_keeps_the_previous_tree() {
        init_logger();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("root");
        std::fs::create_dir(&root).unwrap();
        make_file(&root, "a/f", 100);
        let service = ScanService::start(root.clone()).unwrap();
        run_full(&service, dot_excluded());
        let before = snapshot(&service);
        std::fs::remove_dir_all(&root).unwrap();
        let completion = run_full(&service, dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Failed);
        assert_eq!(report_of(&completion).generations.after_apply, 1);
        assert_eq!(snapshot(&service), before);
        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.generation, 1);
        assert_eq!(read.status.snapshot, tree::SnapshotState::Complete);
        assert!(matches!(
            read.status.last_scan.unwrap().outcome,
            report::ScanOutcome::Failed(_)
        ));
        // 回復したら次のスキャンは成功する
        std::fs::create_dir(&root).unwrap();
        make_file(&root, "b/g", 5);
        let completion = run_full(&service, dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Executed);
        assert_eq!(
            service
                .read_directory(std::path::Path::new(""))
                .unwrap()
                .status
                .generation,
            2
        );
        service.shutdown();
    }

    #[test]
    fn worker_ioprio_is_recorded_for_idle_and_best_effort() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        for (parallelism, io_priority) in [
            (3, settings::IoPriority::Idle),
            (2, settings::IoPriority::best_effort(2).unwrap()),
        ] {
            let completion = run_full(&service, settings_with_io(parallelism, io_priority, &[]));
            let report = report_of(&completion);
            assert_eq!(report.worker_ioprio.len(), parallelism);
            for (index, worker) in report.worker_ioprio.iter().enumerate() {
                assert_eq!(worker.worker_index, index);
                assert_eq!(worker.requested, io_priority.to_ioprio_value());
                assert_eq!(worker.set, report::IoprioSetResult::Success);
                assert_eq!(
                    worker.read_back,
                    report::IoprioGetResult::Value(io_priority.to_ioprio_value())
                );
                assert_eq!(worker.status(), report::IoprioStatus::Ok);
            }
        }
        service.shutdown();
    }

    #[test]
    fn shutdown_completes_the_running_scan_and_stops_the_queued_ones() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        service.shared.test_gate.arm();
        let ticket_running = service.request_full_scan(settings(1, &[])).unwrap();
        service.shared.test_gate.wait_reached();
        // 設定が違うので統合されず、3 要素が残る（うち 1 つは 2 件の待ち手）
        let ticket_q1 = service.request_full_scan(settings(2, &[])).unwrap();
        let ticket_q2 = service
            .request_partial_scan(std::path::Path::new("a"), settings(3, &[]))
            .unwrap();
        let ticket_q3 = service.request_full_scan(settings(2, &[])).unwrap();
        assert_eq!(service.shared.lock_state().queue.len(), 2);

        std::thread::scope(|scope| {
            let stopper = scope.spawn(|| service.shutdown());
            // 停止が始まってから実行中の 1 件を再開する
            while !service.shared.lock_state().stopping {
                std::thread::yield_now();
            }
            // 新しい要求は受け付けない
            assert!(service.request_full_scan(settings(5, &[])).is_err());
            service.shared.test_gate.release();
            stopper.join().unwrap();
        });

        let running = wait_completion(ticket_running);
        assert_eq!(running.disposition, request::Disposition::Executed);
        assert_eq!(report_of(&running).outcome, report::ScanOutcome::Success);
        for (ticket, expected_requested) in [
            (ticket_q1, request::ScanTarget::full()),
            (
                ticket_q2,
                request::ScanTarget::partial(std::path::Path::new("a")).unwrap(),
            ),
            (ticket_q3, request::ScanTarget::full()),
        ] {
            let completion = wait_completion(ticket);
            assert_eq!(completion.disposition, request::Disposition::Stopped);
            assert_eq!(completion.execution_id, None);
            assert_eq!(completion.executed, None);
            assert!(completion.report.is_none());
            assert_eq!(completion.requested, expected_requested);
        }
        // 停止後の要求は Err、二重の停止も安全
        assert!(service.request_full_scan(settings(1, &[])).is_err());
        service.shutdown();
        service.shutdown();
        // 実行された 1 件だけが反映されている
        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.generation, 1);
    }

    #[test]
    fn drop_stops_the_service_and_notifies_queued_requests() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        service.shared.test_gate.arm();
        let ticket_running = service.request_full_scan(settings(1, &[])).unwrap();
        service.shared.test_gate.wait_reached();
        let ticket_queued = service.request_full_scan(settings(2, &[])).unwrap();
        let shared = std::sync::Arc::clone(&service.shared);
        std::thread::scope(|scope| {
            let dropper = scope.spawn(move || drop(service));
            while !shared.lock_state().stopping {
                std::thread::yield_now();
            }
            shared.test_gate.release();
            dropper.join().unwrap();
        });
        assert_eq!(
            wait_completion(ticket_running).disposition,
            request::Disposition::Executed
        );
        assert_eq!(
            wait_completion(ticket_queued).disposition,
            request::Disposition::Stopped
        );
    }

    #[test]
    fn shutdown_without_requests_returns_and_is_idempotent() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        service.shutdown();
        service.shutdown();
        assert!(service.request_full_scan(dot_excluded()).is_err());
    }

    #[test]
    fn panic_in_the_scan_thread_fails_the_request_and_the_thread_continues() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        service.shared.test_gate.arm_panic();
        let completion = run_partial(&service, "a", dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Failed);
        let report = report_of(&completion);
        let report::ScanOutcome::Failed(reason) = &report.outcome else {
            panic!("outcome should be failed: {:?}", report.outcome);
        };
        assert!(reason.contains("injected panic"), "{reason}");
        assert_eq!(report.request_ids, vec![completion.request_id]);
        let read = service.read_directory(std::path::Path::new("")).unwrap();
        assert_eq!(read.status.generation, 1);
        assert!(matches!(
            read.status.last_scan.unwrap().outcome,
            report::ScanOutcome::Failed(_)
        ));
        // 次の要求は処理される
        let completion = run_partial(&service, "a", dot_excluded());
        assert_eq!(completion.disposition, request::Disposition::Executed);
        assert_eq!(report_of(&completion).generations.after_apply, 2);
        service.shutdown();
    }

    #[test]
    fn only_inconsistent_tree_errors_abort_the_process() {
        init_logger();
        assert_eq!(
            handle_apply_error(&tree::TreeError::Inconsistent("x".to_string())),
            ApplyErrorHandling::AbortProcess
        );
        assert_eq!(
            handle_apply_error(&tree::TreeError::GenerationOverflow),
            ApplyErrorHandling::AbortProcess
        );
        assert_eq!(
            handle_apply_error(&tree::TreeError::NotScanned),
            ApplyErrorHandling::FailScan
        );
        assert_eq!(
            handle_apply_error(&tree::TreeError::PathNotFound(std::path::PathBuf::from(
                "a"
            ))),
            ApplyErrorHandling::FailScan
        );
        assert_eq!(
            handle_apply_error(&tree::TreeError::DuplicateName(std::ffi::OsString::from(
                "a"
            ))),
            ApplyErrorHandling::FailScan
        );
    }

    #[test]
    fn non_fatal_apply_error_leaves_the_tree_unchanged() {
        init_logger();
        // 未スキャンのツリーに部分木を反映しようとすると NotScanned（致命的ではない）
        let mut target = tree::Tree::new();
        let result = apply_scanned(
            &mut target,
            ScannedTree::Partial {
                effective_path: std::path::PathBuf::from("a"),
                subtree: tree::DirNode::new(std::ffi::OsString::from("a"), 1, 1),
            },
            false,
            std::time::SystemTime::now(),
        );
        assert_eq!(result.err(), Some(tree::TreeError::NotScanned));
        assert_eq!(target.generation(), 0);
        assert_eq!(target.snapshot_state(), tree::SnapshotState::Unscanned);
    }

    #[test]
    fn readers_are_not_blocked_forever_while_scans_are_applied() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let mut readers = Vec::new();
            for _ in 0..4 {
                readers.push(scope.spawn(|| {
                    let mut last_generation: u64 = 0;
                    let mut reads: u64 = 0;
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        let read = service.read_directory(std::path::Path::new("a")).unwrap();
                        assert!(read.status.generation >= last_generation);
                        last_generation = read.status.generation;
                        reads += 1;
                    }
                    reads
                }));
            }
            for round in 0..20 {
                let completion = if round % 2 == 0 {
                    run_full(&service, dot_excluded())
                } else {
                    run_partial(&service, "a", dot_excluded())
                };
                assert_eq!(completion.disposition, request::Disposition::Executed);
            }
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
            for reader in readers {
                assert!(reader.join().unwrap() > 0);
            }
        });
        assert_eq!(
            service
                .read_directory(std::path::Path::new(""))
                .unwrap()
                .status
                .generation,
            21
        );
        service.shutdown();
    }

    #[test]
    fn scan_waits_for_a_reader_holding_the_lock_and_then_applies() {
        init_logger();
        let dir = sample_tree();
        let service = ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&service, dot_excluded());
        let guard = service.shared.tree.read().unwrap();
        let ticket = service
            .request_partial_scan(std::path::Path::new("a"), dot_excluded())
            .unwrap();
        // 読み取りロックを持っている間は反映できないので、完了は届かない
        assert!(ticket
            .wait_timeout(std::time::Duration::from_millis(300))
            .unwrap()
            .is_none());
        drop(guard);
        let completion = wait_completion(ticket);
        assert_eq!(completion.disposition, request::Disposition::Executed);
        assert!(
            report_of(&completion).durations.lock_wait >= std::time::Duration::from_millis(100)
        );
        service.shutdown();
    }
}
