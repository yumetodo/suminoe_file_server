//! スキャン要求の検証、統合規則（キュー）、チケットと完了通知（設計書 6.2, 6.3）。
//!
//! このモジュールはファイルシステムに触れない純粋なロジックだけを持つ。
//! `request_id`・`execution_id` の発番はサービス側の責務で、ここでは `u64` で受け取る。

/// 部分スキャンの対象パス。ルートからの相対パスで、検証・正規化済み。
///
/// 検証と正規化の方針（設計書 6.3）:
/// - 絶対パス、`..` を含むパス、空のパスは拒否する。
/// - `.` の成分、重複した `/`、末尾の `/` は取り除く（`./a//b/./` は `a/b`）。
///   ここで取り除いた結果が空になるパス（`.` や `./`）は空のパスとして拒否する。
///   ルート全体を読み直したい場合はフルスキャンを要求する。
/// - ファイルシステムには触れないので、シンボリックリンクや存在の確認はしない。
///   非 UTF-8 の名前はそのまま持つ。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PartialPath(std::path::PathBuf);

impl PartialPath {
    /// 検証して正規化する。拒否する場合は `Err`。
    pub fn new(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut normalized = std::path::PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Normal(name) => normalized.push(name),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    anyhow::bail!(
                        "partial scan path must not contain '..': {}",
                        crate::dir_scan::report::escape_path_for_log(path)
                    );
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    anyhow::bail!(
                        "partial scan path must be relative to the root: {}",
                        crate::dir_scan::report::escape_path_for_log(path)
                    );
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            anyhow::bail!(
                "partial scan path must not be empty (use a full scan to rescan the root): {}",
                crate::dir_scan::report::escape_path_for_log(path)
            );
        }
        Ok(Self(normalized))
    }

    /// 正規化後のパス。
    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

/// スキャンの対象（種類と範囲）。要求された種類と実行された種類の両方に使う。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ScanTarget {
    /// ルート全体
    Full,
    /// ルートからの相対パスのサブツリー
    Partial(PartialPath),
}

impl ScanTarget {
    pub fn full() -> Self {
        Self::Full
    }

    /// 部分スキャンの対象を作る。パスの検証に失敗したら `Err`（[`PartialPath::new`]）。
    pub fn partial(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self::Partial(PartialPath::new(path)?))
    }

    pub fn kind(&self) -> crate::dir_scan::report::ScanKind {
        match self {
            Self::Full => crate::dir_scan::report::ScanKind::Full,
            Self::Partial(_) => crate::dir_scan::report::ScanKind::Partial,
        }
    }

    /// 部分スキャンなら対象のパス。フルスキャンなら `None`。
    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Full => None,
            Self::Partial(path) => Some(path.as_path()),
        }
    }

    /// `self` を実行すれば `other` の範囲も読み直せるか（設計書 6.3）。
    /// フルスキャンはすべてを含む。部分スキャンは、同じまたは配下のパスの部分スキャンを含む。
    /// パスの包含はコンポーネント単位で、`a/b` は `a` に含まれるが `ab` は `a` に含まれない。
    pub fn contains(&self, other: &ScanTarget) -> bool {
        match (self, other) {
            (Self::Full, _) => true,
            (Self::Partial(_), Self::Full) => false,
            (Self::Partial(outer), Self::Partial(inner)) => {
                inner.as_path().starts_with(outer.as_path())
            }
        }
    }
}

impl std::fmt::Display for ScanTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("full"),
            Self::Partial(path) => write!(
                f,
                "partial({})",
                crate::dir_scan::report::escape_path_for_log(path.as_path())
            ),
        }
    }
}

/// 要求の行き先（設計書 6.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// その要求として実行された
    Executed,
    /// 別の要求に統合されて実行された。`into_request_id` は統合先の要求
    Merged { into_request_id: u64 },
    /// 実行したがツリーに反映されなかった。統合された要求も、統合先と同じく失敗になる
    Failed,
    /// 実行される前にサービスが停止した
    Stopped,
}

/// 完了通知（設計書 6.2）。要求ごとに 1 つ届く。
#[derive(Clone, Debug)]
pub struct Completion {
    pub request_id: u64,
    pub disposition: Disposition,
    /// 実行された場合、そのスキャンの ID
    pub execution_id: Option<u64>,
    /// 要求された種類と対象
    pub requested: ScanTarget,
    /// 実際に走ったスキャンの種類と対象。実行されなかった（停止）なら `None`。
    /// `requested` が部分で、これがフルなら格上げされたことが分かる
    pub executed: Option<ScanTarget>,
    /// 実行された場合、そのスキャンのレポート（統合された要求の間で共有する）
    pub report: Option<std::sync::Arc<crate::dir_scan::report::ScanReport>>,
}

/// チケット。要求を積んだ呼び出し側が持ち、完了通知を待つ（設計書 6.1）。
#[derive(Debug)]
pub struct Ticket {
    request_id: u64,
    receiver: std::sync::mpsc::Receiver<Completion>,
}

impl Ticket {
    /// チケットと、キューに積む待ち手の組を作る。
    pub fn new(request_id: u64, requested: ScanTarget) -> (Ticket, Waiter) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let ticket = Ticket {
            request_id,
            receiver,
        };
        let waiter = Waiter {
            request_id,
            requested,
            sender,
        };
        (ticket, waiter)
    }

    pub fn request_id(&self) -> u64 {
        self.request_id
    }

    /// 完了通知が届くまで待つ。通知を送らずに待ち手が破棄された場合は `Err`。
    pub fn wait(self) -> anyhow::Result<Completion> {
        use anyhow::Context;
        self.receiver.recv().with_context(|| {
            format!(
                "the waiter of request {} was dropped without a completion",
                self.request_id
            )
        })
    }

    /// 最大 `timeout` まで待つ。時間内に届かなければ `Ok(None)`。
    /// 通知を送らずに待ち手が破棄された場合は `Err`。
    pub fn wait_timeout(&self, timeout: std::time::Duration) -> anyhow::Result<Option<Completion>> {
        match self.receiver.recv_timeout(timeout) {
            Ok(completion) => Ok(Some(completion)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!(
                    "the waiter of request {} was dropped without a completion",
                    self.request_id
                )
            }
        }
    }
}

/// 待ち手。元の要求の `request_id` と、完了通知の送信口を持つ。
#[derive(Debug)]
pub struct Waiter {
    pub request_id: u64,
    /// 元の要求の種類と対象
    pub requested: ScanTarget,
    sender: std::sync::mpsc::Sender<Completion>,
}

impl Waiter {
    fn notify(self, completion: Completion) {
        let request_id = self.request_id;
        if self.sender.send(completion).is_err() {
            // チケットが先に破棄された。呼び出し側が結果を要らなくなっただけなので続ける
            log::debug!("the ticket of request {request_id} was dropped before completion");
        }
    }
}

/// キューの要素。実行するスキャンの内容と、待ち手の一覧を持つ。
#[derive(Debug)]
pub struct QueueItem {
    pub settings: crate::dir_scan::settings::ScanSettings,
    /// 実行するスキャンの対象（格上げ前）
    pub target: ScanTarget,
    /// この要素の `target` を決めた要求。統合された要求の完了通知の統合先になる。
    /// 新しい要求が既存の要素を飲み込んだ場合はその新しい要求、既存の要素に飲み込まれた
    /// 場合は既存の要素のままとなる
    pub representative_request_id: u64,
    /// 待ち手の一覧。要素の位置の順（飲み込まれた要素の待ち手を先に、新しい要求を最後に）
    pub waiters: Vec<Waiter>,
}

impl QueueItem {
    /// この要素に含まれる `request_id` の一覧（`ScanReport::request_ids` 用）。
    pub fn request_ids(&self) -> Vec<u64> {
        self.waiters.iter().map(|w| w.request_id).collect()
    }

    /// スキャンを実行した結果を、全待ち手に通知する。
    /// `executed` は実際に走ったスキャンの種類と対象（格上げ後）。
    /// レポートの `outcome` が `Failed` なら、全待ち手の行き先は [`Disposition::Failed`]。
    pub fn notify_executed(
        self,
        executed: &ScanTarget,
        report: &std::sync::Arc<crate::dir_scan::report::ScanReport>,
    ) {
        let failed = matches!(
            report.outcome,
            crate::dir_scan::report::ScanOutcome::Failed(_)
        );
        let representative = self.representative_request_id;
        for waiter in self.waiters {
            let disposition = if failed {
                Disposition::Failed
            } else if waiter.request_id == representative {
                Disposition::Executed
            } else {
                Disposition::Merged {
                    into_request_id: representative,
                }
            };
            let completion = Completion {
                request_id: waiter.request_id,
                disposition,
                execution_id: Some(report.execution_id),
                requested: waiter.requested.clone(),
                executed: Some(executed.clone()),
                report: Some(std::sync::Arc::clone(report)),
            };
            waiter.notify(completion);
        }
    }

    /// 実行される前にサービスが停止したことを、全待ち手に通知する。
    pub fn notify_stopped(self) {
        for waiter in self.waiters {
            let completion = Completion {
                request_id: waiter.request_id,
                disposition: Disposition::Stopped,
                execution_id: None,
                requested: waiter.requested.clone(),
                executed: None,
                report: None,
            };
            waiter.notify(completion);
        }
    }
}

/// [`ScanQueue::push`] の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushOutcome {
    /// 統合せず、末尾に積んだ
    Appended,
    /// 既存の要素に飲み込まれた
    MergedIntoExisting,
    /// 既存の要素を `count`（1 以上）個飲み込み、一番古いものの位置を引き継いだ
    Absorbed { count: usize },
}

/// 要求キュー。積むときに統合する（設計書 6.3）。
///
/// 実行中のスキャンは含めない。`pop_front` で取り出した要素は、もうキューの外にある。
/// 排他制御は呼び出し側（サービス）が行う。
///
/// 不変条件: 設定が同じ 2 要素の間には包含関係がない（あれば `push` のときに統合されている）。
#[derive(Debug, Default)]
pub struct ScanQueue {
    items: std::collections::VecDeque<QueueItem>,
}

impl ScanQueue {
    pub fn new() -> Self {
        Self {
            items: std::collections::VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 先頭から順に要素を見る（テストや表示用）。
    pub fn iter(&self) -> std::collections::vec_deque::Iter<'_, QueueItem> {
        self.items.iter()
    }

    /// 要求をキューに積む。待ち手の `requested` が、実行するスキャンの対象になる。
    ///
    /// - 設定が同じ要素のうち、新しい要求を含むものがあれば、その待ち手に加わる。
    /// - そうでなければ、設定が同じ要素のうち新しい要求に含まれるものをすべて飲み込み、
    ///   飲み込んだ要素のうち一番古いものの位置に、1 つの要素として置く。
    ///   飲み込むものがなければ末尾に積む。
    pub fn push(
        &mut self,
        settings: crate::dir_scan::settings::ScanSettings,
        waiter: Waiter,
    ) -> PushOutcome {
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|item| item.settings == settings && item.target.contains(&waiter.requested))
        {
            existing.waiters.push(waiter);
            return PushOutcome::MergedIntoExisting;
        }

        let old_items = std::mem::take(&mut self.items);
        let mut kept: std::collections::VecDeque<QueueItem> =
            std::collections::VecDeque::with_capacity(old_items.len() + 1);
        let mut waiters: Vec<Waiter> = Vec::new();
        let mut oldest_slot: Option<usize> = None;
        let mut absorbed_count = 0;
        for item in old_items {
            if item.settings == settings && waiter.requested.contains(&item.target) {
                if oldest_slot.is_none() {
                    oldest_slot = Some(kept.len());
                }
                absorbed_count += 1;
                waiters.extend(item.waiters);
            } else {
                kept.push_back(item);
            }
        }
        let representative_request_id = waiter.request_id;
        let target = waiter.requested.clone();
        waiters.push(waiter);
        let new_item = QueueItem {
            settings,
            target,
            representative_request_id,
            waiters,
        };
        self.items = kept;
        match oldest_slot {
            Some(slot) => {
                self.items.insert(slot, new_item);
                PushOutcome::Absorbed {
                    count: absorbed_count,
                }
            }
            None => {
                self.items.push_back(new_item);
                PushOutcome::Appended
            }
        }
    }

    /// 先頭の要素を取り出す。取り出した要素はキューから外れ、以後は何も統合されない。
    pub fn pop_front(&mut self) -> Option<QueueItem> {
        self.items.pop_front()
    }

    /// 残っている要素をすべて、先頭から順に取り出す（停止時用）。
    /// 取り出した要素には [`QueueItem::notify_stopped`] を呼ぶ。
    pub fn drain_all(&mut self) -> Vec<QueueItem> {
        self.items.drain(..).collect()
    }
}

/// 実行直前の格上げの判定（設計書 6.3）。純粋な判定で、ツリーには触れない。
///
/// `tree_exclude_patterns` は、公開しているツリーを作ったフルスキャンの除外パターン。
/// 公開しているツリーがまだない（未スキャン）なら `None`。
/// 次の場合に `true`（部分スキャンをフルスキャンに格上げする）:
/// - `tree_exclude_patterns` が `None`（未スキャン）。
/// - 要求の除外パターンが `tree_exclude_patterns` と違う。
///
/// 並列度と I/O 優先度はツリーの内容に影響しないので比べない。
pub fn needs_full_upgrade(
    request_settings: &crate::dir_scan::settings::ScanSettings,
    tree_exclude_patterns: Option<&crate::dir_scan::settings::ExcludePatterns>,
) -> bool {
    match tree_exclude_patterns {
        None => true,
        Some(patterns) => *patterns != request_settings.exclude_patterns,
    }
}

/// 実行直前に決める、実際に走らせるスキャンの対象。
/// フルスキャンはそのまま。部分スキャンは [`needs_full_upgrade`] なら [`ScanTarget::Full`] にする。
pub fn effective_target(
    requested: &ScanTarget,
    request_settings: &crate::dir_scan::settings::ScanSettings,
    tree_exclude_patterns: Option<&crate::dir_scan::settings::ExcludePatterns>,
) -> ScanTarget {
    match requested {
        ScanTarget::Full => ScanTarget::Full,
        ScanTarget::Partial(_) => {
            if needs_full_upgrade(request_settings, tree_exclude_patterns) {
                ScanTarget::Full
            } else {
                requested.clone()
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

    fn settings_with(
        parallelism: usize,
        patterns: &[&str],
    ) -> crate::dir_scan::settings::ScanSettings {
        let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
        crate::dir_scan::settings::ScanSettings::new(
            crate::dir_scan::settings::Parallelism::new(parallelism).unwrap(),
            crate::dir_scan::settings::IoPriority::Idle,
            crate::dir_scan::settings::ExcludePatterns::new(&owned).unwrap(),
        )
    }

    fn partial(path: &str) -> ScanTarget {
        ScanTarget::partial(std::path::Path::new(path)).unwrap()
    }

    fn sample_report(
        execution_id: u64,
        request_ids: Vec<u64>,
        outcome: crate::dir_scan::report::ScanOutcome,
    ) -> std::sync::Arc<crate::dir_scan::report::ScanReport> {
        std::sync::Arc::new(crate::dir_scan::report::ScanReport {
            execution_id,
            request_ids,
            kind: crate::dir_scan::report::ScanKind::Full,
            requested_path: None,
            effective_path: std::path::PathBuf::new(),
            parent_retries: Vec::new(),
            settings: settings_with(1, &[]),
            started_at: std::time::UNIX_EPOCH,
            finished_at: std::time::UNIX_EPOCH,
            durations: crate::dir_scan::report::ScanDurations::default(),
            worker_ioprio: Vec::new(),
            dir_count: 0,
            file_count: 0,
            root_total_size: 0,
            hash_collision_count: 0,
            errors: crate::dir_scan::report::ErrorCounts::new(),
            generations: crate::dir_scan::report::ScanGenerations {
                at_start: 0,
                before_apply: 0,
                after_apply: 0,
            },
            outcome,
        })
    }

    /// キューに積み、チケットを返す。
    fn push(
        queue: &mut ScanQueue,
        request_id: u64,
        settings: &crate::dir_scan::settings::ScanSettings,
        requested: ScanTarget,
    ) -> (Ticket, PushOutcome) {
        let (ticket, waiter) = Ticket::new(request_id, requested);
        let outcome = queue.push(settings.clone(), waiter);
        (ticket, outcome)
    }

    fn queue_shape(queue: &ScanQueue) -> Vec<(ScanTarget, Vec<u64>)> {
        queue
            .iter()
            .map(|item| (item.target.clone(), item.request_ids()))
            .collect()
    }

    // ---- パスの検証 ----

    #[test]
    fn partial_path_rejects_absolute() {
        init_logger();

        // Act
        let absolute = ScanTarget::partial(std::path::Path::new("/a"));
        let root = ScanTarget::partial(std::path::Path::new("/"));

        // Assert
        assert!(absolute.is_err());
        assert!(root.is_err());
    }

    #[test]
    fn partial_path_rejects_parent_dir() {
        init_logger();

        // Arrange
        let paths = ["..", "../a", "a/..", "a/../b", "a/b/../.."];

        // Act & Assert
        for path in paths {
            assert!(
                ScanTarget::partial(std::path::Path::new(path)).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn partial_path_rejects_empty() {
        init_logger();

        // Arrange
        let paths = ["", ".", "./", "./."];

        // Act & Assert
        for path in paths {
            assert!(
                ScanTarget::partial(std::path::Path::new(path)).is_err(),
                "{path:?}"
            );
        }
    }

    #[test]
    fn partial_path_normalizes_cur_dir_and_slashes() {
        init_logger();

        // Arrange
        let paths = ["a/b", "./a/b", "a//b", "a/./b", "a/b/", "./a//b/./"];

        // Act & Assert
        for path in paths {
            assert_eq!(partial(path), partial("a/b"), "{path}");
            assert_eq!(
                partial(path).path(),
                Some(std::path::Path::new("a/b")),
                "{path}"
            );
        }
    }

    #[test]
    fn partial_path_keeps_non_utf8_names() {
        init_logger();

        // Arrange
        let name = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"a\xffb");
        let path = std::path::Path::new("dir").join(name);

        // Act
        let target = ScanTarget::partial(&path).unwrap();

        // Assert
        assert_eq!(target.path(), Some(path.as_path()));
    }

    #[test]
    fn partial_path_does_not_strip_dot_prefixed_names() {
        init_logger();

        // Act
        let hidden_dir = partial(".cache/x");
        let dots = ScanTarget::partial(std::path::Path::new("..."));

        // Assert
        assert_eq!(hidden_dir.path(), Some(std::path::Path::new(".cache/x")));
        assert!(dots.is_ok());
    }

    // ---- 包含関係 ----

    #[test]
    fn contains_follows_design() {
        init_logger();

        // Arrange
        let full = ScanTarget::full();

        // Act
        let full_in_full = full.contains(&full);
        let partial_in_full = full.contains(&partial("a"));
        let full_in_partial = partial("a").contains(&full);
        let same_partial = partial("a").contains(&partial("a"));
        let child_in_parent = partial("a").contains(&partial("a/b"));
        let grandchild_in_parent = partial("a").contains(&partial("a/b/c"));
        let parent_in_child = partial("a/b").contains(&partial("a"));
        let longer_sibling_name = partial("a").contains(&partial("ab"));
        let shorter_sibling_name = partial("ab").contains(&partial("a"));
        let same_name_elsewhere = partial("a").contains(&partial("b/a"));

        // Assert
        assert!(full_in_full);
        assert!(partial_in_full);
        assert!(!full_in_partial);
        assert!(same_partial);
        assert!(child_in_parent);
        assert!(grandchild_in_parent);
        assert!(!parent_in_child);
        assert!(!longer_sibling_name);
        assert!(!shorter_sibling_name);
        assert!(!same_name_elsewhere);
    }

    #[test]
    fn scan_target_kind_and_display() {
        init_logger();

        // Arrange
        let full = ScanTarget::full();
        let partial_target = partial("a/b");

        // Act
        let full_kind = full.kind();
        let partial_kind = partial_target.kind();
        let full_path = full.path();
        let full_display = full.to_string();
        let partial_display = partial_target.to_string();

        // Assert
        assert_eq!(full_kind, crate::dir_scan::report::ScanKind::Full);
        assert_eq!(partial_kind, crate::dir_scan::report::ScanKind::Partial);
        assert_eq!(full_path, None);
        assert_eq!(full_display, "full");
        assert_eq!(partial_display, "partial(a/b)");
    }

    // ---- 統合規則（設計書 6.3 の例） ----

    #[test]
    fn design_example_a_b_c_d() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let y = settings_with(2, &[]);
        let mut queue = ScanQueue::new();

        // Act
        // A: X, 部分 a/b
        let (_ticket_a, outcome_a) = push(&mut queue, 1, &x, partial("a/b"));

        // Assert
        assert_eq!(outcome_a, PushOutcome::Appended);
        assert_eq!(queue_shape(&queue), vec![(partial("a/b"), vec![1])]);

        // Act
        // B: Y, 部分 z
        let (_ticket_b, outcome_b) = push(&mut queue, 2, &y, partial("z"));

        // Assert
        assert_eq!(outcome_b, PushOutcome::Appended);
        assert_eq!(
            queue_shape(&queue),
            vec![(partial("a/b"), vec![1]), (partial("z"), vec![2])]
        );

        // Act
        // C: X, 部分 a（A を含む）-> A の位置に、待ち手 A・C
        let (_ticket_c, outcome_c) = push(&mut queue, 3, &x, partial("a"));

        // Assert
        assert_eq!(outcome_c, PushOutcome::Absorbed { count: 1 });
        assert_eq!(
            queue_shape(&queue),
            vec![(partial("a"), vec![1, 3]), (partial("z"), vec![2])]
        );

        // Act
        // D: X, フル（C を含む）-> C の位置に、待ち手 A・C・D
        let (_ticket_d, outcome_d) = push(&mut queue, 4, &x, ScanTarget::full());

        // Assert
        assert_eq!(outcome_d, PushOutcome::Absorbed { count: 1 });
        assert_eq!(
            queue_shape(&queue),
            vec![(ScanTarget::full(), vec![1, 3, 4]), (partial("z"), vec![2])]
        );
        assert_eq!(queue.iter().next().unwrap().representative_request_id, 4);
        assert_eq!(queue.iter().next().unwrap().settings, x);
        assert_eq!(queue.iter().nth(1).unwrap().settings, y);
    }

    #[test]
    fn contained_request_merges_into_existing_item() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (_t1, _) = push(&mut queue, 1, &x, partial("a"));

        // Act
        let (_t2, outcome) = push(&mut queue, 2, &x, partial("a/b"));

        // Assert
        assert_eq!(outcome, PushOutcome::MergedIntoExisting);

        // Act
        let (_t3, outcome) = push(&mut queue, 3, &x, partial("a"));

        // Assert
        assert_eq!(outcome, PushOutcome::MergedIntoExisting);
        assert_eq!(queue_shape(&queue), vec![(partial("a"), vec![1, 2, 3])]);
        assert_eq!(queue.iter().next().unwrap().representative_request_id, 1);
    }

    #[test]
    fn sibling_prefix_names_are_not_merged() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (_t1, _) = push(&mut queue, 1, &x, partial("a"));

        // Act
        let (_t2, outcome) = push(&mut queue, 2, &x, partial("ab"));

        // Assert
        assert_eq!(outcome, PushOutcome::Appended);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn full_absorbs_many_items_and_takes_oldest_position() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let y = settings_with(2, &[]);
        let mut queue = ScanQueue::new();

        // Act
        let (_t1, _) = push(&mut queue, 1, &y, partial("p"));
        let (_t2, _) = push(&mut queue, 2, &x, partial("a"));
        let (_t3, _) = push(&mut queue, 3, &x, partial("b"));
        let (_t4, _) = push(&mut queue, 4, &y, partial("q"));
        let (_t5, _) = push(&mut queue, 5, &x, partial("c/d"));
        let (_t6, _) = push(&mut queue, 6, &x, partial("b/x"));

        // Assert
        assert_eq!(
            queue_shape(&queue),
            vec![
                (partial("p"), vec![1]),
                (partial("a"), vec![2]),
                (partial("b"), vec![3, 6]),
                (partial("q"), vec![4]),
                (partial("c/d"), vec![5]),
            ]
        );

        // Act
        let (_t7, outcome) = push(&mut queue, 7, &x, ScanTarget::full());

        // Assert
        assert_eq!(outcome, PushOutcome::Absorbed { count: 3 });
        assert_eq!(
            queue_shape(&queue),
            vec![
                (partial("p"), vec![1]),
                (ScanTarget::full(), vec![2, 3, 6, 5, 7]),
                (partial("q"), vec![4]),
            ]
        );
    }

    #[test]
    fn partial_absorbs_only_descendants_with_same_settings() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let y = settings_with(4, &["**/.*"]);
        let mut queue = ScanQueue::new();
        let (_t1, _) = push(&mut queue, 1, &x, partial("a/b"));
        let (_t2, _) = push(&mut queue, 2, &y, partial("a/c"));
        let (_t3, _) = push(&mut queue, 3, &x, partial("a/d"));

        // Act
        let (_t4, outcome) = push(&mut queue, 4, &x, partial("a"));

        // Assert
        assert_eq!(outcome, PushOutcome::Absorbed { count: 2 });
        assert_eq!(
            queue_shape(&queue),
            vec![(partial("a"), vec![1, 3, 4]), (partial("a/c"), vec![2])]
        );
    }

    #[test]
    fn settings_differing_only_in_io_priority_or_excludes_are_not_merged() {
        init_logger();

        // Arrange
        let base = settings_with(4, &["**/.*"]);
        let other_exclude = settings_with(4, &["**/*.tmp"]);
        let other_io = crate::dir_scan::settings::ScanSettings::new(
            crate::dir_scan::settings::Parallelism::new(4).unwrap(),
            crate::dir_scan::settings::IoPriority::best_effort(4).unwrap(),
            crate::dir_scan::settings::ExcludePatterns::new(&["**/.*".to_string()]).unwrap(),
        );
        let mut queue = ScanQueue::new();
        let (_t1, _) = push(&mut queue, 1, &base, ScanTarget::full());

        // Act
        let (_t2, o2) = push(&mut queue, 2, &other_exclude, partial("a"));
        let (_t3, o3) = push(&mut queue, 3, &other_io, partial("a"));

        // Assert
        assert_eq!(o2, PushOutcome::Appended);
        assert_eq!(o3, PushOutcome::Appended);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn exclude_patterns_in_different_order_are_the_same_settings() {
        init_logger();

        // Arrange
        let first = settings_with(4, &["b", "a", "a"]);
        let second = settings_with(4, &["a", "b"]);
        let mut queue = ScanQueue::new();
        let (_t1, _) = push(&mut queue, 1, &first, partial("x"));

        // Act
        let (_t2, outcome) = push(&mut queue, 2, &second, partial("x/y"));

        // Assert
        assert_eq!(outcome, PushOutcome::MergedIntoExisting);
    }

    #[test]
    fn popped_item_is_not_merged_into() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (_t1, _) = push(&mut queue, 1, &x, partial("a"));

        // Act
        let running = queue.pop_front().unwrap();

        // Assert
        assert!(queue.is_empty());

        // Act
        let (_t2, outcome) = push(&mut queue, 2, &x, partial("a"));

        // Assert
        assert_eq!(outcome, PushOutcome::Appended);
        assert_eq!(running.request_ids(), vec![1]);
        assert_eq!(queue_shape(&queue), vec![(partial("a"), vec![2])]);
    }

    // ---- チケットと完了通知 ----

    #[test]
    fn merged_waiters_all_receive_the_same_execution() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (ticket_a, _) = push(&mut queue, 1, &x, partial("a/b"));
        let (ticket_c, _) = push(&mut queue, 3, &x, partial("a"));
        let (ticket_d, _) = push(&mut queue, 4, &x, ScanTarget::full());
        let item = queue.pop_front().unwrap();
        let report = sample_report(
            9,
            item.request_ids(),
            crate::dir_scan::report::ScanOutcome::Success,
        );

        // Act
        item.notify_executed(&ScanTarget::full(), &report);
        let completion_a = ticket_a.wait().unwrap();
        let completion_c = ticket_c.wait().unwrap();
        let completion_d = ticket_d.wait().unwrap();

        // Assert
        assert_eq!(completion_a.request_id, 1);
        assert_eq!(
            completion_a.disposition,
            Disposition::Merged { into_request_id: 4 }
        );
        assert_eq!(completion_a.execution_id, Some(9));
        assert_eq!(completion_a.requested, partial("a/b"));
        assert_eq!(completion_a.executed, Some(ScanTarget::full()));
        assert!(std::sync::Arc::ptr_eq(
            completion_a.report.as_ref().unwrap(),
            &report
        ));

        assert_eq!(
            completion_c.disposition,
            Disposition::Merged { into_request_id: 4 }
        );
        assert_eq!(completion_c.requested, partial("a"));

        assert_eq!(completion_d.disposition, Disposition::Executed);
        assert_eq!(completion_d.requested, ScanTarget::full());
        assert_eq!(completion_d.executed, Some(ScanTarget::full()));
    }

    #[test]
    fn upgrade_and_merge_are_reported_separately() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (ticket_a, _) = push(&mut queue, 1, &x, partial("a/b"));
        let (ticket_c, _) = push(&mut queue, 2, &x, partial("a"));
        let item = queue.pop_front().unwrap();
        let report = sample_report(
            1,
            item.request_ids(),
            crate::dir_scan::report::ScanOutcome::SuccessWithErrors,
        );

        // Act
        let executed = effective_target(&item.target, &item.settings, None);

        // Assert
        // 部分スキャン a に格上げされてフルスキャンが走った
        assert_eq!(executed, ScanTarget::full());

        // Act
        item.notify_executed(&executed, &report);
        let completion_a = ticket_a.wait().unwrap();
        let completion_c = ticket_c.wait().unwrap();

        // Assert
        assert_eq!(
            completion_a.disposition,
            Disposition::Merged { into_request_id: 2 }
        );
        assert_eq!(completion_a.requested, partial("a/b"));
        assert_eq!(completion_a.executed, Some(ScanTarget::full()));
        assert_eq!(completion_c.disposition, Disposition::Executed);
        assert_eq!(completion_c.requested, partial("a"));
        assert_eq!(completion_c.executed, Some(ScanTarget::full()));
    }

    #[test]
    fn failed_report_makes_every_waiter_failed() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (ticket_a, _) = push(&mut queue, 1, &x, partial("a/b"));
        let (ticket_c, _) = push(&mut queue, 2, &x, partial("a"));
        let item = queue.pop_front().unwrap();
        let report = sample_report(
            3,
            item.request_ids(),
            crate::dir_scan::report::ScanOutcome::Failed("boom".to_string()),
        );

        // Act
        item.notify_executed(&partial("a"), &report);

        // Assert
        for ticket in [ticket_a, ticket_c] {
            let completion = ticket.wait().unwrap();
            assert_eq!(completion.disposition, Disposition::Failed);
            assert_eq!(completion.execution_id, Some(3));
            assert_eq!(completion.executed, Some(partial("a")));
            assert!(completion.report.is_some());
        }
    }

    #[test]
    fn stopped_notifies_every_waiter_of_every_remaining_item() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let y = settings_with(2, &[]);
        let mut queue = ScanQueue::new();
        let (t1, _) = push(&mut queue, 1, &x, partial("a/b"));
        let (t2, _) = push(&mut queue, 2, &y, partial("z"));
        let (t3, _) = push(&mut queue, 3, &x, partial("a"));

        // Act
        for item in queue.drain_all() {
            item.notify_stopped();
        }

        // Assert
        assert!(queue.is_empty());
        let expected = [
            (t1, 1, partial("a/b")),
            (t2, 2, partial("z")),
            (t3, 3, partial("a")),
        ];
        for (ticket, request_id, requested) in expected {
            let completion = ticket.wait().unwrap();
            assert_eq!(completion.request_id, request_id);
            assert_eq!(completion.disposition, Disposition::Stopped);
            assert_eq!(completion.execution_id, None);
            assert_eq!(completion.requested, requested);
            assert_eq!(completion.executed, None);
            assert!(completion.report.is_none());
        }
    }

    #[test]
    fn wait_fails_when_waiter_is_dropped_without_completion() {
        init_logger();

        // Arrange
        let (ticket, waiter) = Ticket::new(5, ScanTarget::full());

        // Act
        let request_id = ticket.request_id();
        let before_drop = ticket.wait_timeout(std::time::Duration::from_millis(1));

        // Assert
        assert_eq!(request_id, 5);
        assert!(before_drop.unwrap().is_none());

        // Act
        drop(waiter);
        let timeout_after_drop = ticket.wait_timeout(std::time::Duration::from_millis(1));
        let wait_after_drop = ticket.wait();

        // Assert
        assert!(timeout_after_drop.is_err());
        assert!(wait_after_drop.is_err());
    }

    #[test]
    fn notify_does_not_fail_when_ticket_is_dropped() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let mut queue = ScanQueue::new();
        let (ticket, _) = push(&mut queue, 1, &x, partial("a"));
        drop(ticket);
        let item = queue.pop_front().unwrap();

        // Act
        item.notify_stopped();
    }

    // ---- 格上げの判定 ----

    #[test]
    fn upgrade_when_tree_is_unscanned() {
        init_logger();

        // Arrange
        let x = settings_with(4, &["**/.*"]);

        // Act
        let needs_upgrade = needs_full_upgrade(&x, None);
        let target = effective_target(&partial("a"), &x, None);

        // Assert
        assert!(needs_upgrade);
        assert_eq!(target, ScanTarget::full());
    }

    #[test]
    fn upgrade_when_exclude_patterns_differ() {
        init_logger();

        // Arrange
        let x = settings_with(4, &["**/.*"]);
        let tree_patterns = crate::dir_scan::settings::ExcludePatterns::new(&[]).unwrap();

        // Act
        let needs_upgrade = needs_full_upgrade(&x, Some(&tree_patterns));
        let target = effective_target(&partial("a"), &x, Some(&tree_patterns));

        // Assert
        assert!(needs_upgrade);
        assert_eq!(target, ScanTarget::full());
    }

    #[test]
    fn no_upgrade_when_exclude_patterns_match_regardless_of_other_settings() {
        init_logger();

        // Arrange
        let tree_patterns =
            crate::dir_scan::settings::ExcludePatterns::new(&["b".to_string(), "a".to_string()])
                .unwrap();
        // 並列度が違い、パターンの並び・重複が違っても、正規形が同じなら格上げしない
        let x = settings_with(8, &["a", "b", "a"]);

        // Act
        let needs_upgrade = needs_full_upgrade(&x, Some(&tree_patterns));
        let target = effective_target(&partial("a"), &x, Some(&tree_patterns));

        // Assert
        assert!(!needs_upgrade);
        assert_eq!(target, partial("a"));
    }

    #[test]
    fn full_request_is_never_changed() {
        init_logger();

        // Arrange
        let x = settings_with(4, &[]);
        let tree_patterns = crate::dir_scan::settings::ExcludePatterns::none();

        // Act
        let with_tree = effective_target(&ScanTarget::full(), &x, Some(&tree_patterns));
        let without_tree = effective_target(&ScanTarget::full(), &x, None);

        // Assert
        assert_eq!(with_tree, ScanTarget::full());
        assert_eq!(without_tree, ScanTarget::full());
    }

    // ---- プロパティテスト（設計書 10） ----

    /// 設定は 3 種類（並列度だけが違う）。
    fn settings_by_index(index: usize) -> crate::dir_scan::settings::ScanSettings {
        settings_with(index + 1, &["**/.*"])
    }

    fn arbitrary_target() -> impl proptest::strategy::Strategy<Value = ScanTarget> {
        use proptest::strategy::Strategy;
        let names = proptest::sample::select(vec!["a", "b", "ab"]);
        let path = proptest::collection::vec(names, 1..=3);
        proptest::option::weighted(0.85, path).prop_map(|path| match path {
            None => ScanTarget::Full,
            Some(components) => {
                let joined: std::path::PathBuf = components.iter().collect();
                ScanTarget::partial(&joined).unwrap()
            }
        })
    }

    fn arbitrary_requests() -> impl proptest::strategy::Strategy<Value = Vec<(usize, ScanTarget)>> {
        proptest::collection::vec((0..3usize, arbitrary_target()), 1..16)
    }

    proptest::proptest! {
        /// 統合後も元の要求の対象範囲を失わない／設定の違う要求は統合されない。
        #[test]
        fn merged_items_cover_requests_and_never_mix_settings(requests in arbitrary_requests()) {
            init_logger();

            // Arrange
            let mut queue = ScanQueue::new();
            let mut tickets = Vec::new();

            // Act
            for (id, (settings_index, target)) in requests.iter().enumerate() {
                let (ticket, waiter) = Ticket::new(id as u64, target.clone());
                queue.push(settings_by_index(*settings_index), waiter);
                tickets.push(ticket);
            }

            // Assert
            let mut seen = std::collections::BTreeSet::new();
            for item in queue.iter() {
                for waiter in &item.waiters {
                    let (settings_index, target) = &requests[waiter.request_id as usize];
                    proptest::prop_assert_eq!(&item.settings, &settings_by_index(*settings_index));
                    proptest::prop_assert!(item.target.contains(target));
                    proptest::prop_assert_eq!(&waiter.requested, target);
                    proptest::prop_assert!(seen.insert(waiter.request_id));
                }
                // 代表の要求はその要素の target そのもの
                let representative = &requests[item.representative_request_id as usize].1;
                proptest::prop_assert_eq!(representative, &item.target);
                proptest::prop_assert!(item
                    .waiters
                    .iter()
                    .any(|w| w.request_id == item.representative_request_id));
            }
            proptest::prop_assert_eq!(seen.len(), requests.len());
            // 設定が同じ要素の間に包含関係はない
            let items: Vec<&QueueItem> = queue.iter().collect();
            for (i, left) in items.iter().enumerate() {
                for (j, right) in items.iter().enumerate() {
                    if i != j && left.settings == right.settings {
                        proptest::prop_assert!(!left.target.contains(&right.target));
                    }
                }
            }
        }

        /// 統合した要素は、統合された要素のうち一番古いものの位置を引き継ぐ。
        /// 統合されなかった要素は積まれた順のまま。
        #[test]
        fn merged_item_takes_the_oldest_position(requests in arbitrary_requests()) {
            init_logger();

            // Arrange
            let mut queue = ScanQueue::new();
            let mut tickets = Vec::new();

            // Act & Assert
            for (id, (settings_index, target)) in requests.iter().enumerate() {
                let settings = settings_by_index(*settings_index);
                // 積む前の各要素の先頭の待ち手の request_id（位置の目印）
                let before: Vec<(u64, Vec<u64>)> = queue
                    .iter()
                    .map(|item| (item.waiters[0].request_id, item.request_ids()))
                    .collect();
                let absorbed: Vec<bool> = queue
                    .iter()
                    .map(|item| item.settings == settings && target.contains(&item.target))
                    .collect();
                let containing = queue
                    .iter()
                    .position(|item| item.settings == settings && item.target.contains(target));

                let (ticket, waiter) = Ticket::new(id as u64, target.clone());
                queue.push(settings, waiter);
                tickets.push(ticket);
                let after: Vec<(u64, Vec<u64>)> = queue
                    .iter()
                    .map(|item| (item.waiters[0].request_id, item.request_ids()))
                    .collect();

                // 積んだ要求が入った要素の位置と、その要素の待ち手
                let position = queue
                    .iter()
                    .position(|item| item.request_ids().contains(&(id as u64)))
                    .unwrap();
                if let Some(index) = containing {
                    // 既存の要素に飲み込まれた: 位置は変わらず、待ち手が増えるだけ
                    proptest::prop_assert_eq!(position, index);
                    proptest::prop_assert_eq!(after.len(), before.len());
                    let heads_before: Vec<u64> = before.iter().map(|(head, _)| *head).collect();
                    let heads_after: Vec<u64> = after.iter().map(|(head, _)| *head).collect();
                    proptest::prop_assert_eq!(heads_before, heads_after);
                } else if let Some(oldest) = absorbed.iter().position(|a| *a) {
                    // 飲み込んだ: 一番古い飲み込まれた要素の位置を引き継ぐ。ほかの飲み込まれなかった要素は順序を保つ
                    proptest::prop_assert_eq!(position, oldest);
                    let kept: Vec<&(u64, Vec<u64>)> = before
                        .iter()
                        .zip(absorbed.iter())
                        .filter(|(_, a)| !**a)
                        .map(|(entry, _)| entry)
                        .collect();
                    let mut after_without_new: Vec<&(u64, Vec<u64>)> = after.iter().collect();
                    after_without_new.remove(position);
                    proptest::prop_assert_eq!(after_without_new, kept);
                    // 新しい要素の待ち手は、飲み込まれた要素の待ち手の和に自分を加えたもの
                    let mut expected_ids: Vec<u64> = Vec::new();
                    for ((_, ids), a) in before.iter().zip(absorbed.iter()) {
                        if *a {
                            expected_ids.extend(ids.iter().copied());
                        }
                    }
                    expected_ids.push(id as u64);
                    proptest::prop_assert_eq!(&after[position].1, &expected_ids);
                } else {
                    // 統合なし: 末尾に積まれ、既存の要素は変わらない
                    proptest::prop_assert_eq!(position, before.len());
                    proptest::prop_assert_eq!(&after[..before.len()], &before[..]);
                    proptest::prop_assert_eq!(after.len(), before.len() + 1);
                }
            }
        }

        /// すべての元の要求の待ち手が、実行でも停止でも完了通知を受け取る。
        #[test]
        fn every_waiter_receives_a_completion(
            requests in arbitrary_requests(),
            executed_count in 0..8usize,
        ) {
            init_logger();

            // Arrange
            let mut queue = ScanQueue::new();
            let mut tickets = Vec::new();
            for (id, (settings_index, target)) in requests.iter().enumerate() {
                let (ticket, waiter) = Ticket::new(id as u64, target.clone());
                queue.push(settings_by_index(*settings_index), waiter);
                tickets.push(ticket);
            }

            // Act
            // 先頭から executed_count 個を実行し、残りは停止で通知する
            for execution_id in 0..executed_count as u64 {
                let Some(item) = queue.pop_front() else { break };
                let report = sample_report(
                    execution_id,
                    item.request_ids(),
                    crate::dir_scan::report::ScanOutcome::Success,
                );
                let executed = item.target.clone();
                item.notify_executed(&executed, &report);
            }
            for item in queue.drain_all() {
                item.notify_stopped();
            }

            // Assert
            for ticket in tickets {
                let request_id = ticket.request_id();
                let completion = ticket.wait().unwrap();
                proptest::prop_assert_eq!(completion.request_id, request_id);
                proptest::prop_assert_eq!(&completion.requested, &requests[request_id as usize].1);
                match completion.disposition {
                    Disposition::Executed | Disposition::Merged { .. } => {
                        proptest::prop_assert!(completion.execution_id.is_some());
                        let executed = completion.executed.as_ref().unwrap();
                        proptest::prop_assert!(executed.contains(&completion.requested));
                        let report = completion.report.as_ref().unwrap();
                        proptest::prop_assert_eq!(Some(report.execution_id), completion.execution_id);
                        proptest::prop_assert!(report.request_ids.contains(&request_id));
                    }
                    Disposition::Stopped => {
                        proptest::prop_assert!(completion.execution_id.is_none());
                        proptest::prop_assert!(completion.executed.is_none());
                        proptest::prop_assert!(completion.report.is_none());
                    }
                    Disposition::Failed => proptest::prop_assert!(false, "unexpected failure"),
                }
            }
        }
    }
}
