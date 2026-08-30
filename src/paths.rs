use std::path::{Path, PathBuf};

pub fn canonical_or_original(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn containing_directory(path: &Path) -> &Path {
    if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    }
}

pub fn portable(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
