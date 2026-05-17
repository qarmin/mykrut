//! Persistent bookmark store under `$XDG_CONFIG_HOME/<app>/bookmarks.toml`.
//!
//! Schema:
//! ```toml
//! [[bookmark]]
//! name = "Code"
//! path = "/home/rafal/code"
//! ```
//!
//! Missing/malformed files load as empty — never panic on user data.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct BookmarkStore {
    #[serde(default, rename = "bookmark")]
    pub entries: Vec<BookmarkEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookmarkEntry {
    pub name: String,
    pub path: String,
}

impl BookmarkStore {
    pub fn upsert(&mut self, name: &str, path: &std::path::Path) {
        let p = path.display().to_string();
        if let Some(existing) = self.entries.iter_mut().find(|e| e.path == p) {
            existing.name = name.to_string();
        } else {
            self.entries.push(BookmarkEntry {
                name: name.to_string(),
                path: p,
            });
        }
    }

    pub fn remove_by_path(&mut self, path: &str) {
        self.entries.retain(|e| e.path != path);
    }
}

pub fn config_dir() -> Option<PathBuf> {
    let base = dirs::config_dir()?;
    Some(base.join(env!("CARGO_PKG_NAME").trim_start_matches("fm-")).join("data"))
}

fn config_file() -> Option<PathBuf> {
    config_dir().map(|d| d.join("bookmarks.toml"))
}

pub fn load() -> BookmarkStore {
    let Some(path) = config_file() else {
        return BookmarkStore::default();
    };
    debug!(path = %path.display(), "loading bookmarks");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BookmarkStore::default();
    };
    match toml::from_str(&text) {
        Ok(store) => store,
        Err(err) => {
            warn!(?err, "malformed bookmarks.toml — starting empty");
            BookmarkStore::default()
        }
    }
}

pub fn save(store: &BookmarkStore) -> std::io::Result<()> {
    let Some(path) = config_file() else {
        return Err(std::io::Error::other("no XDG config dir"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(store).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::fs_atomic::write_atomic(&path, text.as_bytes())?;
    debug!(path = %path.display(), count = store.entries.len(), "bookmarks saved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_same_path() {
        let mut s = BookmarkStore::default();
        s.upsert("First", &PathBuf::from("/a"));
        s.upsert("Second", &PathBuf::from("/a"));
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].name, "Second");
    }

    #[test]
    fn remove_by_path_works() {
        let mut s = BookmarkStore::default();
        s.upsert("A", &PathBuf::from("/a"));
        s.upsert("B", &PathBuf::from("/b"));
        s.remove_by_path("/a");
        assert_eq!(s.entries.len(), 1);
        assert_eq!(s.entries[0].path, "/b");
    }

    #[test]
    fn roundtrip_toml() {
        let mut s = BookmarkStore::default();
        s.upsert("Code", &PathBuf::from("/home/x/code"));
        s.upsert("Notes", &PathBuf::from("/home/x/notes"));
        let text = toml::to_string_pretty(&s).unwrap();
        let parsed: BookmarkStore = toml::from_str(&text).unwrap();
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].path, "/home/x/code");
    }
}
