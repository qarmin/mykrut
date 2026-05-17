use std::cmp::Ordering;

use mykrut_core::FileEntry;
use slint::ComponentHandle;

use crate::state::AppStateRc;
use crate::{Callabler, MainWindow, Settings, SortKey, SortOrder};

pub fn wire(app: &MainWindow, state: AppStateRc) {
    let weak = app.as_weak();
    app.global::<Callabler>().on_sort_by(move |key| {
        let app = weak.upgrade().expect("MainWindow alive in sort-by");
        let settings = app.global::<Settings>();
        let current_key = settings.get_sort_key();
        if current_key == key {
            // Toggle direction.
            let new_order = match settings.get_sort_order() {
                SortOrder::Ascending => SortOrder::Descending,
                SortOrder::Descending => SortOrder::Ascending,
            };
            settings.set_sort_order(new_order);
        } else {
            settings.set_sort_key(key);
            settings.set_sort_order(SortOrder::Ascending);
        }
        let carry = super::navigation::snapshot_thumbs(&state);
        super::navigation::rebuild_rows(&app, &state, &carry);
    });
}

/// Sort `indices` in place by the given key/order. `entries[indices[i]]` is the visible row.
/// Directories always come first (typical file-manager convention).
pub fn sort_indices(indices: &mut [usize], entries: &[FileEntry], key: SortKey, order: SortOrder) {
    indices.sort_by(|&a, &b| {
        let ea = &entries[a];
        let eb = &entries[b];

        let dir_cmp = (!ea.is_dir()).cmp(&!eb.is_dir());
        if dir_cmp != Ordering::Equal {
            return dir_cmp; // directories first regardless of sort
        }

        let base = match key {
            SortKey::Name => natural_cmp(&ea.display_name, &eb.display_name),
            SortKey::Size => ea.size.cmp(&eb.size),
            SortKey::Mtime => ea.mtime.cmp(&eb.mtime),
            SortKey::Kind => ea.mime.cmp(&eb.mime),
        };

        if matches!(order, SortOrder::Descending) {
            base.reverse()
        } else {
            base
        }
    });
}

/// Case-insensitive natural-order comparison (`file2` < `file10`), delegated to
/// the `lexical-sort` crate so we don't hand-maintain a Unicode-aware comparator.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    lexical_sort::natural_lexical_cmp(a, b)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use mykrut_core::{FileEntry, FileType, Permissions};

    use super::*;

    fn make(name: &str, ft: FileType, size: u64) -> FileEntry {
        FileEntry {
            path: name.into(),
            display_name: name.to_string(),
            file_type: ft,
            mime: None,
            size,
            mtime: SystemTime::UNIX_EPOCH,
            permissions: Permissions::default(),
            is_hidden: false,
            is_symlink: false,
        }
    }

    #[test]
    fn directories_come_first() {
        let entries = vec![
            make("zfile.txt", FileType::Regular, 100),
            make("adir", FileType::Directory, 0),
            make("afile.txt", FileType::Regular, 50),
        ];
        let mut idx: Vec<usize> = (0..3).collect();
        sort_indices(&mut idx, &entries, SortKey::Name, SortOrder::Ascending);
        // First should be the directory regardless of name.
        assert_eq!(entries[idx[0]].display_name, "adir");
    }

    #[test]
    fn natural_order_numbers() {
        assert_eq!(natural_cmp("file2", "file10"), Ordering::Less);
        assert_eq!(natural_cmp("file10", "file2"), Ordering::Greater);
        assert_eq!(natural_cmp("img1.png", "img1.png"), Ordering::Equal);
        assert_eq!(natural_cmp("track9", "track10"), Ordering::Less);
        // Pure-text still orders lexicographically.
        assert_eq!(natural_cmp("apple", "banana"), Ordering::Less);
        // Case-insensitive grouping: "Apple" sorts next to "apple", before "banana".
        assert_eq!(natural_cmp("Apple", "banana"), Ordering::Less);
    }

    #[test]
    fn natural_order_sorts_file_list() {
        let names = ["f10.txt", "f2.txt", "f1.txt"];
        let entries: Vec<FileEntry> = names.iter().map(|n| make(n, FileType::Regular, 0)).collect();
        let mut idx: Vec<usize> = (0..3).collect();
        sort_indices(&mut idx, &entries, SortKey::Name, SortOrder::Ascending);
        let sorted: Vec<&str> = idx.iter().map(|&i| entries[i].display_name.as_str()).collect();
        assert_eq!(sorted, ["f1.txt", "f2.txt", "f10.txt"]);
    }

    #[test]
    fn ascending_then_descending() {
        let entries = vec![
            make("b.txt", FileType::Regular, 200),
            make("a.txt", FileType::Regular, 100),
            make("c.txt", FileType::Regular, 300),
        ];
        let mut idx: Vec<usize> = (0..3).collect();
        sort_indices(&mut idx, &entries, SortKey::Name, SortOrder::Ascending);
        assert_eq!(entries[idx[0]].display_name, "a.txt");

        let mut idx: Vec<usize> = (0..3).collect();
        sort_indices(&mut idx, &entries, SortKey::Size, SortOrder::Descending);
        assert_eq!(entries[idx[0]].size, 300);
    }
}
