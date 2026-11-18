use std::path::PathBuf;

/// Resolve the directory where Nicotine writes its lock file. Integration
/// tests set `NICOTINE_RUNTIME_DIR` to a private tmp directory so the lock
/// file doesn't collide with a real running daemon's. In production:
/// - Linux: `/tmp` (matches the historical hardcoded paths).
/// - Windows: the user's cache dir under a `nicotine` subdir.
fn runtime_path(filename: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("NICOTINE_RUNTIME_DIR") {
        let mut p = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&p);
        p.push(filename);
        return p;
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/tmp/{}", filename))
    }
    #[cfg(windows)]
    {
        let mut p = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
        p.push("nicotine");
        let _ = std::fs::create_dir_all(&p);
        p.push(filename);
        p
    }
}

pub fn lock_file_path() -> PathBuf {
    runtime_path("nicotine-cycle.lock")
}

/// Where `nicotine start` writes the daemonized process's stdout/stderr:
/// `$XDG_STATE_HOME/nicotine` (default `~/.local/state/nicotine`). Kept out
/// of `/tmp`, which is often a size-limited tmpfs where a runaway log
/// starves every other program writing temp files.
#[cfg(unix)]
pub fn log_dir() -> PathBuf {
    let mut p = dirs::state_dir()
        .or_else(dirs::cache_dir)
        .unwrap_or_else(std::env::temp_dir);
    p.push("nicotine");
    p
}
