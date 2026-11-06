//! What the cleanup screen offers to delete. Nothing is deleted here.

use crate::recycle;
use crate::scan::{self, EntryClass, ScanOptions, ScanProgress, ScanTree};
use crate::win;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const RECENT: Duration = Duration::from_secs(24 * 60 * 60);
const PARTIAL_EXTENSIONS: [&str; 3] = ["crdownload", "part", "partial"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JunkKind {
    Temp,
    BrowserCache,
    CrashLogs,
    DevData,
    GeneralCache,
    PartialDownloads,
    RecycleBin,
}

#[derive(Clone, Debug)]
pub struct JunkItem {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub checked: bool,
}

#[derive(Clone, Debug)]
pub struct JunkCategory {
    pub kind: JunkKind,
    pub items: Vec<JunkItem>,
    /// A location exists but could not be listed. A missing one is just empty.
    pub unreadable: bool,
    /// Only for `RecycleBin`, which has no items.
    pub bin_size: u64,
}

#[derive(Clone, Debug)]
pub struct JunkRoots {
    pub temp: PathBuf,
    pub local_appdata: PathBuf,
    pub profile: PathBuf,
    pub downloads: PathBuf,
}

impl JunkRoots {
    /// An unset variable gives an empty path, which `scan_junk` treats as missing.
    pub fn from_env() -> Self {
        let var = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_default()
        };
        let profile = var("USERPROFILE");
        let downloads = if profile.as_os_str().is_empty() {
            PathBuf::new()
        } else {
            profile.join("Downloads")
        };
        JunkRoots {
            temp: var("TEMP"),
            local_appdata: var("LOCALAPPDATA"),
            profile,
            downloads,
        }
    }
}

struct Found {
    items: Vec<JunkItem>,
    unreadable: bool,
}

impl Found {
    fn new() -> Self {
        Found {
            items: Vec::new(),
            unreadable: false,
        }
    }

    fn into_category(self, kind: JunkKind) -> JunkCategory {
        JunkCategory {
            kind,
            items: self.items,
            unreadable: self.unreadable,
            bin_size: 0,
        }
    }
}

fn options() -> ScanOptions {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    ScanOptions {
        threads,
        excluded: Vec::new(),
    }
}

/// Not there, or not a folder. The OS code may arrive as an HRESULT
/// (`0x8007xxxx`) from the Windows API wrappers.
fn is_missing(e: &io::Error) -> bool {
    const FILE_NOT_FOUND: i32 = 2;
    const PATH_NOT_FOUND: i32 = 3;
    const NOT_A_DIRECTORY: i32 = 267;
    let Some(code) = e.raw_os_error() else {
        return e.kind() == io::ErrorKind::NotFound;
    };
    let win32 = if code as u32 & 0xFFFF_0000 == 0x8007_0000 {
        code & 0xFFFF
    } else {
        code
    };
    matches!(win32, FILE_NOT_FOUND | PATH_NOT_FOUND | NOT_A_DIRECTORY)
}

/// Sizes a folder with the disk map's scan. `Ok(None)` when it does not exist.
fn scan_folder(dir: &Path, progress: &ScanProgress) -> io::Result<Option<ScanTree>> {
    if dir.as_os_str().is_empty() {
        return Ok(None);
    }
    match scan::scan(dir, &options(), progress) {
        Ok(tree) => Ok(Some(tree)),
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Lists a folder, `Ok(None)` when it does not exist.
fn list(dir: &Path) -> io::Result<Option<Vec<win::DirEntry>>> {
    if dir.as_os_str().is_empty() {
        return Ok(None);
    }
    match win::list_dir(dir) {
        Ok(entries) => Ok(Some(entries)),
        Err(e) if is_missing(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The children of `dir` as items. Links are skipped.
fn add_children(
    found: &mut Found,
    dir: &Path,
    progress: &ScanProgress,
    checked: impl Fn(Option<SystemTime>) -> bool,
) {
    match scan_folder(dir, progress) {
        Ok(None) => {}
        Err(_) => found.unreadable = true,
        Ok(Some(tree)) => {
            for &id in &tree.nodes[tree.root].children {
                let node = &tree.nodes[id];
                if node.class == EntryClass::Link {
                    continue;
                }
                found.items.push(JunkItem {
                    path: dir.join(&node.name),
                    name: node.name.clone(),
                    size: node.size,
                    checked: checked(node.modified),
                });
            }
        }
    }
}

/// The folder `dir` itself as one item, named `label`.
fn add_folder(found: &mut Found, dir: &Path, label: String, progress: &ScanProgress) {
    match scan_folder(dir, progress) {
        Ok(None) => {}
        Err(_) => found.unreadable = true,
        Ok(Some(tree)) => found.items.push(JunkItem {
            path: dir.to_path_buf(),
            name: label,
            size: tree.nodes[tree.root].size,
            checked: true,
        }),
    }
}

/// Real folders directly under `dir`, links excluded.
fn subdirs(found: &mut Found, dir: &Path) -> Vec<String> {
    match list(dir) {
        Ok(None) => Vec::new(),
        Err(_) => {
            found.unreadable = true;
            Vec::new()
        }
        Ok(Some(entries)) => entries
            .into_iter()
            .filter(|e| scan::classify(e.attrs, e.reparse_tag) == EntryClass::Dir)
            .map(|e| e.name)
            .collect(),
    }
}

fn is_recent(modified: Option<SystemTime>, now: SystemTime) -> bool {
    match modified {
        None => true,
        Some(m) => now.duration_since(m).map_or(true, |age| age < RECENT),
    }
}

fn temp(roots: &JunkRoots, now: SystemTime, progress: &ScanProgress) -> Found {
    let mut found = Found::new();
    add_children(&mut found, &roots.temp, progress, |m| !is_recent(m, now));
    found
}

fn browser_cache(roots: &JunkRoots, progress: &ScanProgress) -> Found {
    let mut found = Found::new();
    let chromium = [
        (
            "Chrome",
            roots.local_appdata.join(r"Google\Chrome\User Data"),
        ),
        (
            "Edge",
            roots.local_appdata.join(r"Microsoft\Edge\User Data"),
        ),
    ];
    for (browser, data) in chromium {
        for profile in subdirs(&mut found, &data) {
            for cache in ["Cache", "Code Cache"] {
                if progress.is_cancelled() {
                    return found;
                }
                let dir = data.join(&profile).join(cache);
                add_folder(
                    &mut found,
                    &dir,
                    format!("{browser} {profile} {cache}"),
                    progress,
                );
            }
        }
    }
    let firefox = roots.local_appdata.join(r"Mozilla\Firefox\Profiles");
    for profile in subdirs(&mut found, &firefox) {
        if progress.is_cancelled() {
            return found;
        }
        let dir = firefox.join(&profile).join("cache2");
        add_folder(
            &mut found,
            &dir,
            format!("Firefox {profile} cache2"),
            progress,
        );
    }
    found
}

fn crash_logs(roots: &JunkRoots, progress: &ScanProgress) -> Found {
    let mut found = Found::new();
    let dirs = [
        roots.local_appdata.join("CrashDumps"),
        roots.local_appdata.join(r"Microsoft\Windows\WER"),
    ];
    for dir in dirs {
        if progress.is_cancelled() {
            break;
        }
        add_children(&mut found, &dir, progress, |_| true);
    }
    found
}

fn dev_data(roots: &JunkRoots, progress: &ScanProgress) -> Found {
    let mut found = Found::new();
    let dirs = [
        ("npm cache", roots.local_appdata.join(r"npm-cache\_cacache")),
        ("pip cache", roots.local_appdata.join(r"pip\Cache")),
        ("Gradle caches", roots.profile.join(r".gradle\caches")),
        (
            "NuGet http-cache",
            roots.local_appdata.join(r"NuGet\v3-cache"),
        ),
    ];
    for (label, dir) in dirs {
        if progress.is_cancelled() {
            break;
        }
        add_folder(&mut found, &dir, label.to_string(), progress);
    }
    found
}

fn general_cache(roots: &JunkRoots, progress: &ScanProgress) -> Found {
    let mut found = Found::new();
    if !roots.profile.as_os_str().is_empty() {
        add_children(&mut found, &roots.profile.join(".cache"), progress, |_| {
            false
        });
    }
    found
}

fn partial_downloads(roots: &JunkRoots) -> Found {
    let mut found = Found::new();
    match list(&roots.downloads) {
        Ok(None) => {}
        Err(_) => found.unreadable = true,
        Ok(Some(entries)) => {
            for e in entries {
                let is_partial = Path::new(&e.name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| PARTIAL_EXTENSIONS.iter().any(|p| x.eq_ignore_ascii_case(p)));
                if !is_partial || scan::classify(e.attrs, e.reparse_tag) != EntryClass::File {
                    continue;
                }
                let path = roots.downloads.join(&e.name);
                let size = match win::allocated_size(&path) {
                    Ok(size) => size,
                    Err(_) => {
                        found.unreadable = true;
                        continue;
                    }
                };
                found.items.push(JunkItem {
                    path,
                    name: e.name,
                    size,
                    checked: true,
                });
            }
        }
    }
    found
}

/// Finds the cleanup categories. Always returns all seven, in enum order. With
/// `progress` cancelled it stops early and returns what it has found so far.
pub fn scan_junk(roots: &JunkRoots, now: SystemTime, progress: &ScanProgress) -> Vec<JunkCategory> {
    let steps: [(JunkKind, &dyn Fn() -> Found); 6] = [
        (JunkKind::Temp, &|| temp(roots, now, progress)),
        (JunkKind::BrowserCache, &|| browser_cache(roots, progress)),
        (JunkKind::CrashLogs, &|| crash_logs(roots, progress)),
        (JunkKind::DevData, &|| dev_data(roots, progress)),
        (JunkKind::GeneralCache, &|| general_cache(roots, progress)),
        (JunkKind::PartialDownloads, &|| partial_downloads(roots)),
    ];
    let mut out = Vec::new();
    for (kind, step) in steps {
        let found = if progress.is_cancelled() {
            Found::new()
        } else {
            step()
        };
        out.push(found.into_category(kind));
    }
    let mut bin = Found::new().into_category(JunkKind::RecycleBin);
    if !progress.is_cancelled() {
        bin.bin_size = recycle::bin_info().map_or(0, |(bytes, _)| bytes);
    }
    out.push(bin);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    struct Fixture {
        _dir: tempfile::TempDir,
        roots: JunkRoots,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let roots = JunkRoots {
            temp: base.join("Temp"),
            local_appdata: base.join("Local"),
            profile: base.join("User"),
            downloads: base.join("User").join("Downloads"),
        };
        Fixture { _dir: dir, roots }
    }

    fn write(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![7u8; bytes]).unwrap();
    }

    fn run(f: &Fixture, now: SystemTime) -> Vec<JunkCategory> {
        scan_junk(&f.roots, now, &ScanProgress::default())
    }

    fn cat(all: &[JunkCategory], kind: JunkKind) -> &JunkCategory {
        all.iter().find(|c| c.kind == kind).unwrap()
    }

    fn names(c: &JunkCategory) -> Vec<String> {
        let mut n: Vec<_> = c.items.iter().map(|i| i.name.clone()).collect();
        n.sort();
        n
    }

    fn far_future() -> SystemTime {
        SystemTime::now() + Duration::from_secs(10 * 24 * 3600)
    }

    #[test]
    fn browser_cache_only_never_profile_data() {
        let f = fixture();
        let l = &f.roots.local_appdata;
        let data = l.join(r"Google\Chrome\User Data");
        write(&data.join(r"Default\Cache\x"), 100);
        write(&data.join(r"Default\Code Cache\y"), 100);
        write(&data.join(r"Default\Login Data"), 100);
        write(&data.join(r"Default\Bookmarks"), 100);
        write(&data.join("Local State"), 100);
        write(&l.join(r"Microsoft\Edge\User Data\Profile 1\Cache\z"), 100);
        write(
            &l.join(r"Mozilla\Firefox\Profiles\abc.default\cache2\e"),
            100,
        );
        write(
            &l.join(r"Mozilla\Firefox\Profiles\abc.default\places.sqlite"),
            100,
        );

        let all = run(&f, far_future());
        let c = cat(&all, JunkKind::BrowserCache);
        assert_eq!(c.items.len(), 4);
        assert!(!c.unreadable);
        let paths: Vec<_> = c.items.iter().map(|i| i.path.clone()).collect();
        assert!(paths.contains(&data.join(r"Default\Cache")));
        assert!(paths.contains(&data.join(r"Default\Code Cache")));
        assert!(paths.contains(&l.join(r"Microsoft\Edge\User Data\Profile 1\Cache")));
        assert!(paths.contains(&l.join(r"Mozilla\Firefox\Profiles\abc.default\cache2")));
        for i in &c.items {
            assert!(i.checked);
            assert!(i.size > 0);
            let last = i.path.file_name().unwrap().to_str().unwrap();
            assert!(["Cache", "Code Cache", "cache2"].contains(&last));
        }
    }

    #[test]
    fn general_cache_unchecked_others_checked() {
        let f = fixture();
        let r = &f.roots;
        write(&r.profile.join(r".cache\tool\a"), 10);
        write(&r.temp.join("old.tmp"), 10);
        write(&r.local_appdata.join(r"CrashDumps\a.dmp"), 10);
        write(
            &r.local_appdata.join(r"Microsoft\Windows\WER\ReportQueue\r"),
            10,
        );
        write(&r.local_appdata.join(r"npm-cache\_cacache\n"), 10);
        write(&r.local_appdata.join(r"pip\Cache\p"), 10);
        write(&r.profile.join(r".gradle\caches\g"), 10);
        write(&r.local_appdata.join(r"NuGet\v3-cache\v"), 10);
        write(&r.downloads.join("a.part"), 10);

        let all = run(&f, far_future());
        assert_eq!(all.len(), 7);
        let g = cat(&all, JunkKind::GeneralCache);
        assert_eq!(names(g), ["tool"]);
        assert!(g.items.iter().all(|i| !i.checked));
        for kind in [
            JunkKind::Temp,
            JunkKind::CrashLogs,
            JunkKind::DevData,
            JunkKind::PartialDownloads,
        ] {
            let c = cat(&all, kind);
            assert!(!c.items.is_empty(), "{kind:?}");
            assert!(c.items.iter().all(|i| i.checked), "{kind:?}");
        }
        assert_eq!(
            names(cat(&all, JunkKind::CrashLogs)),
            ["ReportQueue", "a.dmp"]
        );
        assert_eq!(
            names(cat(&all, JunkKind::DevData)),
            [
                "Gradle caches",
                "NuGet http-cache",
                "npm cache",
                "pip cache"
            ]
        );
    }

    #[test]
    fn recent_temp_items_unchecked() {
        let f = fixture();
        write(&f.roots.temp.join("fresh.tmp"), 10);
        write(&f.roots.temp.join(r"dir\inner"), 10);
        let now = SystemTime::now();

        let all = run(&f, now);
        let t = cat(&all, JunkKind::Temp);
        assert_eq!(t.items.len(), 2);
        assert!(t.items.iter().all(|i| !i.checked));

        let later = now + Duration::from_secs(25 * 3600);
        let all = run(&f, later);
        assert!(cat(&all, JunkKind::Temp).items.iter().all(|i| i.checked));

        let almost = now + Duration::from_secs(23 * 3600);
        let all = run(&f, almost);
        assert!(cat(&all, JunkKind::Temp).items.iter().all(|i| !i.checked));
    }

    #[test]
    fn unknown_or_future_date_counts_as_recent() {
        let now = SystemTime::now();
        assert!(is_recent(None, now));
        assert!(is_recent(Some(now + Duration::from_secs(60)), now));
        assert!(is_recent(Some(now - Duration::from_secs(3600)), now));
        assert!(!is_recent(Some(now - RECENT), now));
    }

    #[test]
    fn downloads_only_partial_extensions() {
        let f = fixture();
        let d = &f.roots.downloads;
        write(&d.join("a.part"), 10);
        write(&d.join("b.zip"), 10);
        write(&d.join("c.CRDOWNLOAD"), 10);
        write(&d.join("d.partial"), 10);
        write(&d.join("part"), 10);
        write(&d.join("e.part.zip"), 10);
        fs::create_dir_all(d.join("dir.part")).unwrap();

        let all = run(&f, far_future());
        let c = cat(&all, JunkKind::PartialDownloads);
        assert_eq!(names(c), ["a.part", "c.CRDOWNLOAD", "d.partial"]);
        assert!(c.items.iter().all(|i| i.checked && i.size > 0));
    }

    #[test]
    fn missing_folders_are_empty_not_errors() {
        let f = fixture();
        let all = run(&f, far_future());
        assert_eq!(all.len(), 7);
        for c in &all {
            assert!(c.items.is_empty(), "{:?}", c.kind);
            assert!(!c.unreadable, "{:?}", c.kind);
        }
    }

    #[test]
    fn empty_roots_scan_nothing() {
        let roots = JunkRoots {
            temp: PathBuf::new(),
            local_appdata: PathBuf::new(),
            profile: PathBuf::new(),
            downloads: PathBuf::new(),
        };
        let all = scan_junk(&roots, SystemTime::now(), &ScanProgress::default());
        assert!(all.iter().all(|c| c.items.is_empty() && !c.unreadable));
    }

    fn junction(link: &Path, target: &Path) -> bool {
        Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap()
            .status
            .success()
    }

    #[test]
    fn link_inside_temp_is_ignored() {
        let f = fixture();
        let outside = f.roots.temp.parent().unwrap().join("outside");
        write(&outside.join("big"), 4096);
        write(&f.roots.temp.join("real.tmp"), 10);
        assert!(junction(&f.roots.temp.join("jump"), &outside));

        let all = run(&f, far_future());
        let t = cat(&all, JunkKind::Temp);
        assert_eq!(names(t), ["real.tmp"]);
        assert!(outside.join("big").exists());
    }

    #[test]
    fn link_as_browser_profile_or_cache_is_not_followed() {
        let f = fixture();
        let data = f.roots.local_appdata.join(r"Google\Chrome\User Data");
        let outside = f.roots.temp.parent().unwrap().join("outside");
        write(&outside.join(r"Cache\x"), 4096);
        fs::create_dir_all(data.join("Real")).unwrap();
        assert!(junction(&data.join("Linked"), &outside));
        assert!(junction(&data.join(r"Real\Cache"), &outside.join("Cache")));

        let all = run(&f, far_future());
        let c = cat(&all, JunkKind::BrowserCache);
        assert!(c
            .items
            .iter()
            .all(|i| !i.path.starts_with(data.join("Linked"))));
    }

    #[test]
    fn cancelled_scan_returns_what_exists() {
        let f = fixture();
        write(&f.roots.temp.join("a"), 10);
        let progress = ScanProgress::default();
        progress.cancel();
        let all = scan_junk(&f.roots, far_future(), &progress);
        assert_eq!(all.len(), 7);
        assert!(all.iter().all(|c| c.items.is_empty()));
    }

    #[test]
    fn sizes_match_scan() {
        let f = fixture();
        write(&f.roots.temp.join(r"d\a"), 100_000);
        write(&f.roots.temp.join(r"d\b"), 5);
        let all = run(&f, far_future());
        let item = &cat(&all, JunkKind::Temp).items[0];
        let tree = scan::scan(&f.roots.temp, &options(), &ScanProgress::default()).unwrap();
        let d = tree.nodes[tree.root].children[0];
        assert_eq!(item.size, tree.nodes[d].size);
    }

    #[test]
    fn recycle_bin_has_no_items() {
        let f = fixture();
        let all = run(&f, far_future());
        let bin = cat(&all, JunkKind::RecycleBin);
        assert!(bin.items.is_empty());
        assert!(!bin.unreadable);
    }
}
