//! The opening screen: the disk map, its list, and the way into a delete. Decisions
//! live in the small functions at the top so they can be tested; the drawing below
//! only calls them.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use egui::{Align, Layout, RichText, Sense};
use katharistis_core::apps::marks::UsageMarks;
use katharistis_core::apps::registry::list_installed;
use katharistis_core::delete::{DeleteMode, DeleteReport, Target};
use katharistis_core::format::format_size;
use katharistis_core::other_space::{other_space, OtherSpace, SystemItem};
use katharistis_core::protect::{canonical_key, is_protected, ProtectContext};
use katharistis_core::recycle::is_guard_name;
use katharistis_core::scan::{
    scan, EntryClass, Node, NodeId, ScanError, ScanOptions, ScanProgress, ScanTree,
};
use katharistis_core::treemap::{map_items, squarify, MapItem, Rect};
use katharistis_core::win;
use windows::Win32::Storage::FileSystem::GetLogicalDrives;

use crate::confirm::{failed_to_lock, map_may_be_stale, ConfirmContext, ConfirmRequest};
use crate::i18n::l;
use crate::jobs::{Job, JobPanicked};
use crate::theme::{self, TilePalette};

/// Tile share and count below which children are merged into "Lainnya".
const MIN_TILE_FRACTION: f32 = 0.005;
const MAX_TILES: usize = 40;
/// `is_protected` makes file system calls, so a frame computes only this many rows.
pub(crate) const FLAG_ROWS_PER_FRAME: usize = 6;
const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    Profile,
    ProgramFiles,
    Windows,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocationProblem {
    NotAbsolute,
}

/// The folder field's text as a path, `None` when empty. Only absolute paths pass:
/// `Games` or `D:Games` would mean whatever the current folder happens to be.
pub fn parse_location(text: &str) -> Result<Option<PathBuf>, LocationProblem> {
    let text = text.trim().trim_matches('"').trim();
    if text.is_empty() {
        return Ok(None);
    }
    let bytes = text.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Ok(Some(PathBuf::from(format!("{text}\\"))));
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Err(LocationProblem::NotAbsolute)
    }
}

/// What the Scan button scans: the folder if one is typed, otherwise the drive.
pub fn scan_target(drive: &str, folder: &str) -> Result<PathBuf, LocationProblem> {
    Ok(parse_location(folder)?.unwrap_or_else(|| PathBuf::from(drive)))
}

/// `C:\`, not a folder, not `C:` and not a network share.
pub fn is_drive_root(p: &Path) -> bool {
    let mut parts = p.components();
    let on_disk = matches!(
        parts.next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    );
    on_disk && parts.next() == Some(Component::RootDir) && parts.next().is_none()
}

/// The roots named by a `GetLogicalDrives` mask.
pub fn drive_letters(mask: u32) -> Vec<String> {
    (0..26u8)
        .filter(|i| mask >> i & 1 == 1)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileAction {
    Enter(NodeId),
    OtherSpace,
    FullList,
    Nothing,
}

/// What a click on a tile does. "Used minus scanned" only means something for a
/// whole drive, so "Lainnya" explains the gap only at the root of a drive scan.
pub fn tile_click(tree: &ScanTree, item: MapItem, at: NodeId) -> TileAction {
    match item.node {
        None if at == tree.root && is_drive_root(&tree.root_path) => TileAction::OtherSpace,
        None => TileAction::FullList,
        Some(id) if tree.nodes[id].class == EntryClass::Dir => TileAction::Enter(id),
        Some(_) => TileAction::Nothing,
    }
}

pub fn is_stale(modified: Option<SystemTime>, days: u32, now: SystemTime) -> bool {
    let limit = Duration::from_secs(u64::from(days) * SECONDS_PER_DAY);
    modified.is_some_and(|m| now.duration_since(m).is_ok_and(|age| age > limit))
}

/// `YYYY-MM-DD` in UTC, `-` when there is no date to show.
pub fn format_date(modified: Option<SystemTime>) -> String {
    let Some(age) = modified.and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok()) else {
        return "-".to_string();
    };
    // Days to civil date, after Howard Hinnant's `civil_from_days`.
    let z = (age.as_secs() / SECONDS_PER_DAY) as i64 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn lower_at_or_under(path_lower: &str, base: &Path) -> bool {
    key_at_or_under(path_lower, &base.to_string_lossy().to_lowercase())
}

/// Both already lowercase.
fn key_at_or_under(path_lower: &str, base_lower: &str) -> bool {
    let base = base_lower.trim_end_matches('\\');
    !base.is_empty()
        && path_lower
            .strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// How many `(path, is a folder)` targets may take more with them than the tree
/// counted: an excluded folder or an unreadable path is at or inside it, or the scan
/// was cancelled before it finished a folder. A file's own size was read whole, so a
/// cancelled scan says nothing about a file. `excluded` is lowercase keys.
pub fn count_holding_more(
    targets: &[(PathBuf, bool)],
    excluded: &[String],
    errors: &[ScanError],
    cancelled: bool,
) -> usize {
    let errors: Vec<String> = errors
        .iter()
        .map(|e| e.path.to_string_lossy().to_lowercase())
        .collect();
    targets
        .iter()
        .filter(|(path, is_folder)| {
            let target = path.to_string_lossy().to_lowercase();
            (*is_folder && cancelled)
                || excluded
                    .iter()
                    .chain(&errors)
                    .any(|missing| key_at_or_under(missing, &target))
        })
        .count()
}

/// The kind of place a path is in, for the map's colours. Says nothing about risk.
pub fn area_of(p: &Path, ctx: &ProtectContext) -> Area {
    let path = p.to_string_lossy().to_lowercase();
    if lower_at_or_under(&path, &ctx.windows_dir) {
        Area::Windows
    } else if ctx
        .program_files
        .iter()
        .any(|pf| lower_at_or_under(&path, pf))
    {
        Area::ProgramFiles
    } else if lower_at_or_under(&path, &ctx.profile) || lower_at_or_under(&path, &ctx.users_dir) {
        Area::Profile
    } else {
        Area::Other
    }
}

/// From the scanned location down to `at`.
pub fn breadcrumb(tree: &ScanTree, at: NodeId) -> Vec<(NodeId, String)> {
    let mut trail = Vec::new();
    let mut cur = at;
    while let Some(parent) = tree.nodes[cur].parent {
        trail.push((cur, tree.nodes[cur].name.clone()));
        cur = parent;
    }
    trail.push((cur, tree.root_path.display().to_string()));
    trail.reverse();
    trail
}

/// The node at a path. An exact name wins; Windows names are otherwise the same in
/// any case, and a report may spell a path differently from the scan.
pub fn find_node(tree: &ScanTree, p: &Path) -> Option<NodeId> {
    let lower = |c: Component<'_>| c.as_os_str().to_string_lossy().to_lowercase();
    let mut given = p.components();
    for part in tree.root_path.components() {
        if lower(given.next()?) != lower(part) {
            return None;
        }
    }
    let mut cur = tree.root;
    for part in given {
        let name = part.as_os_str().to_string_lossy();
        let kids = &tree.nodes[cur].children;
        let lowered = name.to_lowercase();
        cur = kids
            .iter()
            .find(|&&c| tree.nodes[c].name == name)
            .or_else(|| {
                kids.iter()
                    .find(|&&c| tree.nodes[c].name.to_lowercase() == lowered)
            })
            .copied()?;
    }
    Some(cur)
}

/// Takes what truly left off the tree: `removed` and `recycled`, never a failure.
/// Returns how many nodes came off.
pub fn apply_report(tree: &mut ScanTree, r: &DeleteReport) -> usize {
    let mut count = 0;
    for path in r.removed.iter().chain(&r.recycled) {
        if let Some(id) = find_node(tree, path).filter(|&id| id != tree.root) {
            tree.remove_subtree(id);
            count += 1;
        }
    }
    count
}

/// `at` while it is still in the tree, otherwise the root.
pub fn location_or_root(tree: &ScanTree, at: NodeId) -> NodeId {
    let mut cur = at;
    while let Some(parent) = tree.nodes[cur].parent {
        cur = parent;
    }
    if cur == tree.root {
        at
    } else {
        tree.root
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowFlags {
    pub protected: bool,
    pub app_owned: bool,
    /// A guard left by a recycle that went wrong; its data may be only here.
    pub guard: bool,
    /// A delete failed in or under this row, so its size may be wrong until a rescan.
    pub locked: bool,
}

/// A row gets a checkbox once it is known to be neither protected, a guard file nor
/// locked.
pub fn can_select(flags: Option<&RowFlags>) -> bool {
    flags.is_some_and(|f| !f.protected && !f.guard && !f.locked)
}

/// What a failed delete leaves unsafe to delete again before a rescan. A partial
/// delete makes the sizes of the failed path wrong, of everything inside it, and of
/// every folder above it, and those sizes are what a report counts as freed. Rows
/// elsewhere keep their true size. Node ids stay valid until the next scan.
#[derive(Debug, Default)]
pub struct Locks {
    failed: BTreeSet<NodeId>,
    above_failed: BTreeSet<NodeId>,
}

impl Locks {
    pub fn add_report(&mut self, tree: &ScanTree, report: &DeleteReport) {
        for id in failed_to_lock(report)
            .into_iter()
            .filter_map(|path| find_node(tree, path))
        {
            self.failed.insert(id);
            let mut up = tree.nodes[id].parent;
            while let Some(parent) = up {
                self.above_failed.insert(parent);
                up = tree.nodes[parent].parent;
            }
        }
    }

    pub fn is_locked(&self, tree: &ScanTree, id: NodeId) -> bool {
        if self.above_failed.contains(&id) {
            return true;
        }
        let mut cur = Some(id);
        while let Some(node) = cur {
            if self.failed.contains(&node) {
                return true;
            }
            cur = tree.nodes[node].parent;
        }
        false
    }
}

/// The confirmation for what is ticked. Anything that could not have a checkbox
/// stays out, whatever the selection says.
pub fn request_from_selection(
    tree: &ScanTree,
    selection: &BTreeSet<NodeId>,
    flags: &HashMap<NodeId, RowFlags>,
    excluded: &[String],
    mode: DeleteMode,
) -> ConfirmRequest {
    let picked: Vec<NodeId> = selection
        .iter()
        .copied()
        .filter(|id| can_select(flags.get(id)))
        .collect();
    let targets: Vec<(PathBuf, bool)> = picked
        .iter()
        .map(|&id| (tree.path_of(id), tree.nodes[id].class == EntryClass::Dir))
        .collect();
    ConfirmRequest {
        warn_app_owned: picked
            .iter()
            .filter(|id| flags.get(id).is_some_and(|f| f.app_owned))
            .count(),
        warn_holds_more: count_holding_more(&targets, excluded, &tree.errors, tree.cancelled),
        items: picked
            .iter()
            .map(|&id| Target {
                path: tree.path_of(id),
                size: tree.nodes[id].size,
            })
            .collect(),
        mode,
        context: ConfirmContext::User,
        empty_bin: None,
        bin_load: None,
        guards: Vec::new(),
    }
}

/// The part of a tree `other_space` reads, owned so a job can have it: the root, its
/// direct children and the errors.
pub fn shallow_tree(tree: &ScanTree) -> ScanTree {
    let copy = |n: &Node, parent| Node {
        name: n.name.clone(),
        size: n.size,
        modified: n.modified,
        class: n.class,
        cloud: n.cloud,
        parent,
        children: Vec::new(),
    };
    let root = &tree.nodes[tree.root];
    let mut nodes = vec![copy(root, None)];
    for &c in &root.children {
        let id = nodes.len();
        nodes.push(copy(&tree.nodes[c], Some(0)));
        nodes[0].children.push(id);
    }
    ScanTree {
        nodes,
        root: 0,
        root_path: tree.root_path.clone(),
        errors: tree
            .errors
            .iter()
            .map(|e| ScanError {
                path: e.path.clone(),
                reason: e.reason.clone(),
            })
            .collect(),
        files: tree.files,
        dirs: tree.dirs,
        cancelled: tree.cancelled,
    }
}

pub fn tile_at(rects: &[Rect], x: f32, y: f32) -> Option<usize> {
    rects.iter().position(|r| {
        r.w > 0.0 && r.h > 0.0 && x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
    })
}

const LABEL_PADDING: f32 = 4.0;

pub fn label_fits(text: (f32, f32), tile: (f32, f32)) -> bool {
    text.0 + 2.0 * LABEL_PADDING <= tile.0 && text.1 + 2.0 * LABEL_PADDING <= tile.1
}

/// Unknown stays unknown, never 0. An item the scan already counted does not explain
/// the gap, and the line says so.
pub fn system_item_line(item: &SystemItem) -> String {
    let size = match item.size {
        Some(size) => format_size(size),
        None => l("unknown", "tidak diketahui").to_string(),
    };
    let mut line = format!("{}: {size}", item.name);
    if item.in_scan {
        line.push_str(l(
            " (already counted in the scan, so it does not explain the gap)",
            " (sudah terhitung dalam pindaian, jadi bukan penjelas selisih)",
        ));
    }
    line
}

/// Where browsers keep their profiles.
pub fn browser_profile_roots(local: Option<&Path>, roaming: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(local) = local {
        for sub in [
            r"Google\Chrome\User Data",
            r"Microsoft\Edge\User Data",
            r"BraveSoftware\Brave-Browser\User Data",
        ] {
            roots.push(local.join(sub));
        }
    }
    if let Some(roaming) = roaming {
        roots.push(roaming.join(r"Mozilla\Firefox\Profiles"));
    }
    roots
}

pub(crate) fn cloud_mark() -> &'static str {
    l("in the cloud", "di cloud")
}

pub(crate) fn app_mark() -> &'static str {
    l("used by an app", "dipakai aplikasi")
}

/// A tile's tooltip.
pub fn hover_lines(
    name: &str,
    size: u64,
    modified: Option<SystemTime>,
    cloud: bool,
    app_owned: bool,
) -> Vec<String> {
    let mut lines = vec![
        name.to_string(),
        format_size(size),
        format!("{} {}", l("Changed:", "Diubah:"), format_date(modified)),
    ];
    if cloud {
        lines.push(cloud_mark().to_string());
    }
    if app_owned {
        lines.push(app_mark().to_string());
    }
    lines
}

/// `excluded` goes through as given; the scan brings it to `canonical_key` itself.
pub fn scan_options(excluded: Vec<PathBuf>) -> ScanOptions {
    ScanOptions {
        threads: std::thread::available_parallelism().map_or(4, |n| n.get()) * 4,
        excluded,
    }
}

pub fn compute_flags(p: &Path, protect: &ProtectContext, marks: &UsageMarks) -> RowFlags {
    RowFlags {
        protected: is_protected(p, protect),
        app_owned: marks.is_app_owned(p) || marks.contains_app_owned(p),
        guard: p
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_guard_name),
        locked: false,
    }
}

fn location_problem_text(problem: LocationProblem) -> &'static str {
    match problem {
        LocationProblem::NotAbsolute => l(
            r"Type a full path, such as D:\Games.",
            r"Tulis path lengkap, misalnya D:\Games.",
        ),
    }
}

fn available_drives() -> Vec<String> {
    drive_letters(unsafe { GetLogicalDrives() })
}

fn system_drive_root() -> String {
    let name = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    format!("{name}\\")
}

struct RunningScan {
    job: Job<io::Result<ScanTree>>,
    progress: ScanProgress,
}

enum OtherLoad {
    Idle,
    Loading(Job<io::Result<OtherSpace>>),
    Ready(OtherSpace),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    Map,
    FullList,
    OtherSpace,
}

struct Tile {
    item: MapItem,
    area: Area,
}

/// What is expensive to work out about one location, kept until the location or
/// the tree changes.
struct LocationView {
    id: NodeId,
    tiles: Vec<Tile>,
    rows: Vec<NodeId>,
    flags: HashMap<NodeId, RowFlags>,
    /// The tree changed under this location, so tiles and rows need building again.
    /// Flags stay, because they are what lets what is still ticked reach a request.
    dirty: bool,
}

impl LocationView {
    fn build(tree: &ScanTree, id: NodeId, protect: &ProtectContext) -> Self {
        let tiles = map_items(tree, id, MIN_TILE_FRACTION, MAX_TILES)
            .into_iter()
            .map(|item| Tile {
                item,
                area: item
                    .node
                    .map_or(Area::Other, |n| area_of(&tree.path_of(n), protect)),
            })
            .collect();
        LocationView {
            id,
            tiles,
            rows: tree.children_by_size(id),
            flags: HashMap::new(),
            dirty: false,
        }
    }

    fn flags_for(
        &mut self,
        tree: &ScanTree,
        id: NodeId,
        protect: &ProtectContext,
        marks: &UsageMarks,
        locks: &Locks,
    ) -> RowFlags {
        *self.flags.entry(id).or_insert_with(|| RowFlags {
            locked: locks.is_locked(tree, id),
            ..compute_flags(&tree.path_of(id), protect, marks)
        })
    }
}

pub enum HomeAction {
    Scan(PathBuf),
    Delete(ConfirmRequest),
}

pub struct HomeState {
    drive: String,
    folder_text: String,
    input_error: Option<&'static str>,
    scan: Option<RunningScan>,
    scan_error: Option<String>,
    space_root: PathBuf,
    space: Option<(u64, u64)>,
    space_job: Option<Job<io::Result<(u64, u64)>>>,
    marks: Option<UsageMarks>,
    marks_failed: bool,
    marks_job: Option<Job<UsageMarks>>,
    location: NodeId,
    view: Option<LocationView>,
    selection: BTreeSet<NodeId>,
    /// The folders the current tree was scanned without, as lowercase keys.
    scan_excluded: Vec<String>,
    pane: Pane,
    other: OtherLoad,
    map_stale: bool,
    locks: Locks,
    /// Counts every change of the tree: a scan, a delete, a stale flag. Screens that
    /// keep ticks or lists made from the tree compare it to know they are out of date.
    generation: u64,
    /// Counts only new trees, which make anything derived from the old one meaningless.
    epoch: u64,
}

/// Reads the installed apps and browser profiles in a job.
fn spawn_marks(ctx: &egui::Context, protect: &ProtectContext) -> Job<UsageMarks> {
    let protect = protect.clone();
    Job::spawn(ctx.clone(), move || {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from);
        let roots = browser_profile_roots(local.as_deref(), roaming.as_deref());
        UsageMarks::with_context(&list_installed(), roots, &protect)
    })
}

impl HomeState {
    pub fn new(ctx: &egui::Context, protect: &ProtectContext) -> Self {
        let drive = system_drive_root();
        let marks_job = spawn_marks(ctx, protect);
        let mut state = HomeState {
            space_root: PathBuf::from(&drive),
            drive,
            folder_text: String::new(),
            input_error: None,
            scan: None,
            scan_error: None,
            space: None,
            space_job: None,
            marks: None,
            marks_failed: false,
            marks_job: Some(marks_job),
            location: 0,
            view: None,
            selection: BTreeSet::new(),
            scan_excluded: Vec::new(),
            pane: Pane::Map,
            other: OtherLoad::Idle,
            map_stale: false,
            locks: Locks::default(),
            generation: 0,
            epoch: 0,
        };
        state.refresh_space(ctx);
        state
    }

    /// Reads the installed apps again, after one was removed. Until it is in, rows get
    /// no tick, and what was worked out from the old marks is dropped.
    pub fn refresh_marks(&mut self, ctx: &egui::Context, protect: &ProtectContext) {
        self.marks = None;
        self.marks_failed = false;
        self.marks_job = Some(spawn_marks(ctx, protect));
        self.view = None;
        self.selection.clear();
        self.generation += 1;
    }

    /// Asks again how much of the shown volume is free.
    pub fn refresh_space(&mut self, ctx: &egui::Context) {
        let root = self.space_root.clone();
        self.space_job = Some(Job::spawn(ctx.clone(), move || win::volume_space(&root)));
    }

    /// Starts a scan. The old tree goes first: nothing can be selected from it while
    /// a newer one is on its way.
    pub fn start_scan(
        &mut self,
        ctx: &egui::Context,
        root: PathBuf,
        excluded: Vec<PathBuf>,
        tree: &mut Option<Arc<ScanTree>>,
    ) {
        *tree = None;
        self.scan_error = None;
        self.new_tree();
        self.reset_view();
        self.location = 0;
        self.map_stale = false;
        self.locks = Locks::default();
        self.space_root = root.clone();
        self.refresh_space(ctx);
        self.scan_excluded = excluded
            .iter()
            .map(|p| canonical_key(p).unwrap_or_else(|| p.to_string_lossy().to_lowercase()))
            .collect();
        let progress = ScanProgress::default();
        let seen = progress.clone();
        let job = Job::spawn(ctx.clone(), move || {
            scan(&root, &scan_options(excluded), &seen)
        });
        self.scan = Some(RunningScan { job, progress });
    }

    fn new_tree(&mut self) {
        self.generation += 1;
        self.epoch += 1;
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn map_stale(&self) -> bool {
        self.map_stale
    }

    pub fn locks(&self) -> &Locks {
        &self.locks
    }

    pub(crate) fn scan_excluded(&self) -> &[String] {
        &self.scan_excluded
    }

    /// `None` until the installed-app list is in; rows get no checkbox before then.
    pub fn marks(&self) -> Option<&UsageMarks> {
        self.marks.as_ref()
    }

    pub fn marks_failed(&self) -> bool {
        self.marks_failed
    }

    pub fn scan_progress(&self) -> Option<&ScanProgress> {
        self.scan.as_ref().map(|s| &s.progress)
    }

    pub fn scan_error(&self) -> Option<&str> {
        self.scan_error.as_deref()
    }

    /// The location Home would scan now: its folder field if that holds a path,
    /// otherwise its drive.
    pub fn selected_location(&self) -> PathBuf {
        scan_target(&self.drive, &self.folder_text).unwrap_or_else(|_| PathBuf::from(&self.drive))
    }

    fn reset_view(&mut self) {
        self.view = None;
        self.selection.clear();
        self.pane = Pane::Map;
        self.other = OtherLoad::Idle;
    }

    fn go_to(&mut self, id: NodeId) {
        self.location = id;
        self.reset_view();
    }

    /// Takes in whatever background work has finished. Runs every frame on every
    /// screen, so a scan finishes also while another tool is open.
    pub fn poll(&mut self, ctx: &egui::Context, tree: &mut Option<Arc<ScanTree>>) {
        if let Some(outcome) = self.marks_job.as_mut().and_then(Job::poll) {
            self.marks_job = None;
            self.marks_failed = outcome.is_err();
            self.marks = Some(outcome.unwrap_or_default());
        }
        if let Some(outcome) = self.space_job.as_mut().and_then(Job::poll) {
            self.space_job = None;
            self.space = outcome.ok().and_then(Result::ok);
        }
        if let OtherLoad::Loading(job) = &mut self.other {
            match job.poll() {
                None => {}
                Some(Ok(Ok(other))) => self.other = OtherLoad::Ready(other),
                Some(Ok(Err(e))) => self.other = OtherLoad::Failed(e.to_string()),
                Some(Err(JobPanicked)) => {
                    self.other = OtherLoad::Failed(
                        l("It stopped unexpectedly.", "Berhenti tiba-tiba.").to_string(),
                    );
                }
            }
        }
        if let Some(outcome) = self.scan.as_mut().and_then(|s| s.job.poll()) {
            self.scan = None;
            match outcome {
                Ok(Ok(new_tree)) => {
                    self.location = new_tree.root;
                    self.reset_view();
                    self.new_tree();
                    *tree = Some(Arc::new(new_tree));
                    self.refresh_space(ctx);
                }
                Ok(Err(e)) => self.scan_error = Some(e.to_string()),
                Err(JobPanicked) => {
                    self.scan_error = Some(
                        l(
                            "The scan stopped unexpectedly. Try again.",
                            "Pemindaian berhenti tiba-tiba. Coba lagi.",
                        )
                        .to_string(),
                    );
                }
            }
        }
    }

    /// Brings the screen in line with a finished delete.
    pub fn after_report(&mut self, tree: &mut ScanTree, report: &DeleteReport) {
        self.generation += 1;
        apply_report(tree, report);
        self.locks.add_report(tree, report);
        let location = location_or_root(tree, self.location);
        if location == self.location {
            self.refresh_location(tree);
        } else {
            self.go_to(location);
        }
        if map_may_be_stale(report) {
            self.mark_stale();
        }
    }

    /// The location is still there but the tree changed. What left or is now locked
    /// comes off the selection; every other tick stays.
    fn refresh_location(&mut self, tree: &ScanTree) {
        let locks = &self.locks;
        let usable = |id: NodeId| location_or_root(tree, id) == id && !locks.is_locked(tree, id);
        self.selection.retain(|&id| usable(id));
        if let Some(view) = &mut self.view {
            view.dirty = true;
            view.flags.retain(|&id, _| location_or_root(tree, id) == id);
            for (&id, flags) in &mut view.flags {
                flags.locked = locks.is_locked(tree, id);
            }
        }
        if self.pane == Pane::OtherSpace {
            self.pane = Pane::Map;
        }
        self.other = OtherLoad::Idle;
    }

    /// A delete ended with an unknown outcome, so the map may be wrong and what is
    /// ticked may be gone already. Sizes from here on would count space twice.
    pub fn mark_stale(&mut self) {
        self.generation += 1;
        self.map_stale = true;
        self.selection.clear();
    }

    fn open_other_space(&mut self, ctx: &egui::Context, tree: &ScanTree) {
        self.pane = Pane::OtherSpace;
        let copy = shallow_tree(tree);
        self.other = OtherLoad::Loading(Job::spawn(ctx.clone(), move || other_space(&copy)));
    }
}

fn tile_fill(p: &TilePalette, item: MapItem, area: Area) -> egui::Color32 {
    if item.node.is_none() {
        return p.rest;
    }
    match area {
        Area::Profile => p.profile,
        Area::ProgramFiles => p.program_files,
        Area::Windows => p.windows,
        Area::Other => p.other,
    }
}

fn rest_name() -> &'static str {
    l("Other", "Lainnya")
}

fn tile_name(tree: &ScanTree, item: MapItem) -> &str {
    item.node
        .map_or(rest_name(), |id| tree.nodes[id].name.as_str())
}

fn paint_label(
    painter: &egui::Painter,
    rect: egui::Rect,
    name: &str,
    size: u64,
    color: egui::Color32,
) {
    let font = egui::FontId::proportional(13.0);
    let tile = (rect.width(), rect.height());
    let title = painter.layout_no_wrap(name.to_string(), font.clone(), color);
    if !label_fits((title.size().x, title.size().y), tile) {
        return;
    }
    let at = rect.min + egui::vec2(LABEL_PADDING, LABEL_PADDING);
    let title_height = title.size().y;
    painter.galley(at, title, color);
    let sub = painter.layout_no_wrap(format_size(size), font, color);
    if label_fits((sub.size().x, title_height + sub.size().y), tile) {
        painter.galley(at + egui::vec2(0.0, title_height), sub, color);
    }
}

/// Draws the map and returns what a click on it asks for. A click never selects
/// anything.
fn draw_map(
    ui: &mut egui::Ui,
    tree: &ScanTree,
    view: &mut LocationView,
    marks: Option<&UsageMarks>,
    protect: &ProtectContext,
    locks: &Locks,
    height: f32,
) -> Option<TileAction> {
    let (resp, painter) =
        ui.allocate_painter(egui::vec2(ui.available_width(), height), Sense::click());
    let area = resp.rect;
    let sizes: Vec<u64> = view.tiles.iter().map(|t| t.item.size).collect();
    let rects = squarify(
        &sizes,
        Rect {
            x: area.min.x,
            y: area.min.y,
            w: area.width(),
            h: area.height(),
        },
    );
    let colors = &theme::palette(ui.visuals().dark_mode).tiles;
    for (tile, r) in view.tiles.iter().zip(&rects) {
        if r.w <= 0.0 || r.h <= 0.0 {
            continue;
        }
        let rect = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h));
        let inner = rect.shrink(1.0);
        painter.rect_filled(inner, 2.0, tile_fill(colors, tile.item, tile.area));
        let name = tile_name(tree, tile.item);
        paint_label(&painter, inner, name, tile.item.size, colors.text);
    }
    if let Some(pos) = resp.hover_pos() {
        if let Some(i) = tile_at(&rects, pos.x, pos.y) {
            let item = view.tiles[i].item;
            let r = rects[i];
            let rect = egui::Rect::from_min_size(egui::pos2(r.x, r.y), egui::vec2(r.w, r.h));
            painter.rect_stroke(
                rect.shrink(1.0),
                2.0,
                egui::Stroke::new(2.0, colors.text),
                egui::StrokeKind::Inside,
            );
            let lines = match item.node {
                Some(id) => {
                    let node = &tree.nodes[id];
                    let owned = marks
                        .is_some_and(|m| view.flags_for(tree, id, protect, m, locks).app_owned);
                    hover_lines(
                        &tree.path_of(id).display().to_string(),
                        node.size,
                        node.modified,
                        node.cloud,
                        owned,
                    )
                }
                None => vec![rest_name().to_string(), format_size(item.size)],
            };
            let clickable = tile_click(tree, item, view.id) != TileAction::Nothing;
            let hover = resp.clone().on_hover_ui_at_pointer(|ui| {
                for line in lines {
                    ui.label(line);
                }
            });
            if clickable {
                hover.on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        }
    }
    if resp.clicked() {
        let pos = resp.interact_pointer_pos()?;
        let i = tile_at(&rects, pos.x, pos.y)?;
        return Some(tile_click(tree, view.tiles[i].item, view.id));
    }
    None
}

fn legend(ui: &mut egui::Ui) {
    let colors = &theme::palette(ui.visuals().dark_mode).tiles;
    let entries = [
        (colors.profile, l("User folders", "Folder pengguna")),
        (colors.program_files, "Program Files"),
        (colors.windows, "Windows"),
        (colors.other, l("Elsewhere", "Tempat lain")),
        (colors.rest, l("Other, grouped", "Lainnya, digabung")),
    ];
    ui.horizontal_wrapped(|ui| {
        for (color, name) in entries {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(rect, 2.0, color);
            ui.weak(name);
            ui.add_space(6.0);
        }
    });
}

pub(crate) fn cell(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    right: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    let layout = if right {
        Layout::right_to_left(Align::Center)
    } else {
        Layout::left_to_right(Align::Center)
    };
    ui.allocate_ui_with_layout(egui::vec2(width, height), layout, |ui| {
        ui.set_min_size(egui::vec2(width, height));
        ui.set_max_width(width);
        add(ui);
    });
}

pub(crate) fn row_marks(node: &Node, flags: Option<&RowFlags>) -> String {
    let mut marks = Vec::new();
    if flags.is_some_and(|f| f.protected) {
        marks.push(l("protected", "dilindungi"));
    }
    if flags.is_some_and(|f| f.locked) {
        marks.push(l("scan again to change", "pindai ulang untuk mengubah"));
    }
    if flags.is_some_and(|f| f.guard) {
        marks.push(l(
            "guard file, may hold a file's data",
            "file pengaman, mungkin berisi data file",
        ));
    }
    if node.class == EntryClass::Link {
        marks.push(l("link", "tautan"));
    }
    if node.cloud {
        marks.push(cloud_mark());
    }
    if flags.is_some_and(|f| f.app_owned) {
        marks.push(app_mark());
    }
    marks.join(", ")
}

pub(crate) const CHECK_WIDTH: f32 = 24.0;
pub(crate) const SIZE_WIDTH: f32 = 80.0;
pub(crate) const DATE_WIDTH: f32 = 84.0;
pub(crate) const MARK_WIDTH: f32 = 150.0;

pub(crate) fn name_width(ui: &egui::Ui) -> f32 {
    let fixed = CHECK_WIDTH + SIZE_WIDTH + DATE_WIDTH + MARK_WIDTH;
    (ui.available_width() - fixed - 4.0 * ui.spacing().item_spacing.x - 16.0).max(60.0)
}

/// Draws the list and returns the folder the user asked to enter. Rows get their
/// flags at most `FLAG_ROWS_PER_FRAME` at a time, and until then have no checkbox.
#[allow(clippy::too_many_arguments)]
fn draw_list(
    ui: &mut egui::Ui,
    tree: &ScanTree,
    view: &mut LocationView,
    selection: &mut BTreeSet<NodeId>,
    ticks_allowed: bool,
    marks: Option<&UsageMarks>,
    protect: &ProtectContext,
    locks: &Locks,
    stale_days: u32,
    height: f32,
) -> Option<NodeId> {
    let row_height = ui.spacing().interact_size.y;
    let stale_color = theme::palette(ui.visuals().dark_mode).stale;
    ui.horizontal(|ui| {
        let width = name_width(ui);
        cell(ui, CHECK_WIDTH, row_height, false, |_| {});
        cell(ui, width, row_height, false, |ui| {
            ui.weak(l("Name", "Nama"));
        });
        cell(ui, MARK_WIDTH, row_height, false, |_| {});
        cell(ui, SIZE_WIDTH, row_height, true, |ui| {
            ui.weak(l("Size", "Ukuran"));
        });
        cell(ui, DATE_WIDTH, row_height, true, |ui| {
            ui.weak(l("Changed", "Diubah"));
        });
    });
    let mut budget = FLAG_ROWS_PER_FRAME;
    let mut enter = None;
    let now = SystemTime::now();
    let rows = view.rows.len();
    egui::ScrollArea::vertical()
        .id_salt(("home-list", view.id))
        .max_height(height.max(row_height))
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows, |ui, range| {
            for i in range {
                let id = view.rows[i];
                let node = &tree.nodes[id];
                if let Some(marks) = marks {
                    if !view.flags.contains_key(&id) && budget > 0 {
                        budget -= 1;
                        view.flags_for(tree, id, protect, marks, locks);
                    }
                }
                let flags = view.flags.get(&id).copied();
                if flags.is_none() && marks.is_some() {
                    ui.ctx().request_repaint();
                }
                ui.horizontal(|ui| {
                    let width = name_width(ui);
                    cell(ui, CHECK_WIDTH, row_height, false, |ui| {
                        if can_select(flags.as_ref()) {
                            let mut on = selection.contains(&id);
                            let tick = egui::Checkbox::new(&mut on, "");
                            if ui.add_enabled(ticks_allowed, tick).changed() {
                                if on {
                                    selection.insert(id);
                                } else {
                                    selection.remove(&id);
                                }
                            }
                        }
                    });
                    cell(ui, width, row_height, false, |ui| {
                        if node.class == EntryClass::Dir {
                            let label = egui::Label::new(format!("{}\\", node.name))
                                .truncate()
                                .sense(Sense::click());
                            if ui
                                .add(label)
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                enter = Some(id);
                            }
                        } else {
                            ui.add(egui::Label::new(node.name.as_str()).truncate());
                        }
                    });
                    cell(ui, MARK_WIDTH, row_height, false, |ui| {
                        let text = RichText::new(row_marks(node, flags.as_ref())).weak();
                        ui.add(egui::Label::new(text).truncate());
                    });
                    cell(ui, SIZE_WIDTH, row_height, true, |ui| {
                        ui.label(format_size(node.size));
                    });
                    cell(ui, DATE_WIDTH, row_height, true, |ui| {
                        let mut date = RichText::new(format_date(node.modified));
                        if is_stale(node.modified, stale_days, now) {
                            date = date.color(stale_color);
                        }
                        ui.label(date);
                    });
                });
            }
        });
    enter
}

fn unreadable_list(ui: &mut egui::Ui, salt: &str, errors: &[ScanError]) {
    let height = ui.text_style_height(&egui::TextStyle::Body);
    egui::ScrollArea::vertical()
        .id_salt(salt)
        .max_height(160.0)
        .auto_shrink([false, true])
        .show_rows(ui, height, errors.len(), |ui, range| {
            for e in &errors[range] {
                let line = format!("{}: {}", e.path.display(), e.reason);
                ui.add(egui::Label::new(line).truncate());
            }
        });
}

fn draw_other_space(ui: &mut egui::Ui, other: &OtherLoad, cancelled: bool) {
    match other {
        OtherLoad::Idle => {}
        OtherLoad::Loading(_) => {
            ui.spinner();
        }
        OtherLoad::Failed(text) => {
            ui.label(format!(
                "{} {text}",
                l("Could not read the drive:", "Drive tidak terbaca:")
            ));
        }
        OtherLoad::Ready(o) => {
            let line = |ui: &mut egui::Ui, name: &str, size: u64| {
                ui.label(format!("{name} {}", format_size(size)));
            };
            line(ui, l("Used on the drive:", "Terpakai di drive:"), o.used);
            line(
                ui,
                l("Found by the scan:", "Ditemukan pindaian:"),
                o.scanned,
            );
            line(
                ui,
                l(
                    "Not explained by the scan:",
                    "Tidak terjelaskan oleh pindaian:",
                ),
                o.unaccounted,
            );
            if cancelled {
                ui.label(l(
                    "The scan was cancelled, so part of this gap is simply what it did not reach.",
                    "Pindaian dibatalkan, jadi sebagian selisih ini hanyalah yang belum terjangkau.",
                ));
            }
            ui.add_space(8.0);
            ui.strong(l("System items", "Item sistem"));
            for item in &o.system_files {
                ui.label(system_item_line(item));
            }
            if !o.unreadable.is_empty() {
                ui.add_space(8.0);
                ui.strong(format!(
                    "{} {}",
                    l("Could not be read:", "Tidak terbaca:"),
                    o.unreadable.len()
                ));
                unreadable_list(ui, "other-unreadable", &o.unreadable);
            }
            ui.add_space(8.0);
            ui.weak(l(
                "For information. Nothing can be deleted from here.",
                "Sebagai informasi. Tidak ada yang bisa dihapus dari sini.",
            ));
        }
    }
}

/// Whether a delete may be asked for. Not on a stale map: its sizes are what a report
/// counts as freed, and after a partial delete they would count the same space again.
pub(crate) fn can_delete(selection: &BTreeSet<NodeId>, marks_ready: bool, map_stale: bool) -> bool {
    !selection.is_empty() && marks_ready && !map_stale
}

/// What a press of a delete button asks for, if it may ask for anything.
pub(crate) fn delete_action(
    tree: &ScanTree,
    flags: &HashMap<NodeId, RowFlags>,
    excluded: &[String],
    selection: &BTreeSet<NodeId>,
    pressed: Option<DeleteMode>,
    allowed: bool,
) -> Option<HomeAction> {
    pressed
        .filter(|_| allowed)
        .map(|mode| request_from_selection(tree, selection, flags, excluded, mode))
        .filter(|request| !request.items.is_empty())
        .map(HomeAction::Delete)
}

/// The panel that explains used against scanned is about a whole drive.
fn offers_gap_panel(tree: &ScanTree) -> bool {
    is_drive_root(&tree.root_path)
}

/// The buttons that lead into a delete. Both only open the confirmation, and
/// permanent is a separate button of its own, never a remembered setting.
pub(crate) fn action_bar(
    ui: &mut egui::Ui,
    tree: &ScanTree,
    flags: &HashMap<NodeId, RowFlags>,
    excluded: &[String],
    selection: &BTreeSet<NodeId>,
    marks_ready: bool,
    map_stale: bool,
) -> Option<HomeAction> {
    let total: u64 = selection.iter().map(|&id| tree.nodes[id].size).sum();
    let can_delete = can_delete(selection, marks_ready, map_stale);
    let pal = theme::palette(ui.visuals().dark_mode);
    let mut mode = None;
    ui.horizontal(|ui| {
        let primary = egui::Button::new(
            RichText::new(l("Move to Recycle Bin", "Pindahkan ke Recycle Bin"))
                .color(pal.on_accent),
        )
        .fill(pal.accent);
        if ui.add_enabled(can_delete, primary).clicked() {
            mode = Some(DeleteMode::RecycleBin);
        }
        if ui
            .add_enabled(
                can_delete,
                egui::Button::new(l("Delete permanently...", "Hapus permanen...")),
            )
            .clicked()
        {
            mode = Some(DeleteMode::Permanent);
        }
        ui.label(format!(
            "{} {}, {}",
            selection.len(),
            l("selected", "dipilih"),
            format_size(total)
        ));
    });
    delete_action(tree, flags, excluded, selection, mode, can_delete)
}

/// Everything below the controls, once there is a tree.
fn body(
    ui: &mut egui::Ui,
    st: &mut HomeState,
    tree: &ScanTree,
    stale_days: u32,
    protect: &ProtectContext,
) -> Option<HomeAction> {
    let mut action = None;
    let location = location_or_root(tree, st.location);
    if location != st.location {
        st.go_to(location);
    }
    let mut go = None;
    ui.horizontal_wrapped(|ui| {
        let trail = breadcrumb(tree, location);
        let last = trail.len() - 1;
        for (i, (id, name)) in trail.into_iter().enumerate() {
            if i > 0 {
                ui.weak(">");
            }
            if ui.add_enabled(i != last, egui::Button::new(name)).clicked() {
                go = Some(id);
            }
        }
    });
    if tree.cancelled {
        ui.label(l(
            "The scan was cancelled. This map is partial, and folders may be bigger than shown.",
            "Pemindaian dibatalkan. Peta ini sebagian, dan folder bisa lebih besar dari yang tampak.",
        ));
    }
    if st.map_stale {
        st.selection.clear();
        ui.horizontal(|ui| {
            ui.label(l(
                "The map may no longer match the disk. Scan again before deleting more.",
                "Peta mungkin tidak lagi sesuai dengan disk. Pindai ulang sebelum menghapus lagi.",
            ));
            if ui.button(l("Scan again", "Pindai ulang")).clicked() {
                action = Some(HomeAction::Scan(tree.root_path.clone()));
            }
        });
    }
    if st.marks_failed {
        ui.label(l(
            "Installed apps could not be read, so \"used by an app\" marks are missing.",
            "Daftar aplikasi tidak terbaca, jadi tanda \"dipakai aplikasi\" tidak ada.",
        ));
    }
    if !tree.errors.is_empty() && st.pane != Pane::OtherSpace {
        egui::CollapsingHeader::new(format!(
            "{} {}",
            l("Could not be read:", "Tidak terbaca:"),
            tree.errors.len()
        ))
        .id_salt("home-unreadable")
        .show(ui, |ui| {
            unreadable_list(ui, "home-unreadable-list", &tree.errors)
        });
    }

    let mut view = st
        .view
        .take()
        .filter(|v| v.id == location)
        .map(|old| {
            if !old.dirty {
                return old;
            }
            LocationView {
                flags: old.flags,
                ..LocationView::build(tree, location, protect)
            }
        })
        .unwrap_or_else(|| LocationView::build(tree, location, protect));
    let marks = st.marks.as_ref();
    let mut tile_action = None;
    let mut open_gap = false;
    if offers_gap_panel(tree) && st.pane != Pane::OtherSpace {
        open_gap = ui
            .button(l(
                "Why doesn't this add up?",
                "Kenapa jumlahnya tidak sama?",
            ))
            .clicked();
    }
    match st.pane {
        Pane::OtherSpace => {
            if ui.button(l("Back to map", "Kembali ke peta")).clicked() {
                st.pane = Pane::Map;
            }
            ui.add_space(6.0);
            draw_other_space(ui, &st.other, tree.cancelled);
        }
        Pane::Map | Pane::FullList => {
            if st.pane == Pane::FullList {
                if ui.button(l("Back to map", "Kembali ke peta")).clicked() {
                    st.pane = Pane::Map;
                }
            } else if view.tiles.is_empty() {
                ui.weak(l(
                    "Nothing with a measurable size here.",
                    "Tidak ada yang berukuran di sini.",
                ));
            } else {
                let height = (ui.available_height() * 0.45).max(120.0);
                tile_action = draw_map(ui, tree, &mut view, marks, protect, &st.locks, height);
                legend(ui);
            }
            ui.add_space(4.0);
            let row = ui.spacing().interact_size.y;
            let height = ui.available_height() - 2.0 * row - 24.0;
            let enter = draw_list(
                ui,
                tree,
                &mut view,
                &mut st.selection,
                !st.map_stale,
                marks,
                protect,
                &st.locks,
                stale_days,
                height,
            );
            go = go.or(enter);
            ui.add_space(4.0);
            action = action.or(action_bar(
                ui,
                tree,
                &view.flags,
                &st.scan_excluded,
                &st.selection,
                marks.is_some(),
                st.map_stale,
            ));
        }
    }
    st.view = Some(view);

    match tile_action {
        Some(TileAction::Enter(id)) => go = Some(id),
        Some(TileAction::OtherSpace) => st.open_other_space(ui.ctx(), tree),
        Some(TileAction::FullList) => st.pane = Pane::FullList,
        Some(TileAction::Nothing) | None => {}
    }
    if open_gap {
        st.open_other_space(ui.ctx(), tree);
    }
    if let Some(id) = go {
        st.go_to(id);
    }
    action
}

/// The location picker, its Scan and Cancel buttons, and the free-space line.
fn controls(ui: &mut egui::Ui, st: &mut HomeState) -> Option<HomeAction> {
    let mut action = None;
    ui.horizontal(|ui| {
        let mut picked = None;
        egui::ComboBox::from_id_salt("home-drive")
            .selected_text(st.drive.as_str())
            .show_ui(ui, |ui| {
                for drive in available_drives() {
                    let on = drive == st.drive;
                    if ui.selectable_label(on, drive.as_str()).clicked() {
                        picked = Some(drive);
                    }
                }
            });
        if let Some(drive) = picked {
            st.space_root = PathBuf::from(&drive);
            st.drive = drive;
            st.folder_text.clear();
            st.input_error = None;
            st.refresh_space(ui.ctx());
        }
        ui.add(
            egui::TextEdit::singleline(&mut st.folder_text)
                .hint_text(l(r"or a folder: D:\Games", r"atau folder: D:\Games"))
                .desired_width(240.0),
        );
        if let Some(running) = &st.scan {
            if ui.button(l("Cancel", "Batalkan")).clicked() {
                running.progress.cancel();
            }
        } else if ui.button(l("Scan", "Pindai")).clicked() {
            match scan_target(&st.drive, &st.folder_text) {
                Ok(root) => {
                    st.input_error = None;
                    action = Some(HomeAction::Scan(root));
                }
                Err(problem) => st.input_error = Some(location_problem_text(problem)),
            }
        }
    });
    if let Some((total, free)) = st.space {
        ui.label(format!(
            "{}: {} {} {}",
            st.space_root.display(),
            format_size(free),
            l("free of", "tersisa dari"),
            format_size(total)
        ));
    }
    if let Some(text) = st.input_error {
        ui.label(text);
    }
    if let Some(err) = &st.scan_error {
        ui.label(format!("{} {err}", l("Could not scan:", "Gagal memindai:")));
    }
    action
}

/// Draws the screen and returns what the app has to do for it.
pub fn show(
    ui: &mut egui::Ui,
    st: &mut HomeState,
    tree: Option<&ScanTree>,
    stale_days: u32,
    protect: &ProtectContext,
) -> Option<HomeAction> {
    let mut action = controls(ui, st);
    ui.add_space(6.0);
    if let Some(running) = &st.scan {
        ui.horizontal(|ui| {
            ui.spinner();
            let text = if running.progress.is_cancelled() {
                l("Cancelling...", "Membatalkan...")
            } else {
                l("Scanning...", "Memindai...")
            };
            ui.label(format!(
                "{text} {} {}, {}",
                running.progress.files(),
                l("files", "file"),
                format_size(running.progress.bytes())
            ));
        });
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    } else if let Some(tree) = tree {
        action = action.or(body(ui, st, tree, stale_days, protect));
    } else {
        ui.weak(l(
            "Press Scan to see what fills this location.",
            "Tekan Pindai untuk melihat apa yang memenuhi lokasi ini.",
        ));
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::delete::{FailReason, Failed};
    use std::fs;

    fn node(name: &str, size: u64, class: EntryClass, parent: Option<NodeId>) -> Node {
        Node {
            name: name.into(),
            size,
            modified: None,
            class,
            cloud: false,
            parent,
            children: Vec::new(),
        }
    }

    /// `C:\t` holding `a\x` (100), `a\y` (20), `b` (10).
    fn tree_at(root: &str) -> ScanTree {
        let mut nodes = vec![
            node("t", 130, EntryClass::Dir, None),
            node("a", 120, EntryClass::Dir, Some(0)),
            node("x", 100, EntryClass::File, Some(1)),
            node("y", 20, EntryClass::File, Some(1)),
            node("b", 10, EntryClass::File, Some(0)),
        ];
        nodes[0].children = vec![1, 4];
        nodes[1].children = vec![2, 3];
        ScanTree {
            nodes,
            root: 0,
            root_path: PathBuf::from(root),
            errors: Vec::new(),
            files: 3,
            dirs: 1,
            cancelled: false,
        }
    }

    fn ctx() -> ProtectContext {
        ProtectContext {
            windows_dir: r"C:\Windows".into(),
            program_files: vec![r"C:\Program Files".into(), r"C:\Program Files (x86)".into()],
            program_data: r"C:\ProgramData".into(),
            users_dir: r"C:\Users".into(),
            profile: r"C:\Users\me".into(),
            own_dirs: vec![r"C:\Users\me\own".into()],
        }
    }

    const DAY: Duration = Duration::from_secs(86_400);

    #[test]
    fn a_target_holding_what_the_scan_left_out_is_counted() {
        let folder = |p: &str| (PathBuf::from(p), true);
        let error = |p: &str| ScanError {
            path: PathBuf::from(p),
            reason: "denied".into(),
        };
        let excluded = [r"c:\t\a\keep".to_string()];
        let errors = [error(r"C:\T\B\Locked")];

        // Holds the excluded folder, in any case of spelling.
        assert_eq!(
            count_holding_more(&[folder(r"C:\T\A")], &excluded, &[], false),
            1
        );
        // Holds an unreadable folder, or is one.
        assert_eq!(
            count_holding_more(&[folder(r"C:\t\b")], &[], &errors, false),
            1
        );
        assert_eq!(
            count_holding_more(&[folder(r"C:\t\b\locked")], &[], &errors, false),
            1
        );
        // Beside them, or only inside them, a target holds nothing it does not show.
        for clean in [r"C:\t\c", r"C:\t\a\keeper", r"C:\t\b\locked\inner"] {
            assert_eq!(
                count_holding_more(&[folder(clean)], &excluded, &errors, false),
                0,
                "{clean}"
            );
        }
    }

    #[test]
    fn a_cancelled_scan_counts_folders_and_not_files() {
        let targets = [
            (PathBuf::from(r"C:\t\dir"), true),
            (PathBuf::from(r"C:\t\file"), false),
        ];
        assert_eq!(count_holding_more(&targets, &[], &[], true), 1);
        assert_eq!(count_holding_more(&targets, &[], &[], false), 0);
    }

    #[test]
    fn the_request_carries_the_count() {
        let mut t = tree_at(r"C:\t");
        let flags: HashMap<NodeId, RowFlags> = [(
            1,
            RowFlags {
                protected: false,
                app_owned: false,
                guard: false,
                locked: false,
            },
        )]
        .into();
        let sel = BTreeSet::from([1]);
        let quiet = request_from_selection(&t, &sel, &flags, &[], DeleteMode::RecycleBin);
        assert_eq!(quiet.warn_holds_more, 0);
        let excluded = [r"c:\t\a\skipped".to_string()];
        let req = request_from_selection(&t, &sel, &flags, &excluded, DeleteMode::RecycleBin);
        assert_eq!(req.warn_holds_more, 1);
        t.cancelled = true;
        let req = request_from_selection(&t, &sel, &flags, &[], DeleteMode::RecycleBin);
        assert_eq!(req.warn_holds_more, 1);
        t.cancelled = false;
        t.errors.push(ScanError {
            path: PathBuf::from(r"C:\t\a\denied"),
            reason: "denied".into(),
        });
        let req = request_from_selection(&t, &sel, &flags, &[], DeleteMode::RecycleBin);
        assert_eq!(req.warn_holds_more, 1);
    }

    #[test]
    fn folder_text_is_refused_unless_absolute() {
        assert_eq!(parse_location("  "), Ok(None));
        assert_eq!(
            parse_location(r#" "D:\Games" "#),
            Ok(Some(PathBuf::from(r"D:\Games")))
        );
        // `D:` alone means the current folder of D:, not its root.
        assert_eq!(parse_location("D:"), Ok(Some(PathBuf::from(r"D:\"))));
        assert_eq!(parse_location("Games"), Err(LocationProblem::NotAbsolute));
        assert_eq!(
            parse_location(r"D:Games"),
            Err(LocationProblem::NotAbsolute)
        );
        assert_eq!(parse_location(r"\Games"), Err(LocationProblem::NotAbsolute));
    }

    #[test]
    fn folder_text_wins_over_the_drive() {
        assert_eq!(scan_target(r"C:\", " "), Ok(PathBuf::from(r"C:\")));
        assert_eq!(scan_target(r"C:\", r"D:\x"), Ok(PathBuf::from(r"D:\x")));
        assert_eq!(scan_target(r"C:\", "x"), Err(LocationProblem::NotAbsolute));
    }

    #[test]
    fn drive_root_is_told_from_a_folder() {
        assert!(is_drive_root(Path::new(r"C:\")));
        assert!(is_drive_root(Path::new(r"\\?\C:\")));
        assert!(!is_drive_root(Path::new(r"C:\Users")));
        assert!(!is_drive_root(Path::new("C:")));
        assert!(!is_drive_root(Path::new(r"\\server\share")));
    }

    #[test]
    fn drive_mask_reads_as_letters() {
        assert_eq!(
            drive_letters(0b1100),
            vec![r"C:\".to_string(), r"D:\".to_string()]
        );
        assert!(drive_letters(0).is_empty());
    }

    #[test]
    fn lainnya_opens_other_space_only_at_a_drive_root() {
        let rest = MapItem {
            node: None,
            size: 5,
        };
        let drive = tree_at(r"C:\");
        assert_eq!(tile_click(&drive, rest, drive.root), TileAction::OtherSpace);
        assert_eq!(tile_click(&drive, rest, 1), TileAction::FullList);
        let folder = tree_at(r"C:\t");
        assert_eq!(tile_click(&folder, rest, folder.root), TileAction::FullList);
    }

    #[test]
    fn folder_tiles_enter_and_file_tiles_do_nothing() {
        let t = tree_at(r"C:\t");
        let folder = MapItem {
            node: Some(1),
            size: 120,
        };
        let file = MapItem {
            node: Some(4),
            size: 10,
        };
        assert_eq!(tile_click(&t, folder, 0), TileAction::Enter(1));
        assert_eq!(tile_click(&t, file, 0), TileAction::Nothing);
    }

    #[test]
    fn lainnya_is_found_wherever_map_items_puts_it() {
        // Many small children add up to more than the biggest kept one.
        let mut t = tree_at(r"C:\t");
        t.nodes[0].children.clear();
        t.nodes.truncate(1);
        t.nodes.push(node("big", 2000, EntryClass::File, Some(0)));
        t.nodes[0].children.push(1);
        for i in 0..2000 {
            let id = t.nodes.len();
            t.nodes
                .push(node(&format!("f{i}"), 2, EntryClass::File, Some(0)));
            t.nodes[0].children.push(id);
        }
        let items = map_items(&t, 0, 0.005, 40);
        let at = items.iter().position(|m| m.node.is_none()).unwrap();
        assert_eq!(at, 0);
        assert_eq!(tile_click(&t, items[at], 0), TileAction::FullList);
    }

    #[test]
    fn stale_uses_the_setting_and_unknown_dates_are_not_stale() {
        let now = SystemTime::UNIX_EPOCH + DAY * 1000;
        assert!(is_stale(Some(now - DAY * 366), 365, now));
        assert!(!is_stale(Some(now - DAY * 364), 365, now));
        assert!(is_stale(Some(now - DAY * 31), 30, now));
        assert!(!is_stale(None, 365, now));
        assert!(!is_stale(Some(now + DAY), 1, now));
    }

    #[test]
    fn dates_format_as_year_month_day() {
        assert_eq!(format_date(Some(SystemTime::UNIX_EPOCH)), "1970-01-01");
        let leap = SystemTime::UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(format_date(Some(leap)), "2024-02-29");
        assert_eq!(format_date(None), "-");
    }

    #[test]
    fn areas_are_told_apart_by_location() {
        let c = ctx();
        assert_eq!(
            area_of(Path::new(r"c:\windows\system32"), &c),
            Area::Windows
        );
        assert_eq!(
            area_of(Path::new(r"C:\Program Files (x86)\App"), &c),
            Area::ProgramFiles
        );
        assert_eq!(area_of(Path::new(r"C:\Users\me\Videos"), &c), Area::Profile);
        assert_eq!(area_of(Path::new(r"C:\Users"), &c), Area::Profile);
        assert_eq!(area_of(Path::new(r"C:\WindowsApps"), &c), Area::Other);
        assert_eq!(area_of(Path::new(r"D:\Games"), &c), Area::Other);
    }

    #[test]
    fn breadcrumb_runs_from_the_scanned_location_down() {
        let t = tree_at(r"C:\t");
        assert_eq!(
            breadcrumb(&t, 1),
            vec![(0, r"C:\t".to_string()), (1, "a".to_string())]
        );
        assert_eq!(breadcrumb(&t, 0), vec![(0, r"C:\t".to_string())]);
    }

    #[test]
    fn find_node_walks_names_and_refuses_the_rest() {
        let t = tree_at(r"C:\t");
        assert_eq!(find_node(&t, Path::new(r"C:\t\a\x")), Some(2));
        assert_eq!(find_node(&t, Path::new(r"c:\T\A\X")), Some(2));
        assert_eq!(find_node(&t, Path::new(r"C:\t")), Some(0));
        assert_eq!(find_node(&t, Path::new(r"C:\t\zz")), None);
        assert_eq!(find_node(&t, Path::new(r"D:\t\a")), None);
    }

    #[test]
    fn only_paths_that_left_come_off_the_tree() {
        let mut t = tree_at(r"C:\t");
        let r = DeleteReport {
            removed: vec![PathBuf::from(r"C:\t\a\x")],
            recycled: vec![PathBuf::from(r"C:\t\b")],
            failed: vec![
                Failed {
                    path: PathBuf::from(r"C:\t\a\y"),
                    size: 20,
                    reason: FailReason::GuardKept(r"C:\t\a\.g".into()),
                },
                Failed {
                    path: PathBuf::from(r"C:\t\a"),
                    size: 120,
                    reason: FailReason::InUse,
                },
            ],
            bytes_freed: 100,
            bytes_in_bin: 10,
        };
        assert_eq!(apply_report(&mut t, &r), 2);
        assert_eq!(t.nodes[0].size, 20);
        assert_eq!(t.nodes[1].size, 20);
        assert_eq!(t.nodes[1].children, vec![3]);
        assert_eq!(t.nodes[0].children, vec![1]);
        assert_eq!(t.files, 1);
    }

    #[test]
    fn a_location_that_left_falls_back_to_the_root() {
        let mut t = tree_at(r"C:\t");
        assert_eq!(location_or_root(&t, 1), 1);
        t.remove_subtree(1);
        assert_eq!(location_or_root(&t, 1), 0);
        assert_eq!(location_or_root(&t, 2), 0);
        assert_eq!(location_or_root(&t, 4), 4);
    }

    #[test]
    fn protected_and_unchecked_rows_have_no_checkbox() {
        assert!(!can_select(None));
        assert!(!can_select(Some(&RowFlags {
            protected: true,
            app_owned: false,
            guard: false,
            locked: false,
        })));
        assert!(can_select(Some(&RowFlags {
            protected: false,
            app_owned: true,
            guard: false,
            locked: false,
        })));
    }

    #[test]
    fn request_counts_app_owned_and_drops_what_cannot_be_selected() {
        let t = tree_at(r"C:\t");
        let plain = RowFlags {
            protected: false,
            app_owned: false,
            guard: false,
            locked: false,
        };
        let owned = RowFlags {
            protected: false,
            app_owned: true,
            guard: false,
            locked: false,
        };
        let shielded = RowFlags {
            protected: true,
            app_owned: true,
            guard: false,
            locked: false,
        };
        let flags = HashMap::from([(1, owned), (4, plain), (3, shielded)]);
        // 2 has no flags yet, 3 is protected: neither may reach the request.
        let sel = BTreeSet::from([1, 2, 3, 4]);
        let req = request_from_selection(&t, &sel, &flags, &[], DeleteMode::RecycleBin);
        assert_eq!(req.mode, DeleteMode::RecycleBin);
        assert_eq!(req.context, ConfirmContext::User);
        assert_eq!(req.warn_app_owned, 1);
        let got: Vec<_> = req.items.iter().map(|i| (i.path.clone(), i.size)).collect();
        assert_eq!(
            got,
            [
                (PathBuf::from(r"C:\t\a"), 120),
                (PathBuf::from(r"C:\t\b"), 10)
            ]
        );
    }

    #[test]
    fn nothing_is_selected_on_arrival_and_permanent_needs_asking_for() {
        let t = tree_at(r"C:\t");
        let req = request_from_selection(
            &t,
            &BTreeSet::new(),
            &HashMap::new(),
            &[],
            DeleteMode::Permanent,
        );
        assert!(req.items.is_empty());
        assert_eq!(req.mode, DeleteMode::Permanent);
    }

    #[test]
    fn shallow_tree_gives_other_space_the_same_answer() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("pagefile.sys"), vec![1u8; 4096]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("f"), vec![1u8; 4096]).unwrap();
        let t = scan(
            dir.path(),
            &scan_options(Vec::new()),
            &ScanProgress::default(),
        )
        .unwrap();
        let copy = shallow_tree(&t);
        assert_eq!(copy.nodes.len(), 3);
        let (a, b) = (other_space(&t).unwrap(), other_space(&copy).unwrap());
        assert_eq!(a.scanned, b.scanned);
        assert_eq!(a.system_files, b.system_files);
        assert!(b.system_files[0].in_scan);
    }

    #[test]
    fn scan_options_carry_the_exclusions_to_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let skip = dir.path().join("skip");
        fs::create_dir(&skip).unwrap();
        fs::write(skip.join("f"), vec![1u8; 4096]).unwrap();
        fs::write(dir.path().join("g"), vec![1u8; 4096]).unwrap();
        let t = scan(
            dir.path(),
            &scan_options(vec![skip]),
            &ScanProgress::default(),
        )
        .unwrap();
        assert_eq!(t.nodes[t.root].size, 4096);
        assert_eq!(t.nodes[t.root].children.len(), 1);
    }

    #[test]
    fn hit_testing_finds_the_tile_under_the_pointer() {
        let rect = |x, y, w, h| Rect { x, y, w, h };
        let rects = [rect(0.0, 0.0, 10.0, 10.0), rect(10.0, 0.0, 5.0, 10.0)];
        assert_eq!(tile_at(&rects, 12.0, 5.0), Some(1));
        assert_eq!(tile_at(&rects, 2.0, 5.0), Some(0));
        assert_eq!(tile_at(&rects, 20.0, 5.0), None);
        assert_eq!(tile_at(&[rect(0.0, 0.0, 0.0, 0.0)], 0.0, 0.0), None);
    }

    #[test]
    fn labels_show_only_when_they_fit() {
        assert!(label_fits((40.0, 12.0), (60.0, 30.0)));
        assert!(!label_fits((80.0, 12.0), (60.0, 30.0)));
        assert!(!label_fits((40.0, 40.0), (60.0, 30.0)));
    }

    #[test]
    fn system_items_never_show_unknown_as_zero() {
        let item = |size, in_scan| SystemItem {
            name: "pagefile.sys".into(),
            size,
            in_scan,
        };
        let unknown = system_item_line(&item(None, false));
        assert!(unknown.contains("unknown"));
        assert!(!unknown.contains("0 B"));
        let known = system_item_line(&item(Some(2048), false));
        assert!(known.contains("2.0 KiB"));
        assert!(!known.contains("counted"));
        assert!(system_item_line(&item(Some(2048), true)).contains("already counted"));
    }

    #[test]
    fn browser_roots_cover_the_main_browsers() {
        let roots = browser_profile_roots(Some(Path::new(r"C:\L")), Some(Path::new(r"C:\R")));
        assert!(roots.contains(&PathBuf::from(r"C:\L\Google\Chrome\User Data")));
        assert!(roots.contains(&PathBuf::from(r"C:\L\Microsoft\Edge\User Data")));
        assert!(roots.contains(&PathBuf::from(r"C:\R\Mozilla\Firefox\Profiles")));
        assert!(browser_profile_roots(None, None).is_empty());
    }

    #[test]
    fn hover_shows_the_full_name_size_date_and_marks() {
        let lines = hover_lines(
            r"C:\t\a very long name",
            2048,
            Some(SystemTime::UNIX_EPOCH),
            true,
            true,
        )
        .join("\n");
        assert!(lines.contains(r"C:\t\a very long name"));
        assert!(lines.contains("2.0 KiB"));
        assert!(lines.contains("1970-01-01"));
        assert!(lines.contains("in the cloud"));
        assert!(lines.contains("used by an app"));
        let plain = hover_lines("n", 1, None, false, false).join("\n");
        assert!(!plain.contains("in the cloud"));
        assert!(!plain.contains("used by an app"));
    }

    #[test]
    fn flags_mark_protected_and_app_owned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("MyApp");
        fs::create_dir(&app).unwrap();
        let entry = katharistis_core::apps::AppEntry {
            key: "k".into(),
            name: "MyApp".into(),
            publisher: None,
            version: None,
            install_date: None,
            size: 0,
            install_location: Some(app.clone()),
            uninstall: None,
        };
        let c = ctx();
        let marks = UsageMarks::with_context(&[entry], Vec::new(), &c);
        let inside = compute_flags(&app.join("data.bin"), &c, &marks);
        assert!(inside.app_owned);
        let outside = compute_flags(&dir.path().join("other.bin"), &c, &marks);
        assert!(!outside.app_owned);
        assert!(compute_flags(Path::new(r"C:\Windows\x"), &c, &marks).protected);
    }

    fn run_frame(ctx: &egui::Context, draw: &mut dyn FnMut(&mut egui::Ui)) {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| draw(ui));
        out.textures_delta.clear();
    }

    fn state(ctx: &egui::Context, c: &ProtectContext) -> HomeState {
        let mut st = HomeState::new(ctx, c);
        st.marks = Some(UsageMarks::default());
        st.marks_job = None;
        st
    }

    #[test]
    fn drawing_a_tree_asks_for_nothing_and_selects_nothing() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        for _ in 0..3 {
            run_frame(&ctx_ui, &mut |ui| {
                assert!(show(ui, &mut st, Some(&t), 365, &c).is_none());
            });
        }
        assert!(st.selection.is_empty());
        assert_eq!(st.pane, Pane::Map);
        // The flags of rows are kept between frames, not worked out again.
        let view = st.view.as_ref().unwrap();
        assert_eq!(view.id, t.root);
        assert_eq!(view.flags.len(), 2);
    }

    #[test]
    fn a_frame_computes_only_a_limited_number_of_rows() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        for i in 0..50 {
            let id = t.nodes.len();
            t.nodes
                .push(node(&format!("f{i}"), 1, EntryClass::File, Some(0)));
            t.nodes[0].children.push(id);
        }
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        run_frame(&ctx_ui, &mut |ui| {
            show(ui, &mut st, Some(&t), 365, &c);
        });
        let seen = st.view.as_ref().unwrap().flags.len();
        assert!((1..=FLAG_ROWS_PER_FRAME).contains(&seen), "{seen}");
    }

    #[test]
    fn rows_stay_without_flags_until_the_app_list_is_in() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.marks = None;
        st.location = t.root;
        run_frame(&ctx_ui, &mut |ui| {
            show(ui, &mut st, Some(&t), 365, &c);
        });
        assert!(st.view.as_ref().unwrap().flags.is_empty());
    }

    #[test]
    fn every_pane_draws() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let t = tree_at(r"C:\");
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        st.pane = Pane::FullList;
        run_frame(&ctx_ui, &mut |ui| {
            assert!(show(ui, &mut st, Some(&t), 365, &c).is_none());
        });
        st.pane = Pane::OtherSpace;
        st.other = OtherLoad::Ready(OtherSpace {
            used: 10,
            scanned: 5,
            unaccounted: 5,
            system_files: vec![SystemItem {
                name: "pagefile.sys".into(),
                size: None,
                in_scan: false,
            }],
            unreadable: vec![ScanError {
                path: PathBuf::from(r"C:\x"),
                reason: "denied".into(),
            }],
        });
        run_frame(&ctx_ui, &mut |ui| {
            assert!(show(ui, &mut st, Some(&t), 365, &c).is_none());
        });
    }

    #[test]
    fn a_stale_map_yields_no_delete_action() {
        let t = tree_at(r"C:\t");
        let plain = RowFlags {
            protected: false,
            app_owned: false,
            guard: false,
            locked: false,
        };
        let flags = HashMap::from([(1, plain)]);
        let sel = BTreeSet::from([1]);
        assert!(can_delete(&sel, true, false));
        assert!(!can_delete(&sel, true, true));
        assert!(!can_delete(&sel, false, false));
        assert!(!can_delete(&BTreeSet::new(), true, false));
        for mode in [DeleteMode::RecycleBin, DeleteMode::Permanent] {
            let allowed = can_delete(&sel, true, true);
            assert!(delete_action(&t, &flags, &[], &sel, Some(mode), allowed).is_none());
            let allowed = can_delete(&sel, true, false);
            assert!(matches!(
                delete_action(&t, &flags, &[], &sel, Some(mode), allowed),
                Some(HomeAction::Delete(_))
            ));
            assert!(delete_action(&t, &flags, &[], &sel, None, allowed).is_none());
        }
    }

    #[test]
    fn a_stale_map_drops_ticks_and_asks_for_nothing() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        st.selection.insert(1);
        // A first-step panic: no report arrives, so only this reaches the ticks.
        st.mark_stale();
        assert!(st.selection.is_empty());
        st.selection.insert(1);
        run_frame(&ctx_ui, &mut |ui| {
            assert!(show(ui, &mut st, Some(&t), 365, &c).is_none());
        });
        assert!(st.selection.is_empty());
        assert!(st.map_stale);
    }

    #[test]
    fn a_partial_failure_stops_the_retry_that_would_count_twice() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        // 100 of a's 120 went, and the true remainder is unknown to the map.
        let partial = DeleteReport {
            failed: vec![in_use(r"C:\t\a", 120)],
            bytes_freed: 100,
            ..Default::default()
        };
        st.after_report(&mut t, &partial);
        assert!(!st.map_stale);

        // The folder, its contents, and the root above it: none can be asked for again.
        for id in [t.root, 1, 2, 3] {
            assert!(st.locks.is_locked(&t, id), "{id} is not locked");
            let flags = HashMap::from([(id, row(&st.locks, &t, id))]);
            assert!(!can_select(flags.get(&id)));
            for mode in [DeleteMode::RecycleBin, DeleteMode::Permanent] {
                let sel = BTreeSet::from([id]);
                assert!(delete_action(&t, &flags, &[], &sel, Some(mode), true).is_none());
            }
        }
        // So nothing can add to the 100 already claimed from this folder.
        let everything = BTreeSet::from([1, 2, 3]);
        let flags: HashMap<_, _> = [1, 2, 3].map(|id| (id, row(&st.locks, &t, id))).into();
        let req = request_from_selection(&t, &everything, &flags, &[], DeleteMode::Permanent);
        assert_eq!(req.items.iter().map(|i| i.size).sum::<u64>(), 0);
    }

    fn in_use(path: &str, size: u64) -> Failed {
        Failed {
            path: PathBuf::from(path),
            size,
            reason: FailReason::InUse,
        }
    }

    fn row(locks: &Locks, t: &ScanTree, id: NodeId) -> RowFlags {
        RowFlags {
            protected: false,
            app_owned: false,
            guard: false,
            locked: locks.is_locked(t, id),
        }
    }

    #[test]
    fn a_failure_locks_its_path_and_folders_above_and_leaves_the_rest() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = 1;
        let r = DeleteReport {
            failed: vec![in_use(r"C:\t\a\x", 100)],
            ..Default::default()
        };
        st.after_report(&mut t, &r);
        assert!(!st.map_stale, "an ordinary failure leaves the map usable");

        assert!(st.locks.is_locked(&t, 2), "the failed path");
        assert!(st.locks.is_locked(&t, 1), "the folder above it");
        assert!(!st.locks.is_locked(&t, 3), "a sibling");
        assert!(!st.locks.is_locked(&t, 4), "a file elsewhere");

        // The sibling still gets a checkbox and still leads to a delete.
        let flags = HashMap::from([(3, row(&st.locks, &t, 3)), (2, row(&st.locks, &t, 2))]);
        assert!(can_select(flags.get(&3)));
        let sel = BTreeSet::from([3]);
        let allowed = can_delete(&sel, true, st.map_stale);
        let Some(HomeAction::Delete(req)) =
            delete_action(&t, &flags, &[], &sel, Some(DeleteMode::RecycleBin), allowed)
        else {
            panic!("expected a delete for the sibling");
        };
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].path, Path::new(r"C:\t\a\y"));

        // Ticking the locked path yields nothing.
        let locked = BTreeSet::from([2]);
        let allowed = can_delete(&locked, true, st.map_stale);
        assert!(delete_action(
            &t,
            &flags,
            &[],
            &locked,
            Some(DeleteMode::Permanent),
            allowed
        )
        .is_none());
    }

    #[test]
    fn a_failure_keeps_other_ticks_and_drops_locked_and_gone_ones() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = 1;
        st.selection.extend([2, 3]);
        let r = DeleteReport {
            failed: vec![in_use(r"C:\t\a\x", 100)],
            ..Default::default()
        };
        st.after_report(&mut t, &r);
        assert_eq!(st.selection, BTreeSet::from([3]));

        let gone = DeleteReport {
            recycled: vec![PathBuf::from(r"C:\t\a\y")],
            bytes_in_bin: 20,
            ..Default::default()
        };
        st.after_report(&mut t, &gone);
        assert!(st.selection.is_empty());
        assert_eq!(st.location, 1);
    }

    #[test]
    fn locks_reach_the_row_when_it_is_drawn_and_end_with_the_scan() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = 1;
        for _ in 0..2 {
            run_frame(&ctx_ui, &mut |ui| {
                show(ui, &mut st, Some(&t), 365, &c);
            });
        }
        assert!(!st.view.as_ref().unwrap().flags[&2].locked);
        let r = DeleteReport {
            failed: vec![in_use(r"C:\t\a\x", 100)],
            ..Default::default()
        };
        st.after_report(&mut t, &r);
        for _ in 0..2 {
            run_frame(&ctx_ui, &mut |ui| {
                show(ui, &mut st, Some(&t), 365, &c);
            });
        }
        let view = st.view.as_ref().unwrap();
        assert!(view.flags[&2].locked);
        assert!(!view.flags[&3].locked);
        let n = node("x", 100, EntryClass::File, Some(1));
        assert!(row_marks(&n, Some(&view.flags[&2])).contains("scan again"));

        let dir = tempfile::tempdir().unwrap();
        let mut none = None;
        st.start_scan(&ctx_ui, dir.path().to_path_buf(), Vec::new(), &mut none);
        assert!(!st.locks.is_locked(&t, 2));
    }

    #[test]
    fn a_guard_still_makes_the_whole_map_stale() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        st.selection.insert(4);
        let r = DeleteReport {
            failed: vec![Failed {
                path: PathBuf::from(r"C:\t\a"),
                size: 120,
                reason: FailReason::GuardKept(r"C:\t\a\.g".into()),
            }],
            ..Default::default()
        };
        st.after_report(&mut t, &r);
        assert!(st.map_stale);
        assert!(st.selection.is_empty());
        assert!(!can_delete(&BTreeSet::from([4]), true, st.map_stale));
    }

    #[test]
    fn guard_rows_have_no_checkbox_and_a_plain_mark() {
        let c = ctx();
        let dir = tempfile::tempdir().unwrap();
        let guard = dir.path().join(".katharistis-guard-123-0");
        let flags = compute_flags(&guard, &c, &UsageMarks::default());
        assert!(flags.guard);
        assert!(!flags.protected);
        assert!(!can_select(Some(&flags)));
        assert!(!compute_flags(&dir.path().join("a.bin"), &c, &UsageMarks::default()).guard);

        let n = node(".katharistis-guard-123-0", 5, EntryClass::File, Some(0));
        assert!(row_marks(&n, Some(&flags)).contains("guard file"));

        // Not through a stale selection either.
        let t = tree_at(r"C:\t");
        let by_id = HashMap::from([(4, flags)]);
        let req =
            request_from_selection(&t, &BTreeSet::from([4]), &by_id, &[], DeleteMode::Permanent);
        assert!(req.items.is_empty());
    }

    #[test]
    fn a_folder_holding_an_install_is_marked_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let games = dir.path().join("Games");
        let entry = katharistis_core::apps::AppEntry {
            key: "k".into(),
            name: "App".into(),
            publisher: None,
            version: None,
            install_date: None,
            size: 0,
            install_location: Some(games.join("App")),
            uninstall: None,
        };
        let c = ctx();
        let marks = UsageMarks::with_context(&[entry], Vec::new(), &c);
        let holding = compute_flags(&games, &c, &marks);
        assert!(holding.app_owned);
        assert!(can_select(Some(&holding)));
        assert!(!compute_flags(&dir.path().join("Gam"), &c, &marks).app_owned);

        let t = tree_at(r"C:\t");
        let by_id = HashMap::from([(1, holding)]);
        let req = request_from_selection(
            &t,
            &BTreeSet::from([1]),
            &by_id,
            &[],
            DeleteMode::RecycleBin,
        );
        assert_eq!(req.warn_app_owned, 1);
        let hover = hover_lines("n", 1, None, false, holding.app_owned).join("\n");
        assert!(hover.contains("used by an app"));
    }

    #[test]
    fn the_gap_button_is_there_for_any_drive_scan_and_no_folder_scan() {
        assert!(offers_gap_panel(&tree_at(r"C:\")));
        assert!(!offers_gap_panel(&tree_at(r"C:\t")));
    }

    #[test]
    fn the_gap_panel_opens_without_losing_the_place_or_the_ticks() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let dir = tempfile::tempdir().unwrap();
        let t = tree_at(dir.path().to_str().unwrap());
        let mut st = state(&ctx_ui, &c);
        st.location = 1;
        st.selection.insert(2);
        st.open_other_space(&ctx_ui, &t);
        assert_eq!(st.pane, Pane::OtherSpace);
        assert_eq!(st.location, 1);
        assert!(st.selection.contains(&2));
    }

    #[test]
    fn a_report_clears_the_selection_and_leaves_a_removed_location() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = 1;
        st.selection.insert(2);
        let r = DeleteReport {
            recycled: vec![PathBuf::from(r"C:\t\a")],
            bytes_in_bin: 120,
            ..Default::default()
        };
        st.after_report(&mut t, &r);
        assert!(st.selection.is_empty());
        assert_eq!(st.location, 0);
        assert!(st.view.is_none());
        assert!(!st.map_stale);
        assert_eq!(t.nodes[0].size, 10);
    }

    #[test]
    fn a_guard_kept_report_flags_the_map_and_keeps_the_node() {
        let ctx_ui = egui::Context::default();
        let c = ctx();
        let mut t = tree_at(r"C:\t");
        let mut st = state(&ctx_ui, &c);
        st.location = t.root;
        let r = DeleteReport {
            failed: vec![Failed {
                path: PathBuf::from(r"C:\t\b"),
                size: 10,
                reason: FailReason::GuardKept(r"C:\t\.g".into()),
            }],
            ..Default::default()
        };
        st.after_report(&mut t, &r);
        assert!(st.map_stale);
        assert_eq!(t.nodes[0].size, 130);
    }
}
