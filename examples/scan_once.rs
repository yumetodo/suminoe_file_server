//! 動作確認・検証用のプログラム（設計書 6.8）。
//!
//! ルートと設定を引数で受け取り、スキャンサービスを立ち上げてフルスキャンを 1 回実行し、
//! チケットで完了を待ってレポートを出力し、サービスを停止して終了する。
//!
//! ```text
//! scan_once <root> --parallelism <1..64> --io-priority <idle|best-effort:0..7>
//!           [--exclude <glob>]... [--partial <relative path>]...
//!           [--dump-dirs] [--dump-nonzero] [--report-rss]
//! ```
//!
//! - 設定の既定値は持たない（設計書 6.2）。`--parallelism` と `--io-priority` は必須。
//! - `--exclude` は複数指定できる。例: `--exclude '**/.*'`
//! - `--partial` は複数指定でき、フルスキャンの後に、指定した順に部分スキャンを続けて実行する。
//! - `--dump-dirs` は、ディレクトリごとの `own_size` を `<パス>\t<own_size>\n` で標準出力に出す。
//!   レポートは標準エラー出力に出す。パスは、ルートに指定した文字列を接頭辞にする
//!   （末尾の `/` は除く）ので、次の `find` の `%h` と同じ形式になる。非 UTF-8 の名前はバイトのまま出す。
//!   `find <root> -name '.*' -prune -o \( -type f -o -type l \) -printf '%h\t%s\n'`
//!   - `find` は、ファイルが 1 つもないディレクトリを出さない。`--dump-dirs` は既定では
//!     `own_size` が 0 のディレクトリも出す。`--dump-nonzero` を付けると 0 のディレクトリを出さない。
//!     サイズが 0 のファイルしかないディレクトリは `find` が `0` で出すので、突き合わせでは
//!     両側から 0 の行を除くか、`find` の出力をディレクトリごとに合計して欠けたディレクトリを 0
//!     として扱う。
//! - `--report-rss` は、`/proc/self/status` の `VmRSS`／`VmHWM` を、スキャン前・スキャン完了後・
//!   終了前（サービス停止後）にログ（info）へ出す。
//! - ログは `env_logger`。`RUST_LOG` で変えられる（未指定なら info）。
//! - 終了コード：0 は成功、1 はスキャンの失敗、2 は引数のエラー。

const USAGE: &str =
    "usage: scan_once <root> --parallelism <1..64> --io-priority <idle|best-effort:0..7> \
[--exclude <glob>]... [--partial <relative path>]... [--dump-dirs] [--dump-nonzero] [--report-rss]";

/// 引数のエラー。終了コード 2。
#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UsageError {}

struct Args {
    root: std::path::PathBuf,
    settings: suminoe_file_server::dir_scan::settings::ScanSettings,
    partial_paths: Vec<std::path::PathBuf>,
    dump_dirs: bool,
    dump_nonzero: bool,
    report_rss: bool,
}

fn usage_error<T>(message: String) -> anyhow::Result<T> {
    Err(anyhow::Error::new(UsageError(message)))
}

fn parse_io_priority(
    value: &str,
) -> anyhow::Result<suminoe_file_server::dir_scan::settings::IoPriority> {
    use anyhow::Context;
    if value == "idle" {
        return Ok(suminoe_file_server::dir_scan::settings::IoPriority::Idle);
    }
    if let Some(level) = value.strip_prefix("best-effort:") {
        let level: u8 = level
            .parse()
            .with_context(|| format!("invalid best-effort level: {level}"))?;
        return suminoe_file_server::dir_scan::settings::IoPriority::best_effort(level)
            .context("invalid best-effort level");
    }
    anyhow::bail!("invalid --io-priority (expected idle or best-effort:0..7): {value}")
}

/// 次の引数を値として取り出す。
fn take_value(
    iter: &mut std::vec::IntoIter<std::ffi::OsString>,
    option: &str,
) -> anyhow::Result<std::ffi::OsString> {
    match iter.next() {
        Some(value) => Ok(value),
        None => usage_error(format!("{option} requires a value")),
    }
}

fn take_utf8_value(
    iter: &mut std::vec::IntoIter<std::ffi::OsString>,
    option: &str,
) -> anyhow::Result<String> {
    let value = take_value(iter, option)?;
    match value.into_string() {
        Ok(value) => Ok(value),
        Err(_) => usage_error(format!("{option} requires a UTF-8 value")),
    }
}

fn parse_args(args: Vec<std::ffi::OsString>) -> anyhow::Result<Args> {
    use anyhow::Context;
    let mut iter = args.into_iter();
    let mut root: Option<std::path::PathBuf> = None;
    let mut parallelism: Option<usize> = None;
    let mut io_priority: Option<suminoe_file_server::dir_scan::settings::IoPriority> = None;
    let mut excludes: Vec<String> = Vec::new();
    let mut partial_paths: Vec<std::path::PathBuf> = Vec::new();
    let mut dump_dirs = false;
    let mut dump_nonzero = false;
    let mut report_rss = false;
    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--parallelism") => {
                let value = take_utf8_value(&mut iter, "--parallelism")?;
                let value: usize = value
                    .parse()
                    .with_context(|| format!("invalid --parallelism: {value}"))
                    .map_err(|e| anyhow::Error::new(UsageError(format!("{e:#}"))))?;
                parallelism = Some(value);
            }
            Some("--io-priority") => {
                let value = take_utf8_value(&mut iter, "--io-priority")?;
                let value = parse_io_priority(&value)
                    .map_err(|e| anyhow::Error::new(UsageError(format!("{e:#}"))))?;
                io_priority = Some(value);
            }
            Some("--exclude") => {
                excludes.push(take_utf8_value(&mut iter, "--exclude")?);
            }
            Some("--partial") => {
                partial_paths.push(std::path::PathBuf::from(take_value(
                    &mut iter,
                    "--partial",
                )?));
            }
            Some("--dump-dirs") => dump_dirs = true,
            Some("--dump-nonzero") => dump_nonzero = true,
            Some("--report-rss") => report_rss = true,
            Some("-h") | Some("--help") => return usage_error(String::new()),
            Some(option) if option.starts_with("--") => {
                return usage_error(format!("unknown option: {option}"));
            }
            _ => {
                if root.is_some() {
                    return usage_error(format!("unexpected argument: {arg:?}"));
                }
                root = Some(std::path::PathBuf::from(arg));
            }
        }
    }
    let root = match root {
        Some(root) => root,
        None => return usage_error("the root directory is required".to_string()),
    };
    let parallelism = match parallelism {
        Some(parallelism) => parallelism,
        None => return usage_error("--parallelism is required".to_string()),
    };
    let io_priority = match io_priority {
        Some(io_priority) => io_priority,
        None => return usage_error("--io-priority is required".to_string()),
    };
    if dump_nonzero && !dump_dirs {
        return usage_error("--dump-nonzero requires --dump-dirs".to_string());
    }
    for partial_path in &partial_paths {
        suminoe_file_server::dir_scan::request::ScanTarget::partial(partial_path).map_err(|e| {
            anyhow::Error::new(UsageError(format!(
                "invalid --partial {partial_path:?}: {e:#}"
            )))
        })?;
    }
    let parallelism = suminoe_file_server::dir_scan::settings::Parallelism::new(parallelism)
        .map_err(|e| anyhow::Error::new(UsageError(format!("invalid --parallelism: {e:#}"))))?;
    let exclude_patterns = suminoe_file_server::dir_scan::settings::ExcludePatterns::new(&excludes)
        .map_err(|e| anyhow::Error::new(UsageError(format!("invalid --exclude: {e:#}"))))?;
    Ok(Args {
        root,
        settings: suminoe_file_server::dir_scan::settings::ScanSettings::new(
            parallelism,
            io_priority,
            exclude_patterns,
        ),
        partial_paths,
        dump_dirs,
        dump_nonzero,
        report_rss,
    })
}

/// `/proc/self/status` の `VmRSS` と `VmHWM` の行を、ログ（info）に出す。
fn log_rss(label: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let status =
        std::fs::read_to_string("/proc/self/status").context("failed to read /proc/self/status")?;
    let mut values: Vec<String> = Vec::new();
    for line in status.lines() {
        if line.starts_with("VmRSS:") || line.starts_with("VmHWM:") {
            let squeezed: Vec<&str> = line.split_whitespace().collect();
            values.push(squeezed.join(" "));
        }
    }
    log::info!("rss [{}]: {}", label, values.join(", "));
    Ok(())
}

/// ルートに指定した文字列を接頭辞にしたパスにする。`find` の `%h` と同じ形式にするため、
/// ルートの末尾の `/` は除く（`Path::components` が除く）。
fn dump_path(root: &std::path::Path, relative: &std::path::Path) -> std::path::PathBuf {
    let normalized_root: std::path::PathBuf = root.components().collect();
    if relative.as_os_str().is_empty() {
        // join("") は末尾に "/" を付けてしまう
        return normalized_root;
    }
    normalized_root.join(relative)
}

fn write_dump(
    out: &mut dyn std::io::Write,
    root: &std::path::Path,
    entries: &[(std::path::PathBuf, u128)],
    nonzero_only: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::ffi::OsStrExt;
    for (relative, own_size) in entries {
        if nonzero_only && *own_size == 0 {
            continue;
        }
        let path = dump_path(root, relative);
        out.write_all(path.as_os_str().as_bytes())
            .context("failed to write a path")?;
        writeln!(out, "\t{own_size}").context("failed to write an own_size")?;
    }
    out.flush().context("failed to flush the directory dump")?;
    Ok(())
}

/// 完了通知のレポートを出力する。スキャンが失敗なら `false`。
fn write_completion(
    out: &mut dyn std::io::Write,
    completion: &suminoe_file_server::dir_scan::request::Completion,
) -> anyhow::Result<bool> {
    use anyhow::Context;
    writeln!(
        out,
        "request {}: {:?} requested={} executed={}",
        completion.request_id,
        completion.disposition,
        completion.requested,
        match &completion.executed {
            Some(executed) => executed.to_string(),
            None => "-".to_string(),
        }
    )
    .context("failed to write the completion")?;
    let mut succeeded = matches!(
        completion.disposition,
        suminoe_file_server::dir_scan::request::Disposition::Executed
            | suminoe_file_server::dir_scan::request::Disposition::Merged { .. }
    );
    if let Some(report) = &completion.report {
        writeln!(out, "{}", report.summary()).context("failed to write the report")?;
        if let suminoe_file_server::dir_scan::report::ScanOutcome::Failed(_) = report.outcome {
            succeeded = false;
        }
    }
    out.flush().context("failed to flush the report")?;
    Ok(succeeded)
}

/// 全体の処理。スキャンがすべて成功なら `true`。
fn run(args: &Args) -> anyhow::Result<bool> {
    use anyhow::Context;
    // 注意: stdout()/stderr() の lock() をスキャンの間じゅう持ってはいけない。スキャン実行スレッドが
    // ログ（env_logger は標準エラー出力へ書く）を出すときに、同じロックを待ってデッドロックする
    let mut dump_out = std::io::BufWriter::new(std::io::stdout());
    let mut stderr_out = std::io::stderr();
    let report_out: &mut dyn std::io::Write = if args.dump_dirs {
        &mut stderr_out
    } else {
        &mut dump_out
    };

    if args.report_rss {
        log_rss("before scan")?;
    }
    let service = suminoe_file_server::dir_scan::ScanService::start(args.root.clone())
        .context("failed to start the scan service")?;
    let mut all_succeeded = true;

    let ticket = service
        .request_full_scan(args.settings.clone())
        .context("failed to request the full scan")?;
    let completion = ticket.wait().context("failed to wait for the full scan")?;
    if !write_completion(report_out, &completion)? {
        all_succeeded = false;
    }
    if args.report_rss {
        log_rss("after full scan")?;
    }

    for partial_path in &args.partial_paths {
        let ticket = service
            .request_partial_scan(partial_path, args.settings.clone())
            .with_context(|| format!("failed to request the partial scan of {partial_path:?}"))?;
        let completion = ticket
            .wait()
            .with_context(|| format!("failed to wait for the partial scan of {partial_path:?}"))?;
        if !write_completion(report_out, &completion)? {
            all_succeeded = false;
        }
        if args.report_rss {
            log_rss(&format!("after partial scan {partial_path:?}"))?;
        }
    }

    if args.dump_dirs {
        let entries = service
            .list_own_sizes()
            .context("failed to list the own sizes")?;
        // report_out が標準エラー出力を借りているので、ここからはダンプ用の書き込み先だけを使う
        write_dump(&mut dump_out, &args.root, &entries, args.dump_nonzero)?;
    }
    service.shutdown();
    if args.report_rss {
        log_rss("before exit")?;
    }
    Ok(all_succeeded)
}

fn main() -> std::process::ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let raw_args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let args = match parse_args(raw_args) {
        Ok(args) => args,
        Err(error) => {
            if let Some(usage) = error.downcast_ref::<UsageError>() {
                if !usage.0.is_empty() {
                    log::error!("{usage}");
                }
            } else {
                log::error!("{error:#}");
            }
            log::error!("{USAGE}");
            return std::process::ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => {
            log::error!("the scan failed");
            std::process::ExitCode::from(1)
        }
        Err(error) => {
            log::error!("{error:#}");
            std::process::ExitCode::from(1)
        }
    }
}
