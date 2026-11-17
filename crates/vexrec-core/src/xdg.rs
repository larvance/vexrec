use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Returns the user's home directory from `HOME`.
pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Returns `$XDG_CONFIG_HOME` or `$HOME/.config`.
pub fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

/// Returns `$XDG_RUNTIME_DIR` or falls back to a per-UID temp directory.
pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp")
}

/// Returns `$XDG_DATA_HOME` or `$HOME/.local/share`.
pub fn data_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"))
}

/// Expands leading `~` or `~/` to the user's home directory.
pub fn expand_tilde<P: AsRef<Path>>(path: P) -> PathBuf {
    let p = path.as_ref();
    let s = p.to_string_lossy();
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    p.to_path_buf()
}

/// Reads `$HOME/.config/user-dirs.dirs` to resolve `XDG_PICTURES_DIR` or `XDG_VIDEOS_DIR`.
fn parse_user_dir(var_name: &str, fallback_sub: &str) -> PathBuf {
    if let Some(env_val) = env::var_os(var_name).filter(|s| !s.is_empty()) {
        return PathBuf::from(env_val);
    }
    let user_dirs_file = config_dir().join("user-dirs.dirs");
    if let Ok(contents) = fs::read_to_string(user_dirs_file) {
        let prefix = format!("{var_name}=");
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || !trimmed.starts_with(&prefix) {
                continue;
            }
            let rhs = trimmed[prefix.len()..].trim().trim_matches('"');
            if let Some(rel) = rhs.strip_prefix("$HOME/") {
                return home_dir().join(rel);
            }
            if rhs == "$HOME" {
                return home_dir();
            }
            if !rhs.is_empty() {
                return PathBuf::from(rhs);
            }
        }
    }
    home_dir().join(fallback_sub)
}

/// Returns the user's XDG Pictures directory.
pub fn picture_dir() -> PathBuf {
    parse_user_dir("XDG_PICTURES_DIR", "Pictures")
}

/// Returns the user's XDG Videos directory.
pub fn video_dir() -> PathBuf {
    parse_user_dir("XDG_VIDEOS_DIR", "Videos")
}
