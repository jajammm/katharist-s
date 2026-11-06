//! Explains the gap between what a scan found and what the drive reports as
//! used. Reads attributes and sizes only and deletes nothing.

use std::io;
use std::path::Path;

use crate::scan::{classify, EntryClass, ScanError, ScanTree};
use crate::win;

/// A well-known system item in the drive root.
#[derive(Debug, PartialEq, Eq)]
pub struct SystemItem {
    pub name: String,
    /// `None` when any part of the item could not be read; never a partial sum.
    pub size: Option<u64>,
    /// True when the scan already counted this item in `scanned`.
    pub in_scan: bool,
}

#[derive(Debug)]
pub struct OtherSpace {
    pub used: u64,
    pub scanned: u64,
    pub unaccounted: u64,
    pub system_files: Vec<SystemItem>,
    pub unreadable: Vec<ScanError>,
}

/// Items in a drive root that a scan cannot account for.
const SYSTEM_NAMES: [&str; 4] = [
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "System Volume Information",
];

/// For a scan of a drive root. Fails only if the volume cannot be queried.
/// The scan lists every entry of the root and sizes files with
/// `allocated_size`, so a system file it could size is already in `scanned`
/// (`in_scan`), while an unreadable folder such as `System Volume Information`
/// stays at size 0 there and shows up in `tree.errors`.
pub fn other_space(tree: &ScanTree) -> io::Result<OtherSpace> {
    let (total, free) = win::volume_space(&tree.root_path)?;
    let used = total.saturating_sub(free);
    let scanned = tree.nodes[tree.root].size;
    let in_scan = |name: &str| {
        tree.nodes[tree.root].children.iter().any(|&c| {
            let n = &tree.nodes[c];
            n.size > 0 && n.name.eq_ignore_ascii_case(name)
        })
    };
    let (system_files, new_errors) = system_items(&tree.root_path, in_scan, &tree.errors);
    let mut unreadable: Vec<ScanError> = tree
        .errors
        .iter()
        .map(|e| ScanError {
            path: e.path.clone(),
            reason: e.reason.clone(),
        })
        .collect();
    unreadable.extend(new_errors);
    Ok(assemble(used, scanned, system_files, unreadable))
}

fn assemble(
    used: u64,
    scanned: u64,
    system_files: Vec<SystemItem>,
    unreadable: Vec<ScanError>,
) -> OtherSpace {
    OtherSpace {
        used,
        scanned,
        unaccounted: used.saturating_sub(scanned),
        system_files,
        unreadable,
    }
}

/// The well-known system items that exist in `root`, biggest first (unknown
/// sizes last), and a `ScanError` for each one that could not be fully read
/// unless `known_errors` already has its path.
fn system_items(
    root: &Path,
    in_scan: impl Fn(&str) -> bool,
    known_errors: &[ScanError],
) -> (Vec<SystemItem>, Vec<ScanError>) {
    let Ok(entries) = win::list_dir(root) else {
        return (Vec::new(), Vec::new());
    };
    let mut items = Vec::new();
    let mut errors = Vec::new();
    for e in &entries {
        if !SYSTEM_NAMES.iter().any(|n| n.eq_ignore_ascii_case(&e.name)) {
            continue;
        }
        let size = match entry_size(root, e) {
            Ok(size) => Some(size),
            Err(err) => {
                if !known_errors.iter().any(|k| k.path == err.path) {
                    errors.push(err);
                }
                None
            }
        };
        items.push(SystemItem {
            name: e.name.clone(),
            size,
            in_scan: in_scan(&e.name),
        });
    }
    items.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    (items, errors)
}

/// Size of one entry, or the first part of it that could not be read.
fn entry_size(dir: &Path, e: &win::DirEntry) -> Result<u64, ScanError> {
    let path = dir.join(&e.name);
    let fail = |err: io::Error| ScanError {
        path: path.clone(),
        reason: err.to_string(),
    };
    match classify(e.attrs, e.reparse_tag) {
        EntryClass::File => win::allocated_size(&path).map_err(fail),
        EntryClass::Dir => {
            let entries = win::list_dir(&path).map_err(fail)?;
            entries.iter().map(|c| entry_size(&path, c)).sum()
        }
        EntryClass::Link => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanOptions, ScanProgress};
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    /// Lifts the deny rule even when an assertion fails, so the temp dir can be removed.
    struct DenyGuard(PathBuf);
    impl Drop for DenyGuard {
        fn drop(&mut self) {
            let _ = Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", "Everyone"])
                .output();
        }
    }

    fn deny(dir: &Path) -> DenyGuard {
        let guard = DenyGuard(dir.to_path_buf());
        let out = Command::new("icacls")
            .arg(dir)
            .args(["/deny", "Everyone:(OI)(CI)(RD,DC)"])
            .output()
            .unwrap();
        assert!(out.status.success(), "icacls failed");
        guard
    }

    const SVI: &str = "System Volume Information";

    #[test]
    fn other_space_never_underflows() {
        let o = assemble(100, 150, Vec::new(), Vec::new());
        assert_eq!(o.unaccounted, 0);
        assert_eq!((o.used, o.scanned), (100, 150));
        let o = assemble(150, 100, Vec::new(), Vec::new());
        assert_eq!(o.unaccounted, 50);
        assert_eq!(assemble(0, 0, Vec::new(), Vec::new()).unaccounted, 0);
        let max = assemble(u64::MAX, u64::MAX, Vec::new(), Vec::new());
        assert_eq!(max.unaccounted, 0);
    }

    #[test]
    fn assemble_keeps_items() {
        let err = ScanError {
            path: PathBuf::from("C:\\x"),
            reason: "Access is denied.".into(),
        };
        let item = SystemItem {
            name: "pagefile.sys".into(),
            size: Some(3),
            in_scan: true,
        };
        let o = assemble(10, 5, vec![item], vec![err]);
        assert_eq!(o.system_files[0].size, Some(3));
        assert_eq!(o.unreadable.len(), 1);
        assert_eq!(o.unreadable[0].reason, "Access is denied.");
    }

    #[test]
    fn finds_system_items_case_insensitively_and_flags_in_scan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("PageFile.sys"), vec![1u8; 4096]).unwrap();
        fs::write(dir.path().join("other.txt"), b"x").unwrap();
        let svi = dir.path().join(SVI);
        fs::create_dir(&svi).unwrap();
        fs::write(svi.join("f"), vec![1u8; 100]).unwrap();
        let (found, errors) = system_items(dir.path(), |n| n == "PageFile.sys", &[]);
        assert!(errors.is_empty());
        assert_eq!(
            found,
            [
                SystemItem {
                    name: "PageFile.sys".into(),
                    size: Some(4096),
                    in_scan: true
                },
                SystemItem {
                    name: SVI.into(),
                    size: Some(100),
                    in_scan: false
                },
            ]
        );
        let (none, _) = system_items(&dir.path().join("missing"), |_| false, &[]);
        assert!(none.is_empty());
    }

    #[test]
    fn unreadable_directory_has_no_size_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let svi = dir.path().join(SVI);
        fs::create_dir(&svi).unwrap();
        fs::write(svi.join("f"), vec![1u8; 100]).unwrap();
        let _guard = deny(&svi);
        let (found, errors) = system_items(dir.path(), |_| false, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].size, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, svi);
        assert!(!errors[0].reason.is_empty());
        // Already recorded by the scan: no duplicate.
        let known = [ScanError {
            path: svi.clone(),
            reason: "denied".into(),
        }];
        let (found, errors) = system_items(dir.path(), |_| false, &known);
        assert_eq!(found[0].size, None);
        assert!(errors.is_empty());
    }

    #[test]
    fn one_unreadable_child_gives_none_not_a_partial_sum() {
        let dir = tempfile::tempdir().unwrap();
        let svi = dir.path().join(SVI);
        let locked = svi.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("hidden"), vec![1u8; 100]).unwrap();
        fs::write(svi.join("ok"), vec![1u8; 4096]).unwrap();
        let _guard = deny(&locked);
        let (found, errors) = system_items(dir.path(), |_| false, &[]);
        assert_eq!(found[0].size, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, locked);
        assert!(!errors[0].reason.is_empty());
    }

    #[test]
    fn other_space_reads_volume_and_copies_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), vec![1u8; 4096]).unwrap();
        let opts = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        let mut t = scan(dir.path(), &opts, &ScanProgress::default()).unwrap();
        t.errors.push(ScanError {
            path: dir.path().join("locked"),
            reason: "denied".into(),
        });
        let o = other_space(&t).unwrap();
        assert!(o.used > 0);
        assert_eq!(o.scanned, t.nodes[t.root].size);
        assert_eq!(o.unaccounted, o.used.saturating_sub(o.scanned));
        assert_eq!(o.unreadable.len(), 1);
        assert!(o.system_files.is_empty());
    }

    #[test]
    fn other_space_marks_scanned_system_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hiberfil.sys"), vec![1u8; 4096]).unwrap();
        let opts = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        let t = scan(dir.path(), &opts, &ScanProgress::default()).unwrap();
        let o = other_space(&t).unwrap();
        assert_eq!(o.system_files.len(), 1);
        assert!(o.system_files[0].in_scan);
        assert_eq!(o.system_files[0].size, Some(4096));
    }
}
