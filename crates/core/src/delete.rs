//! The only way anything gets deleted. Every target passes the protection gate
//! here first, whichever mode it is deleted in.

use std::io;
use std::path::{Path, PathBuf};

use crate::protect::{canonical_for_delete, is_protected, lexical_spelling, ProtectContext};
use crate::recycle::{recycle_one, RecycleOutcome};
use crate::scan::{self, classify, EntryClass};
use crate::win::{self, DirEntry};

const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteMode {
    RecycleBin,
    Permanent,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailReason {
    Protected,
    NotFound,
    InUse,
    AccessDenied,
    NotRecyclable,
    /// The shell deleted the file and it could not be put back under its name. The
    /// data is safe, at this path.
    GuardKept(PathBuf),
    Other(String),
}

#[derive(Clone, Debug)]
pub struct Failed {
    pub path: PathBuf,
    pub size: u64,
    pub reason: FailReason,
}

#[derive(Debug, Default)]
pub struct DeleteReport {
    pub removed: Vec<PathBuf>,
    pub recycled: Vec<PathBuf>,
    pub failed: Vec<Failed>,
    pub bytes_freed: u64,
    pub bytes_in_bin: u64,
}

/// Accepts a Win32 code or the `HRESULT` that wraps one, since `windows` errors
/// turned into `io::Error` carry the latter.
pub(crate) fn reason_from_os_code(code: i32) -> FailReason {
    let win32 = if code as u32 & 0xFFFF_0000 == 0x8007_0000 {
        code & 0xFFFF
    } else {
        code
    };
    match win32 {
        32 | 33 => FailReason::InUse,
        5 => FailReason::AccessDenied,
        2 | 3 => FailReason::NotFound,
        _ => FailReason::Other(io::Error::from_raw_os_error(code).to_string()),
    }
}

fn reason_from_io(err: &io::Error) -> FailReason {
    match err.raw_os_error() {
        Some(code) => reason_from_os_code(code),
        None => FailReason::Other(err.to_string()),
    }
}

/// Keeps what to tell the user about a partial delete: the first problem, unless
/// a file turns out to be in use, which is the one the user can do something about.
#[derive(Default)]
struct Problem(Option<FailReason>);

impl Problem {
    fn note(&mut self, err: &io::Error) {
        let reason = reason_from_io(err);
        // Gone by the time we got there: what was wanted has happened.
        if reason == FailReason::NotFound {
            return;
        }
        if self.0.is_none() || reason == FailReason::InUse {
            self.0 = Some(reason);
        }
    }
}

/// Removes one file, link or empty directory, without entering it.
fn remove_entry(path: &Path, attrs: u32) -> io::Result<()> {
    let remove = || {
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            win::remove_directory(path)
        } else {
            win::delete_file(path)
        }
    };
    let first = remove();
    if first.is_err()
        && attrs & FILE_ATTRIBUTE_READONLY != 0
        && win::set_attributes(path, attrs & !FILE_ATTRIBUTE_READONLY).is_ok()
    {
        return remove();
    }
    first
}

enum Step {
    Visit(PathBuf, DirEntry),
    RemoveEmptied(PathBuf, u32),
}

/// Removes everything it can under `root`. Links are removed as links. A file
/// that cannot go is skipped so the rest still does. Keeps its own stack because
/// a tree can be nested deeper than the thread's stack allows.
fn remove_tree(root: &Path, entry: DirEntry) -> Problem {
    let mut problem = Problem::default();
    let mut steps = vec![Step::Visit(root.to_path_buf(), entry)];
    while let Some(step) = steps.pop() {
        match step {
            Step::RemoveEmptied(path, attrs) => {
                if let Err(e) = remove_entry(&path, attrs) {
                    problem.note(&e);
                }
            }
            Step::Visit(path, entry) => match classify(entry.attrs, entry.reparse_tag) {
                EntryClass::Dir => {
                    let children = win::list_dir(&path);
                    match children {
                        Ok(children) => {
                            steps.push(Step::RemoveEmptied(path.clone(), entry.attrs));
                            for child in children {
                                let child_path = path.join(&child.name);
                                steps.push(Step::Visit(child_path, child));
                            }
                        }
                        Err(e) => problem.note(&e),
                    }
                }
                EntryClass::File | EntryClass::Link => {
                    if let Err(e) = remove_entry(&path, entry.attrs) {
                        problem.note(&e);
                    }
                }
            },
        }
    }
    problem
}

/// What is left at `path`, measured the way the scan measured it. `None` if that
/// cannot be told, and then the caller claims nothing was freed.
fn remaining_size(path: &Path) -> Option<u64> {
    let entry = match win::entry_info(path) {
        Ok(entry) => entry,
        Err(e) if reason_from_io(&e) == FailReason::NotFound => return Some(0),
        Err(_) => return None,
    };
    match classify(entry.attrs, entry.reparse_tag) {
        EntryClass::Link => Some(0),
        EntryClass::File => win::allocated_size(path).ok(),
        EntryClass::Dir => {
            let opts = scan::ScanOptions {
                threads: 1,
                excluded: Vec::new(),
            };
            let tree = scan::scan(path, &opts, &scan::ScanProgress::default()).ok()?;
            Some(tree.nodes[tree.root].size)
        }
    }
}

fn delete_permanently(target: &Target, path: &Path, entry: DirEntry, report: &mut DeleteReport) {
    let problem = remove_tree(path, entry);
    match win::entry_info(path) {
        Err(e) if reason_from_io(&e) == FailReason::NotFound => {
            report.removed.push(target.path.clone());
            report.bytes_freed += target.size;
        }
        _ => {
            let remaining = remaining_size(path).unwrap_or(target.size);
            report.bytes_freed += target.size.saturating_sub(remaining);
            let reason = problem
                .0
                .unwrap_or_else(|| FailReason::Other("still present after deleting".into()));
            report.failed.push(failure(target, reason));
        }
    }
}

fn failure(target: &Target, reason: FailReason) -> Failed {
    Failed {
        path: target.path.clone(),
        size: target.size,
        reason,
    }
}

fn delete_one(target: &Target, mode: DeleteMode, ctx: &ProtectContext, report: &mut DeleteReport) {
    // Once per target, not per file walked: the check costs filesystem calls, and
    // nothing below a checked folder can be reached except through that folder.
    let Some(path) = canonical_for_delete(&target.path) else {
        report.failed.push(failure(target, FailReason::Protected));
        return;
    };
    if is_protected(&path, ctx) {
        report.failed.push(failure(target, FailReason::Protected));
        return;
    }
    let entry = match win::entry_info(&path) {
        Ok(entry) => entry,
        Err(e) => {
            report.failed.push(failure(target, reason_from_io(&e)));
            return;
        }
    };
    match mode {
        DeleteMode::Permanent => delete_permanently(target, &path, entry, report),
        DeleteMode::RecycleBin => recycle_target(target, report),
    }
}

/// Runs after the gate. The shell gets the path on the drive it was given on, not
/// the resolved one: whether an item can be recycled is for the shell to say about
/// the place the user is deleting from, and a `subst` drive must answer for itself
/// rather than borrow the bin of the folder it maps to.
fn recycle_target(target: &Target, report: &mut DeleteReport) {
    let Some(spelling) = lexical_spelling(&target.path) else {
        report.failed.push(failure(target, FailReason::Protected));
        return;
    };
    match recycle_one(&spelling) {
        Ok(RecycleOutcome::InBin) => {
            report.recycled.push(target.path.clone());
            report.bytes_in_bin += target.size;
        }
        Ok(RecycleOutcome::DeletedPermanently) => {
            report.removed.push(target.path.clone());
            report.bytes_freed += target.size;
        }
        Err(reason) => report.failed.push(failure(target, reason)),
    }
}

/// Deletes each target, or reports why not. Paths in the report are the paths
/// given. Sizes come from the caller and are only counted for what really went:
/// `bytes_in_bin` is still on the disk and never counts as freed.
pub fn delete(targets: &[Target], mode: DeleteMode, ctx: &ProtectContext) -> DeleteReport {
    let mut report = DeleteReport::default();
    for target in targets {
        delete_one(target, mode, ctx, &mut report);
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protect::canonical_for_delete;
    use crate::recycle::{bin_info, real_quota_bytes, recycle_with_unknown_quota, RESTORED};
    use crate::{scan, win};
    use std::fs;
    use std::io::{Read, Write};
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;
    use std::process::Command;
    use tempfile::{Builder, TempDir};
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;

    const TIB: u64 = 1 << 40;
    const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

    fn tmp() -> TempDir {
        Builder::new()
            .prefix("katharistis-test-")
            .tempdir()
            .unwrap()
    }

    /// Every entry is real and absolute, and none of them touches the temp folder.
    fn ctx() -> ProtectContext {
        ProtectContext {
            windows_dir: r"C:\Windows".into(),
            program_files: vec![r"C:\Program Files".into(), r"C:\Program Files (x86)".into()],
            program_data: r"C:\ProgramData".into(),
            users_dir: r"C:\Users".into(),
            profile: r"C:\Users\katharistis-test-profile".into(),
            own_dirs: vec![r"C:\Users\katharistis-test-profile\own".into()],
        }
    }

    fn size_of(p: &Path) -> u64 {
        if p.is_dir() {
            let opts = scan::ScanOptions {
                threads: 2,
                excluded: Vec::new(),
            };
            let tree = scan::scan(p, &opts, &scan::ScanProgress::default()).unwrap();
            tree.nodes[tree.root].size
        } else {
            win::allocated_size(p).unwrap()
        }
    }

    fn t(p: &Path) -> Target {
        Target {
            path: p.to_path_buf(),
            size: size_of(p),
        }
    }

    /// The only way tests reach `delete`: anything resolving outside the temp
    /// folder stops the test before a single file is touched.
    fn del(targets: &[Target], mode: DeleteMode, c: &ProtectContext) -> DeleteReport {
        let temp = canonical_for_delete(&std::env::temp_dir().join("x")).unwrap();
        let temp = temp.parent().unwrap().to_str().unwrap().to_lowercase();
        for target in targets {
            let resolved = canonical_for_delete(&target.path).unwrap();
            let resolved = resolved.to_str().unwrap().to_lowercase();
            assert!(
                resolved.starts_with(&format!(r"{temp}\")),
                "refusing to run a delete outside temp: {resolved}"
            );
        }
        delete(targets, mode, c)
    }

    fn junction(link: &Path, target: &Path) {
        let out = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd runs");
        assert!(out.status.success(), "mklink failed: {out:?}");
        assert!(
            link.exists(),
            "junction must work for the test to mean anything"
        );
    }

    fn lock(p: &Path) -> fs::File {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(p)
            .unwrap()
    }

    fn write_kib(p: &Path, kib: usize) {
        fs::write(p, vec![b'x'; kib * 1024]).unwrap();
    }

    fn set_read_only(p: &Path) {
        let mut perms = fs::metadata(p).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(p, perms).unwrap();
    }

    fn assert_no_guard(dir: &Path) {
        for entry in win::list_dir(dir).unwrap() {
            assert!(
                !entry.name.starts_with(".katharistis-guard-"),
                "guard left behind: {}",
                entry.name
            );
        }
    }

    #[test]
    fn refuses_protected_and_touches_nothing() {
        let dir = tmp();
        let guarded = dir.path().join("guarded");
        fs::create_dir(&guarded).unwrap();
        let guarded_file = guarded.join("katharistis-test-keep.txt");
        write_kib(&guarded_file, 1);
        let c = ProtectContext {
            own_dirs: vec![guarded.clone()],
            ..ctx()
        };
        for mode in [DeleteMode::Permanent, DeleteMode::RecycleBin] {
            for target in [&guarded_file, &guarded, dir.path()] {
                let r = del(&[t(target)], mode, &c);
                assert_eq!(r.failed.len(), 1, "{target:?}");
                assert_eq!(r.failed[0].reason, FailReason::Protected, "{target:?}");
                assert_eq!(r.failed[0].path, *target);
                assert!(r.removed.is_empty() && r.recycled.is_empty());
                assert_eq!((r.bytes_freed, r.bytes_in_bin), (0, 0));
                assert!(guarded_file.exists(), "{target:?}");
            }
        }
    }

    #[test]
    fn protected_through_a_junction_is_refused() {
        let dir = tmp();
        let guarded = dir.path().join("guarded");
        fs::create_dir(&guarded).unwrap();
        let keep = guarded.join("katharistis-test-keep.txt");
        write_kib(&keep, 1);
        let link = dir.path().join("link");
        junction(&link, &guarded);
        let c = ProtectContext {
            own_dirs: vec![guarded.clone()],
            ..ctx()
        };
        for mode in [DeleteMode::Permanent, DeleteMode::RecycleBin] {
            let r = del(&[t(&link.join("katharistis-test-keep.txt"))], mode, &c);
            assert_eq!(r.failed[0].reason, FailReason::Protected);
            assert!(keep.exists());
        }
    }

    #[test]
    fn a_path_that_cannot_be_pinned_down_counts_as_protected() {
        for mode in [DeleteMode::Permanent, DeleteMode::RecycleBin] {
            for p in [
                "relative-katharistis-test",
                r"C:\Users\x:stream",
                r"\\.\C:\x",
            ] {
                let target = Target {
                    path: p.into(),
                    size: 10,
                };
                let r = delete(&[target], mode, &ctx());
                assert_eq!(r.failed[0].reason, FailReason::Protected, "{p}");
                assert_eq!(r.bytes_freed, 0);
            }
        }
    }

    #[test]
    fn a_context_with_an_empty_entry_refuses_everything() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-keep.txt");
        write_kib(&f, 1);
        let c = ProtectContext {
            program_data: PathBuf::new(),
            ..ctx()
        };
        let r = del(&[t(&f)], DeleteMode::Permanent, &c);
        assert_eq!(r.failed[0].reason, FailReason::Protected);
        assert!(f.exists());
    }

    #[test]
    fn permanent_removes_tree_and_counts_bytes() {
        let dir = tmp();
        let top = dir.path().join("top");
        fs::create_dir_all(top.join(r"a\b\empty")).unwrap();
        write_kib(&top.join("one.bin"), 8);
        write_kib(&top.join(r"a\two.bin"), 16);
        write_kib(&top.join(r"a\b\three.bin"), 32);
        let survivor = dir.path().join("survivor.txt");
        write_kib(&survivor, 1);
        let target = t(&top);
        assert_eq!(target.size, 56 * 1024);

        let r = del(&[target], DeleteMode::Permanent, &ctx());

        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.removed, std::slice::from_ref(&top));
        assert!(r.recycled.is_empty());
        assert_eq!(r.bytes_freed, 56 * 1024);
        assert_eq!(r.bytes_in_bin, 0);
        assert!(!top.exists());
        assert!(survivor.exists());
    }

    #[test]
    fn permanent_does_not_follow_junction() {
        let dir = tmp();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let keep = outside.join("keep.txt");
        write_kib(&keep, 4);

        // As the target itself.
        let link = dir.path().join("link");
        junction(&link, &outside);
        let r = del(&[t(&link)], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(r.removed, std::slice::from_ref(&link));
        assert!(fs::symlink_metadata(&link).is_err(), "the junction goes");
        assert!(keep.exists(), "what it points at stays");

        // Inside a tree being deleted.
        let top = dir.path().join("top");
        fs::create_dir(&top).unwrap();
        junction(&top.join("nested"), &outside);
        write_kib(&top.join("own.bin"), 4);
        let r = del(&[t(&top)], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!top.exists());
        assert!(keep.exists());
        assert_eq!(r.bytes_freed, 4 * 1024);
    }

    #[test]
    fn locked_file_is_reported_and_siblings_are_removed() {
        let dir = tmp();
        let top = dir.path().join("top");
        fs::create_dir_all(top.join("sub")).unwrap();
        let locked = top.join("locked.bin");
        write_kib(&locked, 8);
        let (sib1, sib2) = (top.join("sibling.bin"), top.join(r"sub\sibling.bin"));
        write_kib(&sib1, 16);
        write_kib(&sib2, 32);
        let target = t(&top);
        let _guard = lock(&locked);

        let r = del(&[target], DeleteMode::Permanent, &ctx());

        assert!(locked.exists());
        assert!(!sib1.exists() && !sib2.exists());
        assert!(r.removed.is_empty());
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].path, top);
        assert_eq!(r.failed[0].reason, FailReason::InUse);
        assert_eq!(r.bytes_freed, 48 * 1024);
    }

    #[test]
    fn a_locked_single_file_frees_nothing() {
        let dir = tmp();
        let locked = dir.path().join("locked.bin");
        write_kib(&locked, 8);
        let target = t(&locked);
        let _guard = lock(&locked);
        let r = del(&[target], DeleteMode::Permanent, &ctx());
        assert_eq!(r.failed[0].reason, FailReason::InUse);
        assert_eq!(r.bytes_freed, 0);
        assert!(locked.exists());
    }

    #[test]
    fn missing_target_counts_nothing() {
        let dir = tmp();
        let gone = dir.path().join("katharistis-test-gone.txt");
        for mode in [DeleteMode::Permanent, DeleteMode::RecycleBin] {
            let target = Target {
                path: gone.clone(),
                size: 1000,
            };
            let r = del(&[target], mode, &ctx());
            assert_eq!(r.failed[0].reason, FailReason::NotFound);
            assert_eq!((r.bytes_freed, r.bytes_in_bin), (0, 0));
            assert!(r.removed.is_empty() && r.recycled.is_empty());
        }
    }

    #[test]
    fn a_second_mention_of_the_same_target_counts_nothing() {
        let dir = tmp();
        let f = dir.path().join("f.bin");
        write_kib(&f, 4);
        let target = t(&f);
        let r = del(&[target.clone(), target], DeleteMode::Permanent, &ctx());
        assert_eq!(r.removed.len(), 1);
        assert_eq!(r.failed[0].reason, FailReason::NotFound);
        assert_eq!(r.bytes_freed, 4 * 1024);
    }

    #[test]
    fn read_only_file_is_removed() {
        let dir = tmp();
        let f = dir.path().join("ro.bin");
        write_kib(&f, 4);
        let mut perms = fs::metadata(&f).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&f, perms).unwrap();
        let r = del(&[t(&f)], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!f.exists());
        assert_eq!(r.bytes_freed, 4 * 1024);
    }

    #[test]
    fn read_only_entries_inside_a_tree_are_removed() {
        let dir = tmp();
        let top = dir.path().join("top");
        let sub = top.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let f = sub.join("ro.bin");
        write_kib(&f, 4);
        for p in [&f, &sub] {
            let mut perms = fs::metadata(p).unwrap().permissions();
            perms.set_readonly(true);
            fs::set_permissions(p, perms).unwrap();
        }
        let r = del(&[t(&top)], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!top.exists());
    }

    #[test]
    fn literal_trailing_dot_name_does_not_hit_its_sibling() {
        let dir = tmp();
        let plain = dir.path().join("katharistis-test-name");
        write_kib(&plain, 1);
        let dotted = dir.path().join("katharistis-test-name.");
        fs::write(win_long(&dotted), b"dotted").unwrap();

        let target = Target {
            path: dotted.clone(),
            size: 6,
        };
        let r = del(&[target], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(plain.exists(), "the sibling without the dot must survive");
        assert!(fs::symlink_metadata(win_long(&dotted)).is_err());
    }

    fn win_long(p: &Path) -> PathBuf {
        PathBuf::from(format!(r"\\?\{}", p.display()))
    }

    #[test]
    fn wildcards_match_nothing() {
        let dir = tmp();
        let keep = dir.path().join("katharistis-test-keep.txt");
        write_kib(&keep, 1);
        for mode in [DeleteMode::Permanent, DeleteMode::RecycleBin] {
            for name in ["*", "katharistis-test-*", "katharistis-test-keep.tx?"] {
                let target = Target {
                    path: dir.path().join(name),
                    size: 1,
                };
                let r = del(&[target], mode, &ctx());
                assert_eq!(r.failed.len(), 1, "{name}");
                assert_eq!(r.bytes_freed + r.bytes_in_bin, 0, "{name}");
                assert!(keep.exists(), "{name}");
            }
        }
    }

    #[test]
    fn permanent_handles_paths_past_max_path() {
        let dir = tmp();
        let top = dir.path().join("top");
        let mut deep = top.clone();
        for _ in 0..6 {
            deep.push("d".repeat(100));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("f.bin"), b"deep").unwrap();
        let r = del(&[t(&top)], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!top.exists());
    }

    #[test]
    fn permanent_survives_very_deep_trees() {
        let dir = tmp();
        let top = dir.path().join("top");
        let mut deep = win_long(&top);
        fs::create_dir(&deep).unwrap();
        for _ in 0..800 {
            deep.push("d");
            fs::create_dir(&deep).unwrap();
        }
        let target = Target {
            path: top.clone(),
            size: 0,
        };
        let r = del(&[target], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!top.exists());
    }

    #[test]
    #[ignore = "uses the real Recycle Bin"]
    fn recycle_moves_file_and_counts_as_in_bin_not_freed() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-recycle.txt");
        write_kib(&f, 4);
        let r = del(&[t(&f)], DeleteMode::RecycleBin, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!f.exists());
        assert_eq!(r.recycled, std::slice::from_ref(&f));
        assert!(r.removed.is_empty());
        assert_eq!(r.bytes_freed, 0);
        assert_eq!(r.bytes_in_bin, 4 * 1024);
        let (_, items) = bin_info().unwrap();
        assert!(items >= 1, "the file just recycled must be counted");
        assert_no_guard(dir.path());
    }

    #[test]
    #[ignore = "uses the real Recycle Bin"]
    fn recycle_moves_a_folder_as_one_item() {
        let dir = tmp();
        let top = dir.path().join("katharistis-test-folder");
        fs::create_dir_all(top.join("sub")).unwrap();
        write_kib(&top.join("a.bin"), 4);
        write_kib(&top.join(r"sub\b.bin"), 8);
        let r = del(&[t(&top)], DeleteMode::RecycleBin, &ctx());
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert!(!top.exists());
        assert_eq!(r.recycled, std::slice::from_ref(&top));
        assert_eq!(r.bytes_freed, 0);
        assert_eq!(r.bytes_in_bin, 12 * 1024);
    }

    #[test]
    fn recycling_a_locked_file_keeps_it() {
        let dir = tmp();
        let locked = dir.path().join("katharistis-test-locked.txt");
        write_kib(&locked, 4);
        let target = t(&locked);
        let _guard = lock(&locked);
        let r = del(&[target], DeleteMode::RecycleBin, &ctx());
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].reason, FailReason::InUse);
        assert_eq!((r.bytes_freed, r.bytes_in_bin), (0, 0));
        assert!(locked.exists());
        assert_eq!(fs::metadata(&locked).unwrap().len(), 4 * 1024);
        assert_no_guard(dir.path());
    }

    #[test]
    fn recycling_a_folder_with_a_locked_file_keeps_that_file() {
        let dir = tmp();
        let top = dir.path().join("katharistis-test-folder-locked");
        fs::create_dir(&top).unwrap();
        let locked = top.join("katharistis-test-locked.txt");
        write_kib(&locked, 4);
        write_kib(&top.join("katharistis-test-other.txt"), 4);
        let target = t(&top);
        let _guard = lock(&locked);
        let r = del(&[target], DeleteMode::RecycleBin, &ctx());
        eprintln!("folder with a locked file: {r:?}");
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert!(locked.exists());
        assert!(r.recycled.is_empty() && r.removed.is_empty());
        assert_eq!(r.bytes_in_bin, 0);
    }

    #[test]
    fn a_junction_does_not_go_to_the_bin() {
        let dir = tmp();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let keep = outside.join("katharistis-test-keep.txt");
        write_kib(&keep, 1);
        let link = dir.path().join("katharistis-test-link");
        junction(&link, &outside);
        let before = bin_info().unwrap();

        let r = del(&[t(&link)], DeleteMode::RecycleBin, &ctx());

        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert!(r.recycled.is_empty() && r.removed.is_empty());
        assert_eq!((r.bytes_freed, r.bytes_in_bin), (0, 0));
        assert!(fs::symlink_metadata(&link).is_ok(), "the link stays");
        assert!(keep.exists(), "its target stays");
        assert_eq!(bin_info().unwrap(), before);
    }

    #[test]
    fn a_folder_holding_a_junction_at_any_depth_does_not_go_to_the_bin() {
        let dir = tmp();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let keep = outside.join("katharistis-test-keep.txt");
        write_kib(&keep, 1);
        let top = dir.path().join("katharistis-test-folder");
        let deep = top.join(r"a\b\c");
        fs::create_dir_all(&deep).unwrap();
        let own = top.join(r"a\own.txt");
        write_kib(&own, 1);
        let link = deep.join("nested-link");
        junction(&link, &outside);
        let before = bin_info().unwrap();

        let r = del(&[t(&top)], DeleteMode::RecycleBin, &ctx());

        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert!(own.exists() && keep.exists());
        assert!(fs::symlink_metadata(&link).is_ok());
        assert_eq!(bin_info().unwrap(), before);

        // Permanent still removes the link and leaves what it points at.
        let r = del(&[t(&top)], DeleteMode::Permanent, &ctx());
        assert!(r.failed.is_empty(), "{r:?}");
        assert!(!top.exists());
        assert!(keep.exists());
    }

    /// Takes the delete rights away and gives them back when dropped, also when the
    /// test fails. Full rights on a folder allow deleting what is in it whatever the
    /// file says, so the folder has to be denied as well.
    struct DeleteDenied {
        file: PathBuf,
        folder: PathBuf,
    }

    impl DeleteDenied {
        fn apply(file: &Path, folder: &Path) -> DeleteDenied {
            let denied = DeleteDenied {
                file: file.to_path_buf(),
                folder: folder.to_path_buf(),
            };
            let user = std::env::var("USERNAME").unwrap();
            for (path, right) in [(file, "DE"), (folder, "DC")] {
                let out = Command::new("icacls")
                    .arg(path)
                    .arg("/deny")
                    .arg(format!("{user}:({right})"))
                    .output()
                    .expect("icacls runs");
                assert!(out.status.success(), "icacls failed: {out:?}");
            }
            denied
        }
    }

    impl Drop for DeleteDenied {
        fn drop(&mut self) {
            let user = std::env::var("USERNAME").unwrap_or_default();
            for path in [&self.file, &self.folder] {
                let _ = Command::new("icacls")
                    .arg(path)
                    .arg("/remove:d")
                    .arg(&user)
                    .output();
            }
        }
    }

    #[test]
    fn a_file_that_may_not_be_deleted_is_reported_as_such() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-denied.txt");
        write_kib(&f, 4);
        let target = t(&f);
        let _denied = DeleteDenied::apply(&f, dir.path());
        let before = bin_info().unwrap();

        let r = del(&[target], DeleteMode::RecycleBin, &ctx());

        eprintln!("a file that may not be deleted: {r:?}");
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::AccessDenied);
        assert_eq!(fs::metadata(&f).unwrap().len(), 4 * 1024);
        assert_no_guard(dir.path());
        assert_eq!(bin_info().unwrap(), before);
    }

    /// Every child fits the quota, and only together do they not.
    #[test]
    #[ignore = "uses the real Recycle Bin"]
    fn a_folder_over_the_quota_stays_when_each_child_fits() {
        let dir = tmp();
        let quota = real_quota_bytes(dir.path()).expect("needs a known quota");
        let top = dir.path().join("katharistis-test-many-sparse");
        fs::create_dir(&top).unwrap();
        let child = quota / 4 * 3;
        let children = [top.join("one.bin"), top.join("two.bin")];
        for path in &children {
            assert_eq!(sparse_file(path, child), child);
        }
        let before = bin_info().unwrap();

        let r = del(&[t(&top)], DeleteMode::RecycleBin, &ctx());

        eprintln!("{child} bytes twice against a quota of {quota}: {r:?}");
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        for path in &children {
            assert_eq!(fs::metadata(path).unwrap().len(), child);
        }
        assert_eq!(bin_info().unwrap(), before);
    }

    #[test]
    fn recycling_a_literal_trailing_dot_name_does_not_hit_its_sibling() {
        let dir = tmp();
        let plain = dir.path().join("katharistis-test-recycle-name");
        write_kib(&plain, 1);
        let dotted = dir.path().join("katharistis-test-recycle-name.");
        fs::write(win_long(&dotted), b"dotted").unwrap();
        let target = Target {
            path: dotted.clone(),
            size: 6,
        };
        let r = del(&[target], DeleteMode::RecycleBin, &ctx());
        eprintln!("recycling a trailing-dot name: {r:?}");
        assert!(plain.exists(), "the sibling without the dot must survive");
        let dotted_there = fs::symlink_metadata(win_long(&dotted)).is_ok();
        assert_eq!(dotted_there, !r.failed.is_empty(), "{r:?}");
        assert_no_guard(dir.path());
    }

    /// A drive letter mapped with `subst`, removed again when dropped, also when
    /// the test fails. Only letters that are free right now are tried.
    struct SubstDrive(char);

    impl SubstDrive {
        fn mount(target: &Path) -> SubstDrive {
            let used = unsafe { GetLogicalDrives() };
            // Not Z or D to K: other tests use those letters as drives that do not exist.
            for letter in ('L'..='X').rev() {
                if used & (1 << (letter as u32 - 'A' as u32)) != 0 {
                    continue;
                }
                let done = Command::new("subst")
                    .arg(format!("{letter}:"))
                    .arg(target)
                    .output()
                    .expect("subst runs");
                if done.status.success() {
                    return SubstDrive(letter);
                }
            }
            panic!("no free drive letter for subst");
        }

        fn path(&self, name: &str) -> PathBuf {
            PathBuf::from(format!(r"{}:\{name}", self.0))
        }
    }

    impl Drop for SubstDrive {
        fn drop(&mut self) {
            let _ = Command::new("subst")
                .arg(format!("{}:", self.0))
                .arg("/D")
                .output();
        }
    }

    #[test]
    fn drive_without_bin_never_deletes_silently() {
        let dir = tmp();
        let drive = SubstDrive::mount(dir.path());
        let file_on_subst = drive.path("katharistis-test-subst.txt");
        write_kib(&file_on_subst, 4);
        let folder_on_subst = drive.path("katharistis-test-subst-folder");
        fs::create_dir(&folder_on_subst).unwrap();
        write_kib(&folder_on_subst.join("inner.bin"), 4);

        let r = del(&[t(&file_on_subst)], DeleteMode::RecycleBin, &ctx());
        eprintln!("file on a subst drive: {r:?}");
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert_eq!(r.failed[0].path, file_on_subst);
        assert!(file_on_subst.exists());
        assert!(r.recycled.is_empty() && r.removed.is_empty());
        assert_eq!((r.bytes_freed, r.bytes_in_bin), (0, 0));
        assert_no_guard(dir.path());

        let r = del(&[t(&folder_on_subst)], DeleteMode::RecycleBin, &ctx());
        eprintln!("folder on a subst drive: {r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert!(folder_on_subst.join("inner.bin").exists());

        // The file stayed, and asking for a permanent delete still works.
        let r = del(&[t(&file_on_subst)], DeleteMode::Permanent, &ctx());
        assert_eq!(r.removed, std::slice::from_ref(&file_on_subst));
        assert!(!file_on_subst.exists());
    }

    #[test]
    fn protection_sees_through_a_subst_drive() {
        let dir = tmp();
        let guarded = dir.path().join("guarded");
        fs::create_dir(&guarded).unwrap();
        let keep = guarded.join("katharistis-test-keep.txt");
        write_kib(&keep, 1);
        let drive = SubstDrive::mount(&guarded);
        let c = ProtectContext {
            own_dirs: vec![guarded.clone()],
            ..ctx()
        };
        for mode in [DeleteMode::RecycleBin, DeleteMode::Permanent] {
            let r = del(&[t(&drive.path("katharistis-test-keep.txt"))], mode, &c);
            assert_eq!(r.failed[0].reason, FailReason::Protected);
            assert!(keep.exists());
        }
    }

    /// A sparse file of `logical` bytes, or the largest this volume allows. The
    /// sparse flag is proven before the length is set, so this can never
    /// allocate real space.
    fn sparse_file(p: &Path, logical: u64) -> u64 {
        fs::File::create(p).unwrap();
        let out = Command::new("fsutil")
            .args(["sparse", "setflag"])
            .arg(p)
            .output()
            .expect("fsutil runs");
        assert!(out.status.success(), "fsutil failed: {out:?}");
        let attrs = fs::metadata(p).unwrap().file_attributes();
        assert_ne!(
            attrs & FILE_ATTRIBUTE_SPARSE_FILE,
            0,
            "not sparse, stopping"
        );

        let file = fs::OpenOptions::new().write(true).open(p).unwrap();
        let mut size = logical;
        while let Err(e) = file.set_len(size) {
            eprintln!("set_len({size}) failed: {e}");
            size /= 2;
            assert!(size >= 64 * (1 << 30), "volume refuses a useful size");
        }
        drop(file);
        let on_disk = win::allocated_size(p).unwrap();
        assert!(
            on_disk < 1 << 20,
            "{on_disk} bytes allocated, expected none"
        );
        eprintln!("sparse file: {size} bytes logical, {on_disk} on disk");
        size
    }

    /// The tests that follow use the file's own logical length, not `Target.size`,
    /// which is what makes an oversized sparse file oversized.
    #[test]
    fn oversized_sparse_file_stays_with_the_real_quota() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-sparse.bin");
        let logical = sparse_file(&f, 4 * TIB);
        let before = bin_info().unwrap();
        let r = del(&[t(&f)], DeleteMode::RecycleBin, &ctx());
        eprintln!("oversized file: {r:?}");
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert!(r.removed.is_empty() && r.recycled.is_empty());
        assert_eq!((r.bytes_freed, r.bytes_in_bin), (0, 0));
        assert_eq!(fs::metadata(&f).unwrap().len(), logical);
        assert_no_guard(dir.path());
        assert_eq!(bin_info().unwrap().1, before.1);
    }

    #[test]
    fn oversized_sparse_file_inside_a_folder_stays() {
        let dir = tmp();
        let top = dir.path().join("katharistis-test-sparse-folder");
        fs::create_dir(&top).unwrap();
        let f = top.join("katharistis-test-sparse.bin");
        let logical = sparse_file(&f, 4 * TIB);
        let small = top.join("katharistis-test-small.txt");
        write_kib(&small, 1);
        let names = |p: &Path| {
            let mut names: Vec<String> = win::list_dir(p)
                .unwrap()
                .into_iter()
                .map(|e| e.name)
                .collect();
            names.sort();
            names
        };
        let listed = names(&top);
        let before = bin_info().unwrap();

        let r = del(&[t(&top)], DeleteMode::RecycleBin, &ctx());

        eprintln!(
            "oversized file in a folder: {r:?}, bin {before:?} to {:?}",
            bin_info().unwrap()
        );
        assert_eq!(r.failed.len(), 1, "{r:?}");
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert_eq!(fs::metadata(&f).unwrap().len(), logical);
        assert_eq!(fs::read(&small).unwrap(), vec![b'x'; 1024]);
        assert_eq!(names(&top), listed, "nothing else in the folder may go");
        assert_eq!(bin_info().unwrap().1, before.1);
    }

    /// Past the preflight, the shell really does delete this for good. The guard
    /// must bring back the same file.
    #[test]
    #[ignore = "uses the real Recycle Bin"]
    fn guard_restores_a_file_the_shell_deleted_for_good() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-sparse.bin");
        let logical = sparse_file(&f, 4 * TIB);
        const MARKER: &[u8] = b"katharistis marker";
        fs::OpenOptions::new()
            .write(true)
            .open(&f)
            .unwrap()
            .write_all(MARKER)
            .unwrap();
        let before = bin_info().unwrap();

        let outcome = recycle_with_unknown_quota(&f);

        eprintln!(
            "guard on an oversized file: {outcome:?}, bin {before:?} to {:?}",
            bin_info().unwrap()
        );
        assert_eq!(outcome, Err(FailReason::NotRecyclable));
        assert!(
            RESTORED.lock().unwrap().contains(&f),
            "the shell must really have deleted it and the guard restored it"
        );
        let meta = fs::metadata(&f).unwrap();
        assert_eq!(meta.len(), logical);
        assert_ne!(meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE, 0);
        let mut head = vec![0; MARKER.len()];
        fs::File::open(&f).unwrap().read_exact(&mut head).unwrap();
        assert_eq!(head, MARKER);
        assert_eq!(win::link_info(&f).unwrap().2, 1, "no extra name remains");
        assert_no_guard(dir.path());
        assert_eq!(bin_info().unwrap().1, before.1);
    }

    /// The other name keeps the link count up, so a count alone would make the
    /// guard look spare after the original name is gone.
    #[test]
    #[ignore = "uses the real Recycle Bin"]
    fn guard_restores_a_file_that_has_another_hard_link() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-sparse.bin");
        let logical = sparse_file(&f, 4 * TIB);
        let other = dir.path().join("katharistis-test-other-name.bin");
        fs::hard_link(&f, &other).unwrap();

        let outcome = recycle_with_unknown_quota(&f);

        eprintln!("guard on a file with two names: {outcome:?}");
        assert_eq!(outcome, Err(FailReason::NotRecyclable));
        assert!(
            RESTORED.lock().unwrap().contains(&f),
            "the shell must really have deleted it and the guard restored it"
        );
        assert_eq!(fs::metadata(&f).unwrap().len(), logical);
        assert!(other.exists());
        assert_eq!(win::link_info(&f).unwrap().2, 2);
        assert_no_guard(dir.path());
    }

    #[test]
    #[ignore = "uses the real Recycle Bin"]
    fn recycling_a_read_only_file_leaves_no_guard() {
        let dir = tmp();
        let f = dir.path().join("katharistis-test-read-only.txt");
        write_kib(&f, 1);
        set_read_only(&f);
        let r = del(&[t(&f)], DeleteMode::RecycleBin, &ctx());
        assert_eq!(r.recycled, std::slice::from_ref(&f), "{r:?}");
        assert!(!f.exists());
        assert_no_guard(dir.path());
    }

    /// Reaches the guard removal without adding anything to the bin.
    #[test]
    fn a_read_only_file_keeps_its_attribute_when_the_guard_goes() {
        let dir = tmp();
        let drive = SubstDrive::mount(dir.path());
        let f = drive.path("katharistis-test-read-only.txt");
        write_kib(&f, 1);
        set_read_only(&f);
        let r = del(&[t(&f)], DeleteMode::RecycleBin, &ctx());
        assert_eq!(r.failed[0].reason, FailReason::NotRecyclable);
        assert!(fs::metadata(&f).unwrap().permissions().readonly());
        assert_eq!(win::link_info(&f).unwrap().2, 1);
        assert_no_guard(dir.path());
    }
}
