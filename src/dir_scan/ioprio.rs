//! ワーカースレッドへの I/O 優先度の設定と読み返し（設計書 6.7）。
//!
//! `libc` に `ioprio_set`／`ioprio_get` のラッパーはないので、システムコールを直接呼ぶ。
//! 対象は常に呼び出したスレッド自身（`IOPRIO_WHO_PROCESS`、`who = 0`）で、プロセス全体の
//! 優先度は変えない。
//!
//! rayon の `start_handler` は各ワーカーのスレッド上で、そのワーカーが仕事を取り始める前に
//! 1 回だけ呼ばれる。ここの処理はパニックせず、失敗は [`report::WorkerIoprioResult`] と
//! [`report::ErrorRecorder`] に記録するだけにする。失敗したワーカーはカーネルの既定の優先度
//! のまま続ける。

/// `ioprio_set(2)`／`ioprio_get(2)` の `which` 引数。`who` がスレッド ID を表す。
/// `who = 0` なら呼び出したスレッド自身。
const IOPRIO_WHO_PROCESS: libc::c_long = 1;

/// 呼び出したスレッド自身を表す `who`。
const IOPRIO_WHO_CURRENT_THREAD: libc::c_long = 0;

/// ワーカースレッドのスタックサイズ。rayon の並列イテレータの入れ子で、深いツリーでは
/// スタックが深くなるため、Rust の標準の既定（2MiB）より大きくする（設計書 8）。
pub const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// 呼び出したスレッドの I/O 優先度を設定する。`ioprio` は `IOPRIO_PRIO_VALUE(class, data)` の
/// 形式（[`crate::dir_scan::settings::IoPriority::to_ioprio_value`]）。失敗なら errno を返す。
pub fn ioprio_set_current_thread(ioprio: i32) -> Result<(), i32> {
    // SAFETY: ioprio_set は 3 つの整数だけを受け取り、ポインタを渡さない。
    // 引数はすべて c_long に揃えて渡している（syscall は可変長引数のため）。
    let ret = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            IOPRIO_WHO_CURRENT_THREAD,
            libc::c_long::from(ioprio),
        )
    };
    if ret == -1 {
        Err(last_errno())
    } else {
        Ok(())
    }
}

/// 呼び出したスレッドの I/O 優先度を読む。返す値は `ioprio_get(2)` の戻り値そのもの
/// （`IOPRIO_PRIO_VALUE(class, data)` の形式）。失敗なら errno を返す。
pub fn ioprio_get_current_thread() -> Result<i32, i32> {
    // SAFETY: ioprio_get は 2 つの整数だけを受け取り、ポインタを渡さない。
    let ret = unsafe {
        libc::syscall(
            libc::SYS_ioprio_get,
            IOPRIO_WHO_PROCESS,
            IOPRIO_WHO_CURRENT_THREAD,
        )
    };
    if ret == -1 {
        return Err(last_errno());
    }
    // 成功時の戻り値は 16 ビットに収まる優先度の値。範囲外ならカーネルの契約違反なので、
    // パニックせずに EIO の失敗として扱う。
    i32::try_from(ret).map_err(|_| libc::EIO)
}

/// 呼び出したスレッドの OS のスレッド ID（`gettid`）。
pub fn current_os_thread_id() -> i32 {
    // SAFETY: gettid は引数を取らず、失敗しない。
    let ret = unsafe { libc::syscall(libc::SYS_gettid) };
    // スレッド ID は pid_t（i32）に収まる。
    i32::try_from(ret).unwrap_or(-1)
}

/// 呼び出したスレッドに、`ioprio` の値（`IOPRIO_PRIO_VALUE` 形式）を設定して読み返す。
/// パニックしない。失敗は返す結果に載せ、`errors` にも記録する。
pub fn apply_ioprio_value_to_current_thread(
    worker_index: usize,
    requested: i32,
    errors: &crate::dir_scan::report::ErrorRecorder,
) -> crate::dir_scan::report::WorkerIoprioResult {
    let os_thread_id = current_os_thread_id();
    let (set, read_back) = match ioprio_set_current_thread(requested) {
        Err(errno) => (
            crate::dir_scan::report::IoprioSetResult::Failure { errno },
            crate::dir_scan::report::IoprioGetResult::NotAttempted,
        ),
        Ok(()) => {
            let read_back = match ioprio_get_current_thread() {
                Ok(value) => crate::dir_scan::report::IoprioGetResult::Value(value),
                Err(errno) => crate::dir_scan::report::IoprioGetResult::Failure { errno },
            };
            (crate::dir_scan::report::IoprioSetResult::Success, read_back)
        }
    };
    let result = crate::dir_scan::report::WorkerIoprioResult {
        worker_index,
        os_thread_id,
        requested,
        set,
        read_back,
    };
    record_ioprio_errors(&result, errors);
    result
}

/// 呼び出したスレッドに `io_priority` を設定して読み返す（ワーカー開始時の処理）。
/// [`apply_ioprio_value_to_current_thread`] を参照。
pub fn apply_io_priority_to_current_thread(
    worker_index: usize,
    io_priority: crate::dir_scan::settings::IoPriority,
    errors: &crate::dir_scan::report::ErrorRecorder,
) -> crate::dir_scan::report::WorkerIoprioResult {
    apply_ioprio_value_to_current_thread(worker_index, io_priority.to_ioprio_value(), errors)
}

/// ワーカーの結果が失敗なら、`ErrorRecorder` に記録する。種類の割り当ては次のとおり。
///
/// - 設定に失敗した: `IoprioSet`（errno あり）
/// - 設定は成功したが読み返しに失敗した: `IoprioGet`（errno あり）
/// - 読み返しをしなかった（設定が成功したのに `NotAttempted`）: `IoprioGet`（errno なし）
/// - 読み返した値が要求値と違う: `IoprioGet`（errno なし。値の食い違いは読み返しの結果の
///   異常として扱う）
///
/// 成功（`IoprioStatus::Ok`）なら何も記録しない。
pub fn record_ioprio_errors(
    result: &crate::dir_scan::report::WorkerIoprioResult,
    errors: &crate::dir_scan::report::ErrorRecorder,
) {
    let no_path = std::path::Path::new("");
    match (result.set, result.read_back) {
        (crate::dir_scan::report::IoprioSetResult::Failure { errno }, _) => {
            errors.record(
                crate::dir_scan::report::ScanErrorKind::IoprioSet,
                Some(errno),
                no_path,
                &format!(
                    "worker={} tid={} ioprio_set({}) failed",
                    result.worker_index,
                    result.os_thread_id,
                    crate::dir_scan::report::format_ioprio_value(result.requested)
                ),
            );
        }
        (
            crate::dir_scan::report::IoprioSetResult::Success,
            crate::dir_scan::report::IoprioGetResult::Failure { errno },
        ) => {
            errors.record(
                crate::dir_scan::report::ScanErrorKind::IoprioGet,
                Some(errno),
                no_path,
                &format!(
                    "worker={} tid={} ioprio_get failed",
                    result.worker_index, result.os_thread_id
                ),
            );
        }
        (
            crate::dir_scan::report::IoprioSetResult::Success,
            crate::dir_scan::report::IoprioGetResult::NotAttempted,
        ) => {
            errors.record(
                crate::dir_scan::report::ScanErrorKind::IoprioGet,
                None,
                no_path,
                &format!(
                    "worker={} tid={} ioprio_get was not attempted",
                    result.worker_index, result.os_thread_id
                ),
            );
        }
        (
            crate::dir_scan::report::IoprioSetResult::Success,
            crate::dir_scan::report::IoprioGetResult::Value(value),
        ) => {
            if value != result.requested {
                errors.record(
                    crate::dir_scan::report::ScanErrorKind::IoprioGet,
                    None,
                    no_path,
                    &format!(
                        "worker={} tid={} ioprio mismatch: requested={} read_back={}",
                        result.worker_index,
                        result.os_thread_id,
                        crate::dir_scan::report::format_ioprio_value(result.requested),
                        crate::dir_scan::report::format_ioprio_value(value)
                    ),
                );
            }
        }
    }
}

/// スキャン 1 回ぶんのスレッドプールと、各ワーカーの I/O 優先度の結果。
///
/// `pool` はスキャンの走査処理が `pool.install` で使う。スキャンが終わったら、この値ごと
/// 破棄する（ワーカーのスレッドは `pool` の drop で終了する）。
pub struct IoprioPool {
    pub pool: rayon::ThreadPool,
    worker_results:
        std::sync::Arc<std::sync::Mutex<Vec<crate::dir_scan::report::WorkerIoprioResult>>>,
}

impl IoprioPool {
    /// 各ワーカーの結果をワーカー番号順に返す。[`build_pool`] が戻った後は、並列度ぶんが揃っている。
    pub fn worker_results(&self) -> Vec<crate::dir_scan::report::WorkerIoprioResult> {
        let mut results = self
            .worker_results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        results.sort_by_key(|r| r.worker_index);
        results
    }
}

/// 並列度ぶんのワーカーを持つスレッドプールを作る。各ワーカーは開始時に `io_priority` を
/// 設定して読み返し、結果を [`IoprioPool::worker_results`] に残す。
///
/// 戻ったとき、全ワーカーの開始時の処理は完了している。rayon の `start_handler` は
/// スレッドを起動した直後にそのスレッド上で呼ばれ、`ThreadPoolBuilder::build` は
/// 全スレッドの `start_handler` の完了を待たない（rayon-core の `main_loop` は「準備完了」
/// の合図を `start_handler` の前に出す）。また `pool.install` は 1 つのワーカーでしか
/// 実行されないので、他のワーカーが開始済みかは分からない。そこで `pool.broadcast` を
/// 1 回呼ぶ。`broadcast` は全ワーカーでそれぞれ 1 回ずつ実行され、全部の完了を待って戻る。
/// ワーカーは `start_handler` が戻ってからでないと仕事（`broadcast` を含む）を取らないので、
/// 戻った時点で全ワーカーの `start_handler` が完了し、結果の書き込みも見える。
///
/// プールを作れなかった場合（スレッドの起動失敗など）と、結果が並列度ぶん揃わなかった
/// 場合は `Err`。呼び出し側はスキャンを失敗にする。
pub fn build_pool(
    parallelism: crate::dir_scan::settings::Parallelism,
    io_priority: crate::dir_scan::settings::IoPriority,
    errors: std::sync::Arc<crate::dir_scan::report::ErrorRecorder>,
) -> anyhow::Result<IoprioPool> {
    use anyhow::Context;
    let worker_count = parallelism.get();
    let worker_results =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(worker_count)));
    let handler_results = std::sync::Arc::clone(&worker_results);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_count)
        .stack_size(WORKER_STACK_SIZE)
        .thread_name(|index| format!("dir_scan_worker_{index}"))
        .start_handler(move |worker_index| {
            // パニックしない。結果を記録するだけ。Mutex が poisoned でも中身を使う。
            let result = apply_io_priority_to_current_thread(worker_index, io_priority, &errors);
            handler_results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(result);
        })
        .build()
        .context("failed to build the dir_scan thread pool")?;
    // 全ワーカーの start_handler の完了を待つバリア
    let broadcast_count = pool.broadcast(|_context| ()).len();
    let recorded_count = worker_results
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len();
    if broadcast_count != worker_count || recorded_count != worker_count {
        anyhow::bail!(
            "dir_scan thread pool started an unexpected number of workers: \
             expected={worker_count} broadcast={broadcast_count} recorded={recorded_count}"
        );
    }
    Ok(IoprioPool {
        pool,
        worker_results,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_logger() {
        env_logger::builder().is_test(true).try_init().ok();
    }

    fn all_priorities() -> Vec<crate::dir_scan::settings::IoPriority> {
        let mut list = vec![crate::dir_scan::settings::IoPriority::Idle];
        for level in 0..=7 {
            list.push(crate::dir_scan::settings::IoPriority::best_effort(level).unwrap());
        }
        list
    }

    fn parallelism(value: usize) -> crate::dir_scan::settings::Parallelism {
        crate::dir_scan::settings::Parallelism::new(value).unwrap()
    }

    #[test]
    fn set_and_get_on_current_thread_round_trip() {
        init_logger();
        // テストスレッドは使い捨てなので、設定を変えても他のテストに影響しない。
        std::thread::spawn(|| {
            for priority in all_priorities() {
                let value = priority.to_ioprio_value();
                ioprio_set_current_thread(value).unwrap();
                assert_eq!(ioprio_get_current_thread().unwrap(), value, "{priority}");
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn invalid_ioprio_is_rejected_with_einval() {
        init_logger();
        std::thread::spawn(|| {
            // 存在しないクラス 7 は EINVAL
            let invalid = 7 << 13;
            assert_eq!(ioprio_set_current_thread(invalid), Err(libc::EINVAL));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn os_thread_id_differs_between_threads() {
        init_logger();
        let main_tid = current_os_thread_id();
        let other_tid = std::thread::spawn(current_os_thread_id).join().unwrap();
        assert!(main_tid > 0);
        assert!(other_tid > 0);
        assert_ne!(main_tid, other_tid);
    }

    #[test]
    fn failure_of_set_is_recorded_in_result_and_errors() {
        init_logger();
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        let invalid = 7 << 13;
        let result = std::thread::scope(|scope| {
            scope
                .spawn(|| apply_ioprio_value_to_current_thread(3, invalid, &errors))
                .join()
                .unwrap()
        });
        assert_eq!(result.worker_index, 3);
        assert_eq!(result.requested, invalid);
        assert!(result.os_thread_id > 0);
        assert_eq!(
            result.set,
            crate::dir_scan::report::IoprioSetResult::Failure {
                errno: libc::EINVAL
            }
        );
        assert_eq!(
            result.read_back,
            crate::dir_scan::report::IoprioGetResult::NotAttempted
        );
        assert_eq!(
            result.status(),
            crate::dir_scan::report::IoprioStatus::SetFailed
        );
        let counts = errors.snapshot();
        assert_eq!(counts.total(), 1);
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::IoprioSet),
            1
        );
        assert_eq!(counts.errno_count(libc::EINVAL), 1);
    }

    #[test]
    fn success_is_recorded_without_errors() {
        init_logger();
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        let result = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    apply_io_priority_to_current_thread(
                        0,
                        crate::dir_scan::settings::IoPriority::Idle,
                        &errors,
                    )
                })
                .join()
                .unwrap()
        });
        assert_eq!(
            result.status(),
            crate::dir_scan::report::IoprioStatus::Ok,
            "{result:?}"
        );
        assert_eq!(
            result.read_back,
            crate::dir_scan::report::IoprioGetResult::Value(
                crate::dir_scan::settings::IoPriority::Idle.to_ioprio_value()
            )
        );
        assert_eq!(errors.snapshot().total(), 0);
    }

    fn synthetic(
        set: crate::dir_scan::report::IoprioSetResult,
        read_back: crate::dir_scan::report::IoprioGetResult,
    ) -> crate::dir_scan::report::WorkerIoprioResult {
        crate::dir_scan::report::WorkerIoprioResult {
            worker_index: 1,
            os_thread_id: 4242,
            requested: crate::dir_scan::settings::IoPriority::Idle.to_ioprio_value(),
            set,
            read_back,
        }
    }

    #[test]
    fn record_errors_distinguishes_set_get_and_mismatch() {
        init_logger();
        let idle = crate::dir_scan::settings::IoPriority::Idle.to_ioprio_value();
        let best_effort = crate::dir_scan::settings::IoPriority::best_effort(4)
            .unwrap()
            .to_ioprio_value();

        // 設定に失敗した: IoprioSet、errno あり
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        record_ioprio_errors(
            &synthetic(
                crate::dir_scan::report::IoprioSetResult::Failure { errno: libc::EPERM },
                crate::dir_scan::report::IoprioGetResult::NotAttempted,
            ),
            &errors,
        );
        let counts = errors.snapshot();
        assert_eq!(counts.total(), 1);
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::IoprioSet),
            1
        );
        assert_eq!(counts.errno_count(libc::EPERM), 1);

        // 設定は成功したが読み返しに失敗した: IoprioGet、errno あり
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        record_ioprio_errors(
            &synthetic(
                crate::dir_scan::report::IoprioSetResult::Success,
                crate::dir_scan::report::IoprioGetResult::Failure { errno: libc::ESRCH },
            ),
            &errors,
        );
        let counts = errors.snapshot();
        assert_eq!(counts.total(), 1);
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::IoprioGet),
            1
        );
        assert_eq!(counts.errno_count(libc::ESRCH), 1);

        // 読み返した値が要求値と違う: IoprioGet、errno なし
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        record_ioprio_errors(
            &synthetic(
                crate::dir_scan::report::IoprioSetResult::Success,
                crate::dir_scan::report::IoprioGetResult::Value(best_effort),
            ),
            &errors,
        );
        let counts = errors.snapshot();
        assert_eq!(counts.total(), 1);
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::IoprioGet),
            1
        );
        assert!(counts.by_errno.is_empty());

        // 設定は成功したのに読み返しをしていない: IoprioGet、errno なし
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        record_ioprio_errors(
            &synthetic(
                crate::dir_scan::report::IoprioSetResult::Success,
                crate::dir_scan::report::IoprioGetResult::NotAttempted,
            ),
            &errors,
        );
        assert_eq!(
            errors
                .snapshot()
                .count(crate::dir_scan::report::ScanErrorKind::IoprioGet),
            1
        );

        // 一致: 何も記録しない
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        record_ioprio_errors(
            &synthetic(
                crate::dir_scan::report::IoprioSetResult::Success,
                crate::dir_scan::report::IoprioGetResult::Value(idle),
            ),
            &errors,
        );
        assert_eq!(errors.snapshot().total(), 0);
    }

    #[test]
    fn pool_workers_read_back_requested_priority() {
        init_logger();
        for priority in all_priorities() {
            let errors = std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new());
            let pool =
                build_pool(parallelism(4), priority, std::sync::Arc::clone(&errors)).unwrap();
            let results = pool.worker_results();
            assert_eq!(results.len(), 4, "{priority}");
            for (index, result) in results.iter().enumerate() {
                assert_eq!(result.worker_index, index, "{priority}");
                assert_eq!(result.requested, priority.to_ioprio_value(), "{priority}");
                assert_eq!(
                    result.set,
                    crate::dir_scan::report::IoprioSetResult::Success,
                    "{priority}: {result:?}"
                );
                assert_eq!(
                    result.read_back,
                    crate::dir_scan::report::IoprioGetResult::Value(priority.to_ioprio_value()),
                    "{priority}: {result:?}"
                );
                assert!(!result.is_failure());
            }
            assert_eq!(errors.snapshot().total(), 0, "{priority}");
        }
    }

    #[test]
    fn pool_records_every_worker_exactly_once() {
        init_logger();
        for worker_count in [1, 2, 8, 64] {
            let errors = std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new());
            let pool = build_pool(
                parallelism(worker_count),
                crate::dir_scan::settings::IoPriority::Idle,
                errors,
            )
            .unwrap();
            assert_eq!(pool.pool.current_num_threads(), worker_count);
            let results = pool.worker_results();
            let indexes: Vec<usize> = results.iter().map(|r| r.worker_index).collect();
            assert_eq!(indexes, (0..worker_count).collect::<Vec<usize>>());
            // OS のスレッド ID はワーカーごとに異なる
            let mut thread_ids: Vec<i32> = results.iter().map(|r| r.os_thread_id).collect();
            thread_ids.sort();
            thread_ids.dedup();
            assert_eq!(thread_ids.len(), worker_count);
        }
    }

    #[test]
    fn worker_thread_has_the_requested_priority_and_name() {
        init_logger();
        let priority = crate::dir_scan::settings::IoPriority::best_effort(6).unwrap();
        let errors = std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new());
        let pool = build_pool(parallelism(3), priority, errors).unwrap();
        // プール内で実行した処理から見ても、読み返した値が要求値で、スレッド名が付いている
        let observed: Vec<(i32, String)> = pool.pool.broadcast(|_context| {
            (
                ioprio_get_current_thread().unwrap(),
                std::thread::current().name().unwrap().to_string(),
            )
        });
        assert_eq!(observed.len(), 3);
        for (value, name) in &observed {
            assert_eq!(*value, priority.to_ioprio_value());
            assert!(name.starts_with("dir_scan_worker_"), "{name}");
        }
    }
}
