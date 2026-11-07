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
use crate::win::{self, ScanEntry};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
const NAME_SURROGATE_TAG_BIT: u32 = 0x2000_0000;
const WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;
const SEEN_SHARDS: usize = 64;

/// Size on disk from what the directory reports: the length for a plain file,
/// what is allocated for a compressed, sparse or cloud one. `None` when a file
/// with data reports no allocation (seen on WOF-compressed files), so only those
/// have to be asked about one by one. Cloud files never are, so nothing recalls.
fn size_from_directory(attrs: u32, logical: u64, allocated: u64) -> Option<u64> {
    if allocated == 0 && logical > 0 && !is_cloud_only(attrs) {
        return None;
    }
    Some(allocated.min(logical))
}

/// How a folder is listed. Tests swap it to cover the plain listing.
type Listing = fn(&Path) -> io::Result<Vec<ScanEntry>>;

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
    listing: Listing,
    seen_ids: Vec<Mutex<HashSet<[u8; 16]>>>,
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

    /// True for a listed name under `dir`. Only the folder needs resolving, and a
    /// listed name is already the long name, so an exclusion costs nothing per file.
    fn is_excluded(&self, dir: &Path, dir_key: Option<&str>, name: &str) -> bool {
        if self.excluded.is_empty() {
            return false;
        }
        // Win32 drops trailing dots and spaces, so only the full rule fits those.
        let plain = !name.ends_with(['.', ' ']);
        match dir_key {
            Some(key) if plain => {
                let key = format!("{}\\{}", key.trim_end_matches('\\'), name.to_lowercase());
                self.excluded.contains(&key)
            }
            _ => canonical_key(&dir.join(name)).is_some_and(|key| self.excluded.contains(&key)),
        }
    }

    /// True the first time a file id is seen. No id means no hard links to count.
    fn first_sighting(&self, id: [u8; 16]) -> bool {
        if id == [0; 16] {
            return true;
        }
        let shard = usize::from(id[0]) ^ (usize::from(id[1]) << 3);
        self.seen_ids[shard % SEEN_SHARDS]
            .lock()
            .unwrap()
            .insert(id)
    }

    /// Size of a file, counting a hard-linked file only the first time it is seen.
    fn file_size(&self, dir: &Path, e: &ScanEntry, cloud: bool) -> u64 {
        let Some(info) = e.sized else {
            return self.file_size_by_opening(&dir.join(&e.name), cloud);
        };
        if !self.first_sighting(info.id) {
            return 0;
        }
        match size_from_directory(e.attrs, info.logical, info.allocated) {
            Some(size) => size,
            None => {
                let path = dir.join(&e.name);
                win::allocated_size(&path).unwrap_or_else(|err| {
                    self.record(&path, &err);
                    0
                })
            }
        }
    }

    /// For a file system whose listing carries no sizes.
    fn file_size_by_opening(&self, path: &Path, cloud: bool) -> u64 {
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

    fn entry(&self, dir: &Path, e: ScanEntry) -> Sub {
        let class = classify(e.attrs, e.reparse_tag);
        let cloud = is_cloud_only(e.attrs);
        let size = match class {
            EntryClass::File => {
                let size = self.file_size(dir, &e, cloud);
                self.progress.0.files.fetch_add(1, Ordering::Relaxed);
                self.progress.0.bytes.fetch_add(size, Ordering::Relaxed);
                size
            }
            EntryClass::Dir | EntryClass::Link => 0,
        };
        let mut sub = Sub {
            name: e.name,
            size,
            modified: e.modified,
            class,
            cloud,
            children: Vec::new(),
        };
        if class == EntryClass::Dir {
            let path = dir.join(&sub.name);
            match self.children(&path) {
                Ok(children) => sub.absorb(children),
                Err(e) => self.record(&path, &e),
            }
        }
        sub
    }

    fn children(&self, dir: &Path) -> io::Result<Vec<Sub>> {
        let entries = (self.listing)(dir)?;
        if self.progress.is_cancelled() {
            return Ok(Vec::new());
        }
        let dir_key = (!self.excluded.is_empty())
            .then(|| canonical_key(dir))
            .flatten();
        let (folders, others): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .filter(|e| !self.is_excluded(dir, dir_key.as_deref(), &e.name))
            .partition(|e| classify(e.attrs, e.reparse_tag) == EntryClass::Dir);
        // Nothing here opens a file, so only folders are worth a thread.
        let mut subs: Vec<Sub> = folders
            .into_par_iter()
            .map(|e| self.entry(dir, e))
            .collect();
        subs.extend(others.into_iter().map(|e| self.entry(dir, e)));
        Ok(subs)
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
    scan_with(root, opts, progress, win::list_dir_sized)
}

fn scan_with(
    root: &Path,
    opts: &ScanOptions,
    progress: &ScanProgress,
    listing: Listing,
) -> io::Result<ScanTree> {
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
        listing,
        seen_ids: (0..SEEN_SHARDS)
            .map(|_| Mutex::new(HashSet::new()))
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

    fn plain_listing(p: &Path) -> io::Result<Vec<ScanEntry>> {
        win::list_dir_plain(p)
    }

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
    fn size_comes_from_the_directory_unless_it_cannot_tell() {
        const NORMAL: u32 = 0x20;
        const COMPRESSED: u32 = 0x800;
        const CLOUD: u32 = 0x400000;
        // Plain file: the length, not the length rounded up to a cluster.
        assert_eq!(size_from_directory(NORMAL, 100, 4096), Some(100));
        assert_eq!(size_from_directory(NORMAL, 0, 0), Some(0));
        // Compressed or sparse: what is allocated.
        assert_eq!(size_from_directory(COMPRESSED, 8192, 4096), Some(4096));
        // Cloud placeholder: nothing is on disk, and it must not be opened.
        assert_eq!(size_from_directory(CLOUD, 1 << 30, 0), Some(0));
        // Data with no allocation reported: the directory cannot tell.
        assert_eq!(size_from_directory(NORMAL, 8192, 0), None);
    }

    fn set_up_mixed_tree(root: &Path) {
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("one"), vec![1u8; MIB]).unwrap();
        fs::hard_link(sub.join("one"), root.join("two")).unwrap();
        fs::write(root.join("small"), vec![1u8; 300]).unwrap();
        fs::write(root.join("empty"), b"").unwrap();
    }

    #[test]
    fn both_listings_agree() {
        let dir = tempfile::tempdir().unwrap();
        set_up_mixed_tree(dir.path());
        let sized = run(dir.path());
        let plain =
            scan_with(dir.path(), &opts(), &ScanProgress::default(), plain_listing).unwrap();
        assert_eq!(sized.nodes[sized.root].size, plain.nodes[plain.root].size);
        assert_eq!(sized.nodes[sized.root].size, MIB as u64 + 300);
        assert_eq!((sized.files, sized.dirs), (plain.files, plain.dirs));
        assert_eq!((sized.files, sized.dirs), (4, 1));
        assert!(sized.errors.is_empty() && plain.errors.is_empty());
        let date = |t: &ScanTree| t.nodes[find(t, t.root, "small")].modified;
        assert_eq!(date(&sized), date(&plain));
        assert!(date(&sized).is_some());
    }

    #[test]
    fn a_compressed_file_counts_what_it_occupies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("packed");
        fs::write(&path, vec![0u8; 4 * MIB]).unwrap();
        let done = Command::new("compact")
            .arg("/c")
            .arg(&path)
            .output()
            .unwrap();
        assert!(done.status.success());
        let truth = win::allocated_size(&path).unwrap();
        assert!(truth < 4 * MIB as u64, "not compressed: {truth}");
        let t = run(dir.path());
        assert_eq!(t.nodes[find(&t, t.root, "packed")].size, truth);
    }

    #[test]
    fn a_sparse_file_counts_what_it_occupies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse");
        fs::write(&path, b"").unwrap();
        let sparse = Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(&path)
            .output()
            .unwrap();
        assert!(sparse.status.success());
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1 << 30)
            .unwrap();
        let truth = win::allocated_size(&path).unwrap();
        let t = run(dir.path());
        assert_eq!(t.nodes[find(&t, t.root, "sparse")].size, truth);
        assert!(truth < 1 << 30);
    }

    #[test]
    fn exclusion_needs_no_exact_spelling_of_case() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("Keep").join("Skip Me");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("inside"), vec![1u8; 100]).unwrap();
        fs::write(dir.path().join("Keep").join("Skip Mea"), vec![1u8; 10]).unwrap();
        let shouting = PathBuf::from(deep.to_string_lossy().to_uppercase());
        let t = scan(
            dir.path(),
            &ScanOptions {
                threads: 2,
                excluded: vec![shouting],
            },
            &ScanProgress::default(),
        )
        .unwrap();
        assert_eq!(t.nodes[t.root].size, 10);
        let keep = find(&t, t.root, "Keep");
        assert_eq!(t.nodes[keep].children.len(), 1);
    }

    #[test]
    fn a_file_nobody_may_open_is_still_counted() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked");
        fs::write(&path, vec![1u8; MIB]).unwrap();
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let t = run(dir.path());
        assert_eq!(t.nodes[find(&t, t.root, "locked")].size, MIB as u64);
        assert!(t.errors.is_empty(), "{:?}", t.errors);
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
