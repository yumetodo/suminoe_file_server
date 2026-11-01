//! 1 回のスキャンの走査処理（設計書 6.5, 6.6）。
//!
//! ルートの open を含むファイルシステムへのアクセスはすべて、スキャンごとに作る rayon の
//! スレッドプール（[`crate::dir_scan::ioprio::build_pool`]）のワーカーの中で行う。
//! 呼び出したスレッドはファイルシステムに触れない。
//!
//! - ディレクトリは、ルートの FD から相対パスを 1 段ずつ `openat(O_DIRECTORY|O_NOFOLLOW)` で
//!   開く（`openat2` は使わない。カーネル 4.18 でも動く）。途中の段の FD はすぐ閉じる。
//! - 開いたディレクトリを `fstat` して dev/ino を得て、祖先に同じものがあれば循環として
//!   中に入らない。
//! - readdir は rustix の [`rustix::fs::Dir`]（getdents64）で行い、種別は `d_type` から決める。
//!   `DT_UNKNOWN` のときだけそのエントリを stat する。
//! - ファイルとリンクは inode 番号順に、開いたディレクトリの FD を基準に `statx`
//!   （`AT_SYMLINK_NOFOLLOW`）して `st_size` を得る。自分の FD を閉じてから、子ディレクトリを
//!   inode 番号順に並べて rayon の並列イテレータで再帰する。
//! - 組み立ては下から行う。子を完成・`finalize` してから親に `add_child` し、親の
//!   `set_files` とエラーの付与をして `finalize` する。ツリーへの反映（ロック）は扱わない。
//!
//! Goal の `find <root> -name '.*' -prune -o \( -type f -o -type l \) -printf '%h\t%s\n'` と
//! 同じ値になる設計にしている（ディレクトリ自身のサイズは含めない、リンクは辿らずリンク
//! 自身のサイズ、FIFO・ソケット・デバイスは数えない、ハードリンクは名前ごとに数える）。

/// デバイス番号と inode 番号の組。循環の検出に使う。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevIno {
    pub dev: u64,
    pub ino: u64,
}

/// 走査の失敗（ツリーには反映しない）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkFailure {
    /// スレッドプールを作れなかった
    PoolBuildFailed(String),
    /// ルートを開けなかった
    RootOpenFailed { errno: i32 },
    /// 部分スキャンの対象が `EACCES`・`EIO` などで開けなかった（親には遡らない）
    TargetOpenFailed {
        path: std::path::PathBuf,
        errno: i32,
    },
    /// 走査の対象のパスが不正（絶対パスや `..` を含む）
    InvalidTarget(std::path::PathBuf),
    /// 走査中にパニックした
    Panicked(String),
    /// ツリーの組み立て中の内部不整合（合計の加算が溢れた、など）
    Internal(String),
}

impl std::fmt::Display for WalkFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PoolBuildFailed(reason) => write!(f, "failed to build thread pool: {reason}"),
            Self::RootOpenFailed { errno } => write!(
                f,
                "failed to open the root directory: {}",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::TargetOpenFailed { path, errno } => write!(
                f,
                "failed to open the scan target {:?}: {}",
                crate::dir_scan::report::escape_path_for_log(path),
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::InvalidTarget(path) => write!(
                f,
                "invalid scan target: {:?}",
                crate::dir_scan::report::escape_path_for_log(path)
            ),
            Self::Panicked(message) => write!(f, "panicked during the walk: {message}"),
            Self::Internal(reason) => write!(f, "internal error during the walk: {reason}"),
        }
    }
}

/// 走査に成功した結果。
#[derive(Debug)]
pub struct WalkSuccess {
    /// 組み立てた部分木（`finalize` 済み）。`name` は `effective_path` の最後の要素
    /// （ルートなら空）
    pub node: crate::dir_scan::tree::DirNode,
    /// `node` のディレクトリ数・ファイル数・ハッシュ衝突の件数
    pub counts: crate::dir_scan::tree::TreeCounts,
}

/// 走査の結果。成功しても失敗しても、ワーカーの I/O 優先度の結果とエラー件数は返す。
#[derive(Debug)]
pub struct WalkResult {
    pub outcome: Result<WalkSuccess, WalkFailure>,
    /// 実際に走査したパス（ルートからの相対パス。親に遡った場合は遡った後。ルートなら空）。
    /// 失敗した場合は、失敗した時点のパス
    pub effective_path: std::path::PathBuf,
    /// 部分スキャンの対象が `ENOENT`・`ENOTDIR`・`ELOOP` で開けず、親に遡った経緯
    pub parent_retries: Vec<crate::dir_scan::report::ParentRetry>,
    /// ワーカーごとの I/O 優先度の結果（ワーカー番号順）。プールを作れなかった場合は空
    pub worker_ioprio: Vec<crate::dir_scan::report::WorkerIoprioResult>,
    /// 走査中に記録したエラーの件数（`errors` の写し）
    pub errors: crate::dir_scan::report::ErrorCounts,
    /// 走査の所要時間（プールの作成から結果が揃うまで）
    pub duration: std::time::Duration,
}

/// 1 回のスキャンの走査を実行する。
///
/// - `root`: ルートのパス。最後の要素がシンボリックリンクなら辿る（ルートは運用者が指定する
///   ため）。その下は一切リンクを辿らない。
/// - `target`: 走査する対象のルートからの相対パス。フルスキャンは空のパス。部分スキャンは
///   [`crate::dir_scan::tree::Tree::resolve_partial_target`] が決めた `effective_path`。
/// - `ancestors`: ルートから `target` の親までのディレクトリの dev/ino（ルートを含み、
///   `target` 自身は含まない）。部分スキャンの走査が、フルスキャンと同じ循環の判定をするため
///   に使う（[`crate::dir_scan::tree::Tree::ancestor_identities`]）。フルスキャンは空。
///   親に遡って対象が変わったときは、遡った段数ぶん末尾を捨てて、遡った後の対象の親までの
///   先頭部分だけを使う。
/// - `name_hasher`: フルスキャンは新規、部分スキャンはツリーのものの複製。
/// - `errors`: スキャン 1 回につき 1 つの記録器。
///
/// プールの作成に失敗した場合と、走査中のパニックは [`WalkFailure`] として返す。
pub fn run_walk(
    root: &std::path::Path,
    target: &std::path::Path,
    ancestors: &[DevIno],
    settings: &crate::dir_scan::settings::ScanSettings,
    name_hasher: &crate::dir_scan::tree::NameHasher,
    errors: std::sync::Arc<crate::dir_scan::report::ErrorRecorder>,
) -> WalkResult {
    let started = std::time::Instant::now();
    let pool = match crate::dir_scan::ioprio::build_pool(
        settings.parallelism,
        settings.io_priority,
        std::sync::Arc::clone(&errors),
    ) {
        Ok(pool) => pool,
        Err(error) => {
            let reason = format!("{error:#}");
            log::warn!("failed to build the thread pool: {reason}");
            return WalkResult {
                outcome: Err(WalkFailure::PoolBuildFailed(reason)),
                effective_path: target.to_path_buf(),
                parent_retries: Vec::new(),
                worker_ioprio: Vec::new(),
                errors: errors.snapshot(),
                duration: started.elapsed(),
            };
        }
    };

    let mut effective_path = target.to_path_buf();
    let mut parent_retries: Vec<crate::dir_scan::report::ParentRetry> = Vec::new();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pool.pool.install(|| {
            walk_in_pool(
                root,
                &mut effective_path,
                ancestors,
                &mut parent_retries,
                settings,
                name_hasher,
                &errors,
            )
        })
    }));
    let outcome = match caught {
        Ok(outcome) => outcome,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            errors.record(
                crate::dir_scan::report::ScanErrorKind::WalkPanic,
                None,
                &effective_path,
                &message,
            );
            Err(WalkFailure::Panicked(message))
        }
    };
    let worker_ioprio = pool.worker_results();
    // `install` が戻った時点で走査は終わっており、I/O 優先度の記録も `build_pool` の中で
    // 揃っている。`drop` はプールを手放すだけで、ワーカーのスレッドの終了は待たない。
    drop(pool);
    let duration = started.elapsed();
    log::debug!(
        "walk finished: effective_path={:?} ok={} duration={:?}",
        crate::dir_scan::report::escape_path_for_log(&effective_path),
        outcome.is_ok(),
        duration
    );
    WalkResult {
        outcome,
        effective_path,
        parent_retries,
        worker_ioprio,
        errors: errors.snapshot(),
        duration,
    }
}

/// `catch_unwind` が捕まえたパニックの内容を文字列にする。
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// ルートから見た走査の対象を、`Normal` な要素だけのパスにする（`.` は飛ばす）。
/// 絶対パスや `..` を含む場合は `None`。
fn normalize_target(target: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut normalized = std::path::PathBuf::new();
    for component in target.components() {
        match component {
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(normalized)
}

/// 開けなかったディレクトリが「走査中に変わった」ものとして扱うエラー番号か
/// （`ENOENT`・`ENOTDIR`・`ELOOP`）。
fn is_changed_during_scan_errno(errno: i32) -> bool {
    errno == libc::ENOENT || errno == libc::ENOTDIR || errno == libc::ELOOP
}

/// `pool.install` の中で実行する部分。ルートと対象を開き、走査して部分木を組み立てる。
///
/// 対象が `ENOENT`・`ENOTDIR`・`ELOOP` で開けなければ、親を対象にしてやり直し、経緯を
/// `parent_retries` に残す（`effective_path` を親に更新する）。`EACCES` などは遡らず失敗。
///
/// `ancestors` は最初の対象のルートからその親までの dev/ino。対象が親に遡ると、遡った段数
/// ぶん末尾を捨てて、遡った後の対象の親までの先頭部分を使う。
fn walk_in_pool(
    root: &std::path::Path,
    effective_path: &mut std::path::PathBuf,
    ancestors: &[DevIno],
    parent_retries: &mut Vec<crate::dir_scan::report::ParentRetry>,
    settings: &crate::dir_scan::settings::ScanSettings,
    name_hasher: &crate::dir_scan::tree::NameHasher,
    errors: &crate::dir_scan::report::ErrorRecorder,
) -> Result<WalkSuccess, WalkFailure> {
    *effective_path = normalize_target(effective_path)
        .ok_or_else(|| WalkFailure::InvalidTarget(effective_path.clone()))?;

    // ルートは pool.install の中（ワーカー上）で開き、その FD を走査全体で共有する
    let root_fd = rustix::fs::openat(
        rustix::fs::CWD,
        root,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|errno| {
        let errno = errno.raw_os_error();
        errors.record(
            crate::dir_scan::report::ScanErrorKind::OpenDirectory,
            Some(errno),
            std::path::Path::new(""),
            &format!(
                "failed to open the root directory {:?}",
                crate::dir_scan::report::escape_path_for_log(root)
            ),
        );
        WalkFailure::RootOpenFailed { errno }
    })?;

    let initial_depth = effective_path.components().count();
    let target_fd = loop {
        match open_relative_dir(rustix::fd::AsFd::as_fd(&root_fd), effective_path) {
            Ok(fd) => break fd,
            Err(errno) => {
                if effective_path.as_os_str().is_empty() {
                    // ルートは開けているので通常は起きない。起きたらルートが開けないのと同じ
                    errors.record(
                        crate::dir_scan::report::ScanErrorKind::OpenDirectory,
                        Some(errno),
                        effective_path,
                        "failed to reopen the root directory",
                    );
                    return Err(WalkFailure::RootOpenFailed { errno });
                }
                if !is_changed_during_scan_errno(errno) {
                    errors.record(
                        crate::dir_scan::report::ScanErrorKind::OpenDirectory,
                        Some(errno),
                        effective_path,
                        "failed to open the scan target",
                    );
                    return Err(WalkFailure::TargetOpenFailed {
                        path: effective_path.clone(),
                        errno,
                    });
                }
                let parent = effective_path
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_default();
                log::debug!(
                    "scan target vanished or changed, retrying with the parent: path={:?} errno={} parent={:?}",
                    crate::dir_scan::report::escape_path_for_log(effective_path),
                    errno,
                    crate::dir_scan::report::escape_path_for_log(&parent)
                );
                parent_retries.push(crate::dir_scan::report::ParentRetry {
                    failed_path: effective_path.clone(),
                    errno,
                    retry_path: parent.clone(),
                });
                *effective_path = parent;
            }
        }
    };

    let context = WalkContext {
        root_fd: rustix::fd::AsFd::as_fd(&root_fd),
        exclude_patterns: &settings.exclude_patterns,
        name_hasher,
        errors,
    };
    // 親に遡った段数ぶん、一覧の末尾を捨てる（遡った後の対象の親までの先頭部分にする）
    let retried_levels = initial_depth.saturating_sub(effective_path.components().count());
    let target_ancestors = &ancestors[..ancestors.len().saturating_sub(retried_levels)];
    let target_name = effective_path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    let node = walk_opened(
        &context,
        target_fd,
        effective_path,
        target_name,
        // 開いた後の fstat が失敗したときの代わりの値（対象は dev/ino を知らない）
        DevIno { dev: 0, ino: 0 },
        target_ancestors,
    )
    .map_err(|error| WalkFailure::Internal(error.to_string()))?;
    let counts = node.counts();
    Ok(WalkSuccess { node, counts })
}

/// 走査全体で共有する参照。
struct WalkContext<'a> {
    /// ルートの FD。ディレクトリを開くときの基準にだけ使い、readdir には使わない
    root_fd: rustix::fd::BorrowedFd<'a>,
    exclude_patterns: &'a crate::dir_scan::settings::ExcludePatterns,
    name_hasher: &'a crate::dir_scan::tree::NameHasher,
    errors: &'a crate::dir_scan::report::ErrorRecorder,
}

fn dir_open_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC
}

/// ルートの FD から、相対パスを 1 段ずつ `openat(O_DIRECTORY|O_NOFOLLOW)` で開く。
/// 途中の段の FD は次の段を開いたらすぐ閉じる。`NOFOLLOW` が全段に効くので、途中に
/// シンボリックリンクがあっても辿らない。空のパスはルート自身を開き直す。
/// 失敗したら、失敗した段の errno を返す。
fn open_relative_dir(
    root_fd: rustix::fd::BorrowedFd<'_>,
    relative_path: &std::path::Path,
) -> Result<rustix::fd::OwnedFd, i32> {
    let mut current: Option<rustix::fd::OwnedFd> = None;
    for component in relative_path.components() {
        let name = match component {
            std::path::Component::Normal(name) => name,
            std::path::Component::CurDir => continue,
            _ => return Err(libc::EINVAL),
        };
        let opened = {
            let base = match &current {
                Some(fd) => rustix::fd::AsFd::as_fd(fd),
                None => root_fd,
            };
            rustix::fs::openat(base, name, dir_open_flags(), rustix::fs::Mode::empty())
                .map_err(rustix::io::Errno::raw_os_error)?
        };
        // 代入で 1 つ前の段の FD を閉じる
        current = Some(opened);
    }
    match current {
        Some(fd) => Ok(fd),
        None => rustix::fs::openat(root_fd, ".", dir_open_flags(), rustix::fs::Mode::empty())
            .map_err(rustix::io::Errno::raw_os_error),
    }
}

/// エントリの種別（readdir の結果、または stat の結果から決める）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryClass {
    /// 数える対象（通常ファイル・シンボリックリンク）
    File,
    /// 再帰する対象
    Directory,
    /// 捨てる（FIFO・ソケット・デバイス）
    Skip,
    /// 種別が分からない（`DT_UNKNOWN`）
    Unknown,
}

fn classify(file_type: rustix::fs::FileType) -> EntryClass {
    match file_type {
        rustix::fs::FileType::RegularFile | rustix::fs::FileType::Symlink => EntryClass::File,
        rustix::fs::FileType::Directory => EntryClass::Directory,
        rustix::fs::FileType::Unknown => EntryClass::Unknown,
        _ => EntryClass::Skip,
    }
}

/// `DT_UNKNOWN` のエントリを、開いたディレクトリの FD を基準に stat（リンクは辿らない）して
/// 種別を決める。
fn lookup_entry_class(
    dir_fd: rustix::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
) -> Result<EntryClass, i32> {
    let stat = rustix::fs::statx(
        dir_fd,
        name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        rustix::fs::StatxFlags::TYPE,
    )
    .map_err(rustix::io::Errno::raw_os_error)?;
    Ok(classify(rustix::fs::FileType::from_raw_mode(u32::from(
        stat.stx_mode,
    ))))
}

/// `DT_UNKNOWN` のエントリの stat の結果（`looked_up`）から、そのエントリの種別を決める。
/// 種別を決められない（`None`）ときは、そのエントリを合計に含めない。
///
/// - 成功: その種別。
/// - `ENOENT`（readdir と stat の間に消えた）: 通常ファイルの stat の `ENOENT` と同じ正常な
///   競合として、`StatFileNotFound` に数える。ノードのエラーにはしない。
/// - その他（`EACCES`・`EIO` など）: `StatUnknownEntry` に数え、`node` にエラーを付ける
///   （エントリが欠けた合計を「完全」と返さないため）。
fn settle_unknown_entry(
    context: &WalkContext<'_>,
    relative_path: &std::path::Path,
    name: &std::ffi::OsStr,
    looked_up: Result<EntryClass, i32>,
    node: &mut crate::dir_scan::tree::DirNode,
) -> Option<EntryClass> {
    match looked_up {
        Ok(class) => Some(class),
        Err(errno) if errno == libc::ENOENT => {
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::stat_file(errno),
                Some(errno),
                &relative_path.join(name),
                "an entry whose type is unknown vanished before it was stat-ed",
            );
            None
        }
        Err(errno) => {
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::StatUnknownEntry,
                Some(errno),
                &relative_path.join(name),
                "failed to stat an entry whose type is unknown",
            );
            node.record_stat_error(errno);
            None
        }
    }
}

/// readdir で得た、再帰または stat する対象のエントリ。
struct PendingEntry {
    ino: u64,
    name: std::ffi::OsString,
}

/// 1 つのディレクトリの readdir の結果。
struct DirListing {
    files: Vec<PendingEntry>,
    directories: Vec<PendingEntry>,
    /// readdir が途中で失敗した場合の errno
    read_dir_errno: Option<i32>,
}

/// ディレクトリを readdir し、数える対象（ファイル・リンク）と再帰する対象（ディレクトリ）に
/// 分ける。FIFO・ソケット・デバイスと、除外パターンに当たるものは捨てる。
/// 種別不明のエントリの stat が失敗したときのエラーは `node` に付ける
/// （[`settle_unknown_entry`]）。
fn read_entries(
    context: &WalkContext<'_>,
    dir: &mut rustix::fs::Dir,
    relative_path: &std::path::Path,
    node: &mut crate::dir_scan::tree::DirNode,
) -> DirListing {
    let mut listing = DirListing {
        files: Vec::new(),
        directories: Vec::new(),
        read_dir_errno: None,
    };
    while let Some(read) = dir.read() {
        let entry = match read {
            Ok(entry) => entry,
            Err(errno) => {
                let errno = errno.raw_os_error();
                context.errors.record(
                    crate::dir_scan::report::ScanErrorKind::ReadDirectory,
                    Some(errno),
                    relative_path,
                    "getdents failed",
                );
                listing.read_dir_errno = Some(errno);
                break;
            }
        };
        let name_bytes = entry.file_name().to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let mut class = classify(entry.file_type());
        if class == EntryClass::Skip {
            continue;
        }
        let name = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name_bytes);
        if context
            .exclude_patterns
            .is_excluded_child(relative_path, name)
        {
            continue;
        }
        if class == EntryClass::Unknown {
            let looked_up = match dir.fd() {
                Ok(dir_fd) => lookup_entry_class(dir_fd, name),
                Err(errno) => Err(errno.raw_os_error()),
            };
            class = match settle_unknown_entry(context, relative_path, name, looked_up, node) {
                Some(class) => class,
                None => continue,
            };
        }
        let pending = PendingEntry {
            ino: entry.ino(),
            name: name.to_os_string(),
        };
        match class {
            EntryClass::File => listing.files.push(pending),
            EntryClass::Directory => listing.directories.push(pending),
            EntryClass::Skip | EntryClass::Unknown => {}
        }
    }
    // inode 番号順（同じ inode はハードリンクなので名前順）に処理する
    listing
        .files
        .sort_unstable_by(|a, b| a.ino.cmp(&b.ino).then_with(|| a.name.cmp(&b.name)));
    listing
        .directories
        .sort_unstable_by(|a, b| a.ino.cmp(&b.ino).then_with(|| a.name.cmp(&b.name)));
    listing
}

/// ファイル・リンクを `statx`（リンクは辿らない）して `st_size` を得る。順序は `files` のまま。
/// `ENOENT` は数えずに進む。その他のエラーはそのファイルを合計に含めず、`node` にエラーを付ける。
fn measure_files(
    context: &WalkContext<'_>,
    dir_fd: rustix::fd::BorrowedFd<'_>,
    relative_path: &std::path::Path,
    files: Vec<PendingEntry>,
    node: &mut crate::dir_scan::tree::DirNode,
) -> Vec<(std::ffi::OsString, u64)> {
    let mut measured: Vec<(std::ffi::OsString, u64)> = Vec::with_capacity(files.len());
    for file in files {
        match rustix::fs::statx(
            dir_fd,
            file.name.as_os_str(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            rustix::fs::StatxFlags::SIZE,
        ) {
            Ok(stat) => measured.push((file.name, stat.stx_size)),
            Err(errno) => {
                let errno = errno.raw_os_error();
                context.errors.record(
                    crate::dir_scan::report::ScanErrorKind::stat_file(errno),
                    Some(errno),
                    &relative_path.join(&file.name),
                    "statx failed",
                );
                node.record_stat_error(errno);
            }
        }
    }
    measured
}

/// 名前が分かっている子ディレクトリを、ルートの FD から相対パスで開いて走査する。
///
/// - `ENOENT`・`ENOTDIR`・`ELOOP` で開けない: `ChangedDuringScan` として数え、`None`
///   （ツリーに載せない）。
/// - その他（`EACCES`・`EPERM`・`EIO` など）で開けない: エラーノード（合計 0）を返す。
fn walk_child(
    context: &WalkContext<'_>,
    relative_path: &std::path::Path,
    name: &std::ffi::OsStr,
    entry_ino: u64,
    parent_dev: u64,
    ancestors: &[DevIno],
) -> Result<Option<crate::dir_scan::tree::DirNode>, crate::dir_scan::tree::TreeError> {
    // 開けないときの代わりの dev/ino。dev は親のもの（マウントポイントでは違いうるが、
    // エラーノードでは使わない）、ino は readdir で得た値
    let fallback = DevIno {
        dev: parent_dev,
        ino: entry_ino,
    };
    match open_relative_dir(context.root_fd, relative_path) {
        Ok(fd) => walk_opened(
            context,
            fd,
            relative_path,
            name.to_os_string(),
            fallback,
            ancestors,
        )
        .map(Some),
        Err(errno) if is_changed_during_scan_errno(errno) => {
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::ChangedDuringScan,
                Some(errno),
                relative_path,
                "directory vanished or changed before it was opened",
            );
            Ok(None)
        }
        Err(errno) => {
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::OpenDirectory,
                Some(errno),
                relative_path,
                "failed to open the directory",
            );
            Ok(Some(open_error_node(name, fallback, errno)?))
        }
    }
}

/// 開けなかったディレクトリのエラーノード（合計 0）。
fn open_error_node(
    name: &std::ffi::OsStr,
    identity: DevIno,
    errno: i32,
) -> Result<crate::dir_scan::tree::DirNode, crate::dir_scan::tree::TreeError> {
    let mut node =
        crate::dir_scan::tree::DirNode::new(name.to_os_string(), identity.dev, identity.ino);
    node.record_open_error(errno);
    node.finalize()?;
    Ok(node)
}

/// テストでパニックを起こすための、ディレクトリの名前。この名前のディレクトリを走査すると
/// パニックする。
#[cfg(test)]
const TEST_PANIC_DIR_NAME: &str = "panic_here_for_walk_test";

/// 開いたディレクトリ（`fd`）を走査して、完成した（`finalize` 済みの）ノードを返す。
///
/// `ancestors` はこのディレクトリの祖先の dev/ino の一覧。同じものがあれば循環として
/// エラー（`CycleDetected`）を付けたノードを返し、中には入らない。
/// `fallback` は `fstat` が失敗したときの代わりの dev/ino。
fn walk_opened(
    context: &WalkContext<'_>,
    fd: rustix::fd::OwnedFd,
    relative_path: &std::path::Path,
    name: std::ffi::OsString,
    fallback: DevIno,
    ancestors: &[DevIno],
) -> Result<crate::dir_scan::tree::DirNode, crate::dir_scan::tree::TreeError> {
    #[cfg(test)]
    {
        if name == TEST_PANIC_DIR_NAME {
            panic!("injected panic for test: {name:?}");
        }
    }

    let identity = match rustix::fs::fstat(&fd) {
        Ok(stat) => DevIno {
            // 64 ビットの Linux（x86_64・aarch64）では u64
            dev: stat.st_dev,
            ino: stat.st_ino,
        },
        Err(errno) => {
            let errno = errno.raw_os_error();
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::OpenDirectory,
                Some(errno),
                relative_path,
                "fstat of the opened directory failed",
            );
            return open_error_node(&name, fallback, errno);
        }
    };
    let mut node = crate::dir_scan::tree::DirNode::new(name, identity.dev, identity.ino);
    if ancestors.contains(&identity) {
        context.errors.record(
            crate::dir_scan::report::ScanErrorKind::CycleDetected,
            None,
            relative_path,
            &format!(
                "the same directory (dev={} ino={}) is one of its ancestors",
                identity.dev, identity.ino
            ),
        );
        node.record_cycle();
        node.finalize()?;
        return Ok(node);
    }

    // この FD を開いている間にファイルを数える。子へ渡す前に閉じる
    let mut dir = match rustix::fs::Dir::new(fd) {
        Ok(dir) => dir,
        Err(errno) => {
            let errno = errno.raw_os_error();
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::OpenDirectory,
                Some(errno),
                relative_path,
                "failed to create the directory stream",
            );
            node.record_open_error(errno);
            node.finalize()?;
            return Ok(node);
        }
    };
    let mut listing = read_entries(context, &mut dir, relative_path, &mut node);
    let files = std::mem::take(&mut listing.files);
    let measured = match dir.fd() {
        Ok(dir_fd) => measure_files(context, dir_fd, relative_path, files, &mut node),
        Err(errno) => {
            let errno = errno.raw_os_error();
            context.errors.record(
                crate::dir_scan::report::ScanErrorKind::ReadDirectory,
                Some(errno),
                relative_path,
                "failed to get the directory fd",
            );
            listing.read_dir_errno.get_or_insert(errno);
            Vec::new()
        }
    };
    drop(dir);

    let mut child_ancestors: Vec<DevIno> = Vec::with_capacity(ancestors.len() + 1);
    child_ancestors.extend_from_slice(ancestors);
    child_ancestors.push(identity);
    let children = walk_children(
        context,
        relative_path,
        &listing.directories,
        identity.dev,
        &child_ancestors,
    )?;

    // 下から組み立てる
    node.set_files(context.name_hasher, &measured)?;
    for child in children.into_iter().flatten() {
        node.add_child(child)?;
    }
    if let Some(errno) = listing.read_dir_errno {
        node.record_read_dir_error(errno);
    }
    node.finalize()?;
    Ok(node)
}

/// 子ディレクトリを（inode 順に並んだまま）rayon の並列イテレータで再帰して走査する。
/// 結果は `directories` と同じ順序。
fn walk_children(
    context: &WalkContext<'_>,
    relative_path: &std::path::Path,
    directories: &[PendingEntry],
    dev: u64,
    child_ancestors: &[DevIno],
) -> Result<Vec<Option<crate::dir_scan::tree::DirNode>>, crate::dir_scan::tree::TreeError> {
    use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
    if directories.is_empty() {
        return Ok(Vec::new());
    }
    directories
        .par_iter()
        .map(|child| {
            walk_child(
                context,
                &relative_path.join(&child.name),
                &child.name,
                child.ino,
                dev,
                child_ancestors,
            )
        })
        .collect()
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

    fn os(bytes: &[u8]) -> &std::ffi::OsStr {
        <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(bytes)
    }

    fn settings(parallelism: usize, patterns: &[&str]) -> crate::dir_scan::settings::ScanSettings {
        let owned: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        crate::dir_scan::settings::ScanSettings::new(
            crate::dir_scan::settings::Parallelism::new(parallelism).unwrap(),
            crate::dir_scan::settings::IoPriority::best_effort(4).unwrap(),
            crate::dir_scan::settings::ExcludePatterns::new(&owned).unwrap(),
        )
    }

    /// `root` の下の `relative` に、サイズ `size` のファイル（スパース）を作る。
    fn make_file(root: &std::path::Path, relative: &str, size: u64) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(size).unwrap();
    }

    fn make_dir(root: &std::path::Path, relative: &str) {
        std::fs::create_dir_all(root.join(relative)).unwrap();
    }

    fn full_walk(
        root: &std::path::Path,
        settings: &crate::dir_scan::settings::ScanSettings,
        hasher: &crate::dir_scan::tree::NameHasher,
    ) -> WalkResult {
        run_walk(
            root,
            std::path::Path::new(""),
            &[],
            settings,
            hasher,
            std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new()),
        )
    }

    fn partial_walk(
        root: &std::path::Path,
        target: &str,
        settings: &crate::dir_scan::settings::ScanSettings,
        hasher: &crate::dir_scan::tree::NameHasher,
    ) -> WalkResult {
        run_walk(
            root,
            std::path::Path::new(target),
            &[],
            settings,
            hasher,
            std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new()),
        )
    }

    fn success(result: WalkResult) -> (WalkSuccess, WalkResult) {
        // outcome を取り出して残りを返す
        let WalkResult {
            outcome,
            effective_path,
            parent_retries,
            worker_ioprio,
            errors,
            duration,
        } = result;
        let ok = outcome.unwrap_or_else(|failure| panic!("walk failed: {failure}"));
        (
            ok,
            WalkResult {
                outcome: Err(WalkFailure::Internal("taken".to_string())),
                effective_path,
                parent_retries,
                worker_ioprio,
                errors,
                duration,
            },
        )
    }

    fn failure(result: &WalkResult) -> &WalkFailure {
        result.outcome.as_ref().expect_err("walk should fail")
    }

    /// 一時ディレクトリにネストしたツリーを作る。
    ///
    /// ```text
    /// f1 (10)
    /// a/f2 (20) a/f3 (30)
    /// a/b/f4 (40)
    /// empty/
    /// ```
    fn sample_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        make_file(dir.path(), "f1", 10);
        make_file(dir.path(), "a/f2", 20);
        make_file(dir.path(), "a/f3", 30);
        make_file(dir.path(), "a/b/f4", 40);
        make_dir(dir.path(), "empty");
        dir
    }

    #[test]
    fn full_walk_sums_nested_directories() {
        init_logger();
        let tree = sample_tree();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(tree.path(), &settings(4, &[]), &hasher));
        let root = &ok.node;
        assert_eq!(root.name(), std::ffi::OsStr::new(""));
        assert_eq!(root.own_size(), 10);
        assert_eq!(root.total_size(), 100);
        let a = root.child(std::ffi::OsStr::new("a")).unwrap();
        assert_eq!(a.own_size(), 50);
        assert_eq!(a.total_size(), 90);
        let b = a.child(std::ffi::OsStr::new("b")).unwrap();
        assert_eq!(b.own_size(), 40);
        assert_eq!(b.total_size(), 40);
        let empty = root.child(std::ffi::OsStr::new("empty")).unwrap();
        assert_eq!(empty.total_size(), 0);
        assert!(empty.is_complete());
        assert!(root.is_complete());
        // root, a, a/b, empty
        assert_eq!(ok.counts.dir_count, 4);
        assert_eq!(ok.counts.file_count, 4);
        assert_eq!(ok.counts.hash_collision_count, 0);
        assert_eq!(ok.counts, root.counts());
        assert!(result.effective_path.as_os_str().is_empty());
        assert!(result.parent_retries.is_empty());
        assert_eq!(result.errors.total(), 0);
    }

    #[test]
    fn hard_links_are_counted_per_name() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "orig", 100);
        std::fs::hard_link(tree.path().join("orig"), tree.path().join("link")).unwrap();
        make_dir(tree.path(), "d");
        std::fs::hard_link(tree.path().join("orig"), tree.path().join("d/another")).unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, _) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        assert_eq!(ok.node.own_size(), 200);
        assert_eq!(ok.node.total_size(), 300);
        assert_eq!(ok.counts.file_count, 3);
    }

    #[test]
    fn directory_own_size_is_not_included() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        for i in 0..200 {
            make_dir(tree.path(), &format!("many_dirs/d{i:03}"));
        }
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, _) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        assert_eq!(ok.node.total_size(), 0);
        assert_eq!(ok.counts.dir_count, 202);
        assert_eq!(ok.counts.file_count, 0);
    }

    #[test]
    fn exclude_patterns_skip_dotfiles_and_subtrees() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "visible", 1);
        // ルート直下の隠しファイルと隠しディレクトリ
        make_file(tree.path(), ".hidden", 1000);
        make_file(tree.path(), ".hidden_dir/x", 1000);
        make_file(tree.path(), "a/keep", 2);
        make_file(tree.path(), "a/.f", 1000);
        make_file(tree.path(), "a/.cache/deep/y", 1000);
        make_file(tree.path(), "a/skip/z", 1000);
        make_file(tree.path(), "a/skip_not/z", 4);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(
            tree.path(),
            &settings(3, &["**/.*", "a/skip"]),
            &hasher,
        ));
        let root = &ok.node;
        assert_eq!(root.own_size(), 1);
        assert!(root.child(std::ffi::OsStr::new(".hidden_dir")).is_none());
        let a = root.child(std::ffi::OsStr::new("a")).unwrap();
        assert_eq!(a.own_size(), 2);
        assert!(a.child(std::ffi::OsStr::new(".cache")).is_none());
        assert!(a.child(std::ffi::OsStr::new("skip")).is_none());
        assert_eq!(
            a.child(std::ffi::OsStr::new("skip_not"))
                .unwrap()
                .own_size(),
            4
        );
        assert_eq!(root.total_size(), 1 + 2 + 4);
        assert_eq!(ok.counts.file_count, 3);
        assert_eq!(ok.counts.dir_count, 3);
        assert_eq!(result.errors.total(), 0);
    }

    #[test]
    fn exclude_patterns_use_path_from_root_in_partial_walk() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "a/keep", 2);
        make_file(tree.path(), "a/b/.dot", 1000);
        make_file(tree.path(), "a/skip/z", 1000);
        make_file(tree.path(), "a/b/c/v", 8);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(partial_walk(
            tree.path(),
            "a",
            &settings(2, &["**/.*", "a/skip"]),
            &hasher,
        ));
        assert_eq!(result.effective_path, std::path::Path::new("a"));
        assert_eq!(ok.node.name(), std::ffi::OsStr::new("a"));
        assert_eq!(ok.node.total_size(), 10);
        assert!(ok.node.child(std::ffi::OsStr::new("skip")).is_none());
    }

    #[test]
    fn symlinks_are_counted_as_files_and_not_followed() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "real/big", 1_000_000);
        make_dir(tree.path(), "d");
        // ディレクトリへのリンク。辿らないので real/big は d の合計に含まれない
        std::os::unix::fs::symlink("../real", tree.path().join("d/to_real")).unwrap();
        // 壊れたリンク
        std::os::unix::fs::symlink("nowhere/at/all", tree.path().join("d/broken")).unwrap();
        // ファイルへのリンク
        std::os::unix::fs::symlink("../real/big", tree.path().join("d/to_big")).unwrap();
        let lstat_size = |name: &str| -> u128 {
            u128::from(
                std::fs::symlink_metadata(tree.path().join("d").join(name))
                    .unwrap()
                    .len(),
            )
        };
        let expected = lstat_size("to_real") + lstat_size("broken") + lstat_size("to_big");
        assert_eq!(lstat_size("to_real"), "../real".len() as u128);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(tree.path(), &settings(4, &[]), &hasher));
        let d = ok.node.child(std::ffi::OsStr::new("d")).unwrap();
        assert_eq!(d.own_size(), expected);
        assert!(d.child(std::ffi::OsStr::new("to_real")).is_none());
        assert_eq!(ok.node.total_size(), 1_000_000 + expected);
        assert_eq!(result.errors.total(), 0);
        assert_eq!(ok.counts.file_count, 4);
        assert_eq!(ok.counts.dir_count, 3);
    }

    #[test]
    fn partial_target_that_is_a_symlink_falls_back_to_parent() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "real/sub/f", 5);
        make_dir(tree.path(), "d");
        make_file(tree.path(), "d/own", 3);
        std::os::unix::fs::symlink("../real", tree.path().join("d/link")).unwrap();
        let link_size = u128::from(
            std::fs::symlink_metadata(tree.path().join("d/link"))
                .unwrap()
                .len(),
        );
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(partial_walk(
            tree.path(),
            "d/link",
            &settings(2, &[]),
            &hasher,
        ));
        assert_eq!(result.effective_path, std::path::Path::new("d"));
        assert_eq!(result.parent_retries.len(), 1);
        let retry = &result.parent_retries[0];
        assert_eq!(retry.failed_path, std::path::Path::new("d/link"));
        assert_eq!(retry.retry_path, std::path::Path::new("d"));
        assert!(retry.errno == libc::ENOTDIR || retry.errno == libc::ELOOP);
        assert_eq!(ok.node.name(), std::ffi::OsStr::new("d"));
        // リンク自身が 1 ファイルとして数えられる
        assert_eq!(ok.node.total_size(), 3 + link_size);
    }

    #[test]
    fn symlink_in_the_middle_of_target_path_is_not_followed() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "real/sub/f", 5);
        make_dir(tree.path(), "d");
        std::os::unix::fs::symlink("../real", tree.path().join("d/link")).unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(partial_walk(
            tree.path(),
            "d/link/sub",
            &settings(2, &[]),
            &hasher,
        ));
        // d/link/sub -> d/link -> d の順に遡る
        assert_eq!(result.effective_path, std::path::Path::new("d"));
        assert_eq!(result.parent_retries.len(), 2);
        assert_eq!(
            result.parent_retries[0].failed_path,
            std::path::Path::new("d/link/sub")
        );
        assert_eq!(
            result.parent_retries[0].retry_path,
            std::path::Path::new("d/link")
        );
        assert_eq!(
            result.parent_retries[1].failed_path,
            std::path::Path::new("d/link")
        );
        assert_eq!(
            result.parent_retries[1].retry_path,
            std::path::Path::new("d")
        );
        assert!(ok.node.child(std::ffi::OsStr::new("link")).is_none());
        // real/sub/f の 5 は含まれない
        assert_eq!(ok.node.total_size(), "../real".len() as u128);
    }

    #[test]
    fn fifo_and_socket_are_not_counted() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "regular", 7);
        rustix::fs::mknodat(
            rustix::fs::CWD,
            tree.path().join("fifo"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(tree.path().join("sock")).unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        assert_eq!(ok.node.own_size(), 7);
        assert_eq!(ok.counts.file_count, 1);
        assert_eq!(result.errors.total(), 0);
    }

    #[test]
    fn non_utf8_names_are_handled_as_bytes() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        let file_name = os(b"file\xff\xfe.txt");
        let dir_name = os(b"dir\xc3");
        std::fs::File::create(tree.path().join(file_name))
            .unwrap()
            .set_len(11)
            .unwrap();
        std::fs::create_dir(tree.path().join(dir_name)).unwrap();
        std::fs::File::create(tree.path().join(dir_name).join(os(b"in\xff")))
            .unwrap()
            .set_len(22)
            .unwrap();
        // 非 UTF-8 の名前は `**/.*` にも `*` で当たりうるが、ここでは当たらないパターンで確認する
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(tree.path(), &settings(2, &["**/.*"]), &hasher));
        assert_eq!(ok.node.own_size(), 11);
        let dir = ok.node.child(dir_name).unwrap();
        assert_eq!(dir.own_size(), 22);
        assert_eq!(ok.node.total_size(), 33);
        assert_eq!(result.errors.total(), 0);
        // 部分スキャンの対象にも非 UTF-8 の名前を使える
        let (partial, _) = success(run_walk(
            tree.path(),
            std::path::Path::new(dir_name),
            &[],
            &settings(2, &[]),
            &hasher,
            std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new()),
        ));
        assert_eq!(partial.node.name(), dir_name);
        assert_eq!(partial.node, *dir);
        // 非 UTF-8 の名前にもワイルドカードは当たる
        let (excluded, _) = success(full_walk(tree.path(), &settings(2, &["dir*"]), &hasher));
        assert!(excluded.node.child(dir_name).is_none());
    }

    #[test]
    fn unreadable_directory_is_kept_as_error_node() {
        init_logger();
        if is_root() {
            return;
        }
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "ok/f", 5);
        make_file(tree.path(), "secret/f", 1000);
        make_file(tree.path(), "top", 3);
        let secret = tree.path().join("secret");
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let root = &ok.node;
        let node = root.child(std::ffi::OsStr::new("secret")).unwrap();
        assert_eq!(node.errors().open_errno, Some(libc::EACCES));
        assert_eq!(node.total_size(), 0);
        assert!(!node.is_complete());
        assert!(!root.is_complete());
        assert_eq!(root.subtree_error_dir_count(), 1);
        // 他は正しく数えられる
        assert_eq!(root.total_size(), 8);
        assert_eq!(
            root.child(std::ffi::OsStr::new("ok")).unwrap().total_size(),
            5
        );
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::OpenDirectory),
            1
        );
        assert_eq!(result.errors.errno_count(libc::EACCES), 1);
        assert_eq!(ok.counts.dir_count, 3);
    }

    #[test]
    fn stat_failure_excludes_file_and_marks_directory() {
        init_logger();
        if is_root() {
            return;
        }
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "noexec/f1", 100);
        make_file(tree.path(), "noexec/f2", 200);
        make_file(tree.path(), "other/f", 1);
        // 読めるが（r）、中のエントリを stat できない（x なし）
        let noexec = tree.path().join("noexec");
        std::fs::set_permissions(&noexec, std::os::unix::fs::PermissionsExt::from_mode(0o400))
            .unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        std::fs::set_permissions(&noexec, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let node = ok.node.child(std::ffi::OsStr::new("noexec")).unwrap();
        assert_eq!(node.errors().stat_eacces, 2);
        assert_eq!(node.errors().open_errno, None);
        assert_eq!(node.own_size(), 0);
        assert!(!node.is_complete());
        assert_eq!(ok.node.total_size(), 1);
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::StatFile),
            2
        );
        assert_eq!(result.errors.errno_count(libc::EACCES), 2);
    }

    #[test]
    fn partial_walk_matches_the_same_part_of_full_walk() {
        init_logger();
        let tree = sample_tree();
        make_file(tree.path(), "a/b/c/f5", 50);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (full, _) = success(full_walk(tree.path(), &settings(4, &["**/.*"]), &hasher));
        let (partial, result) = success(partial_walk(
            tree.path(),
            "a/b",
            &settings(4, &["**/.*"]),
            &hasher,
        ));
        assert_eq!(result.effective_path, std::path::Path::new("a/b"));
        assert!(result.parent_retries.is_empty());
        let expected = full
            .node
            .child(std::ffi::OsStr::new("a"))
            .unwrap()
            .child(std::ffi::OsStr::new("b"))
            .unwrap();
        assert_eq!(partial.node.name(), std::ffi::OsStr::new("b"));
        assert_eq!(partial.node.total_size(), 90);
        assert_eq!(&partial.node, expected);
        // ファイルを変更してからの部分スキャンは、同じ状態のフルスキャンと一致する
        make_file(tree.path(), "a/b/c/f6", 6);
        std::fs::remove_file(tree.path().join("a/b/f4")).unwrap();
        let (full2, _) = success(full_walk(tree.path(), &settings(4, &["**/.*"]), &hasher));
        let (partial2, _) = success(partial_walk(
            tree.path(),
            "a/b",
            &settings(1, &["**/.*"]),
            &hasher,
        ));
        let expected2 = full2
            .node
            .child(std::ffi::OsStr::new("a"))
            .unwrap()
            .child(std::ffi::OsStr::new("b"))
            .unwrap();
        assert_eq!(&partial2.node, expected2);
        assert_eq!(partial2.node.total_size(), 56);
    }

    #[test]
    fn partial_target_that_vanished_falls_back_to_parent() {
        init_logger();
        let tree = sample_tree();
        std::fs::remove_dir_all(tree.path().join("a/b")).unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(partial_walk(tree.path(), "a/b", &settings(2, &[]), &hasher));
        assert_eq!(result.effective_path, std::path::Path::new("a"));
        assert_eq!(
            result.parent_retries,
            vec![crate::dir_scan::report::ParentRetry {
                failed_path: std::path::PathBuf::from("a/b"),
                errno: libc::ENOENT,
                retry_path: std::path::PathBuf::from("a"),
            }]
        );
        assert_eq!(ok.node.name(), std::ffi::OsStr::new("a"));
        assert_eq!(ok.node.total_size(), 50);
    }

    #[test]
    fn partial_target_replaced_by_file_falls_back_to_parent() {
        init_logger();
        let tree = sample_tree();
        std::fs::remove_dir_all(tree.path().join("a/b")).unwrap();
        make_file(tree.path(), "a/b", 77);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, result) = success(partial_walk(tree.path(), "a/b", &settings(2, &[]), &hasher));
        assert_eq!(result.effective_path, std::path::Path::new("a"));
        assert_eq!(result.parent_retries.len(), 1);
        assert_eq!(result.parent_retries[0].errno, libc::ENOTDIR);
        // 置き換わったファイルは a の直下のファイルとして数えられる
        assert_eq!(ok.node.own_size(), 20 + 30 + 77);
    }

    #[test]
    fn partial_target_falls_back_up_to_the_root() {
        init_logger();
        let tree = sample_tree();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (full, _) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        let (ok, result) = success(partial_walk(tree.path(), "x/y", &settings(2, &[]), &hasher));
        assert!(result.effective_path.as_os_str().is_empty());
        assert_eq!(result.parent_retries.len(), 2);
        assert_eq!(
            result.parent_retries[1].retry_path,
            std::path::PathBuf::new()
        );
        assert_eq!(ok.node.name(), std::ffi::OsStr::new(""));
        assert_eq!(ok.node, full.node);
    }

    #[test]
    fn partial_target_that_is_unreadable_fails_without_retry() {
        init_logger();
        if is_root() {
            return;
        }
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "a/secret/f", 5);
        let secret = tree.path().join("a/secret");
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let result = partial_walk(tree.path(), "a/secret", &settings(2, &[]), &hasher);
        std::fs::set_permissions(&secret, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        assert_eq!(
            failure(&result),
            &WalkFailure::TargetOpenFailed {
                path: std::path::PathBuf::from("a/secret"),
                errno: libc::EACCES,
            }
        );
        assert!(result.parent_retries.is_empty());
        assert_eq!(result.effective_path, std::path::Path::new("a/secret"));
        assert_eq!(result.worker_ioprio.len(), 2);
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::OpenDirectory),
            1
        );
        assert!(failure(&result).to_string().contains("a/secret"));
    }

    #[test]
    fn missing_or_non_directory_root_fails() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let missing = tree.path().join("missing");
        let result = full_walk(&missing, &settings(2, &[]), &hasher);
        assert_eq!(
            failure(&result),
            &WalkFailure::RootOpenFailed {
                errno: libc::ENOENT
            }
        );
        // 失敗でも ioprio の結果とエラー件数は返る
        assert_eq!(result.worker_ioprio.len(), 2);
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::OpenDirectory),
            1
        );

        make_file(tree.path(), "file", 1);
        let result = full_walk(&tree.path().join("file"), &settings(2, &[]), &hasher);
        assert_eq!(
            failure(&result),
            &WalkFailure::RootOpenFailed {
                errno: libc::ENOTDIR
            }
        );

        // 部分スキャンでもルートが開けなければ失敗（親に遡らない）
        let result = partial_walk(&missing, "a", &settings(2, &[]), &hasher);
        assert_eq!(
            failure(&result),
            &WalkFailure::RootOpenFailed {
                errno: libc::ENOENT
            }
        );
        assert!(result.parent_retries.is_empty());
    }

    #[test]
    fn root_that_is_a_symlink_to_a_directory_is_followed() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "real/f", 9);
        std::os::unix::fs::symlink("real", tree.path().join("root_link")).unwrap();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, _) = success(full_walk(
            &tree.path().join("root_link"),
            &settings(2, &[]),
            &hasher,
        ));
        assert_eq!(ok.node.total_size(), 9);
    }

    #[test]
    fn invalid_target_is_rejected() {
        init_logger();
        let tree = sample_tree();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let result = partial_walk(tree.path(), "a/../b", &settings(1, &[]), &hasher);
        assert!(matches!(failure(&result), WalkFailure::InvalidTarget(_)));
        let result = partial_walk(tree.path(), "/abs", &settings(1, &[]), &hasher);
        assert!(matches!(failure(&result), WalkFailure::InvalidTarget(_)));
        // `.` は飛ばす
        let (ok, result) = success(partial_walk(tree.path(), "./a", &settings(1, &[]), &hasher));
        assert_eq!(result.effective_path, std::path::Path::new("a"));
        assert_eq!(ok.node.name(), std::ffi::OsStr::new("a"));
    }

    fn open_dir(path: &std::path::Path) -> rustix::fd::OwnedFd {
        rustix::fs::openat(
            rustix::fs::CWD,
            path,
            dir_open_flags(),
            rustix::fs::Mode::empty(),
        )
        .unwrap()
    }

    fn dev_ino_of(fd: &rustix::fd::OwnedFd) -> DevIno {
        let stat = rustix::fs::fstat(fd).unwrap();
        DevIno {
            dev: stat.st_dev,
            ino: stat.st_ino,
        }
    }

    #[test]
    fn directory_that_is_its_own_ancestor_is_a_cycle() {
        init_logger();
        let tree = sample_tree();
        let root_fd = open_dir(tree.path());
        let a_identity = dev_ino_of(&open_dir(&tree.path().join("a")));
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let exclude = crate::dir_scan::settings::ExcludePatterns::none();
        let context = WalkContext {
            root_fd: rustix::fd::AsFd::as_fd(&root_fd),
            exclude_patterns: &exclude,
            name_hasher: &hasher,
            errors: &errors,
        };

        // 祖先の一覧に `a` の dev/ino を入れてルートから走査すると、`a` が循環になる
        let node = walk_opened(
            &context,
            open_dir(tree.path()),
            std::path::Path::new(""),
            std::ffi::OsString::new(),
            DevIno { dev: 0, ino: 0 },
            &[a_identity],
        )
        .unwrap();
        let a = node.child(std::ffi::OsStr::new("a")).unwrap();
        assert!(a.errors().cycle_detected);
        assert_eq!(a.total_size(), 0);
        assert_eq!(a.children().len(), 0);
        assert_eq!(a.dev(), a_identity.dev);
        assert_eq!(a.ino(), a_identity.ino);
        // 中に入らないので a のファイルは数えない。他は数える
        assert_eq!(node.total_size(), 10);
        assert!(!node.is_complete());
        assert_eq!(node.subtree_error_dir_count(), 1);
        let counts = errors.snapshot();
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::CycleDetected),
            1
        );
        // 循環には errno が無い
        assert_eq!(counts.by_errno.len(), 0);

        // 祖先に自分自身があればそのディレクトリ自身が循環になる
        let own_identity = dev_ino_of(&open_dir(tree.path()));
        let cyclic = walk_opened(
            &context,
            open_dir(tree.path()),
            std::path::Path::new(""),
            std::ffi::OsString::new(),
            DevIno { dev: 0, ino: 0 },
            &[own_identity],
        )
        .unwrap();
        assert!(cyclic.errors().cycle_detected);
        assert_eq!(cyclic.total_size(), 0);
        assert_eq!(
            errors
                .snapshot()
                .count(crate::dir_scan::report::ScanErrorKind::CycleDetected),
            2
        );
    }

    fn identity_of_path(path: &std::path::Path) -> DevIno {
        dev_ino_of(&open_dir(path))
    }

    fn walk_with_ancestors(
        root: &std::path::Path,
        target: &str,
        ancestors: &[DevIno],
    ) -> WalkResult {
        run_walk(
            root,
            std::path::Path::new(target),
            ancestors,
            &settings(2, &[]),
            &crate::dir_scan::tree::NameHasher::new(),
            std::sync::Arc::new(crate::dir_scan::report::ErrorRecorder::new()),
        )
    }

    #[test]
    fn partial_walk_treats_a_descendant_equal_to_a_given_ancestor_as_a_cycle() {
        init_logger();
        let tree = sample_tree();
        let root_identity = identity_of_path(tree.path());
        let b_identity = identity_of_path(&tree.path().join("a/b"));

        // 対象 `a` の祖先の一覧（ルート）に、対象の子孫 `a/b` の dev/ino を意図的に入れる。
        // bind マウントで `a/b/loop` が `a` を指す場合の、`a` 側の見え方と同じ
        let (ok, result) = success(walk_with_ancestors(
            tree.path(),
            "a",
            &[root_identity, b_identity],
        ));
        let a = &ok.node;
        let b = a.child(std::ffi::OsStr::new("b")).unwrap();
        assert!(b.errors().cycle_detected);
        assert_eq!(b.children().len(), 0);
        // `a/b/f4`（40）は数えない
        assert_eq!(b.total_size(), 0);
        assert_eq!(a.total_size(), 50);
        assert!(!a.is_complete());
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::CycleDetected),
            1
        );

        // 祖先の一覧が空なら（フルスキャンの対象がルートのとき以外は誤りだが）、循環にならない
        let (ok, result) = success(walk_with_ancestors(tree.path(), "a", &[]));
        assert_eq!(ok.node.total_size(), 90);
        assert!(ok.node.is_complete());
        assert_eq!(result.errors.total(), 0);
    }

    #[test]
    fn partial_walk_target_equal_to_an_ancestor_is_a_cycle() {
        init_logger();
        let tree = sample_tree();
        let root_identity = identity_of_path(tree.path());
        let a_identity = identity_of_path(&tree.path().join("a"));
        // 対象 `a/b` の祖先（ルート・`a`）に `a/b` 自身は含まれないので循環ではない
        let (ok, _) = success(walk_with_ancestors(
            tree.path(),
            "a/b",
            &[root_identity, a_identity],
        ));
        assert_eq!(ok.node.total_size(), 40);
        assert!(ok.node.is_complete());

        // 対象 `a` 自身の dev/ino が祖先にあれば、対象が循環のノードになる
        let (ok, result) = success(walk_with_ancestors(tree.path(), "a", &[a_identity]));
        assert!(ok.node.errors().cycle_detected);
        assert_eq!(ok.node.name(), std::ffi::OsStr::new("a"));
        assert_eq!(ok.node.total_size(), 0);
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::CycleDetected),
            1
        );
    }

    #[test]
    fn partial_walk_uses_only_the_leading_ancestors_after_retrying_with_the_parent() {
        init_logger();
        let tree = sample_tree();
        let root_identity = identity_of_path(tree.path());
        let a_identity = identity_of_path(&tree.path().join("a"));
        let b_identity = identity_of_path(&tree.path().join("a/b"));
        // 対象 `a/b/gone` は無く、`a/b` に遡る。最初の対象の祖先は [ルート, a, a/b] だが、
        // 遡った後の対象 `a/b` の祖先は [ルート, a] だけ。`a/b` 自身は循環にならない
        let (ok, result) = success(walk_with_ancestors(
            tree.path(),
            "a/b/gone",
            &[root_identity, a_identity, b_identity],
        ));
        assert_eq!(result.effective_path, std::path::Path::new("a/b"));
        assert_eq!(result.parent_retries.len(), 1);
        assert!(!ok.node.errors().cycle_detected);
        assert_eq!(ok.node.total_size(), 40);
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::CycleDetected),
            0
        );

        // ルートまで遡ったときは、祖先の一覧は空として扱う
        let (ok, result) = success(walk_with_ancestors(tree.path(), "gone", &[root_identity]));
        assert!(result.effective_path.as_os_str().is_empty());
        assert!(!ok.node.errors().cycle_detected);
        assert_eq!(ok.node.total_size(), 100);
    }

    #[test]
    fn unknown_entry_type_is_looked_up_with_statx() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "file", 1);
        make_dir(tree.path(), "dir");
        std::os::unix::fs::symlink("dir", tree.path().join("link")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            tree.path().join("fifo"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR,
            0,
        )
        .unwrap();
        let dir_fd = open_dir(tree.path());
        let borrowed = rustix::fd::AsFd::as_fd(&dir_fd);
        assert_eq!(
            lookup_entry_class(borrowed, std::ffi::OsStr::new("file")),
            Ok(EntryClass::File)
        );
        assert_eq!(
            lookup_entry_class(borrowed, std::ffi::OsStr::new("dir")),
            Ok(EntryClass::Directory)
        );
        // リンクは辿らない（リンク自身はファイルとして数える）
        assert_eq!(
            lookup_entry_class(borrowed, std::ffi::OsStr::new("link")),
            Ok(EntryClass::File)
        );
        assert_eq!(
            lookup_entry_class(borrowed, std::ffi::OsStr::new("fifo")),
            Ok(EntryClass::Skip)
        );
        assert_eq!(
            lookup_entry_class(borrowed, std::ffi::OsStr::new("missing")),
            Err(libc::ENOENT)
        );
    }

    /// `settle_unknown_entry` に失敗結果を注入して、戻り値・ノードのエラー・エラー件数を返す。
    fn settle_with(
        looked_up: Result<EntryClass, i32>,
    ) -> (
        Option<EntryClass>,
        crate::dir_scan::tree::DirNode,
        crate::dir_scan::report::ErrorCounts,
    ) {
        let tree = tempfile::tempdir().unwrap();
        let root_fd = open_dir(tree.path());
        let errors = crate::dir_scan::report::ErrorRecorder::new();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let exclude = crate::dir_scan::settings::ExcludePatterns::none();
        let context = WalkContext {
            root_fd: rustix::fd::AsFd::as_fd(&root_fd),
            exclude_patterns: &exclude,
            name_hasher: &hasher,
            errors: &errors,
        };
        let mut node = crate::dir_scan::tree::DirNode::new(std::ffi::OsString::new(), 1, 2);
        let class = settle_unknown_entry(
            &context,
            std::path::Path::new("dir"),
            std::ffi::OsStr::new("entry"),
            looked_up,
            &mut node,
        );
        node.finalize().unwrap();
        (class, node, errors.snapshot())
    }

    #[test]
    fn unknown_entry_that_vanished_is_a_benign_race() {
        init_logger();
        let (class, node, counts) = settle_with(Err(libc::ENOENT));
        assert_eq!(class, None);
        // ノードにエラーを付けず、スキャンを「エラーあり」にもしない
        assert!(node.is_complete());
        assert!(!node.errors().has_error());
        assert_eq!(counts.total_excluding_races(), 0);
        // 件数には、ファイルの stat の ENOENT と同じ種類で数える
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::StatFileNotFound),
            1
        );
        assert_eq!(
            counts.count(crate::dir_scan::report::ScanErrorKind::StatUnknownEntry),
            0
        );
    }

    #[test]
    fn unknown_entry_stat_failure_marks_the_directory_incomplete() {
        init_logger();
        for errno in [libc::EACCES, libc::EIO, libc::EOVERFLOW] {
            let (class, node, counts) = settle_with(Err(errno));
            assert_eq!(class, None, "errno={errno}");
            assert!(!node.is_complete(), "errno={errno}");
            assert!(node.errors().has_error(), "errno={errno}");
            assert_eq!(node.subtree_error_dir_count(), 1);
            assert_eq!(
                counts.count(crate::dir_scan::report::ScanErrorKind::StatUnknownEntry),
                1
            );
            assert_eq!(counts.total_excluding_races(), 1);
        }
        let (_, node, _) = settle_with(Err(libc::EACCES));
        assert_eq!(node.errors().stat_eacces, 1);
        let (_, node, _) = settle_with(Err(libc::EIO));
        assert_eq!(node.errors().stat_eio, 1);
        let (_, node, _) = settle_with(Err(libc::EOVERFLOW));
        assert_eq!(node.errors().stat_other, 1);
    }

    #[test]
    fn unknown_entry_that_was_stat_ed_keeps_its_class() {
        init_logger();
        for class in [EntryClass::File, EntryClass::Directory, EntryClass::Skip] {
            let (settled, node, counts) = settle_with(Ok(class));
            assert_eq!(settled, Some(class));
            assert!(node.is_complete());
            assert_eq!(counts.total(), 0);
        }
    }

    #[test]
    fn classify_maps_every_file_type() {
        init_logger();
        assert_eq!(
            classify(rustix::fs::FileType::RegularFile),
            EntryClass::File
        );
        assert_eq!(classify(rustix::fs::FileType::Symlink), EntryClass::File);
        assert_eq!(
            classify(rustix::fs::FileType::Directory),
            EntryClass::Directory
        );
        assert_eq!(classify(rustix::fs::FileType::Fifo), EntryClass::Skip);
        assert_eq!(classify(rustix::fs::FileType::Socket), EntryClass::Skip);
        assert_eq!(
            classify(rustix::fs::FileType::CharacterDevice),
            EntryClass::Skip
        );
        assert_eq!(
            classify(rustix::fs::FileType::BlockDevice),
            EntryClass::Skip
        );
        assert_eq!(classify(rustix::fs::FileType::Unknown), EntryClass::Unknown);
    }

    #[test]
    fn open_relative_dir_opens_one_component_at_a_time_without_following_links() {
        init_logger();
        let tree = sample_tree();
        std::os::unix::fs::symlink("a", tree.path().join("link_to_a")).unwrap();
        let root_fd = open_dir(tree.path());
        let borrowed = rustix::fd::AsFd::as_fd(&root_fd);
        let b = open_relative_dir(borrowed, std::path::Path::new("a/b")).unwrap();
        assert_eq!(
            dev_ino_of(&b),
            dev_ino_of(&open_dir(&tree.path().join("a/b")))
        );
        let root_again = open_relative_dir(borrowed, std::path::Path::new("")).unwrap();
        assert_eq!(dev_ino_of(&root_again), dev_ino_of(&root_fd));
        assert_eq!(
            open_relative_dir(borrowed, std::path::Path::new("missing/x")).err(),
            Some(libc::ENOENT)
        );
        // 途中の段のリンクも、最後の段のリンクも辿らない
        let through_link = open_relative_dir(borrowed, std::path::Path::new("link_to_a/b")).err();
        assert!(matches!(through_link, Some(e) if e == libc::ENOTDIR || e == libc::ELOOP));
        let last_link = open_relative_dir(borrowed, std::path::Path::new("link_to_a")).err();
        assert!(matches!(last_link, Some(e) if e == libc::ENOTDIR || e == libc::ELOOP));
        // ファイルは開けない
        assert_eq!(
            open_relative_dir(borrowed, std::path::Path::new("f1")).err(),
            Some(libc::ENOTDIR)
        );
        assert_eq!(
            open_relative_dir(borrowed, std::path::Path::new("../x")).err(),
            Some(libc::EINVAL)
        );
    }

    fn wide_tree() -> tempfile::TempDir {
        let tree = tempfile::tempdir().unwrap();
        for d in 0..12u64 {
            for e in 0..4u64 {
                for f in 0..5u64 {
                    make_file(
                        tree.path(),
                        &format!("d{d}/e{e}/f{f}"),
                        d * 100 + e * 10 + f,
                    );
                }
                make_file(tree.path(), &format!("d{d}/e{e}/.hidden"), 99999);
            }
            make_dir(tree.path(), &format!("d{d}/empty"));
            make_file(tree.path(), &format!("d{d}/top"), 1);
        }
        tree
    }

    #[test]
    fn result_does_not_depend_on_parallelism() {
        init_logger();
        let tree = wide_tree();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (base, base_result) =
            success(full_walk(tree.path(), &settings(1, &["**/.*"]), &hasher));
        assert_eq!(base_result.worker_ioprio.len(), 1);
        // 12 + 12*4 + 12 + root
        assert_eq!(base.counts.dir_count, 1 + 12 + 48 + 12);
        assert_eq!(base.counts.file_count, 12 * 4 * 5 + 12);
        for parallelism in [4, 64] {
            let (other, result) = success(full_walk(
                tree.path(),
                &settings(parallelism, &["**/.*"]),
                &hasher,
            ));
            assert_eq!(other.node, base.node, "parallelism={parallelism}");
            assert_eq!(other.counts, base.counts);
            assert_eq!(result.worker_ioprio.len(), parallelism);
        }
    }

    #[test]
    fn result_reports_ioprio_and_error_counts() {
        init_logger();
        let tree = sample_tree();
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let result = full_walk(tree.path(), &settings(3, &[]), &hasher);
        assert_eq!(result.worker_ioprio.len(), 3);
        let indices: Vec<usize> = result
            .worker_ioprio
            .iter()
            .map(|w| w.worker_index)
            .collect();
        assert_eq!(indices, vec![0, 1, 2]);
        let requested = crate::dir_scan::settings::IoPriority::best_effort(4)
            .unwrap()
            .to_ioprio_value();
        for worker in &result.worker_ioprio {
            assert_eq!(worker.requested, requested);
            assert!(!worker.is_failure(), "{worker:?}");
        }
        assert_eq!(result.errors.total(), 0);
        assert!(result.outcome.is_ok());
        assert!(result.duration > std::time::Duration::ZERO);
    }

    #[test]
    fn hash_collisions_are_counted() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        make_file(tree.path(), "one", 1);
        make_file(tree.path(), "two", 2);
        make_file(tree.path(), "three", 4);
        let hasher = crate::dir_scan::tree::NameHasher::with_test_mapping(|_| 7);
        let (ok, _) = success(full_walk(tree.path(), &settings(1, &[]), &hasher));
        assert_eq!(ok.node.own_size(), 7);
        assert_eq!(ok.counts.file_count, 3);
        assert_eq!(ok.counts.hash_collision_count, 2);
    }

    #[test]
    fn panic_during_walk_is_returned_as_failure_and_thread_continues() {
        init_logger();
        let tree = sample_tree();
        make_file(tree.path(), &format!("a/{TEST_PANIC_DIR_NAME}/f"), 1);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let result = full_walk(tree.path(), &settings(4, &[]), &hasher);
        match failure(&result) {
            WalkFailure::Panicked(message) => {
                assert!(message.contains("injected panic"), "{message}");
            }
            other => panic!("unexpected failure: {other}"),
        }
        assert_eq!(
            result
                .errors
                .count(crate::dir_scan::report::ScanErrorKind::WalkPanic),
            1
        );
        // 失敗でも ioprio の結果は返る
        assert_eq!(result.worker_ioprio.len(), 4);
        assert!(failure(&result).to_string().contains("injected panic"));

        // 呼び出したスレッドは続行でき、次のスキャンも実行できる
        std::fs::remove_dir_all(tree.path().join(format!("a/{TEST_PANIC_DIR_NAME}"))).unwrap();
        let (ok, _) = success(full_walk(tree.path(), &settings(4, &[]), &hasher));
        assert_eq!(ok.node.total_size(), 100);
    }

    #[test]
    fn pool_build_failure_is_a_failure_variant() {
        init_logger();
        // プールを作れない状況は作りにくいので、表示だけ確認する
        let failure = WalkFailure::PoolBuildFailed("boom".to_string());
        assert!(failure.to_string().contains("boom"));
        assert!(WalkFailure::Internal("x".to_string())
            .to_string()
            .contains("internal"));
    }

    #[test]
    fn deep_tree_is_walked_with_bounded_descriptors() {
        init_logger();
        let tree = tempfile::tempdir().unwrap();
        let mut relative = String::new();
        for level in 0..60 {
            relative.push_str(&format!("l{level}/"));
        }
        make_file(tree.path(), &format!("{relative}leaf"), 3);
        let hasher = crate::dir_scan::tree::NameHasher::new();
        let (ok, _) = success(full_walk(tree.path(), &settings(2, &[]), &hasher));
        assert_eq!(ok.node.total_size(), 3);
        assert_eq!(ok.counts.dir_count, 61);
    }
}
