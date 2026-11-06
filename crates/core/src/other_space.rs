//! Explains the gap between what a scan found and what the drive reports as
//! used. Reads attributes and sizes only and deletes nothing.

use std::io;
use std::path::Path;

use crate::scan::{classify, EntryClass, ScanError, ScanTree};
use crate::win;

#[derive(Debug)]
pub struct OtherSpace {
    pub used: u64,
    pub scanned: u64,
    pub unaccounted: u64,
    pub system_files: Vec<(String, u64)>,
    pub unreadable: Vec<ScanError>,
}

/// Items in a drive root that a scan cannot account for.
const SYSTEM_NAMES: [&str; 4] = [
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "System Volume Information",
];

/// For a scan of a drive root. Fails only if the volume cannot be queried;
/// a system item whose size cannot be read is listed with size 0.
pub fn other_space(tree: &ScanTree) -> io::Result<OtherSpace> {
    let (total, free) = win::volume_space(&tree.root_path)?;
    let used = total.saturating_sub(free);
    let scanned = tree.nodes[tree.root].size;
    let system_files = system_files_in(&tree.root_path);
    let unreadable = tree
        .errors
        .iter()
        .map(|e| ScanError {
            path: e.path.clone(),
            reason: e.reason.clone(),
        })
        .collect();
    Ok(assemble(used, scanned, system_files, unreadable))
}

fn assemble(
    used: u64,
    scanned: u64,
    system_files: Vec<(String, u64)>,
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

/// Which of the well-known system items exist in `root`, with their size,
/// biggest first. Names are matched ignoring case and reported as found on disk.
fn system_files_in(root: &Path) -> Vec<(String, u64)> {
    let Ok(entries) = win::list_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<(String, u64)> = entries
        .into_iter()
        .filter(|e| SYSTEM_NAMES.iter().any(|n| n.eq_ignore_ascii_case(&e.name)))
        .map(|e| {
            let size = entry_size(root, &e);
            (e.name, size)
        })
        .collect();
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    found
}

/// Size of one entry; anything that cannot be read counts as 0.
fn entry_size(dir: &Path, e: &win::DirEntry) -> u64 {
    let path = dir.join(&e.name);
    match classify(e.attrs, e.reparse_tag) {
        EntryClass::File => win::allocated_size(&path).unwrap_or(0),
        EntryClass::Dir => win::list_dir(&path)
            .map(|entries| entries.iter().map(|c| entry_size(&path, c)).sum())
            .unwrap_or(0),
        EntryClass::Link => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanOptions, ScanProgress};
    use std::fs;
    use std::path::PathBuf;

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
        let o = assemble(10, 5, vec![("pagefile.sys".into(), 3)], vec![err]);
        assert_eq!(o.system_files, [("pagefile.sys".to_string(), 3)]);
        assert_eq!(o.unreadable.len(), 1);
        assert_eq!(o.unreadable[0].reason, "Access is denied.");
    }

    #[test]
    fn finds_system_items_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("PageFile.sys"), vec![1u8; 4096]).unwrap();
        fs::write(dir.path().join("other.txt"), b"x").unwrap();
        let svi = dir.path().join("System Volume Information");
        fs::create_dir(&svi).unwrap();
        fs::write(svi.join("f"), vec![1u8; 100]).unwrap();
        let found = system_files_in(dir.path());
        assert_eq!(
            found,
            [
                ("PageFile.sys".to_string(), 4096),
                ("System Volume Information".to_string(), 100)
            ]
        );
        assert!(system_files_in(&dir.path().join("missing")).is_empty());
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
}
