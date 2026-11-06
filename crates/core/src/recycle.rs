//! Sending items to the Recycle Bin through `IFileOperation`. `SHFileOperation`
//! and the `trash` crate delete for good, without saying so, when an item cannot
//! go to the bin. The shell's own answer is not enough either: it flags a single
//! file too big for the bin as recyclable and then deletes it. So files and folders
//! are checked against the bin's settings first, files also get a guard that can
//! put them back, and only an item the shell reports creating in the bin counts as
//! recycled. Links stay out of the bin.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use windows::core::{HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::E_ABORT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Shell::{
    FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl,
    IShellItem, SHCreateItemFromParsingName, SHEmptyRecycleBinW, SHQueryRecycleBinW,
    COPYENGINE_E_ACCESS_DENIED_SRC, COPYENGINE_E_PATH_NOT_FOUND_SRC,
    COPYENGINE_E_RECYCLE_BIN_NOT_FOUND, COPYENGINE_E_RECYCLE_FORCE_NUKE,
    COPYENGINE_E_RECYCLE_PATH_TOO_LONG, COPYENGINE_E_RECYCLE_SIZE_TOO_BIG,
    COPYENGINE_E_RECYCLE_UNKNOWN_ERROR, COPYENGINE_E_SHARING_VIOLATION_SRC, FOFX_RECYCLEONDELETE,
    FOF_ALLOWUNDO, FOF_NO_UI, SHERB_NOPROGRESSUI, SHERB_NOSOUND, SHQUERYRBINFO, SIGDN_FILESYSPATH,
    TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows_core::{implement, Ref};

use crate::delete::{reason_from_os_code, FailReason};
use crate::scan::{classify, EntryClass};
use crate::win;

const BIT_BUCKET_VOLUMES: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume";
const GUARD_PREFIX: &str = ".katharistis-guard-";

pub enum PreDelete {
    Proceed,
    Abort,
}

/// What happened to an item the shell reports as deleted. `InBin` means the
/// shell named the item it created in the bin; anything less is treated as gone
/// for good, so nobody is told a file can be restored when that is unproven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecycleOutcome {
    InBin,
    DeletedPermanently,
}

/// `Proceed` only if the shell says it will recycle this item. Without the flag
/// the shell would delete it for good, since `FOF_NO_UI` answers its question.
pub fn decide_pre_delete(flags: u32) -> PreDelete {
    if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 != 0 {
        PreDelete::Proceed
    } else {
        PreDelete::Abort
    }
}

#[derive(Default)]
struct SinkState {
    aborted: bool,
    in_bin: u32,
    deleted_for_good: u32,
    first_error: Option<HRESULT>,
}

fn lock(state: &Mutex<SinkState>) -> MutexGuard<'_, SinkState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[implement(IFileOperationProgressSink)]
struct Sink {
    state: Arc<Mutex<SinkState>>,
}

impl IFileOperationProgressSink_Impl for Sink_Impl {
    fn PreDeleteItem(&self, flags: u32, _item: Ref<IShellItem>) -> windows_core::Result<()> {
        match decide_pre_delete(flags) {
            PreDelete::Proceed => Ok(()),
            PreDelete::Abort => {
                lock(&self.state).aborted = true;
                Err(E_ABORT.into())
            }
        }
    }

    fn PostDeleteItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        result: HRESULT,
        newly_created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        let mut state = lock(&self.state);
        if result.is_err() {
            state.first_error.get_or_insert(result);
        } else if newly_created.is_null() {
            state.deleted_for_good += 1;
        } else {
            state.in_bin += 1;
        }
        Ok(())
    }

    fn StartOperations(&self) -> windows_core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _result: HRESULT) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _newly_created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _destination: Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostMoveItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _destination: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _newly_created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _destination: Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _flags: u32,
        _item: Ref<IShellItem>,
        _destination: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _newly_created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PreNewItem(
        &self,
        _flags: u32,
        _destination: Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _flags: u32,
        _destination: Ref<IShellItem>,
        _new_name: &PCWSTR,
        _template: &PCWSTR,
        _attrs: u32,
        _result: HRESULT,
        _newly_created: Ref<IShellItem>,
    ) -> windows_core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _total: u32, _so_far: u32) -> windows_core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows_core::Result<()> {
        Ok(())
    }
}

fn reason_from_hresult(hr: HRESULT) -> FailReason {
    match hr {
        COPYENGINE_E_SHARING_VIOLATION_SRC => FailReason::InUse,
        COPYENGINE_E_ACCESS_DENIED_SRC => FailReason::AccessDenied,
        COPYENGINE_E_PATH_NOT_FOUND_SRC => FailReason::NotFound,
        COPYENGINE_E_RECYCLE_BIN_NOT_FOUND
        | COPYENGINE_E_RECYCLE_FORCE_NUKE
        | COPYENGINE_E_RECYCLE_PATH_TOO_LONG
        | COPYENGINE_E_RECYCLE_SIZE_TOO_BIG
        | COPYENGINE_E_RECYCLE_UNKNOWN_ERROR => FailReason::NotRecyclable,
        _ => reason_from_os_code(hr.0),
    }
}

/// The verdict once the shell has finished. A claim of success is not taken at
/// its word: the item must be gone, and only a reported bin item counts as in
/// the bin. An abort outranks everything because it may be why anything else
/// looks odd.
fn conclude(
    state: &SinkState,
    performed: Result<(), HRESULT>,
    any_aborted: bool,
    still_there: bool,
) -> Result<RecycleOutcome, FailReason> {
    if state.aborted {
        return Err(FailReason::NotRecyclable);
    }
    if let Some(hr) = state.first_error {
        return Err(reason_from_hresult(hr));
    }
    if let Err(hr) = performed {
        return Err(reason_from_hresult(hr));
    }
    if still_there {
        return Err(FailReason::Other(if any_aborted {
            "the shell aborted the operation".into()
        } else {
            "still present after the shell reported success".into()
        }));
    }
    if state.in_bin > 0 && state.deleted_for_good == 0 {
        Ok(RecycleOutcome::InBin)
    } else {
        Ok(RecycleOutcome::DeletedPermanently)
    }
}

struct ComGuard;

impl ComGuard {
    fn new() -> Result<ComGuard, FailReason> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        if hr.is_err() {
            return Err(FailReason::Other(format!("CoInitializeEx: {hr:?}")));
        }
        Ok(ComGuard)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn shell_item_path(item: &IShellItem) -> windows_core::Result<String> {
    unsafe {
        let name: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let text = name.to_string();
        CoTaskMemFree(Some(name.0 as *const _));
        text.map_err(|_| E_ABORT.into())
    }
}

fn perform(p: &Path) -> Result<RecycleOutcome, FailReason> {
    #[cfg(test)]
    SHELL_CALLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(p.to_path_buf());

    let shell_error = |e: windows_core::Error| reason_from_hresult(e.code());
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.map_err(shell_error)?;

    // The shell parses names its own way and strips what Win32 would strip, so
    // `name.` can come back as the sibling `name`. Never act on a different item.
    let parsed = shell_item_path(&item).map_err(shell_error)?;
    if parsed.to_lowercase() != p.to_string_lossy().to_lowercase() {
        return Err(FailReason::NotRecyclable);
    }

    let state = Arc::new(Mutex::new(SinkState::default()));
    let sink: IFileOperationProgressSink = Sink {
        state: Arc::clone(&state),
    }
    .into();

    let op: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }.map_err(shell_error)?;
    unsafe { op.SetOperationFlags(FOF_ALLOWUNDO | FOF_NO_UI | FOFX_RECYCLEONDELETE) }
        .map_err(shell_error)?;
    let cookie = unsafe { op.Advise(&sink) }.map_err(shell_error)?;
    unsafe { op.DeleteItem(&item, None) }.map_err(shell_error)?;

    let performed = unsafe { op.PerformOperations() }.map_err(|e| e.code());
    let any_aborted = unsafe { op.GetAnyOperationsAborted() }
        .map(|aborted| aborted.as_bool())
        .unwrap_or(true);
    let _ = unsafe { op.Unadvise(cookie) };

    let still_there = !matches!(
        win::entry_info(p).map_err(|e| reason_from_os_code(e.raw_os_error().unwrap_or(0))),
        Err(FailReason::NotFound)
    );
    let state = lock(&state);
    conclude(&state, performed, any_aborted, still_there)
}

/// Sends one absolute path to the bin, or leaves it alone with the reason. This
/// does no protection check: go through [`crate::delete::delete`].
///
/// Runs on its own thread, because the apartment of the calling thread is not
/// ours to choose or change.
pub(crate) fn recycle_one(p: &Path) -> Result<RecycleOutcome, FailReason> {
    on_recycle_thread(p, None)
}

/// Skips the preflight, which is how a test gets a file too big for the bin past
/// it and in front of the guard.
#[cfg(test)]
pub(crate) fn recycle_with_unknown_quota(p: &Path) -> Result<RecycleOutcome, FailReason> {
    on_recycle_thread(p, Some(Preflight::QuotaUnknown))
}

#[cfg(test)]
pub(crate) fn real_quota_bytes(p: &Path) -> Option<u64> {
    let mib = read_bin_settings(p).max_capacity_mib?;
    Some(u64::from(mib) * (1 << 20))
}

fn on_recycle_thread(p: &Path, preflight: Option<Preflight>) -> Result<RecycleOutcome, FailReason> {
    let path = p.to_path_buf();
    let worker = thread::Builder::new()
        .name("katharistis-recycle".into())
        .spawn(move || {
            let _com = ComGuard::new()?;
            recycle_item(&path, preflight)
        })
        .map_err(|e| FailReason::Other(e.to_string()))?;
    worker
        .join()
        .unwrap_or_else(|_| Err(FailReason::Other("the recycle thread panicked".into())))
}

fn io_reason(e: io::Error) -> FailReason {
    reason_from_os_code(e.raw_os_error().unwrap_or(0))
}

/// `PreDeleteItem` carries the same flags for a single file too big for the bin as
/// for one that fits, and the shell then deletes it for good, so its answer alone
/// is not enough. Links stay out of the bin altogether: a recycled junction adds a
/// size to the bin's total that nothing explains.
fn recycle_item(p: &Path, forced: Option<Preflight>) -> Result<RecycleOutcome, FailReason> {
    let entry = win::entry_info(p).map_err(io_reason)?;
    match classify(entry.attrs, entry.reparse_tag) {
        EntryClass::Link => Err(FailReason::NotRecyclable),
        EntryClass::Dir => recycle_folder(p),
        EntryClass::File => recycle_file(p, &entry.name, forced),
    }
}

/// A folder has no guard, so it goes to the shell only if everything says it fits.
fn recycle_folder(p: &Path) -> Result<RecycleOutcome, FailReason> {
    recycle_folder_with_settings(p, read_bin_settings(p))
}

fn recycle_folder_with_settings(
    p: &Path,
    settings: BinSettings,
) -> Result<RecycleOutcome, FailReason> {
    let Some(logical_len) = folder_logical_len(p).map_err(io_reason)? else {
        return Err(FailReason::NotRecyclable);
    };
    match decide_preflight(settings, logical_len) {
        Preflight::Fits => perform(p),
        Preflight::Refuse | Preflight::QuotaUnknown => Err(FailReason::NotRecyclable),
    }
}

/// The logical length of everything under `root`, or `None` if a link is in there
/// at any depth. Links are not entered and no file's data is opened, so nothing is
/// recalled from the cloud.
fn folder_logical_len(root: &Path) -> io::Result<Option<u64>> {
    let mut total = 0u64;
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in win::list_dir(&folder)? {
            let path = folder.join(&entry.name);
            match classify(entry.attrs, entry.reparse_tag) {
                EntryClass::Link => return Ok(None),
                EntryClass::Dir => folders.push(path),
                EntryClass::File => total = total.saturating_add(win::logical_len(&path)?),
            }
        }
    }
    Ok(Some(total))
}

fn recycle_file(
    p: &Path,
    name_on_disk: &str,
    forced: Option<Preflight>,
) -> Result<RecycleOutcome, FailReason> {
    let logical_len = win::logical_len(p).map_err(io_reason)?;
    let preflight = forced.unwrap_or_else(|| decide_preflight(read_bin_settings(p), logical_len));
    if preflight == Preflight::Refuse {
        return Err(FailReason::NotRecyclable);
    }
    // If this file cannot be deleted the shell will not get any further, and a guard
    // made now might not be removable either, so this is the place to say why.
    win::check_deletable(p).map_err(io_reason)?;

    match Guard::create(p, name_on_disk) {
        Some(guard) => {
            let outcome = perform(p);
            guard.settle(p, outcome)
        }
        None if preflight == Preflight::Fits => perform(p),
        None => Err(FailReason::NotRecyclable),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preflight {
    Refuse,
    Fits,
    QuotaUnknown,
}

#[derive(Clone, Copy, Debug)]
struct BinSettings {
    nuke_on_delete: Option<bool>,
    max_capacity_mib: Option<u32>,
}

/// Only a refusal is final. Windows can have other reasons to delete for good, such
/// as policy settings, which is why the guard exists as well.
fn decide_preflight(settings: BinSettings, logical_len: u64) -> Preflight {
    let quota = settings
        .max_capacity_mib
        .map(|mib| u64::from(mib) * (1 << 20));
    if settings.nuke_on_delete == Some(true) || quota.is_some_and(|quota| logical_len > quota) {
        return Preflight::Refuse;
    }
    match (settings.nuke_on_delete, quota) {
        (Some(false), Some(_)) => Preflight::Fits,
        _ => Preflight::QuotaUnknown,
    }
}

fn read_dword(key: &str, value: &str) -> Option<u32> {
    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    }
    .ok()
    .ok()?;
    Some(data)
}

fn read_bin_settings(p: &Path) -> BinSettings {
    let key = win::volume_guid(p).map(|guid| format!(r"{BIT_BUCKET_VOLUMES}\{guid}"));
    let read = |value: &str| key.as_deref().and_then(|key| read_dword(key, value));
    BinSettings {
        nuke_on_delete: read("NukeOnDelete").map(|value| value != 0),
        max_capacity_mib: read("MaxCapacity"),
    }
}

/// What the guard has put back. A file the shell never touched looks the same
/// afterwards, so this is how a test knows the restore really ran.
#[cfg(test)]
pub(crate) static RESTORED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Every path handed to the shell. A refusal by the preflight and an abort by the
/// shell give the same reason, so this is how a test tells them apart.
#[cfg(test)]
pub(crate) static SHELL_CALLS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// A second name for a file, created beside it for the length of the shell call.
/// What the shell does to the original name cannot reach the data while this name
/// exists, so a file the shell deletes for good can be put back.
struct Guard {
    path: PathBuf,
    restore_to: PathBuf,
    links_before: u32,
}

impl Guard {
    /// `None` if no link can be made: a cloud placeholder, FAT, a read-only folder.
    fn create(original: &Path, name_on_disk: &str) -> Option<Guard> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let (_, _, links_before) = win::link_info(original).ok()?;
        let folder = original.parent()?;
        for _ in 0..4 {
            let path = folder.join(format!(
                "{GUARD_PREFIX}{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            if win::create_hard_link(&path, original).is_ok() {
                return Some(Guard {
                    path,
                    restore_to: folder.join(name_on_disk),
                    links_before,
                });
            }
        }
        None
    }

    /// Ends the guard. The guard may hold the only remaining copy, so it is removed
    /// only with proof that the data lives on under another name; otherwise it
    /// becomes the original again or, failing that, stays and is named by
    /// `FailReason::GuardKept`.
    ///
    /// If the original is still there, the shell's reason is the one worth telling.
    /// A guard that then cannot go is a spare name for a file that is safe.
    fn settle(
        self,
        original: &Path,
        outcome: Result<RecycleOutcome, FailReason>,
    ) -> Result<RecycleOutcome, FailReason> {
        let kept = || Err(FailReason::GuardKept(self.path.clone()));
        let Ok((volume, id, links)) = with_retries(|| win::link_info(&self.path)) else {
            return kept();
        };
        let remove = || with_retries(|| win::delete_name(&self.path));
        // One more name than before the guard: the bin item and this one.
        if outcome == Ok(RecycleOutcome::InBin) && links > self.links_before {
            return match remove() {
                Ok(()) => outcome,
                Err(e) => Err(FailReason::Other(format!(
                    "recycled, but {} could not be removed: {e}",
                    self.path.display()
                ))),
            };
        }
        match with_retries(|| win::link_info(original)) {
            Ok((original_volume, original_id, _))
                if (original_volume, original_id) == (volume, id) =>
            {
                let removed = remove();
                match (outcome, removed) {
                    (Err(reason), _) => Err(reason),
                    // Present means the shell did not succeed, so this should not
                    // happen; either way nothing was recycled.
                    (Ok(_), Ok(())) => Err(FailReason::NotRecyclable),
                    (Ok(_), Err(e)) => Err(FailReason::Other(format!(
                        "{} could not be removed: {e}",
                        self.path.display()
                    ))),
                }
            }
            Err(e) if is_missing(&e) => {
                match with_retries(|| win::rename_no_replace(&self.path, &self.restore_to)) {
                    Ok(()) => {
                        #[cfg(test)]
                        RESTORED
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(self.restore_to.clone());
                        outcome.and(Err(FailReason::NotRecyclable))
                    }
                    Err(_) => kept(),
                }
            }
            _ => kept(),
        }
    }
}

fn is_missing(e: &io::Error) -> bool {
    reason_from_os_code(e.raw_os_error().unwrap_or(0)) == FailReason::NotFound
}

/// Other programs, such as a virus scanner, can hold a file for a moment. Something
/// that is simply not there will not come back, so that answer is final.
fn with_retries<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut result = op();
    for _ in 0..10 {
        match &result {
            Err(e) if !is_missing(e) => thread::sleep(Duration::from_millis(50)),
            _ => break,
        }
        result = op();
    }
    result
}

/// `(bytes, items)` in the Recycle Bin, over all drives.
pub fn bin_info() -> io::Result<(u64, u64)> {
    let mut info = SHQUERYRBINFO {
        cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32,
        ..Default::default()
    };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }.map_err(io::Error::from)?;
    Ok((info.i64Size.max(0) as u64, info.i64NumItems.max(0) as u64))
}

/// `SHERB_NOCONFIRMATION` stays out of this on purpose: emptying the bin cannot
/// be undone, so the system asks.
const EMPTY_BIN_FLAGS: u32 = SHERB_NOSOUND | SHERB_NOPROGRESSUI;

/// Empties the Recycle Bin on all drives, after the system's own confirmation.
pub fn empty_bin() -> io::Result<()> {
    unsafe { SHEmptyRecycleBinW(None, PCWSTR::null(), EMPTY_BIN_FLAGS) }.map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use tempfile::{Builder, TempDir};
    use windows::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows::Win32::UI::Shell::SHERB_NOCONFIRMATION;

    fn guarded_file() -> (TempDir, PathBuf, Guard) {
        let dir = Builder::new()
            .prefix("katharistis-test-")
            .tempdir()
            .unwrap();
        let file = dir.path().join("katharistis-test-file.txt");
        fs::write(&file, "the data").unwrap();
        let guard = Guard::create(&file, "katharistis-test-file.txt").unwrap();
        (dir, file, guard)
    }

    fn exclusive(p: &Path) -> fs::File {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(p)
            .unwrap()
    }

    #[test]
    fn the_restore_outlasts_a_handle_that_goes_away() {
        let (_dir, file, guard) = guarded_file();
        let guard_path = guard.path.clone();
        fs::remove_file(&file).unwrap();
        let held = exclusive(&guard_path);
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(150));
            drop(held);
        });

        let outcome = guard.settle(&file, Ok(RecycleOutcome::DeletedPermanently));

        release.join().unwrap();
        assert_eq!(outcome, Err(FailReason::NotRecyclable));
        assert_eq!(fs::read_to_string(&file).unwrap(), "the data");
        assert!(!guard_path.exists());
    }

    #[test]
    fn a_restore_that_stays_blocked_names_where_the_data_is() {
        let (_dir, file, guard) = guarded_file();
        let guard_path = guard.path.clone();
        fs::remove_file(&file).unwrap();
        let held = exclusive(&guard_path);

        let outcome = guard.settle(&file, Ok(RecycleOutcome::DeletedPermanently));

        drop(held);
        assert_eq!(outcome, Err(FailReason::GuardKept(guard_path.clone())));
        assert_eq!(fs::read_to_string(&guard_path).unwrap(), "the data");
    }

    #[test]
    fn a_new_file_at_the_original_name_is_not_overwritten() {
        let (_dir, file, guard) = guarded_file();
        let guard_path = guard.path.clone();
        fs::remove_file(&file).unwrap();
        fs::write(&file, "something new").unwrap();

        let outcome = guard.settle(&file, Ok(RecycleOutcome::DeletedPermanently));

        assert_eq!(outcome, Err(FailReason::GuardKept(guard_path.clone())));
        assert_eq!(fs::read_to_string(&file).unwrap(), "something new");
        assert_eq!(fs::read_to_string(&guard_path).unwrap(), "the data");
    }

    #[test]
    fn the_shells_reason_survives_a_guard_that_cannot_go() {
        let (_dir, file, guard) = guarded_file();
        let guard_path = guard.path.clone();
        let held = exclusive(&guard_path);

        let outcome = guard.settle(&file, Err(FailReason::AccessDenied));

        drop(held);
        assert_eq!(outcome, Err(FailReason::AccessDenied));
        assert_eq!(fs::read_to_string(&file).unwrap(), "the data");
        assert!(guard_path.exists(), "nothing could remove it");
    }

    /// Two files of 600 KiB against whatever `settings` says. Each would fit a quota
    /// of 1 MiB; only their sum does not, so a wrong length fails the quota test.
    ///
    /// This runs on the test thread, where COM is not initialized: if the decision
    /// is ever wrong, `perform` records the call and then fails to create the file
    /// operation, so the folder still does not reach the bin.
    fn assert_refused_before_the_shell(settings: BinSettings) {
        let dir = Builder::new()
            .prefix("katharistis-test-")
            .tempdir()
            .unwrap();
        let top = dir.path().join("katharistis-test-folder");
        fs::create_dir(&top).unwrap();
        let children = [top.join("one.bin"), top.join("two.bin")];
        for child in &children {
            fs::write(child, vec![b'x'; 600 * 1024]).unwrap();
        }

        let outcome = recycle_folder_with_settings(&top, settings);

        assert!(
            !SHELL_CALLS.lock().unwrap().contains(&top),
            "the shell was called for a folder that should be refused"
        );
        assert_eq!(outcome, Err(FailReason::NotRecyclable));
        for child in &children {
            assert_eq!(fs::metadata(child).unwrap().len(), 600 * 1024);
        }
    }

    #[test]
    fn folder_preflight_refuses_an_unknown_quota_before_the_shell() {
        assert_refused_before_the_shell(settings(Some(false), None));
    }

    #[test]
    fn folder_preflight_refuses_unknown_bin_state_before_the_shell() {
        assert_refused_before_the_shell(settings(None, Some(1000)));
    }

    #[test]
    fn folder_preflight_refuses_a_turned_off_bin_before_the_shell() {
        assert_refused_before_the_shell(settings(Some(true), Some(1000)));
    }

    #[test]
    fn folder_preflight_refuses_a_sum_over_the_quota_before_the_shell() {
        assert_refused_before_the_shell(settings(Some(false), Some(1)));
    }

    #[test]
    fn the_folder_walk_sums_lengths_and_does_not_enter_links() {
        let dir = Builder::new()
            .prefix("katharistis-test-")
            .tempdir()
            .unwrap();
        let top = dir.path().join("top");
        fs::create_dir_all(top.join(r"a\b")).unwrap();
        fs::write(top.join("one"), [0; 100]).unwrap();
        fs::write(top.join(r"a\b\two"), [0; 23]).unwrap();
        assert_eq!(folder_logical_len(&top).unwrap(), Some(123));

        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(top.join(r"a\b\link"))
            .arg(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(folder_logical_len(&top).unwrap(), None);
        assert!(folder_logical_len(&top.join("missing")).is_err());
    }

    #[test]
    fn pre_delete_aborts_without_recycle_flag() {
        let recycle = TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32;
        assert!(matches!(decide_pre_delete(recycle), PreDelete::Proceed));
        assert!(matches!(
            decide_pre_delete(recycle | 0x4),
            PreDelete::Proceed
        ));
        assert!(matches!(decide_pre_delete(0), PreDelete::Abort));
        assert!(matches!(decide_pre_delete(!recycle), PreDelete::Abort));
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn reads_the_settings_of_the_volume_holding_the_temp_folder() {
        let temp = std::env::temp_dir();
        let guid = win::volume_guid(&temp).expect("the temp folder is on a volume");
        assert!(guid.starts_with('{') && guid.ends_with('}'), "{guid}");
        let read = read_bin_settings(&temp);
        eprintln!("bin settings of {guid}: {read:?}");
        eprintln!("preflight for 4 TiB: {:?}", decide_preflight(read, 4 << 40));
    }

    fn settings(nuke: Option<bool>, mib: Option<u32>) -> BinSettings {
        BinSettings {
            nuke_on_delete: nuke,
            max_capacity_mib: mib,
        }
    }

    #[test]
    fn preflight_refuses_when_the_bin_is_turned_off() {
        for mib in [Some(100), None] {
            let nuke = settings(Some(true), mib);
            assert_eq!(decide_preflight(nuke, 1), Preflight::Refuse);
        }
    }

    #[test]
    fn preflight_compares_the_length_with_the_quota() {
        let known = settings(Some(false), Some(100));
        assert_eq!(decide_preflight(known, 1), Preflight::Fits);
        assert_eq!(decide_preflight(known, 100 * MIB), Preflight::Fits);
        assert_eq!(decide_preflight(known, 100 * MIB + 1), Preflight::Refuse);
    }

    #[test]
    fn preflight_says_so_when_a_value_is_missing() {
        for missing in [
            settings(Some(false), None),
            settings(None, Some(100)),
            settings(None, None),
        ] {
            assert_eq!(decide_preflight(missing, 1), Preflight::QuotaUnknown);
        }
        // What is known still counts: this is over the quota either way.
        let over = settings(None, Some(100));
        assert_eq!(decide_preflight(over, 100 * MIB + 1), Preflight::Refuse);
    }

    fn state(in_bin: u32, deleted_for_good: u32) -> SinkState {
        SinkState {
            in_bin,
            deleted_for_good,
            ..Default::default()
        }
    }

    #[test]
    fn only_a_reported_bin_item_counts_as_recycled() {
        assert_eq!(
            conclude(&state(1, 0), Ok(()), false, false),
            Ok(RecycleOutcome::InBin)
        );
        for not_proven in [state(0, 1), state(0, 0), state(3, 1)] {
            assert_eq!(
                conclude(&not_proven, Ok(()), false, false),
                Ok(RecycleOutcome::DeletedPermanently)
            );
        }
    }

    #[test]
    fn an_abort_or_error_outranks_a_success() {
        let aborted = SinkState {
            aborted: true,
            ..state(1, 0)
        };
        assert_eq!(
            conclude(&aborted, Ok(()), false, false),
            Err(FailReason::NotRecyclable)
        );
        let failed = SinkState {
            first_error: Some(COPYENGINE_E_SHARING_VIOLATION_SRC),
            ..state(1, 0)
        };
        assert_eq!(
            conclude(&failed, Ok(()), false, true),
            Err(FailReason::InUse)
        );
        assert_eq!(
            conclude(
                &state(0, 0),
                Err(COPYENGINE_E_ACCESS_DENIED_SRC),
                false,
                true
            ),
            Err(FailReason::AccessDenied)
        );
    }

    #[test]
    fn claimed_success_with_the_item_still_there_is_a_failure() {
        for any_aborted in [false, true] {
            assert!(matches!(
                conclude(&state(1, 0), Ok(()), any_aborted, true),
                Err(FailReason::Other(_))
            ));
        }
    }

    #[test]
    fn maps_shell_errors() {
        assert_eq!(
            reason_from_hresult(COPYENGINE_E_RECYCLE_SIZE_TOO_BIG),
            FailReason::NotRecyclable
        );
        assert_eq!(
            reason_from_hresult(COPYENGINE_E_PATH_NOT_FOUND_SRC),
            FailReason::NotFound
        );
        assert_eq!(
            reason_from_hresult(ERROR_SHARING_VIOLATION.to_hresult()),
            FailReason::InUse
        );
        assert!(matches!(
            reason_from_hresult(HRESULT(0x8004_0005_u32 as i32)),
            FailReason::Other(_)
        ));
    }

    #[test]
    fn emptying_the_bin_keeps_the_confirmation() {
        assert_eq!(EMPTY_BIN_FLAGS & SHERB_NOCONFIRMATION, 0);
    }

    #[test]
    fn bin_info_answers() {
        bin_info().unwrap();
    }
}
