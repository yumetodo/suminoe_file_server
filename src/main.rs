use rayon::prelude::*;
use std::fs;
use std::os::unix::fs::DirEntryExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

struct Node {
    #[allow(dead_code)]
    name: String,
    children: Vec<Node>,
}

// ツリー内のディレクトリ数（root を含む）
fn count(n: &Node) -> usize {
    1 + n.children.iter().map(count).sum::<usize>()
}

fn name_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_string();
    if name.starts_with('.') { None } else { Some(name) }
}

// test/dir_scan/1 の tree_scan と同じロジック（is_dir() あり、エラーで subtree ごと消える）
fn scan_orig(path: &Path) -> Option<Node> {
    let name = name_of(path)?;
    let mut children = Vec::new();
    if path.is_dir() {
        for entry in fs::read_dir(path).ok()? {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() { continue; }
            if let Some(c) = scan_orig(&entry.path()) { children.push(c); }
        }
    }
    Some(Node { name, children })
}

fn subdirs(path: &Path, sort_ino: bool) -> Vec<PathBuf> {
    let Ok(rd) = fs::read_dir(path) else { return Vec::new() };
    let mut v: Vec<(u64, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| (e.ino(), e.path()))
        .collect();
    if sort_ino { v.sort_unstable_by_key(|(i, _)| *i); }
    v.into_iter().map(|(_, p)| p).collect()
}

// is_dir() を除去し、エラーは continue
fn scan_seq(path: &Path, sort_ino: bool) -> Option<Node> {
    let name = name_of(path)?;
    let children = subdirs(path, sort_ino).iter().filter_map(|p| scan_seq(p, sort_ino)).collect();
    Some(Node { name, children })
}

fn scan_par(path: &Path) -> Option<Node> {
    let name = name_of(path)?;
    let children = subdirs(path, false).par_iter().filter_map(|p| scan_par(p)).collect();
    Some(Node { name, children })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <orig|seq|seq-ino|par:N> <dir>", args[0]);
        std::process::exit(1);
    }
    let mode = args[1].as_str();
    let root = fs::canonicalize(&args[2]).expect("bad path");
    let start = Instant::now();
    let tree = match mode {
        "orig" => scan_orig(&root),
        "seq" => scan_seq(&root, false),
        "seq-ino" => scan_seq(&root, true),
        m if m.starts_with("par:") => {
            let n: usize = m[4..].parse().expect("bad thread count");
            let pool = rayon::ThreadPoolBuilder::new().num_threads(n).build().unwrap();
            pool.install(|| scan_par(&root))
        }
        _ => panic!("unknown mode"),
    };
    let elapsed = start.elapsed();
    let dirs = tree.as_ref().map(count).unwrap_or(0);
    println!("mode={mode}\tdirs={dirs}\tscan={:.3}s", elapsed.as_secs_f64());
}
