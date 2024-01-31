use chrono::{serde::ts_seconds, DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::process::exit;
use std::fs;
use std::path::Path;

#[derive(Serialize)]
struct DirectoryTreeResult {
    dir: Option<Value>,
    #[serde(with = "ts_seconds")]
    last_scaned_at: DateTime<Utc>,
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 { exit(1) }
    let path = Path::new(&args[1]);
    let tree = serde_json::to_string(&create_directory_tree(&path)).unwrap();
    println!("{}", tree);
}
fn create_directory_tree(path: &Path) -> DirectoryTreeResult {
    DirectoryTreeResult {
        dir: tree_scan(path),
        last_scaned_at: Utc::now(),
    }
}

fn tree_scan(path: &Path) -> Option<Value> {
    let name = path.file_name()?.to_str()?.to_string();
    if name.starts_with('.') {
        return None;
    }
    let mut tree: Vec<Value> = Vec::new();
    tree.push(Value::String(name));
    if path.is_dir() {
        let entries = fs::read_dir(path).ok()?;
        for entry in entries {
            let entry = entry.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                continue;
            }
            let child_path = entry.path();
            let child_tree = tree_scan(&child_path);
            if let Some(child_tree) = child_tree {
                tree.push(child_tree);
            }
        }
    }
    Some(Value::Array(tree))
}
