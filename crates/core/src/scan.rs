//! Parallel disk scan into an arena tree. Nothing here reads file contents, and
//! name-surrogate reparse points (junctions, symlinks, mount points) are
//! recorded but never entered, so a scan stays on one tree and one volume.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rayon::prelude::*;

use crate::protect::canonical_key;
use crate::win::{self, DirEntry};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
const NAME_SURROGATE_TAG_BIT: u32 = 0x2000_0000;
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryClass {
    Dir,
    File,
    Link,
}

/// `Link` for name-surrogate reparse points. Cloud tags are not name
/// surrogates, so cloud folders and files are still scanned.
pub fn classify(attrs: u32, tag: u32) -> EntryClass {
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE_TAG_BIT != 0 {
        EntryClass::Link
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        EntryClass::Dir
    } else {
        EntryClass::File
    }
}

pub fn is_cloud_only(attrs: u32) -> bool {
    attrs
        & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_OFFLINE)
        != 0
}

pub type NodeId = usize;

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub class: EntryClass,
    pub cloud: bool,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
}

#[derive(Clone, Debug)]
pub struct ScanError {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct ScanTree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
    pub root_path: PathBuf,
    pub errors: Vec<ScanError>,
    pub files: u64,
    pub dirs: u64,
    pub cancelled: bool,
}

impl ScanTree {
    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut names = Vec::new();
        let mut cur = id;
        while let Some(parent) = self.nodes[cur].parent {
            names.push(self.nodes[cur].name.as_str());
            cur = parent;
        }
        let mut path = self.root_path.clone();
        path.extend(names.iter().rev());
        path
    }

    pub fn children_by_size(&self, id: NodeId) -> Vec<NodeId> {
        let mut kids = self.nodes[id].children.clone();
        kids.sort_by(|&a, &b| {
            let (a, b) = (&self.nodes[a], &self.nodes[b]);
            b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name))
        });
        kids
    }

    /// Detaches `id` from its parent and subtracts its size from every
    /// ancestor. Node ids stay valid. The root cannot be removed.
    pub fn remove_subtree(&mut self, id: NodeId) {
        let Some(parent) = self.nodes[id].parent else {
            return;
        };
        let size = self.nodes[id].size;
        let (mut files, mut dirs) = (0, 0);
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            match self.nodes[cur].class {
                EntryClass::File => files += 1,
                EntryClass::Dir => dirs += 1,
                EntryClass::Link => {}
            }
            stack.extend(&self.nodes[cur].children);
        }
        self.nodes[parent].children.retain(|&c| c != id);
        self.nodes[id].parent = None;
        let mut up = Some(parent);
        while let Some(a) = up {
            self.nodes[a].size = self.nodes[a].size.saturating_sub(size);
            up = self.nodes[a].parent;
        }
        self.files = self.files.saturating_sub(files);
        self.dirs = self.dirs.saturating_sub(dirs);
    }
}

pub struct ScanOptions {
    pub threads: usize,
    pub excluded: Vec<PathBuf>,
}

#[derive(Default)]
struct ProgressInner {
    cancelled: AtomicBool,
    files: AtomicU64,
    bytes: AtomicU64,
}

#[derive(Clone, Default)]
pub struct ScanProgress(Arc<ProgressInner>);

impl ScanProgress {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed)
    }
    pub fn files(&self) -> u64 {
        self.0.files.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.0.bytes.load(Ordering::Relaxed)
    }
}

/// A finished subtree, built by a worker and merged into the arena at the end.
struct Sub {
    name: String,
    size: u64,
    modified: Option<SystemTime>,
    class: EntryClass,
    cloud: bool,
    children: Vec<Sub>,
}

struct Ctx<'a> {
    excluded: HashSet<String>,
    seen_links: Mutex<HashSet<(u64, u64)>>,
    errors: Mutex<Vec<ScanError>>,
    progress: &'a ScanProgress,
}

impl Ctx<'_> {
    fn record(&self, path: &Path, err: &io::Error) {
        self.errors.lock().unwrap().push(ScanError {
            path: path.to_path_buf(),
            reason: err.to_string(),
        });
    }

    fn is_excluded(&self, path: &Path) -> bool {
        !self.excluded.is_empty()
            && canonical_key(path).is_some_and(|key| self.excluded.contains(&key))
    }

    /// Size of a file, counting a hard-linked file only the first time it is seen.
    fn file_size(&self, path: &Path, cloud: bool) -> u64 {
        let size = match win::allocated_size(path) {
            Ok(size) => size,
            Err(e) => {
                self.record(path, &e);
                return 0;
            }
        };
        if cloud {
            return size;
        }
        match win::link_info(path) {
            Ok((volume, id, links)) if links > 1 => {
                if self.seen_links.lock().unwrap().insert((volume, id)) {
                    size
                } else {
                    0
                }
            }
            _ => size,
        }
    }

    fn entry(&self, dir: &Path, e: DirEntry) -> Option<Sub> {
        let path = dir.join(&e.name);
        if self.is_excluded(&path) {
            return None;
        }
        let class = classify(e.attrs, e.reparse_tag);
        let cloud = is_cloud_only(e.attrs);
        let mut sub = Sub {
            name: e.name,
            size: 0,
            modified: e.modified,
            class,
            cloud,
            children: Vec::new(),
        };
        match class {
            EntryClass::Link => {}
            EntryClass::File => {
                sub.size = self.file_size(&path, cloud);
                self.progress.0.files.fetch_add(1, Ordering::Relaxed);
                self.progress.0.bytes.fetch_add(sub.size, Ordering::Relaxed);
            }
            EntryClass::Dir => match self.children(&path) {
                Ok(children) => sub.absorb(children),
                Err(e) => self.record(&path, &e),
            },
        }
        Some(sub)
    }

    fn children(&self, dir: &Path) -> io::Result<Vec<Sub>> {
        let entries = win::list_dir(dir)?;
        if self.progress.is_cancelled() {
            return Ok(Vec::new());
        }
        Ok(entries
            .into_par_iter()
            .filter_map(|e| self.entry(dir, e))
            .collect())
    }
}

impl Sub {
    /// Takes the children and folds their size and newest date into this folder.
    fn absorb(&mut self, children: Vec<Sub>) {
        for c in &children {
            self.size += c.size;
            self.modified = self.modified.max(c.modified);
        }
        self.children = children;
    }
}

fn flatten(sub: Sub, parent: Option<NodeId>, nodes: &mut Vec<Node>) -> NodeId {
    let id = nodes.len();
    nodes.push(Node {
        name: sub.name,
        size: sub.size,
        modified: sub.modified,
        class: sub.class,
        cloud: sub.cloud,
        parent,
        children: Vec::new(),
    });
    let kids = sub
        .children
        .into_iter()
        .map(|c| flatten(c, Some(id), nodes))
        .collect();
    nodes[id].children = kids;
    id
}

/// Scans `root`. An unreadable or missing root is an `Err`; folders that fail
/// further down are listed in `errors`. A cancelled scan returns what it has.
pub fn scan(root: &Path, opts: &ScanOptions, progress: &ScanProgress) -> io::Result<ScanTree> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads)
        .stack_size(WORKER_STACK_SIZE)
        .build()
        .map_err(io::Error::other)?;
    let ctx = Ctx {
        excluded: opts
            .excluded
            .iter()
            .filter_map(|p| canonical_key(p))
            .collect(),
        seen_links: Mutex::new(HashSet::new()),
        errors: Mutex::new(Vec::new()),
        progress,
    };
    let name = root
        .file_name()
        .unwrap_or(root.as_os_str())
        .to_string_lossy()
        .into_owned();
    pool.install(|| {
        let children = ctx.children(root)?;
        let mut top = Sub {
            name,
            size: 0,
            modified: None,
            class: EntryClass::Dir,
            cloud: false,
            children: Vec::new(),
        };
        top.absorb(children);
        let mut nodes = Vec::new();
        let root_id = flatten(top, None, &mut nodes);
        let count = |class| nodes.iter().filter(|n| n.class == class).count() as u64;
        let (files, dirs) = (count(EntryClass::File), count(EntryClass::Dir) - 1);
        Ok(ScanTree {
            nodes,
            root: root_id,
            root_path: root.to_path_buf(),
            errors: std::mem::take(&mut *ctx.errors.lock().unwrap()),
            files,
            dirs,
            cancelled: progress.is_cancelled(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    const MIB: usize = 1024 * 1024;

    fn opts() -> ScanOptions {
        ScanOptions {
            threads: 4,
            excluded: Vec::new(),
        }
    }

    fn run(root: &Path) -> ScanTree {
        scan(root, &opts(), &ScanProgress::default()).unwrap()
    }

    fn find(t: &ScanTree, parent: NodeId, name: &str) -> NodeId {
        *t.nodes[parent]
            .children
            .iter()
            .find(|&&c| t.nodes[c].name == name)
            .unwrap_or_else(|| panic!("no child {name}"))
    }

    #[test]
    fn classify_follows_cloud_but_not_links() {
        const R: u32 = 0x400;
        const D: u32 = 0x10;
        assert!(matches!(classify(D | R, 0xA0000003), EntryClass::Link));
        assert!(matches!(classify(R, 0xA000000C), EntryClass::Link));
        assert!(matches!(classify(D | R, 0x9000701A), EntryClass::Dir));
        assert!(matches!(classify(R, 0x9000001A), EntryClass::File));
    }

    #[test]
    fn cloud_only_flags() {
        assert!(is_cloud_only(0x400000));
        assert!(is_cloud_only(0x40000));
        assert!(is_cloud_only(0x1000));
        assert!(!is_cloud_only(0x20));
    }

    #[test]
    fn sums_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir(&a).unwrap();
        fs::write(a.join("big"), vec![1u8; MIB]).unwrap();
        fs::write(a.join("small"), vec![1u8; 1024]).unwrap();
        let t = run(dir.path());
        let a_id = find(&t, t.root, "a");
        assert!(t.nodes[a_id].size >= MIB as u64);
        assert_eq!(t.nodes[t.root].size, t.nodes[a_id].size);
        let order = t.children_by_size(a_id);
        let names: Vec<_> = order.iter().map(|&i| t.nodes[i].name.as_str()).collect();
        assert_eq!(names, ["big", "small"]);
        assert_eq!(t.files, 2);
        assert_eq!(t.dirs, 1);
        assert!(!t.cancelled);
    }

    #[test]
    fn hard_link_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let single = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one"), vec![1u8; MIB]).unwrap();
        fs::hard_link(dir.path().join("one"), dir.path().join("two")).unwrap();
        fs::write(single.path().join("one"), vec![1u8; MIB]).unwrap();
        let linked = run(dir.path());
        let alone = run(single.path());
        assert_eq!(linked.nodes[linked.root].size, alone.nodes[alone.root].size);
        assert!(linked.nodes[linked.root].size >= MIB as u64);
    }

    /// Removes the junction itself, never its target.
    struct JunctionGuard(PathBuf);
    impl Drop for JunctionGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }

    fn mklink_junction(link: &Path, target: &Path) -> JunctionGuard {
        let out = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "mklink failed");
        JunctionGuard(link.to_path_buf())
    }

    #[test]
    fn junction_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f"), vec![1u8; MIB]).unwrap();
        let scanned = dir.path().join("scanned");
        fs::create_dir(&scanned).unwrap();
        let _guard = mklink_junction(&scanned.join("j"), &target);
        let t = run(&scanned);
        let j = find(&t, t.root, "j");
        assert_eq!(t.nodes[j].size, 0);
        assert!(matches!(t.nodes[j].class, EntryClass::Link));
        assert!(t.nodes[j].children.is_empty());
        assert_eq!(t.nodes[t.root].size, 0);
    }

    /// Lifts the deny rule even when the assertion fails, so the temp dir can be removed.
    struct DenyGuard(PathBuf);
    impl Drop for DenyGuard {
        fn drop(&mut self) {
            let _ = Command::new("icacls")
                .arg(&self.0)
                .args(["/remove:d", "Everyone"])
                .output();
        }
    }

    #[test]
    fn unreadable_folder_is_recorded_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden"), b"x").unwrap();
        fs::write(dir.path().join("ok"), vec![1u8; 4096]).unwrap();
        let _guard = DenyGuard(locked.clone());
        let out = Command::new("icacls")
            .arg(&locked)
            .args(["/deny", "Everyone:(OI)(CI)(RD,DC)"])
            .output()
            .unwrap();
        assert!(out.status.success(), "icacls failed");
        let t = run(dir.path());
        assert_eq!(t.errors.len(), 1);
        assert!(t.errors[0].path.ends_with("locked"));
        assert!(t.nodes[t.root].size >= 4096);
        assert_eq!(t.files, 1);
    }

    #[test]
    fn folder_date_is_newest_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        let file = deep.join("f");
        fs::write(&file, b"x").unwrap();
        let when = SystemTime::now() + Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(when)
            .unwrap();
        let t = run(dir.path());
        let a = find(&t, t.root, "a");
        let b = find(&t, a, "b");
        let f = find(&t, b, "f");
        let want = t.nodes[f].modified.unwrap();
        assert_eq!(t.nodes[t.root].modified, Some(want));
        assert_eq!(t.nodes[a].modified, Some(want));
        let ahead = want.duration_since(SystemTime::now()).unwrap();
        assert!(ahead > Duration::from_secs(3000));
    }

    #[test]
    fn long_path_over_260() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = PathBuf::from(format!(r"\\?\{}", dir.path().display()));
        for _ in 0..6 {
            deep.push("d".repeat(50));
        }
        fs::create_dir_all(&deep).unwrap();
        assert!(deep.as_os_str().len() > 260);
        fs::write(deep.join("f"), vec![1u8; 4096]).unwrap();
        let t = run(dir.path());
        assert_eq!(t.files, 1);
        assert!(t.errors.is_empty());
        assert!(t.nodes[t.root].size >= 4096);
    }

    #[test]
    fn excluded_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("skip")).unwrap();
        fs::write(dir.path().join("skip").join("f"), vec![1u8; MIB]).unwrap();
        fs::write(dir.path().join("keep"), vec![1u8; 4096]).unwrap();
        let o = ScanOptions {
            threads: 2,
            excluded: vec![dir.path().join("SKIP")],
        };
        let t = scan(dir.path(), &o, &ScanProgress::default()).unwrap();
        assert!(t.nodes[t.root]
            .children
            .iter()
            .all(|&c| t.nodes[c].name != "skip"));
        assert!(t.nodes[t.root].size < MIB as u64);
        assert_eq!(t.files, 1);
    }

    #[test]
    fn cancel_returns_partial() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), b"x").unwrap();
        let p = ScanProgress::default();
        p.cancel();
        assert!(p.is_cancelled());
        let t = scan(dir.path(), &opts(), &p).unwrap();
        assert!(t.cancelled);
    }

    #[test]
    fn missing_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(scan(&missing, &opts(), &ScanProgress::default()).is_err());
    }

    #[test]
    fn progress_counts_files_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("f"), vec![1u8; MIB]).unwrap();
        let p = ScanProgress::default();
        scan(dir.path(), &opts(), &p).unwrap();
        assert_eq!(p.files(), 1);
        assert!(p.bytes() >= MIB as u64);
    }

    #[test]
    fn path_of_joins_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a").join("f"), b"x").unwrap();
        let t = run(dir.path());
        let f = find(&t, find(&t, t.root, "a"), "f");
        assert_eq!(t.path_of(f), dir.path().join("a").join("f"));
        assert_eq!(t.path_of(t.root), dir.path());
    }

    #[test]
    fn remove_subtree_updates_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir_all(a.join("b")).unwrap();
        fs::write(a.join("b").join("big"), vec![1u8; MIB]).unwrap();
        fs::write(a.join("keep"), vec![1u8; 4096]).unwrap();
        let mut t = run(dir.path());
        let a_id = find(&t, t.root, "a");
        let b_id = find(&t, a_id, "b");
        let (root_before, a_before, b_size) =
            (t.nodes[t.root].size, t.nodes[a_id].size, t.nodes[b_id].size);
        assert!(b_size >= MIB as u64);
        t.remove_subtree(b_id);
        assert_eq!(t.nodes[a_id].size, a_before - b_size);
        assert_eq!(t.nodes[t.root].size, root_before - b_size);
        assert!(!t.nodes[a_id].children.contains(&b_id));
        assert_eq!(t.files, 1);
        assert_eq!(t.dirs, 1);
    }
}
