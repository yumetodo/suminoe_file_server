//! ディレクトリスキャンの E2E テスト（設計書 4 Goal 1〜3 と 10）。
//!
//! 一時ディレクトリの合成ツリーに対して `ScanService` を動かし、`find` コマンドの結果と突き合わせる。
//! `find` がない環境ではテストは失敗する（飛ばさない）。
//! `examples/scan_once` は、テスト内で `cargo build --example scan_once` してから実行する。

fn init_logger() {
    env_logger::builder().is_test(true).try_init().ok();
}

fn is_root_user() -> bool {
    // SAFETY: geteuid は引数を取らず、失敗しない。
    unsafe { libc::geteuid() == 0 }
}

/// `find` の `-name '.*'` が起点自身にも当たるので、名前が `.` で始まる一時ディレクトリ
/// （`tempfile::tempdir()` の既定は `.tmp...`）は使わない。
fn new_temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("scan_e2e_")
        .tempdir()
        .unwrap()
}

fn make_file(root: &std::path::Path, relative: &str, size: u64) {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(size).unwrap();
}

fn make_file_os(root: &std::path::Path, relative: &[u8], size: u64) {
    use std::os::unix::ffi::OsStrExt;
    let path = root.join(std::ffi::OsStr::from_bytes(relative));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(size).unwrap();
}

fn make_fifo(path: &std::path::Path) {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
}

/// 合成ツリー。ネスト、空ディレクトリ、隠しファイル／ディレクトリ、シンボリックリンク
/// （ファイル・ディレクトリ・壊れたリンク・循環）、FIFO、ソケット、非 UTF-8 の名前、
/// ハードリンク、サイズ 0 のファイル、4GiB を超える疎ファイルを含む。
fn build_synthetic_tree(root: &std::path::Path) -> std::os::unix::net::UnixListener {
    make_file(root, "f_root", 100);
    make_file(root, ".hidden_root_file", 7);
    make_file(root, ".hidden_dir/x", 50);
    make_file(root, ".hidden_dir/.y/z", 51);
    make_file(root, "a/f1", 1000);
    make_file(root, "a/f2_empty", 0);
    make_file(root, "a/.hid", 9);
    make_file(root, "a/.hdir/y", 99);
    make_file(root, "a/b/c/deep", 4097);
    make_file(root, "a/b/c/deep2", 1);
    make_file(root, "a/b/.git/config", 33);
    make_file(root, "sparse/big", 5_000_000_000);
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::create_dir_all(root.join("a/empty_nested/deeper")).unwrap();
    make_file(root, "zero_only/z1", 0);
    make_file(root, "zero_only/z2", 0);
    // シンボリックリンク（サイズはリンクの長さ。辿らない）
    std::os::unix::fs::symlink("f1", root.join("a/link_file")).unwrap();
    std::os::unix::fs::symlink("b", root.join("a/link_dir")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/target/xxxxxxxx", root.join("a/dangling")).unwrap();
    std::os::unix::fs::symlink("../..", root.join("a/b/loop")).unwrap();
    std::os::unix::fs::symlink("a", root.join("link_to_a")).unwrap();
    // ハードリンク（名前ごとに数える）
    std::fs::hard_link(root.join("a/f1"), root.join("a/b/f1_hardlink")).unwrap();
    std::fs::hard_link(root.join("a/f1"), root.join("f1_hardlink2")).unwrap();
    // FIFO・ソケットは数えない
    make_fifo(&root.join("a/fifo"));
    make_fifo(&root.join("fifo_root"));
    let socket = std::os::unix::net::UnixListener::bind(root.join("a/sock")).unwrap();
    // 非 UTF-8 の名前
    make_file_os(root, b"d\xff\xfe/f\x80", 33);
    make_file_os(root, b"d\xff\xfe/sub\xc3\x28/g", 44);
    make_file_os(root, b"\xc3\x28", 11);
    make_file_os(root, b"a/\xe3\x81", 12);
    // 名前に空白とタブ以外の記号
    make_file(root, "sp ace/name with space", 21);
    make_file(root, "a/foo.bar", 3);
    socket
}

fn settings(
    parallelism: usize,
    io_priority: suminoe_file_server::dir_scan::settings::IoPriority,
    excludes: &[&str],
) -> suminoe_file_server::dir_scan::settings::ScanSettings {
    let owned: Vec<String> = excludes.iter().map(|s| s.to_string()).collect();
    suminoe_file_server::dir_scan::settings::ScanSettings::new(
        suminoe_file_server::dir_scan::settings::Parallelism::new(parallelism).unwrap(),
        io_priority,
        suminoe_file_server::dir_scan::settings::ExcludePatterns::new(&owned).unwrap(),
    )
}

fn dot_excluded(parallelism: usize) -> suminoe_file_server::dir_scan::settings::ScanSettings {
    settings(
        parallelism,
        suminoe_file_server::dir_scan::settings::IoPriority::Idle,
        &["**/.*"],
    )
}

fn wait(
    ticket: suminoe_file_server::dir_scan::request::Ticket,
) -> suminoe_file_server::dir_scan::request::Completion {
    ticket
        .wait_timeout(std::time::Duration::from_secs(120))
        .unwrap()
        .expect("timed out while waiting for the completion")
}

fn run_full(
    service: &suminoe_file_server::dir_scan::ScanService,
    settings: suminoe_file_server::dir_scan::settings::ScanSettings,
) -> suminoe_file_server::dir_scan::request::Completion {
    let completion = wait(service.request_full_scan(settings).unwrap());
    assert_eq!(
        completion.disposition,
        suminoe_file_server::dir_scan::request::Disposition::Executed
    );
    completion
}

fn run_partial(
    service: &suminoe_file_server::dir_scan::ScanService,
    path: &str,
    settings: suminoe_file_server::dir_scan::settings::ScanSettings,
) -> suminoe_file_server::dir_scan::request::Completion {
    wait(
        service
            .request_partial_scan(std::path::Path::new(path), settings)
            .unwrap(),
    )
}

/// ディレクトリのパス（バイト列）ごとの `own_size`。
type DirSizes = std::collections::BTreeMap<Vec<u8>, u128>;

/// サービスのツリーから、ルートのパスを接頭辞にした絶対パスごとの `own_size` を読む。
fn scanner_own_sizes(
    service: &suminoe_file_server::dir_scan::ScanService,
    root: &std::path::Path,
) -> DirSizes {
    use std::os::unix::ffi::OsStrExt;
    let mut result = DirSizes::new();
    for (relative, own_size) in service.list_own_sizes().unwrap() {
        let path = if relative.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&relative)
        };
        let previous = result.insert(path.as_os_str().as_bytes().to_vec(), own_size);
        assert!(previous.is_none(), "duplicated directory: {path:?}");
    }
    result
}

struct FindResult {
    /// `%h` ごとの `%s` の総和
    sizes: DirSizes,
    /// 標準エラー出力の行
    stderr_lines: Vec<String>,
}

/// Goal 1 の `find` を実行し、ディレクトリ別に合計する。メッセージを固定するため `LC_ALL=C`。
fn run_find(root: &std::path::Path) -> FindResult {
    let output = std::process::Command::new("find")
        .arg(root)
        .args([
            "-name", ".*", "-prune", "-o", "(", "-type", "f", "-o", "-type", "l",
        ])
        .args([")", "-printf", "%h\\t%s\\n"])
        .env("LC_ALL", "C")
        .output()
        .expect("failed to run find (find is required for this test)");
    let mut sizes = DirSizes::new();
    for line in output.stdout.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let tab = line
            .iter()
            .rposition(|b| *b == b'\t')
            .expect("a find line without a tab");
        let size: u128 = std::str::from_utf8(&line[tab + 1..])
            .unwrap()
            .parse()
            .unwrap();
        *sizes.entry(line[..tab].to_vec()).or_insert(0) += size;
    }
    let stderr_lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(|l| l.to_string())
        .collect();
    FindResult {
        sizes,
        stderr_lines,
    }
}

fn nonzero(sizes: &DirSizes) -> DirSizes {
    sizes
        .iter()
        .filter(|(_, size)| **size > 0)
        .map(|(path, size)| (path.clone(), *size))
        .collect()
}

fn describe(sizes: &DirSizes) -> String {
    let mut text = String::new();
    for (path, size) in sizes {
        text.push_str(&format!("{}\t{}\n", String::from_utf8_lossy(path), size));
    }
    text
}

/// スキャナの結果と `find` の結果が、サイズが 0 でないディレクトリについて完全に一致し、
/// `find` が出したディレクトリ（サイズ 0 のファイルだけのものを含む）がスキャナにも存在する。
fn assert_matches_find(scanner: &DirSizes, find: &DirSizes) {
    for path in find.keys() {
        assert!(
            scanner.contains_key(path),
            "find reports a directory the scanner does not have: {}",
            String::from_utf8_lossy(path)
        );
    }
    let scanner_nonzero = nonzero(scanner);
    let find_nonzero = nonzero(find);
    assert!(
        scanner_nonzero == find_nonzero,
        "own_size mismatch\n--- scanner ---\n{}--- find ---\n{}",
        describe(&scanner_nonzero),
        describe(&find_nonzero)
    );
}

#[test]
fn goal1_full_scan_matches_find_on_the_synthetic_tree() {
    use std::os::unix::ffi::OsStrExt;
    init_logger();
    let dir = new_temp_dir();
    let _socket = build_synthetic_tree(dir.path());
    let find = run_find(dir.path());
    assert!(find.stderr_lines.is_empty(), "{:?}", find.stderr_lines);
    // 合成ツリーの前提が崩れていないこと（find 側のサニティ）
    assert_eq!(
        find.sizes[dir.path().as_os_str().as_bytes()],
        100 + 11 + 1 + 1000
    );
    for parallelism in [1, 4, 16] {
        let service =
            suminoe_file_server::dir_scan::ScanService::start(dir.path().to_path_buf()).unwrap();
        let completion = run_full(&service, dot_excluded(parallelism));
        let report = completion.report.as_ref().unwrap();
        assert_eq!(
            report.outcome,
            suminoe_file_server::dir_scan::report::ScanOutcome::Success
        );
        assert_eq!(report.errors.total(), 0);
        let scanner = scanner_own_sizes(&service, dir.path());
        assert_matches_find(&scanner, &find.sizes);
        // ルート直下：f_root、非 UTF-8 名のファイル、link_to_a（リンクの長さ 1）、f1 のハードリンク
        assert_eq!(
            scanner[dir.path().as_os_str().as_bytes()],
            100 + 11 + 1 + 1000
        );
        assert_eq!(report.dir_count as usize, scanner.len());
        service.shutdown();
    }
}

#[test]
fn goal1_exclude_none_counts_hidden_files_too() {
    init_logger();
    let dir = new_temp_dir();
    let _socket = build_synthetic_tree(dir.path());
    let service =
        suminoe_file_server::dir_scan::ScanService::start(dir.path().to_path_buf()).unwrap();
    run_full(
        &service,
        settings(
            2,
            suminoe_file_server::dir_scan::settings::IoPriority::Idle,
            &[],
        ),
    );
    let scanner = scanner_own_sizes(&service, dir.path());
    // 除外なしの `find`（-name の prune なし）と一致する
    let output = std::process::Command::new("find")
        .arg(dir.path())
        .args([
            "(",
            "-type",
            "f",
            "-o",
            "-type",
            "l",
            ")",
            "-printf",
            "%h\\t%s\\n",
        ])
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let mut expected = DirSizes::new();
    for line in output.stdout.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let tab = line.iter().rposition(|b| *b == b'\t').unwrap();
        let size: u128 = std::str::from_utf8(&line[tab + 1..])
            .unwrap()
            .parse()
            .unwrap();
        *expected.entry(line[..tab].to_vec()).or_insert(0) += size;
    }
    assert_matches_find(&scanner, &expected);
    service.shutdown();
}

/// chmod したディレクトリの権限を、テストが失敗しても戻す（一時ディレクトリを消せるように）。
struct RestorePermissions {
    paths: Vec<std::path::PathBuf>,
}

impl Drop for RestorePermissions {
    fn drop(&mut self) {
        for path in &self.paths {
            if let Err(error) =
                std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            {
                log::error!("failed to restore the permissions of {path:?}: {error}");
            }
        }
    }
}

fn set_mode(path: &std::path::Path, mode: u32) {
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
}

/// `find` の標準エラー出力（`LC_ALL=C`）の `find: 'PATH': Permission denied` から PATH を取る。
fn permission_denied_paths(lines: &[String]) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    for line in lines {
        let rest = match line.strip_prefix("find: '") {
            Some(rest) => rest,
            None => panic!("unexpected find message: {line}"),
        };
        let path = match rest.strip_suffix("': Permission denied") {
            Some(path) => path,
            None => panic!("unexpected find message: {line}"),
        };
        result.push(std::path::PathBuf::from(path));
    }
    result
}

#[test]
fn goal1_unreadable_directories_match_find_errors() {
    init_logger();
    if is_root_user() {
        // root は権限を無視するので、このテストは意味がない
        return;
    }
    let dir = new_temp_dir();
    make_file(dir.path(), "ok/f", 10);
    make_file(dir.path(), "ok/nested/g", 20);
    make_file(dir.path(), "ok/locked_000/inner", 30);
    make_file(dir.path(), "ok/locked_100/inner", 31);
    make_file(dir.path(), "locked_top/inner/deeper", 40);
    make_file(dir.path(), "read_only/h1", 50);
    make_file(dir.path(), "read_only/h2", 51);
    make_file(dir.path(), "after/z", 60);
    let locked = [
        (dir.path().join("ok/locked_000"), 0o000),
        (dir.path().join("ok/locked_100"), 0o100),
        (dir.path().join("locked_top"), 0o000),
        // 名前は読めるが、中のエントリを stat できない
        (dir.path().join("read_only"), 0o400),
    ];
    let _restore = RestorePermissions {
        paths: locked.iter().map(|(path, _)| path.clone()).collect(),
    };
    for (path, mode) in &locked {
        set_mode(path, *mode);
    }

    let find = run_find(dir.path());
    // find が出した PATH がディレクトリならそのディレクトリ、ファイルなら親ディレクトリが、
    // 読めなかったディレクトリ
    let mut find_error_dirs = std::collections::BTreeSet::new();
    for path in permission_denied_paths(&find.stderr_lines) {
        let is_dir = std::fs::symlink_metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        if is_dir {
            find_error_dirs.insert(path);
        } else {
            find_error_dirs.insert(path.parent().unwrap().to_path_buf());
        }
    }
    assert_eq!(
        find_error_dirs.len(),
        4,
        "{:?} / {:?}",
        find_error_dirs,
        find.stderr_lines
    );

    let service =
        suminoe_file_server::dir_scan::ScanService::start(dir.path().to_path_buf()).unwrap();
    let completion = run_full(&service, dot_excluded(4));
    let report = completion.report.as_ref().unwrap();
    assert_eq!(
        report.outcome,
        suminoe_file_server::dir_scan::report::ScanOutcome::SuccessWithErrors
    );
    assert!(report.errors.errno_count(libc::EACCES) > 0);

    // スキャナがエラーとして記録したディレクトリ：完全でなく、完全でない子を持たないもの
    let mut scanner_error_dirs = std::collections::BTreeSet::new();
    let mut pending = vec![std::path::PathBuf::new()];
    while let Some(path) = pending.pop() {
        let read = service.read_directory(&path).unwrap();
        let found = match read.directory {
            suminoe_file_server::dir_scan::tree::DirectoryLookup::Found(found) => found,
            suminoe_file_server::dir_scan::tree::DirectoryLookup::NotFound => {
                panic!("directory not found: {path:?}")
            }
        };
        for child in &found.children {
            pending.push(path.join(&child.name));
        }
        if !found.complete && found.children.iter().all(|child| child.complete) {
            let absolute = if path.as_os_str().is_empty() {
                dir.path().to_path_buf()
            } else {
                dir.path().join(&path)
            };
            scanner_error_dirs.insert(absolute);
        }
    }
    assert_eq!(scanner_error_dirs, find_error_dirs);

    let read = service.read_directory(std::path::Path::new("")).unwrap();
    assert_eq!(
        read.status.snapshot,
        suminoe_file_server::dir_scan::tree::SnapshotState::Incomplete
    );
    // 読めなかったディレクトリを除いた own_size は find と一致する
    let scanner = scanner_own_sizes(&service, dir.path());
    assert_matches_find(&scanner, &find.sizes);
    service.shutdown();
}

#[test]
fn goal2_partial_scan_matches_a_fresh_full_scan() {
    init_logger();
    let dir = new_temp_dir();
    let _socket = build_synthetic_tree(dir.path());
    make_file(dir.path(), "c2/k", 5);
    let service =
        suminoe_file_server::dir_scan::ScanService::start(dir.path().to_path_buf()).unwrap();
    run_full(&service, dot_excluded(2));

    // 同じ状態をフルスキャンし直した結果（ディレクトリごとの own_size・total_size・完全さ）
    let fresh = || -> std::collections::BTreeMap<std::path::PathBuf, (u128, u128, bool)> {
        let fresh_service =
            suminoe_file_server::dir_scan::ScanService::start(dir.path().to_path_buf()).unwrap();
        run_full(&fresh_service, dot_excluded(3));
        let result = tree_snapshot(&fresh_service);
        fresh_service.shutdown();
        result
    };
    assert_eq!(tree_snapshot(&service), fresh());

    // 1. ファイルの追加（既存のディレクトリ）
    make_file(dir.path(), "a/b/c/added", 123);
    let completion = run_partial(&service, "a/b/c", dot_excluded(3));
    check_partial(&completion, "a/b/c", "a/b/c");
    assert_eq!(tree_snapshot(&service), fresh());

    // 2. ファイルの削除とサイズ変更
    std::fs::remove_file(dir.path().join("a/f1")).unwrap();
    make_file(dir.path(), "a/b/c/deep", 9999);
    let completion = run_partial(&service, "a", dot_excluded(1));
    check_partial(&completion, "a", "a");
    assert_eq!(tree_snapshot(&service), fresh());

    // 3. ディレクトリの追加（ツリーにないパスは、辿れた一番深いディレクトリを走査する）
    make_file(dir.path(), "a/newdir/x/file", 77);
    let completion = run_partial(&service, "a/newdir/x", dot_excluded(2));
    check_partial(&completion, "a/newdir/x", "a");
    assert_eq!(tree_snapshot(&service), fresh());

    // 4. ディレクトリの削除（消えたディレクトリを指定すると、親に遡ってやり直す）
    std::fs::remove_dir_all(dir.path().join("a/b/c")).unwrap();
    let completion = run_partial(&service, "a/b/c", dot_excluded(2));
    let report = completion.report.as_ref().unwrap();
    assert_eq!(
        report.outcome,
        suminoe_file_server::dir_scan::report::ScanOutcome::Success
    );
    assert_eq!(report.parent_retries.len(), 1);
    assert_eq!(tree_snapshot(&service), fresh());

    // 5. ディレクトリがファイルに変わった
    std::fs::remove_dir_all(dir.path().join("sp ace")).unwrap();
    make_file(dir.path(), "sp ace", 8);
    run_partial(&service, "sp ace", dot_excluded(2));
    assert_eq!(tree_snapshot(&service), fresh());

    // 6. 複数の要求を続けて積む（異なる場所。待たずに積んで、すべて待つ）
    std::fs::remove_file(dir.path().join("c2/k")).unwrap();
    make_file(dir.path(), "c2/new", 4);
    make_file(dir.path(), "empty/now_full", 6);
    make_file(dir.path(), "zero_only/z3", 1);
    let tickets: Vec<_> = ["c2", "empty", "zero_only"]
        .iter()
        .map(|path| {
            service
                .request_partial_scan(std::path::Path::new(path), dot_excluded(2))
                .unwrap()
        })
        .collect();
    for ticket in tickets {
        let completion = wait(ticket);
        assert!(matches!(
            completion.disposition,
            suminoe_file_server::dir_scan::request::Disposition::Executed
                | suminoe_file_server::dir_scan::request::Disposition::Merged { .. }
        ));
    }
    assert_eq!(tree_snapshot(&service), fresh());

    // 7. 部分スキャンの要求の除外パターンが違うと、フルスキャンに格上げされる
    make_file(dir.path(), "a/.now_visible", 1);
    let completion = run_partial(
        &service,
        "a",
        settings(
            2,
            suminoe_file_server::dir_scan::settings::IoPriority::Idle,
            &[],
        ),
    );
    let report = completion.report.as_ref().unwrap();
    assert_eq!(
        report.kind,
        suminoe_file_server::dir_scan::report::ScanKind::Full
    );
    service.shutdown();
}

fn check_partial(
    completion: &suminoe_file_server::dir_scan::request::Completion,
    requested: &str,
    effective: &str,
) {
    assert_eq!(
        completion.disposition,
        suminoe_file_server::dir_scan::request::Disposition::Executed
    );
    let report = completion.report.as_ref().unwrap();
    assert_eq!(
        report.outcome,
        suminoe_file_server::dir_scan::report::ScanOutcome::Success
    );
    assert_eq!(
        report.kind,
        suminoe_file_server::dir_scan::report::ScanKind::Partial
    );
    assert_eq!(
        report.requested_path.as_deref(),
        Some(std::path::Path::new(requested))
    );
    assert_eq!(report.effective_path, std::path::PathBuf::from(effective));
}

/// ディレクトリごとの (own_size, total_size, complete)。
fn tree_snapshot(
    service: &suminoe_file_server::dir_scan::ScanService,
) -> std::collections::BTreeMap<std::path::PathBuf, (u128, u128, bool)> {
    let mut result = std::collections::BTreeMap::new();
    let mut pending = vec![std::path::PathBuf::new()];
    while let Some(path) = pending.pop() {
        match service.read_directory(&path).unwrap().directory {
            suminoe_file_server::dir_scan::tree::DirectoryLookup::Found(read) => {
                for child in &read.children {
                    pending.push(path.join(&child.name));
                }
                result.insert(path, (read.own_size, read.total_size, read.complete));
            }
            suminoe_file_server::dir_scan::tree::DirectoryLookup::NotFound => {
                panic!("directory not found: {path:?}")
            }
        }
    }
    result
}

#[test]
fn goal3_parallelism_and_io_priority_are_applied_per_request() {
    init_logger();
    let dir = new_temp_dir();
    let _socket = build_synthetic_tree(dir.path());
    let service =
        suminoe_file_server::dir_scan::ScanService::start(dir.path().to_path_buf()).unwrap();
    let priorities = [
        suminoe_file_server::dir_scan::settings::IoPriority::Idle,
        suminoe_file_server::dir_scan::settings::IoPriority::best_effort(0).unwrap(),
        suminoe_file_server::dir_scan::settings::IoPriority::best_effort(4).unwrap(),
        suminoe_file_server::dir_scan::settings::IoPriority::best_effort(7).unwrap(),
    ];
    // 設定の組ごとに要求を積む（設定が違うので統合されない）。待たずにすべて積んでから待つ
    let mut tickets = Vec::new();
    for parallelism in [1, 2, 4, 8] {
        for io_priority in priorities {
            let requested = settings(parallelism, io_priority, &["**/.*"]);
            let ticket = service.request_full_scan(requested.clone()).unwrap();
            tickets.push((requested, ticket));
        }
    }
    let mut execution_ids = std::collections::BTreeSet::new();
    for (requested, ticket) in tickets {
        let completion = wait(ticket);
        assert_eq!(
            completion.disposition,
            suminoe_file_server::dir_scan::request::Disposition::Executed
        );
        let report = completion.report.as_ref().unwrap();
        assert!(execution_ids.insert(report.execution_id));
        assert_eq!(report.settings, requested);
        assert_eq!(report.worker_ioprio.len(), requested.parallelism.get());
        let expected = requested.io_priority.to_ioprio_value();
        let mut thread_ids = std::collections::BTreeSet::new();
        for (index, worker) in report.worker_ioprio.iter().enumerate() {
            assert_eq!(worker.worker_index, index);
            assert_eq!(worker.requested, expected);
            assert_eq!(
                worker.set,
                suminoe_file_server::dir_scan::report::IoprioSetResult::Success,
                "{requested:?}"
            );
            assert_eq!(
                worker.read_back,
                suminoe_file_server::dir_scan::report::IoprioGetResult::Value(expected),
                "{requested:?}"
            );
            assert_eq!(
                worker.status(),
                suminoe_file_server::dir_scan::report::IoprioStatus::Ok
            );
            assert!(thread_ids.insert(worker.os_thread_id));
        }
    }
    assert_eq!(execution_ids.len(), 16);
    service.shutdown();
}

/// `cargo build --example scan_once` を（テストプロセスごとに 1 回）実行し、実行ファイルのパスを返す。
fn scan_once_exe() -> std::path::PathBuf {
    static BUILD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILD
        .get_or_init(|| {
            // テストの実行ファイルは target/<profile>/deps/ にある
            let test_exe = std::env::current_exe().unwrap();
            let profile_dir = test_exe.parent().unwrap().parent().unwrap().to_path_buf();
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut command = std::process::Command::new(cargo);
            command.args(["build", "--example", "scan_once"]);
            if profile_dir.file_name() == Some(std::ffi::OsStr::new("release")) {
                command.arg("--release");
            }
            let status = command.status().expect("failed to run cargo");
            assert!(status.success(), "cargo build --example scan_once failed");
            let exe = profile_dir.join("examples").join("scan_once");
            assert!(exe.exists(), "not found: {exe:?}");
            exe
        })
        .clone()
}

fn run_scan_once(args: &[&std::ffi::OsStr]) -> std::process::Output {
    std::process::Command::new(scan_once_exe())
        .args(args)
        .env("RUST_LOG", "info")
        .output()
        .unwrap()
}

fn expected_dump(sizes: &DirSizes) -> Vec<u8> {
    let mut expected: Vec<u8> = Vec::new();
    for (path, size) in nonzero(sizes) {
        expected.extend_from_slice(&path);
        expected.extend_from_slice(format!("\t{size}\n").as_bytes());
    }
    expected
}

fn os(text: &str) -> &std::ffi::OsStr {
    std::ffi::OsStr::new(text)
}

#[test]
fn scan_once_dump_dirs_matches_find() {
    init_logger();
    let dir = new_temp_dir();
    let _socket = build_synthetic_tree(dir.path());
    let find = run_find(dir.path());
    let expected = expected_dump(&find.sizes);
    let output = run_scan_once(&[
        dir.path().as_os_str(),
        os("--parallelism"),
        os("4"),
        os("--io-priority"),
        os("best-effort:4"),
        os("--exclude"),
        os("**/.*"),
        os("--dump-dirs"),
        os("--dump-nonzero"),
        os("--report-rss"),
    ]);
    assert!(output.status.success(), "{:?}", output.status);
    // 標準出力にはダンプだけ。バイト列まで find の総和と一致する
    assert!(
        output.stdout == expected,
        "--- scan_once ---\n{}--- find ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&expected)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("outcome=success"), "{stderr}");
    assert!(stderr.contains("rss [before scan]"), "{stderr}");
    assert!(stderr.contains("rss [after full scan]"), "{stderr}");
    assert!(stderr.contains("rss [before exit]"), "{stderr}");
    assert!(stderr.contains("VmHWM"), "{stderr}");

    // --dump-nonzero なしでは、own_size が 0 のディレクトリも出る。ルートに末尾の "/" を付けても同じ
    let mut root_with_slash = dir.path().as_os_str().to_os_string();
    root_with_slash.push("/");
    let output = run_scan_once(&[
        root_with_slash.as_os_str(),
        os("--parallelism"),
        os("2"),
        os("--io-priority"),
        os("idle"),
        os("--exclude"),
        os("**/.*"),
        os("--dump-dirs"),
    ]);
    assert!(output.status.success(), "{:?}", output.status);
    let mut nonzero_lines: Vec<u8> = Vec::new();
    let mut zero_dirs = 0;
    for line in output.stdout.split_inclusive(|b| *b == b'\n') {
        if line.ends_with(b"\t0\n") {
            zero_dirs += 1;
        } else {
            nonzero_lines.extend_from_slice(line);
        }
    }
    assert!(zero_dirs >= 3, "empty, zero_only and so on: {zero_dirs}");
    assert_eq!(nonzero_lines, expected);
}

#[test]
fn scan_once_prints_the_report_to_stdout_and_runs_partial_scans() {
    init_logger();
    let dir = new_temp_dir();
    let _socket = build_synthetic_tree(dir.path());
    let output = run_scan_once(&[
        dir.path().as_os_str(),
        os("--parallelism"),
        os("2"),
        os("--io-priority"),
        os("idle"),
        os("--exclude"),
        os("**/.*"),
        os("--exclude"),
        os("sparse"),
        os("--partial"),
        os("a/b"),
        os("--partial"),
        os("zero_only"),
    ]);
    assert!(output.status.success(), "{:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("kind=full"), "{stdout}");
    assert_eq!(stdout.matches("kind=partial").count(), 2, "{stdout}");
    assert!(stdout.contains("effective_path=\"a/b\""), "{stdout}");
    assert!(stdout.contains("ioprio: workers=2 failed=0"), "{stdout}");
    assert!(
        stdout.contains("exclude_patterns=[\"**/.*\", \"sparse\"]"),
        "{stdout}"
    );
}

#[test]
fn scan_once_exit_codes() {
    init_logger();
    let dir = new_temp_dir();
    make_file(dir.path(), "f", 1);
    // 引数のエラーは 2
    let missing_parallelism =
        run_scan_once(&[dir.path().as_os_str(), os("--io-priority"), os("idle")]);
    assert_eq!(missing_parallelism.status.code(), Some(2));
    let out_of_range = run_scan_once(&[
        dir.path().as_os_str(),
        os("--parallelism"),
        os("65"),
        os("--io-priority"),
        os("idle"),
    ]);
    assert_eq!(out_of_range.status.code(), Some(2));
    let bad_priority = run_scan_once(&[
        dir.path().as_os_str(),
        os("--parallelism"),
        os("2"),
        os("--io-priority"),
        os("best-effort:8"),
    ]);
    assert_eq!(bad_priority.status.code(), Some(2));
    let realtime = run_scan_once(&[
        dir.path().as_os_str(),
        os("--parallelism"),
        os("2"),
        os("--io-priority"),
        os("realtime:0"),
    ]);
    assert_eq!(realtime.status.code(), Some(2));
    let bad_partial = run_scan_once(&[
        dir.path().as_os_str(),
        os("--parallelism"),
        os("2"),
        os("--io-priority"),
        os("idle"),
        os("--partial"),
        os("../x"),
    ]);
    assert_eq!(bad_partial.status.code(), Some(2));
    // 開けないルートは、スキャンの失敗で 1
    let missing_root = dir.path().join("does_not_exist");
    let failed = run_scan_once(&[
        missing_root.as_os_str(),
        os("--parallelism"),
        os("2"),
        os("--io-priority"),
        os("idle"),
    ]);
    assert_eq!(failed.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&failed.stdout);
    assert!(stdout.contains("outcome=failed"), "{stdout}");
}

/// ユーザー名前空間とマウント名前空間を作って bind マウントできる環境か。
/// 使えない環境ではそれを使うテストを飛ばす。
fn can_bind_mount_in_a_new_namespace() -> bool {
    let probe = new_temp_dir();
    std::fs::create_dir_all(probe.path().join("src")).unwrap();
    std::fs::create_dir_all(probe.path().join("dst")).unwrap();
    let unshare_works = std::process::Command::new("unshare")
        .args(["-rm", "true"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !unshare_works {
        return false;
    }
    std::process::Command::new("unshare")
        .args(["-rm", "mount", "--bind"])
        .arg(probe.path().join("src"))
        .arg(probe.path().join("dst"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 新しいマウント名前空間の中で `<root>/a` を `<root>/a/b/loop` に bind マウントしてから、
/// `scan_once` をフルスキャン（と `--partial`）で実行し、`--dump-dirs` の出力を返す。
fn run_scan_once_with_bind_loop(root: &std::path::Path, partial_paths: &[&str]) -> Vec<u8> {
    let script = r#"
root="$1"
exe="$2"
shift 2
mount --bind "$root/a" "$root/a/b/loop" || exit 90
exec "$exe" "$root" --parallelism 2 --io-priority idle --exclude '**/.*' --dump-dirs "$@"
"#;
    let mut command = std::process::Command::new("unshare");
    command
        .args(["-rm", "sh", "-c", script, "sh"])
        .arg(root)
        .arg(scan_once_exe());
    for partial_path in partial_paths {
        command.arg("--partial").arg(partial_path);
    }
    let output = command.env("RUST_LOG", "info").output().unwrap();
    assert!(
        output.status.success(),
        "status={:?}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// 設計書 6.5 の循環の検出：部分スキャンは対象より上の祖先も循環の判定に使うので、
/// bind マウントで `a/b/loop` が `a` を指していても、結果はフルスキャンと一致する。
#[test]
fn goal2_partial_scan_matches_a_full_scan_on_a_bind_mount_loop() {
    init_logger();
    if !can_bind_mount_in_a_new_namespace() {
        eprintln!("skipped: `unshare -rm` with a bind mount is not available");
        return;
    }
    let dir = new_temp_dir();
    make_file(dir.path(), "f_root", 1);
    make_file(dir.path(), "a/f1", 10);
    make_file(dir.path(), "a/b/f2", 20);
    make_file(dir.path(), "a/b/c/f3", 40);
    std::fs::create_dir_all(dir.path().join("a/b/loop")).unwrap();

    let full_only = run_scan_once_with_bind_loop(dir.path(), &[]);
    let dump = String::from_utf8_lossy(&full_only).to_string();
    // ループの中には入らない（`a/b/loop` 自身は出るが、その下は出ない）
    assert!(dump.contains("/a/b/loop\t0\n"), "{dump}");
    assert!(!dump.contains("/a/b/loop/"), "{dump}");
    assert!(dump.contains("/a/b/c\t40\n"), "{dump}");

    for partial_paths in [
        vec!["a/b"],
        vec!["a"],
        vec!["a/b/loop"],
        vec!["a/b/c", "a/b", "a"],
    ] {
        let with_partial = run_scan_once_with_bind_loop(dir.path(), &partial_paths);
        assert!(
            with_partial == full_only,
            "partial={partial_paths:?}\n--- partial ---\n{}--- full ---\n{}",
            String::from_utf8_lossy(&with_partial),
            dump
        );
    }
}
