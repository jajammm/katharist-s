//! Duplicate files. A group names one survivor (`keep`); the rest are
//! candidates for deletion, so the survivor must never be lost.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::analysis::reachable_files;
use crate::scan::{is_cloud_only, EntryClass, ScanProgress, ScanTree};
use crate::win;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateGroup {
    /// Logical length in bytes of each copy.
    pub size: u64,
    pub keep: PathBuf,
    pub others: Vec<PathBuf>,
}

type Hash = [u8; 32];
type FileId = (u64, u64);

/// Logical length of a regular file, without opening it.
fn logical_len(p: &Path) -> Option<u64> {
    let md = std::fs::symlink_metadata(p).ok()?;
    md.is_file().then_some(md.len())
}

/// Streaming SHA-256. `None` if unreadable or cancelled mid-file.
fn hash_file(p: &Path, progress: Option<&ScanProgress>) -> Option<Hash> {
    let mut f = File::open(p).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if progress.is_some_and(ScanProgress::is_cancelled) {
            return None;
        }
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hasher.finalize().into())
}

/// Survivor order: shortest path, then alphabetical.
fn by_keep_order(a: &Path, b: &Path) -> std::cmp::Ordering {
    a.as_os_str()
        .len()
        .cmp(&b.as_os_str().len())
        .then_with(|| a.cmp(b))
}

/// Groups files of equal content. Returns nothing when cancelled, since a
/// half-hashed set must not decide what is deleted.
pub fn find_duplicates(
    tree: &ScanTree,
    min_bytes: u64,
    progress: &ScanProgress,
) -> Vec<DuplicateGroup> {
    let mut by_size: BTreeMap<u64, Vec<(PathBuf, FileId)>> = BTreeMap::new();
    for id in reachable_files(tree) {
        if progress.is_cancelled() {
            return Vec::new();
        }
        let node = &tree.nodes[id];
        if node.cloud || node.class != EntryClass::File {
            continue;
        }
        let path = tree.path_of(id);
        let Some(len) = logical_len(&path) else {
            continue;
        };
        if len == 0 || len < min_bytes {
            continue;
        }
        let Ok((volume, file_id, _)) = win::link_info(&path) else {
            continue;
        };
        by_size
            .entry(len)
            .or_default()
            .push((path, (volume, file_id)));
    }

    let mut groups = Vec::new();
    for (size, mut entries) in by_size {
        entries.sort_by(|a, b| by_keep_order(&a.0, &b.0));
        let mut seen_ids = HashSet::new();
        let paths: Vec<PathBuf> = entries
            .into_iter()
            .filter(|(_, id)| seen_ids.insert(*id))
            .map(|(p, _)| p)
            .collect();
        if paths.len() < 2 {
            continue;
        }
        let mut by_hash: BTreeMap<Hash, Vec<PathBuf>> = BTreeMap::new();
        for path in paths {
            // The tree may be hours old: a file dehydrated since the scan would be
            // downloaded by reading it.
            if !may_read(attrs_of(&path)) {
                continue;
            }
            if let Some(h) = hash_file(&path, Some(progress)) {
                by_hash.entry(h).or_default().push(path);
            }
        }
        if progress.is_cancelled() {
            return Vec::new();
        }
        for (_, mut members) in by_hash {
            if members.len() < 2 {
                continue;
            }
            members.sort_by(|a, b| by_keep_order(a, b));
            let keep = members.remove(0);
            members.sort();
            groups.push(DuplicateGroup {
                size,
                keep,
                others: members,
            });
        }
    }
    groups.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.keep.cmp(&b.keep)));
    groups
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (win::link_info(a), win::link_info(b)) {
        (Ok((va, ia, _)), Ok((vb, ib, _))) => (va, ia) == (vb, ib),
        _ => false,
    }
}

/// The file attributes of `p` itself, read from its directory entry and not from
/// its content, so asking never recalls a cloud file.
fn attrs_of(p: &Path) -> Option<u32> {
    win::entry_info(p).ok().map(|e| e.attrs)
}

/// A file may be read only when its attributes are known and it is not cloud-only.
/// Reading a cloud-only file would download it.
fn may_read(attrs: Option<u32>) -> bool {
    attrs.is_some_and(|a| !is_cloud_only(a))
}

/// Re-reads the disk right before deleting `selected`. A keeper or selected file
/// that is now cloud-only fails without being read. Otherwise true only if `keep`
/// still exists with the group's length, is not itself selected (by path or
/// as the same file), and every selected file is still byte-identical to it.
pub fn targets_still_safe(group: &DuplicateGroup, selected: &[PathBuf]) -> bool {
    // Before any byte is read: a file that went cloud-only since the scan is not
    // safe, and checking it must not download it.
    if !may_read(attrs_of(&group.keep)) || !selected.iter().all(|s| may_read(attrs_of(s))) {
        return false;
    }
    if logical_len(&group.keep) != Some(group.size) {
        return false;
    }
    let Some(keep_hash) = hash_file(&group.keep, None) else {
        return false;
    };
    for s in selected {
        if s == &group.keep || same_file(s, &group.keep) {
            return false;
        }
        if logical_len(s) != Some(group.size) {
            return false;
        }
        if hash_file(s, None) != Some(keep_hash) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{large_files, stale_files};
    use crate::scan::{scan, EntryClass, Node, ScanOptions};
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn do_scan(root: &Path) -> ScanTree {
        let o = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        scan(root, &o, &ScanProgress::default()).unwrap()
    }

    fn write(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, data).unwrap();
        p
    }

    fn find(tree: &ScanTree, name: &str) -> usize {
        (0..tree.nodes.len())
            .find(|&i| tree.nodes[i].name == name)
            .unwrap()
    }

    #[test]
    fn large_and_stale_filter_and_sort() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "small.bin", &[1; 10]);
        write(d.path(), "mid.bin", &vec![2; 20_000]);
        write(d.path(), "big.bin", &vec![3; 90_000]);
        let mut tree = do_scan(d.path());
        let big = large_files(&tree, 10_000);
        let names: Vec<_> = big.iter().map(|&i| tree.nodes[i].name.as_str()).collect();
        assert_eq!(names, ["big.bin", "mid.bin"]);

        let now = SystemTime::now();
        let year = Duration::from_secs(365 * 86_400);
        let (mid, big_id, small) = (
            find(&tree, "mid.bin"),
            find(&tree, "big.bin"),
            find(&tree, "small.bin"),
        );
        tree.nodes[mid].modified = Some(now - year * 3);
        tree.nodes[big_id].modified = Some(now - year * 2);
        tree.nodes[small].modified = None;
        let root = tree.root;
        tree.nodes[root].modified = Some(now - year * 5);
        let stale = stale_files(&tree, year * 2 - Duration::from_secs(1), now);
        assert_eq!(stale, vec![mid, big_id]);
        assert!(stale_files(&tree, year * 4, now).is_empty());
    }

    #[test]
    fn duplicates_group_by_content_not_name() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        let a = write(d.path(), "a.txt", &[7; 5000]);
        let b = write(&d.path().join("sub"), "different_name.dat", &[7; 5000]);
        let c = write(d.path(), "zzzzzz_copy.txt", &[7; 5000]);
        let tree = do_scan(d.path());
        let groups = find_duplicates(&tree, 1000, &ScanProgress::default());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].size, 5000);
        assert_eq!(groups[0].keep, a);
        let mut expect = vec![b, c];
        expect.sort();
        assert_eq!(groups[0].others, expect);
    }

    #[test]
    fn same_size_different_content_not_grouped() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a", &[1; 4000]);
        write(d.path(), "b", &[2; 4000]);
        let tree = do_scan(d.path());
        assert!(find_duplicates(&tree, 1, &ScanProgress::default()).is_empty());
    }

    #[test]
    fn min_bytes_and_empty_files_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a", &[1; 100]);
        write(d.path(), "b", &[1; 100]);
        write(d.path(), "e1", &[]);
        write(d.path(), "e2", &[]);
        let tree = do_scan(d.path());
        assert!(find_duplicates(&tree, 101, &ScanProgress::default()).is_empty());
        let g = find_duplicates(&tree, 0, &ScanProgress::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].size, 100);
    }

    #[test]
    fn hard_links_are_not_duplicates() {
        let d = tempfile::tempdir().unwrap();
        let a = write(d.path(), "a", &[5; 4000]);
        fs::hard_link(&a, d.path().join("b")).unwrap();
        let tree = do_scan(d.path());
        assert!(find_duplicates(&tree, 1, &ScanProgress::default()).is_empty());
        write(d.path(), "c", &[5; 4000]);
        let tree = do_scan(d.path());
        let g = find_duplicates(&tree, 1, &ScanProgress::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].others.len(), 1);
    }

    #[test]
    fn cloud_files_are_never_read() {
        let d = tempfile::tempdir().unwrap();
        let mut tree = do_scan(d.path());
        let root = tree.root;
        for name in ["ghost1.bin", "ghost2.bin"] {
            tree.nodes.push(Node {
                name: name.into(),
                size: 4000,
                modified: None,
                class: EntryClass::File,
                cloud: true,
                parent: Some(root),
                children: Vec::new(),
            });
            let id = tree.nodes.len() - 1;
            tree.nodes[root].children.push(id);
        }
        assert!(find_duplicates(&tree, 1, &ScanProgress::default()).is_empty());
    }

    #[test]
    fn a_file_gone_cloud_only_since_the_scan_is_not_read() {
        let d = tempfile::tempdir().unwrap();
        let a = write(d.path(), "a", &[9; 3000]);
        let b = write(d.path(), "bb", &[9; 3000]);
        let gone = write(d.path(), "ccc", &[9; 3000]);
        let tree = do_scan(d.path());
        // The tree still says it is local. FILE_ATTRIBUTE_OFFLINE is one of the
        // attributes `is_cloud_only` reads, and an ordinary file can carry it.
        let wide = win::to_long_path(&gone);
        unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(0x1000),
            )
        }
        .unwrap();
        let g = find_duplicates(&tree, 1, &ScanProgress::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].keep, a);
        assert_eq!(g[0].others, [b]);
    }

    #[test]
    fn keep_is_never_in_others() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..5 {
            write(d.path(), &format!("f{i}"), &[9; 3000]);
        }
        let tree = do_scan(d.path());
        let g = find_duplicates(&tree, 1, &ScanProgress::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].others.len(), 4);
        assert!(!g[0].others.contains(&g[0].keep));
        let mut sorted = g[0].others.clone();
        sorted.sort();
        assert_eq!(sorted, g[0].others);
    }

    #[test]
    fn vanished_files_are_left_out() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a", &[3; 3000]);
        write(d.path(), "b", &[3; 3000]);
        let gone = write(d.path(), "c", &[3; 3000]);
        let tree = do_scan(d.path());
        fs::remove_file(gone).unwrap();
        let g = find_duplicates(&tree, 1, &ScanProgress::default());
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].others.len(), 1);
    }

    #[test]
    fn stops_being_safe_when_keeper_changes() {
        let d = tempfile::tempdir().unwrap();
        let a = write(d.path(), "a", &[4; 3000]);
        let b = write(d.path(), "bb", &[4; 3000]);
        let tree = do_scan(d.path());
        let g = &find_duplicates(&tree, 1, &ScanProgress::default())[0];
        assert_eq!(g.keep, a);
        assert!(targets_still_safe(g, std::slice::from_ref(&b)));
        assert!(!targets_still_safe(g, &[a.clone(), b.clone()]));
        fs::write(&a, [5; 3000]).unwrap();
        assert!(!targets_still_safe(g, std::slice::from_ref(&b)));
        fs::write(&a, [4; 3000]).unwrap();
        assert!(targets_still_safe(g, std::slice::from_ref(&b)));
        fs::write(&b, [6; 3000]).unwrap();
        assert!(!targets_still_safe(g, std::slice::from_ref(&b)));
        fs::write(&b, [4; 3000]).unwrap();
        fs::write(&a, [4; 10]).unwrap();
        assert!(!targets_still_safe(g, std::slice::from_ref(&b)));
        fs::remove_file(&a).unwrap();
        assert!(!targets_still_safe(g, std::slice::from_ref(&b)));
    }

    #[test]
    fn vanished_selection_is_not_safe() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a", &[4; 3000]);
        let b = write(d.path(), "bb", &[4; 3000]);
        let tree = do_scan(d.path());
        let g = &find_duplicates(&tree, 1, &ScanProgress::default())[0];
        fs::remove_file(&b).unwrap();
        assert!(!targets_still_safe(g, &[b]));
    }

    #[test]
    fn keeper_alias_by_hard_link_is_not_safe() {
        let d = tempfile::tempdir().unwrap();
        let a = write(d.path(), "a", &[4; 3000]);
        write(d.path(), "bb", &[4; 3000]);
        let alias = d.path().join("alias");
        fs::hard_link(&a, &alias).unwrap();
        let tree = do_scan(d.path());
        let g = &find_duplicates(&tree, 1, &ScanProgress::default())[0];
        assert!(!targets_still_safe(g, &[alias]));
    }

    #[test]
    fn cloud_only_or_unknown_attributes_are_never_read() {
        assert!(may_read(Some(0x20)));
        assert!(!may_read(Some(0x40_0000)));
        assert!(!may_read(Some(0x4_0000)));
        assert!(!may_read(Some(0x1000)));
        assert!(!may_read(None));
    }

    #[test]
    fn local_files_pass_the_attribute_gate() {
        let d = tempfile::tempdir().unwrap();
        let a = write(d.path(), "a", &[4; 3000]);
        assert!(may_read(attrs_of(&a)));
        assert!(!may_read(attrs_of(&d.path().join("missing"))));
    }

    #[test]
    fn cancel_stops_hashing() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a", &[1; 3000]);
        write(d.path(), "b", &[1; 3000]);
        let tree = do_scan(d.path());
        assert_eq!(find_duplicates(&tree, 1, &ScanProgress::default()).len(), 1);
        let p = ScanProgress::default();
        p.cancel();
        assert!(find_duplicates(&tree, 1, &p).is_empty());
    }
}
