//! Recently opened locations, kept per user across restarts: what the Open
//! prompt lists below its input.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const MAX: usize = 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Unix seconds when it was last opened.
    pub used: u64,
}

/// Where the list lives: `$XDG_STATE_HOME/codesnob`, else
/// `~/.local/state/codesnob`, with `-dev` for debug builds.
pub fn default_dir() -> Option<PathBuf> {
    let name = if cfg!(debug_assertions) {
        "codesnob-dev"
    } else {
        "codesnob"
    };
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join(name))
}

fn file(dir: &Path) -> PathBuf {
    dir.join("recent.json")
}

pub fn load(dir: &Path) -> Vec<Entry> {
    std::fs::read(file(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Move `dir` to the front of the list and save it. Failures to save are
/// ignored: the list is a convenience.
pub fn record(state: &Path, dir: &Path, base: Option<&str>) {
    let mut list = load(state);
    list.retain(|e| !(e.dir == dir && e.base.as_deref() == base));
    let used = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    list.insert(
        0,
        Entry {
            dir: dir.to_path_buf(),
            base: base.map(str::to_owned),
            used,
        },
    );
    list.truncate(MAX);
    if std::fs::create_dir_all(state).is_ok()
        && let Ok(json) = serde_json::to_vec_pretty(&list)
    {
        let tmp = state.join("recent.json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(tmp, file(state));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_first_without_duplicates_and_capped() {
        let state = crate::testutil::fresh_dir("recent");
        for i in 0..25 {
            record(&state, Path::new(&format!("/r/{i}")), None);
        }
        record(&state, Path::new("/r/3"), None);
        let list = load(&state);
        assert_eq!(list.len(), MAX);
        assert_eq!(list[0].dir, Path::new("/r/3"));
        assert_eq!(list[1].dir, Path::new("/r/24"));
        assert_eq!(
            list.iter().filter(|e| e.dir == Path::new("/r/3")).count(),
            1
        );
        let _ = std::fs::remove_dir_all(state);
    }
}
