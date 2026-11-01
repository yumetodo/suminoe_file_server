//! スキャンのレポートとエラーの種類（設計書 6.2, 6.5, 6.7）。

/// 1 回のスキャンにつき、種類ごとに `log::warn!` に出す件数の上限。
/// 以降は件数だけ数え、`debug` レベルでは全件出す。
pub const WARN_LOG_LIMIT_PER_KIND: u64 = 100;

/// エラーの種類（設計書 6.5）。OS のエラー番号は別に数える（[`ErrorCounts::by_errno`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScanErrorKind {
    /// ディレクトリを開けない
    OpenDirectory,
    /// readdir（getdents）が失敗した
    ReadDirectory,
    /// ファイルの stat が `ENOENT`（readdir と stat の間に消えた。正常な競合）
    StatFileNotFound,
    /// ファイルの stat が `ENOENT` 以外で失敗した
    StatFile,
    /// 種別が不明（`DT_UNKNOWN`）のエントリの stat が失敗した
    StatUnknownEntry,
    /// 走査中の変更（ディレクトリが `ENOENT`・`ENOTDIR`・`ELOOP` で開けない）
    ChangedDuringScan,
    /// 循環を検出した
    CycleDetected,
    /// `ioprio_set` の失敗
    IoprioSet,
    /// `ioprio_get` の失敗
    IoprioGet,
    /// 走査中のパニック
    WalkPanic,
}

impl ScanErrorKind {
    pub const ALL: [ScanErrorKind; 10] = [
        ScanErrorKind::OpenDirectory,
        ScanErrorKind::ReadDirectory,
        ScanErrorKind::StatFileNotFound,
        ScanErrorKind::StatFile,
        ScanErrorKind::StatUnknownEntry,
        ScanErrorKind::ChangedDuringScan,
        ScanErrorKind::CycleDetected,
        ScanErrorKind::IoprioSet,
        ScanErrorKind::IoprioGet,
        ScanErrorKind::WalkPanic,
    ];

    /// ファイルの stat の失敗を、errno に応じて `StatFileNotFound`（`ENOENT`）か `StatFile` に分ける。
    pub fn stat_file(errno: i32) -> Self {
        if errno == libc::ENOENT {
            Self::StatFileNotFound
        } else {
            Self::StatFile
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenDirectory => "OpenDirectory",
            Self::ReadDirectory => "ReadDirectory",
            Self::StatFileNotFound => "StatFileNotFound",
            Self::StatFile => "StatFile",
            Self::StatUnknownEntry => "StatUnknownEntry",
            Self::ChangedDuringScan => "ChangedDuringScan",
            Self::CycleDetected => "CycleDetected",
            Self::IoprioSet => "IoprioSet",
            Self::IoprioGet => "IoprioGet",
            Self::WalkPanic => "WalkPanic",
        }
    }
}

impl std::fmt::Display for ScanErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 種類別・OS のエラー番号別のエラー件数と、ログを抑制した件数。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorCounts {
    /// 種類別の件数（ログを抑制したものも含む）
    pub by_kind: std::collections::BTreeMap<ScanErrorKind, u64>,
    /// OS のエラー番号別の件数（エラー番号が無いエラー、例えば循環やパニックは数えない）
    pub by_errno: std::collections::BTreeMap<i32, u64>,
    /// 種類別の、`warn` への出力を抑制した件数
    pub suppressed_by_kind: std::collections::BTreeMap<ScanErrorKind, u64>,
}

impl ErrorCounts {
    pub fn new() -> Self {
        Self::default()
    }

    /// エラー 1 件を数える。
    pub fn record(&mut self, kind: ScanErrorKind, errno: Option<i32>) {
        *self.by_kind.entry(kind).or_insert(0) += 1;
        if let Some(errno) = errno {
            *self.by_errno.entry(errno).or_insert(0) += 1;
        }
    }

    /// 別の集計を足し込む。
    pub fn merge(&mut self, other: &ErrorCounts) {
        for (kind, count) in &other.by_kind {
            *self.by_kind.entry(*kind).or_insert(0) += count;
        }
        for (errno, count) in &other.by_errno {
            *self.by_errno.entry(*errno).or_insert(0) += count;
        }
        for (kind, count) in &other.suppressed_by_kind {
            *self.suppressed_by_kind.entry(*kind).or_insert(0) += count;
        }
    }

    pub fn count(&self, kind: ScanErrorKind) -> u64 {
        self.by_kind.get(&kind).copied().unwrap_or(0)
    }

    pub fn errno_count(&self, errno: i32) -> u64 {
        self.by_errno.get(&errno).copied().unwrap_or(0)
    }

    pub fn suppressed_count(&self, kind: ScanErrorKind) -> u64 {
        self.suppressed_by_kind.get(&kind).copied().unwrap_or(0)
    }

    pub fn total(&self) -> u64 {
        self.by_kind.values().sum()
    }

    pub fn suppressed_total(&self) -> u64 {
        self.suppressed_by_kind.values().sum()
    }

    /// 正常な競合（`StatFileNotFound`）と走査中の変更（`ChangedDuringScan`）を除いた件数。
    /// 「エラーありで成功」かどうかの判断に使える。
    pub fn total_excluding_races(&self) -> u64 {
        self.total()
            - self.count(ScanErrorKind::StatFileNotFound)
            - self.count(ScanErrorKind::ChangedDuringScan)
    }
}

/// エラーの記録器。ワーカーから `&self` で共有して使う。
/// 1 スキャンにつき 1 つ作り、種類ごとに先頭 [`WARN_LOG_LIMIT_PER_KIND`] 件だけ
/// `log::warn!` に出す。以降は `log::debug!` に出し（`debug` レベルでは全件見える）、
/// 抑制した件数を数える。
#[derive(Debug)]
pub struct ErrorRecorder {
    warn_limit_per_kind: u64,
    counts: std::sync::Mutex<ErrorCounts>,
}

impl ErrorRecorder {
    /// 上限は [`WARN_LOG_LIMIT_PER_KIND`]。
    pub fn new() -> Self {
        Self::with_warn_limit(WARN_LOG_LIMIT_PER_KIND)
    }

    /// 上限を指定する（テスト用）。
    pub fn with_warn_limit(warn_limit_per_kind: u64) -> Self {
        Self {
            warn_limit_per_kind,
            counts: std::sync::Mutex::new(ErrorCounts::new()),
        }
    }

    /// エラー 1 件を記録してログに出す。`warn` に出した（抑制しなかった）なら `true`。
    /// `path` はルートからの相対パス（ログにはエスケープして出す）。
    pub fn record(
        &self,
        kind: ScanErrorKind,
        errno: Option<i32>,
        path: &std::path::Path,
        detail: &str,
    ) -> bool {
        let within_limit = {
            let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
            counts.record(kind, errno);
            // record で数えたあとの件数が上限以内なら出す
            let within_limit = counts.count(kind) <= self.warn_limit_per_kind;
            if !within_limit {
                *counts.suppressed_by_kind.entry(kind).or_insert(0) += 1;
            }
            within_limit
        };
        if within_limit {
            log::warn!(
                "scan error: kind={} errno={} path={} detail={}",
                kind,
                format_errno(errno),
                escape_path_for_log(path),
                detail
            );
        } else {
            log::debug!(
                "scan error (suppressed in warn): kind={} errno={} path={} detail={}",
                kind,
                format_errno(errno),
                escape_path_for_log(path),
                detail
            );
        }
        within_limit
    }

    /// これまでの集計の写し。
    pub fn snapshot(&self) -> ErrorCounts {
        self.counts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Default for ErrorRecorder {
    fn default() -> Self {
        Self::new()
    }
}

fn format_errno(errno: Option<i32>) -> String {
    match errno {
        Some(errno) => format!("{} ({})", errno, std::io::Error::from_raw_os_error(errno)),
        None => "none".to_string(),
    }
}

/// バイト列をログ出力用にエスケープする。
/// 正しい UTF-8 の部分はそのまま出し、制御文字は `\u{..}`、バックスラッシュは `\\`、
/// 非 UTF-8 のバイトは `\xNN` にする。
pub fn escape_bytes_for_log(bytes: &[u8]) -> String {
    let mut escaped = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c == '\\' {
                escaped.push_str("\\\\");
            } else if c.is_control() {
                escaped.push_str(&format!("\\u{{{:x}}}", u32::from(c)));
            } else {
                escaped.push(c);
            }
        }
        for byte in chunk.invalid() {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

/// 名前をログ出力用にエスケープする（[`escape_bytes_for_log`]）。
pub fn escape_os_str_for_log(name: &std::ffi::OsStr) -> String {
    escape_bytes_for_log(std::os::unix::ffi::OsStrExt::as_bytes(name))
}

/// パスをログ出力用にエスケープする（[`escape_bytes_for_log`]）。
pub fn escape_path_for_log(path: &std::path::Path) -> String {
    escape_os_str_for_log(path.as_os_str())
}

/// `ioprio_set` の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoprioSetResult {
    Success,
    Failure { errno: i32 },
}

/// `ioprio_get` の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoprioGetResult {
    /// 読み返しをしなかった
    NotAttempted,
    /// 読み返した値（`ioprio_get` が返した値そのもの）
    Value(i32),
    Failure {
        errno: i32,
    },
}

/// ワーカーの I/O 優先度の結果の分類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoprioStatus {
    /// 設定に成功し、読み返した値が要求値と一致した
    Ok,
    /// 設定に失敗した
    SetFailed,
    /// 設定は成功したが、読み返しに失敗した（または読み返さなかった）
    GetFailed,
    /// 設定も読み返しも成功したが、読み返した値が要求値と違う
    Mismatch,
}

/// ワーカー 1 つの I/O 優先度の結果（設計書 6.7）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerIoprioResult {
    /// ワーカー番号（rayon の `start_handler` に渡される添字）
    pub worker_index: usize,
    /// OS のスレッド ID（`gettid`）
    pub os_thread_id: i32,
    /// 要求した値（[`crate::dir_scan::settings::IoPriority::to_ioprio_value`]）
    pub requested: i32,
    pub set: IoprioSetResult,
    pub read_back: IoprioGetResult,
}

impl WorkerIoprioResult {
    pub fn status(&self) -> IoprioStatus {
        match (self.set, self.read_back) {
            (IoprioSetResult::Failure { .. }, _) => IoprioStatus::SetFailed,
            (IoprioSetResult::Success, IoprioGetResult::Value(value)) => {
                if value == self.requested {
                    IoprioStatus::Ok
                } else {
                    IoprioStatus::Mismatch
                }
            }
            (IoprioSetResult::Success, _) => IoprioStatus::GetFailed,
        }
    }

    pub fn is_failure(&self) -> bool {
        self.status() != IoprioStatus::Ok
    }
}

/// `ioprio` の値を `idle`・`best-effort:N` などの文字列にする。
pub fn format_ioprio_value(value: i32) -> String {
    let class = value >> 13;
    let data = value & ((1 << 13) - 1);
    match class {
        0 => format!("none:{data}"),
        1 => format!("realtime:{data}"),
        2 => format!("best-effort:{data}"),
        3 => format!("idle:{data}"),
        _ => format!("unknown({value:#x})"),
    }
}

/// 所要時間の内訳（単調増加する時計で測る）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanDurations {
    /// 全体
    pub total: std::time::Duration,
    /// 走査
    pub walk: std::time::Duration,
    /// 書き込みロックを取るまでの待ち
    pub lock_wait: std::time::Duration,
    /// 反映（書き込みロックを持っている間）
    pub apply: std::time::Duration,
    /// 古いツリー（部分木）の解放
    pub drop_old_tree: std::time::Duration,
}

/// `generation` の、スキャン開始時・反映前・反映後の値（設計書 6.6）。
/// 反映されなかったスキャン（失敗）では、増やさないので 3 つとも（観測した）同じ値になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanGenerations {
    pub at_start: u64,
    pub before_apply: u64,
    pub after_apply: u64,
}

/// スキャンの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScanKind {
    Full,
    Partial,
}

impl std::fmt::Display for ScanKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("full"),
            Self::Partial => f.write_str("partial"),
        }
    }
}

/// 部分スキャンの対象が `ENOENT`・`ENOTDIR`・`ELOOP` で開けず、親に遡ってやり直した経緯の 1 段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParentRetry {
    /// 開けなかったディレクトリ（ルートからの相対パス）
    pub failed_path: std::path::PathBuf,
    /// 開けなかった理由の errno
    pub errno: i32,
    /// やり直した先（親。ルートなら空のパス）
    pub retry_path: std::path::PathBuf,
}

/// スキャンの結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanOutcome {
    /// エラーなしで成功
    Success,
    /// エラーを含んだまま成功（ツリーは「不完全」になりうる）
    SuccessWithErrors,
    /// ツリーに反映されなかった（ルートが開けない、部分スキャンの対象が `EACCES` などで開けない、
    /// 走査中のパニックなど）。理由の文字列を持つ
    Failed(String),
}

/// スキャンのレポート本体（設計書 6.2「スキャンのレポートには次を含める」）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    pub execution_id: u64,
    /// この実行に含まれる `request_id` の一覧（統合された要求を含む）
    pub request_ids: Vec<u64>,
    /// 実際に走ったスキャンの種類
    pub kind: ScanKind,
    /// 要求されたパス（ルートからの相対パス）。フルスキャンの要求なら `None`。
    /// `kind` が `Full` で `Some` なら、部分スキャンがフルスキャンに格上げされたことを表す
    pub requested_path: Option<std::path::PathBuf>,
    /// 実際に走査したパス（ルートからの相対パス）。ルート全体なら空のパス
    pub effective_path: std::path::PathBuf,
    /// 親に遡ってやり直した場合の経緯（遡らなければ空）
    pub parent_retries: Vec<ParentRetry>,
    /// 要求された設定（除外パターンは正規化後のもの）
    pub settings: crate::dir_scan::settings::ScanSettings,
    /// 開始時刻（システム時刻）
    pub started_at: std::time::SystemTime,
    /// 終了時刻（システム時刻）
    pub finished_at: std::time::SystemTime,
    pub durations: ScanDurations,
    /// ワーカーごとの I/O 優先度の結果（ワーカー番号順）
    pub worker_ioprio: Vec<WorkerIoprioResult>,
    pub dir_count: u64,
    pub file_count: u64,
    /// 走査したサブツリーの合計サイズ（フルスキャンならルートの合計サイズ）
    pub root_total_size: u128,
    pub hash_collision_count: u64,
    pub errors: ErrorCounts,
    pub generations: ScanGenerations,
    pub outcome: ScanOutcome,
}

fn system_time_to_string(time: std::time::SystemTime) -> String {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => format!("{}.{:03}", elapsed.as_secs(), elapsed.subsec_millis()),
        Err(_) => "before-epoch".to_string(),
    }
}

impl ScanReport {
    /// `log::info!` に出す要約。ワーカーごとの I/O 優先度は失敗したものだけを含める。
    pub fn summary(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        let requested_path = match &self.requested_path {
            Some(path) => format!("\"{}\"", escape_path_for_log(path)),
            None => "full".to_string(),
        };
        let effective_path = format!("\"{}\"", escape_path_for_log(&self.effective_path));
        let outcome = match &self.outcome {
            ScanOutcome::Success => "success".to_string(),
            ScanOutcome::SuccessWithErrors => "success-with-errors".to_string(),
            ScanOutcome::Failed(reason) => format!("failed({reason})"),
        };
        lines.push(format!(
            "scan finished: execution_id={} request_ids={:?} kind={} requested_path={} effective_path={} outcome={}",
            self.execution_id, self.request_ids, self.kind, requested_path, effective_path, outcome
        ));
        for retry in &self.parent_retries {
            lines.push(format!(
                "  parent retry: \"{}\" (errno {}) -> \"{}\"",
                escape_path_for_log(&retry.failed_path),
                retry.errno,
                escape_path_for_log(&retry.retry_path)
            ));
        }
        lines.push(format!(
            "  settings: parallelism={} io_priority={} exclude_patterns={:?}",
            self.settings.parallelism.get(),
            self.settings.io_priority,
            self.settings.exclude_patterns.patterns()
        ));
        lines.push(format!(
            "  started_at={} finished_at={}",
            system_time_to_string(self.started_at),
            system_time_to_string(self.finished_at)
        ));
        lines.push(format!(
            "  durations: total={:.3}s walk={:.3}s lock_wait={:.3}s apply={:.3}s drop_old_tree={:.3}s",
            self.durations.total.as_secs_f64(),
            self.durations.walk.as_secs_f64(),
            self.durations.lock_wait.as_secs_f64(),
            self.durations.apply.as_secs_f64(),
            self.durations.drop_old_tree.as_secs_f64()
        ));
        lines.push(format!(
            "  dirs={} files={} root_total_size={} hash_collisions={}",
            self.dir_count, self.file_count, self.root_total_size, self.hash_collision_count
        ));
        lines.push(format!(
            "  generation: at_start={} before_apply={} after_apply={}",
            self.generations.at_start, self.generations.before_apply, self.generations.after_apply
        ));
        let mut error_parts: Vec<String> = Vec::new();
        for kind in ScanErrorKind::ALL {
            let count = self.errors.count(kind);
            if count > 0 {
                error_parts.push(format!(
                    "{}={}(suppressed {})",
                    kind,
                    count,
                    self.errors.suppressed_count(kind)
                ));
            }
        }
        let errno_parts: Vec<String> = self
            .errors
            .by_errno
            .iter()
            .map(|(errno, count)| format!("{errno}={count}"))
            .collect();
        lines.push(format!(
            "  errors: total={} suppressed={} by_kind=[{}] by_errno=[{}]",
            self.errors.total(),
            self.errors.suppressed_total(),
            error_parts.join(", "),
            errno_parts.join(", ")
        ));
        let failed_workers: Vec<&WorkerIoprioResult> = self
            .worker_ioprio
            .iter()
            .filter(|w| w.is_failure())
            .collect();
        lines.push(format!(
            "  ioprio: workers={} failed={}",
            self.worker_ioprio.len(),
            failed_workers.len()
        ));
        for worker in failed_workers {
            lines.push(format!(
                "    worker {} (tid {}): status={:?} requested={} set={:?} read_back={:?}",
                worker.worker_index,
                worker.os_thread_id,
                worker.status(),
                format_ioprio_value(worker.requested),
                worker.set,
                worker.read_back
            ));
        }
        lines.join("\n")
    }

    /// 要約を `log::info!` に出す。
    pub fn log_summary(&self) {
        log::info!("{}", self.summary());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_logger() {
        env_logger::builder().is_test(true).try_init().ok();
    }

    fn sample_settings() -> crate::dir_scan::settings::ScanSettings {
        crate::dir_scan::settings::ScanSettings::new(
            crate::dir_scan::settings::Parallelism::new(4).unwrap(),
            crate::dir_scan::settings::IoPriority::Idle,
            crate::dir_scan::settings::ExcludePatterns::new(&["**/.*".to_string()]).unwrap(),
        )
    }

    fn worker(
        worker_index: usize,
        requested: i32,
        set: IoprioSetResult,
        read_back: IoprioGetResult,
    ) -> WorkerIoprioResult {
        WorkerIoprioResult {
            worker_index,
            os_thread_id: 1000 + worker_index as i32,
            requested,
            set,
            read_back,
        }
    }

    fn sample_report() -> ScanReport {
        let idle = crate::dir_scan::settings::IoPriority::Idle.to_ioprio_value();
        let mut errors = ErrorCounts::new();
        errors.record(ScanErrorKind::OpenDirectory, Some(libc::EACCES));
        errors.record(ScanErrorKind::StatFileNotFound, Some(libc::ENOENT));
        errors.record(ScanErrorKind::CycleDetected, None);
        *errors
            .suppressed_by_kind
            .entry(ScanErrorKind::OpenDirectory)
            .or_insert(0) += 1;
        ScanReport {
            execution_id: 7,
            request_ids: vec![3, 5],
            kind: ScanKind::Partial,
            requested_path: Some(std::path::PathBuf::from("a/b/file.txt")),
            effective_path: std::path::PathBuf::from("a/b"),
            parent_retries: vec![ParentRetry {
                failed_path: std::path::PathBuf::from("a/b/c"),
                errno: libc::ENOENT,
                retry_path: std::path::PathBuf::from("a/b"),
            }],
            settings: sample_settings(),
            started_at: std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123),
            finished_at: std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(1_700_000_001_456),
            durations: ScanDurations {
                total: std::time::Duration::from_millis(1500),
                walk: std::time::Duration::from_millis(1000),
                lock_wait: std::time::Duration::from_millis(2),
                apply: std::time::Duration::from_millis(3),
                drop_old_tree: std::time::Duration::from_millis(4),
            },
            worker_ioprio: vec![
                worker(
                    0,
                    idle,
                    IoprioSetResult::Success,
                    IoprioGetResult::Value(idle),
                ),
                worker(
                    1,
                    idle,
                    IoprioSetResult::Failure { errno: libc::EPERM },
                    IoprioGetResult::Value(0),
                ),
                worker(2, idle, IoprioSetResult::Success, IoprioGetResult::Value(0)),
            ],
            dir_count: 10,
            file_count: 20,
            root_total_size: 123_456_789_012_345_678_901_234,
            hash_collision_count: 2,
            errors,
            generations: ScanGenerations {
                at_start: 4,
                before_apply: 5,
                after_apply: 6,
            },
            outcome: ScanOutcome::SuccessWithErrors,
        }
    }

    #[test]
    fn stat_file_kind_distinguishes_enoent() {
        init_logger();
        assert_eq!(
            ScanErrorKind::stat_file(libc::ENOENT),
            ScanErrorKind::StatFileNotFound
        );
        assert_eq!(
            ScanErrorKind::stat_file(libc::EACCES),
            ScanErrorKind::StatFile
        );
        assert_eq!(ScanErrorKind::stat_file(libc::EIO), ScanErrorKind::StatFile);
    }

    #[test]
    fn all_kinds_are_listed_once_and_named() {
        init_logger();
        let set: std::collections::BTreeSet<ScanErrorKind> =
            ScanErrorKind::ALL.iter().copied().collect();
        assert_eq!(set.len(), ScanErrorKind::ALL.len());
        let names: std::collections::BTreeSet<&str> =
            ScanErrorKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(names.len(), ScanErrorKind::ALL.len());
        assert_eq!(ScanErrorKind::WalkPanic.to_string(), "WalkPanic");
    }

    #[test]
    fn error_counts_aggregate_by_kind_and_errno() {
        init_logger();
        let mut counts = ErrorCounts::new();
        counts.record(ScanErrorKind::OpenDirectory, Some(libc::EACCES));
        counts.record(ScanErrorKind::OpenDirectory, Some(libc::EACCES));
        counts.record(ScanErrorKind::StatFile, Some(libc::EACCES));
        counts.record(ScanErrorKind::StatFile, Some(libc::EIO));
        counts.record(ScanErrorKind::CycleDetected, None);
        assert_eq!(counts.count(ScanErrorKind::OpenDirectory), 2);
        assert_eq!(counts.count(ScanErrorKind::StatFile), 2);
        assert_eq!(counts.count(ScanErrorKind::CycleDetected), 1);
        assert_eq!(counts.count(ScanErrorKind::WalkPanic), 0);
        assert_eq!(counts.errno_count(libc::EACCES), 3);
        assert_eq!(counts.errno_count(libc::EIO), 1);
        // errno の無いエラーは errno 別には数えない
        assert_eq!(counts.by_errno.values().sum::<u64>(), 4);
        assert_eq!(counts.total(), 5);
    }

    #[test]
    fn error_counts_merge_adds_everything() {
        init_logger();
        let mut a = ErrorCounts::new();
        a.record(ScanErrorKind::ReadDirectory, Some(libc::EIO));
        a.suppressed_by_kind.insert(ScanErrorKind::ReadDirectory, 1);
        let mut b = ErrorCounts::new();
        b.record(ScanErrorKind::ReadDirectory, Some(libc::EIO));
        b.record(ScanErrorKind::WalkPanic, None);
        b.suppressed_by_kind.insert(ScanErrorKind::ReadDirectory, 2);
        a.merge(&b);
        assert_eq!(a.count(ScanErrorKind::ReadDirectory), 2);
        assert_eq!(a.count(ScanErrorKind::WalkPanic), 1);
        assert_eq!(a.errno_count(libc::EIO), 2);
        assert_eq!(a.suppressed_count(ScanErrorKind::ReadDirectory), 3);
        assert_eq!(a.suppressed_total(), 3);
    }

    #[test]
    fn total_excluding_races_ignores_enoent_and_changes() {
        init_logger();
        let mut counts = ErrorCounts::new();
        counts.record(ScanErrorKind::StatFileNotFound, Some(libc::ENOENT));
        counts.record(ScanErrorKind::ChangedDuringScan, Some(libc::ENOTDIR));
        assert_eq!(counts.total(), 2);
        assert_eq!(counts.total_excluding_races(), 0);
        counts.record(ScanErrorKind::StatFile, Some(libc::EACCES));
        assert_eq!(counts.total_excluding_races(), 1);
    }

    #[test]
    fn recorder_logs_first_limit_per_kind_then_suppresses() {
        init_logger();
        let recorder = ErrorRecorder::new();
        let path = std::path::Path::new("a/b");
        let mut warned = 0;
        for _ in 0..105 {
            if recorder.record(ScanErrorKind::OpenDirectory, Some(libc::EACCES), path, "x") {
                warned += 1;
            }
        }
        assert_eq!(warned, 100);
        // 別の種類は別に数える
        assert!(recorder.record(ScanErrorKind::ReadDirectory, Some(libc::EIO), path, "y"));
        let counts = recorder.snapshot();
        assert_eq!(counts.count(ScanErrorKind::OpenDirectory), 105);
        assert_eq!(counts.suppressed_count(ScanErrorKind::OpenDirectory), 5);
        assert_eq!(counts.count(ScanErrorKind::ReadDirectory), 1);
        assert_eq!(counts.suppressed_count(ScanErrorKind::ReadDirectory), 0);
        assert_eq!(counts.errno_count(libc::EACCES), 105);
        assert_eq!(counts.suppressed_total(), 5);
    }

    #[test]
    fn recorder_default_limit_is_100() {
        init_logger();
        assert_eq!(WARN_LOG_LIMIT_PER_KIND, 100);
        let recorder = ErrorRecorder::with_warn_limit(2);
        let path = std::path::Path::new("");
        assert!(recorder.record(ScanErrorKind::WalkPanic, None, path, ""));
        assert!(recorder.record(ScanErrorKind::WalkPanic, None, path, ""));
        assert!(!recorder.record(ScanErrorKind::WalkPanic, None, path, ""));
        assert_eq!(
            recorder
                .snapshot()
                .suppressed_count(ScanErrorKind::WalkPanic),
            1
        );
    }

    #[test]
    fn recorder_is_shareable_between_threads() {
        init_logger();
        let recorder = ErrorRecorder::new();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        recorder.record(
                            ScanErrorKind::StatFile,
                            Some(libc::EIO),
                            std::path::Path::new("d"),
                            "t",
                        );
                    }
                });
            }
        });
        let counts = recorder.snapshot();
        assert_eq!(counts.count(ScanErrorKind::StatFile), 200);
        assert_eq!(counts.suppressed_count(ScanErrorKind::StatFile), 100);
    }

    #[test]
    fn escape_keeps_plain_utf8() {
        init_logger();
        assert_eq!(escape_bytes_for_log(b"a/b.txt"), "a/b.txt");
        assert_eq!(escape_bytes_for_log("日本語/é".as_bytes()), "日本語/é");
        assert_eq!(escape_bytes_for_log(b""), "");
    }

    #[test]
    fn escape_non_utf8_control_and_backslash() {
        init_logger();
        assert_eq!(escape_bytes_for_log(b"a\xffb"), "a\\xffb");
        assert_eq!(escape_bytes_for_log(b"\xc3"), "\\xc3");
        assert_eq!(escape_bytes_for_log(b"a\nb\tc"), "a\\u{a}b\\u{9}c");
        assert_eq!(escape_bytes_for_log(b"a\\b"), "a\\\\b");
        assert_eq!(escape_bytes_for_log(b"\x1b[0m"), "\\u{1b}[0m");
        // 有効な文字と無効なバイトが混ざる場合
        assert_eq!(escape_bytes_for_log("é\u{ff}".as_bytes()), "éÿ");
        assert_eq!(escape_bytes_for_log(b"\xe3\x81"), "\\xe3\\x81");
    }

    #[test]
    fn escape_os_str_and_path() {
        init_logger();
        let os_str = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"d/\xfe");
        assert_eq!(escape_os_str_for_log(os_str), "d/\\xfe");
        assert_eq!(escape_path_for_log(std::path::Path::new(os_str)), "d/\\xfe");
    }

    #[test]
    fn worker_ioprio_status_is_classified() {
        init_logger();
        let want = crate::dir_scan::settings::IoPriority::best_effort(3)
            .unwrap()
            .to_ioprio_value();
        assert_eq!(
            worker(
                0,
                want,
                IoprioSetResult::Success,
                IoprioGetResult::Value(want)
            )
            .status(),
            IoprioStatus::Ok
        );
        assert_eq!(
            worker(
                0,
                want,
                IoprioSetResult::Failure { errno: libc::EPERM },
                IoprioGetResult::NotAttempted
            )
            .status(),
            IoprioStatus::SetFailed
        );
        assert_eq!(
            worker(
                0,
                want,
                IoprioSetResult::Success,
                IoprioGetResult::Failure {
                    errno: libc::EINVAL
                }
            )
            .status(),
            IoprioStatus::GetFailed
        );
        assert_eq!(
            worker(
                0,
                want,
                IoprioSetResult::Success,
                IoprioGetResult::NotAttempted
            )
            .status(),
            IoprioStatus::GetFailed
        );
        assert_eq!(
            worker(0, want, IoprioSetResult::Success, IoprioGetResult::Value(0)).status(),
            IoprioStatus::Mismatch
        );
        assert!(worker(0, want, IoprioSetResult::Success, IoprioGetResult::Value(0)).is_failure());
        assert!(!worker(
            0,
            want,
            IoprioSetResult::Success,
            IoprioGetResult::Value(want)
        )
        .is_failure());
    }

    #[test]
    fn ioprio_value_formatting() {
        init_logger();
        assert_eq!(
            format_ioprio_value(crate::dir_scan::settings::IoPriority::Idle.to_ioprio_value()),
            "idle:0"
        );
        assert_eq!(
            format_ioprio_value(
                crate::dir_scan::settings::IoPriority::best_effort(5)
                    .unwrap()
                    .to_ioprio_value()
            ),
            "best-effort:5"
        );
        assert_eq!(format_ioprio_value(0), "none:0");
        assert_eq!(format_ioprio_value(1 << 13), "realtime:0");
        assert_eq!(format_ioprio_value(7 << 13), "unknown(0xe000)");
    }

    #[test]
    fn durations_default_is_zero() {
        init_logger();
        let d = ScanDurations::default();
        assert_eq!(d.total, std::time::Duration::ZERO);
        assert_eq!(d.drop_old_tree, std::time::Duration::ZERO);
    }

    #[test]
    fn scan_kind_display() {
        init_logger();
        assert_eq!(ScanKind::Full.to_string(), "full");
        assert_eq!(ScanKind::Partial.to_string(), "partial");
    }

    #[test]
    fn summary_contains_every_report_item() {
        init_logger();
        let report = sample_report();
        let summary = report.summary();
        // execution_id と request_id
        assert!(summary.contains("execution_id=7"), "{summary}");
        assert!(summary.contains("[3, 5]"), "{summary}");
        // 種類・パス・遡りの経緯
        assert!(summary.contains("kind=partial"), "{summary}");
        assert!(
            summary.contains("requested_path=\"a/b/file.txt\""),
            "{summary}"
        );
        assert!(summary.contains("effective_path=\"a/b\""), "{summary}");
        assert!(summary.contains("parent retry: \"a/b/c\""), "{summary}");
        // 設定（正規化後の除外パターン）
        assert!(summary.contains("parallelism=4"), "{summary}");
        assert!(summary.contains("io_priority=idle"), "{summary}");
        assert!(summary.contains("\"**/.*\""), "{summary}");
        // 時刻と所要時間の内訳
        assert!(summary.contains("started_at=1700000000.123"), "{summary}");
        assert!(summary.contains("finished_at=1700000001.456"), "{summary}");
        assert!(summary.contains("total=1.500s"), "{summary}");
        assert!(summary.contains("walk=1.000s"), "{summary}");
        assert!(summary.contains("lock_wait=0.002s"), "{summary}");
        assert!(summary.contains("apply=0.003s"), "{summary}");
        assert!(summary.contains("drop_old_tree=0.004s"), "{summary}");
        // 件数
        assert!(summary.contains("dirs=10"), "{summary}");
        assert!(summary.contains("files=20"), "{summary}");
        assert!(
            summary.contains("root_total_size=123456789012345678901234"),
            "{summary}"
        );
        assert!(summary.contains("hash_collisions=2"), "{summary}");
        // エラー
        assert!(
            summary.contains("OpenDirectory=1(suppressed 1)"),
            "{summary}"
        );
        assert!(
            summary.contains("StatFileNotFound=1(suppressed 0)"),
            "{summary}"
        );
        assert!(summary.contains("CycleDetected=1"), "{summary}");
        assert!(summary.contains("total=3 suppressed=1"), "{summary}");
        assert!(
            summary.contains(&format!("{}=1", libc::EACCES)),
            "{summary}"
        );
        // generation
        assert!(summary.contains("at_start=4"), "{summary}");
        assert!(summary.contains("before_apply=5"), "{summary}");
        assert!(summary.contains("after_apply=6"), "{summary}");
        assert!(summary.contains("outcome=success-with-errors"), "{summary}");
    }

    #[test]
    fn summary_lists_only_failed_workers() {
        init_logger();
        let report = sample_report();
        let summary = report.summary();
        assert!(summary.contains("ioprio: workers=3 failed=2"), "{summary}");
        assert!(!summary.contains("worker 0 "), "{summary}");
        assert!(
            summary.contains("worker 1 (tid 1001): status=SetFailed"),
            "{summary}"
        );
        assert!(
            summary.contains("worker 2 (tid 1002): status=Mismatch"),
            "{summary}"
        );
    }

    #[test]
    fn summary_for_full_scan_and_failure() {
        init_logger();
        let mut report = sample_report();
        report.kind = ScanKind::Full;
        report.requested_path = None;
        report.effective_path = std::path::PathBuf::new();
        report.parent_retries.clear();
        report.worker_ioprio.clear();
        report.outcome = ScanOutcome::Failed("root open failed".to_string());
        let summary = report.summary();
        assert!(summary.contains("kind=full"), "{summary}");
        assert!(summary.contains("requested_path=full"), "{summary}");
        assert!(summary.contains("effective_path=\"\""), "{summary}");
        assert!(!summary.contains("parent retry"), "{summary}");
        assert!(
            summary.contains("outcome=failed(root open failed)"),
            "{summary}"
        );
        assert!(summary.contains("ioprio: workers=0 failed=0"), "{summary}");
    }

    #[test]
    fn summary_escapes_non_utf8_paths() {
        init_logger();
        let mut report = sample_report();
        report.effective_path = std::path::PathBuf::from(
            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"a/\xff"),
        );
        assert!(report.summary().contains("effective_path=\"a/\\xff\""));
    }

    #[test]
    fn log_summary_does_not_panic() {
        init_logger();
        sample_report().log_summary();
    }
}
