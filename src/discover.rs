//! Markdown file discovery helper shared by the `mq-db` CLI and other
//! front-ends (e.g. `mq-mcp`'s `db_index` tool) that index files/directories
//! given by the caller.

use std::path::{Path, PathBuf};

/// Collects every `.md`/`.markdown` file among `paths`, descending into
/// directories (recursively when `recursive` is `true`). Paths that don't
/// exist are skipped with a warning printed to stderr; non-Markdown files
/// passed explicitly are silently skipped.
pub fn collect_markdown_files(paths: &[PathBuf], recursive: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            if is_markdown(path) {
                files.push(path.clone());
            }
        } else if path.is_dir() {
            collect_dir(path, recursive, &mut files);
        } else {
            eprintln!("Warning: {} does not exist, skipping", path.display());
        }
    }
    files
}

fn collect_dir(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() && is_markdown(&path) {
            out.push(path);
        } else if path.is_dir() && recursive {
            collect_dir(&path, recursive, out);
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("md") | Some("markdown")
    )
}
