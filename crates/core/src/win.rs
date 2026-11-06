use std::ffi::c_void;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, Prefix};
use std::time::{Duration, SystemTime};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, ERROR_SHARING_VIOLATION, FILETIME, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CreateHardLinkW, DeleteFileW, FileDispositionInfoEx, FindClose, FindExInfoBasic,
    FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, GetCompressedFileSizeW,
    GetDiskFreeSpaceExW, GetFileInformationByHandle, GetFinalPathNameByHandleW, GetLongPathNameW,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, MoveFileExW, RemoveDirectoryW,
    SetFileAttributesW, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE,
    FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO_EX, FILE_DISPOSITION_INFO_EX_FLAGS, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FIND_FIRST_EX_LARGE_FETCH, GETFINALPATHNAMEBYHANDLE_FLAGS, MOVE_FILE_FLAGS, OPEN_EXISTING,
    VOLUME_NAME_DOS, WIN32_FIND_DATAW,
};

/// `*`, `?` and the three characters Win32 also treats as DOS wildcards.
const WILDCARDS: [u16; 5] = [
    b'*' as u16,
    b'?' as u16,
    b'<' as u16,
    b'>' as u16,
    b'"' as u16,
];

fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(Some(0)).collect()
}

/// Null-terminated wide path. Absolute paths get the `\\?\` prefix, and because
/// that prefix turns off Win32 normalization, `/`, `.` and `..` are resolved
/// here. Trailing dots and spaces stay, so a file literally named that way can
/// still be reached. Relative and already-prefixed paths are returned as given.
pub fn to_long_path(p: &Path) -> Vec<u16> {
    let mut components = p.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) if p.has_root() => prefix,
        _ => return to_wide(p.as_os_str()),
    };
    let mut out = OsString::new();
    match prefix.kind() {
        Prefix::Disk(letter) => {
            out.push(r"\\?\");
            out.push(char::from(letter).to_string());
            out.push(":");
        }
        Prefix::UNC(server, share) => {
            out.push(r"\\?\UNC\");
            out.push(server);
            out.push(r"\");
            out.push(share);
        }
        _ => return to_wide(p.as_os_str()),
    }

    let mut parts: Vec<&OsStr> = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => parts.push(name),
            Component::ParentDir => {
                parts.pop();
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        out.push(r"\");
    }
    for part in parts {
        out.push(r"\");
        out.push(part);
    }
    to_wide(&out)
}

fn without_verbatim_prefix(path: String) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path
    }
}

/// Runs a Win32 call that fills a buffer and returns the length written, or the
/// size it needs when the buffer is too small, or 0 on failure.
fn read_wide_string(mut call: impl FnMut(&mut [u16]) -> u32) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    loop {
        let len = call(&mut buf) as usize;
        if len == 0 {
            return None;
        }
        if len < buf.len() {
            return String::from_utf16(&buf[..len]).ok();
        }
        buf.resize(len + 1, 0);
    }
}

/// `GetLongPathNameW`, without the `\\?\` prefix. `None` if the path does not exist.
pub(crate) fn long_path_name(p: &Path) -> Option<String> {
    let wide = to_long_path(p);
    read_wide_string(|buf| unsafe { GetLongPathNameW(PCWSTR(wide.as_ptr()), Some(buf)) })
        .map(without_verbatim_prefix)
}

/// `GetFinalPathNameByHandleW` with every reparse point followed, without the
/// `\\?\` prefix. `None` if the path cannot be opened.
pub(crate) fn final_path_name(p: &Path) -> Option<String> {
    let wide = to_long_path(p);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .ok()?;
    let flags = GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0);
    let path = read_wide_string(|buf| unsafe { GetFinalPathNameByHandleW(handle, buf, flags) });
    // Nothing useful can be done if closing a query-only handle fails.
    let _ = unsafe { CloseHandle(handle) };
    path.map(without_verbatim_prefix)
}

/// One directory entry, without `.` and `..`.
pub struct DirEntry {
    pub name: String,
    pub attrs: u32,
    /// Reparse tag; only meaningful when `attrs` has the reparse point bit.
    pub reparse_tag: u32,
    pub modified: Option<SystemTime>,
}

fn filetime_to_system(ft: FILETIME) -> Option<SystemTime> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let ticks = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    let since_unix = ticks.checked_sub(UNIX_EPOCH_TICKS)?;
    let nanos = since_unix % 10_000_000 * 100;
    Some(SystemTime::UNIX_EPOCH + Duration::new(since_unix / 10_000_000, nanos as u32))
}

fn wide_without_nul(p: &Path) -> Vec<u16> {
    let mut wide = to_long_path(p);
    wide.pop();
    wide
}

/// Lists a directory with `FindFirstFileExW`. Names are read from the directory
/// itself, nothing is opened, so cloud placeholders are not hydrated.
pub fn list_dir(p: &Path) -> io::Result<Vec<DirEntry>> {
    let mut pattern = wide_without_nul(p);
    const SEP: u16 = b'\\' as u16;
    if pattern.last() != Some(&SEP) {
        pattern.push(SEP);
    }
    pattern.push(b'*' as u16);
    pattern.push(0);

    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(pattern.as_ptr()),
            FindExInfoBasic,
            &mut data as *mut _ as *mut c_void,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }
    .map_err(io::Error::from)?;

    let mut out = Vec::new();
    let result = loop {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..len];
        if name != [b'.' as u16] && name != [b'.' as u16, b'.' as u16] {
            out.push(DirEntry {
                name: String::from_utf16_lossy(name),
                attrs: data.dwFileAttributes,
                reparse_tag: data.dwReserved0,
                modified: filetime_to_system(data.ftLastWriteTime),
            });
        }
        match unsafe { FindNextFileW(handle, &mut data) } {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NO_MORE_FILES.to_hresult() => break Ok(()),
            Err(e) => break Err(io::Error::from(e)),
        }
    };
    let _ = unsafe { FindClose(handle) };
    result.map(|()| out)
}

/// The directory entry of `p` itself, without following it if it is a link. The
/// name is passed to `FindFirstFileExW`, which would read a wildcard as a pattern
/// and answer about some other file, so wildcards are refused.
pub fn entry_info(p: &Path) -> io::Result<DirEntry> {
    let has_wildcard = p
        .file_name()
        .is_some_and(|name| name.encode_wide().any(|c| WILDCARDS.contains(&c)));
    if has_wildcard {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "wildcard in file name",
        ));
    }
    let wide = to_long_path(p);
    let mut data = WIN32_FIND_DATAW::default();
    let handle = unsafe {
        FindFirstFileExW(
            PCWSTR(wide.as_ptr()),
            FindExInfoBasic,
            &mut data as *mut _ as *mut c_void,
            FindExSearchNameMatch,
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
    }
    .map_err(io::Error::from)?;
    let _ = unsafe { FindClose(handle) };
    let len = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(data.cFileName.len());
    Ok(DirEntry {
        name: String::from_utf16_lossy(&data.cFileName[..len]),
        attrs: data.dwFileAttributes,
        reparse_tag: data.dwReserved0,
        modified: filetime_to_system(data.ftLastWriteTime),
    })
}

/// `DeleteFileW`. On a file symlink this removes the link.
pub(crate) fn delete_file(p: &Path) -> io::Result<()> {
    let wide = to_long_path(p);
    unsafe { DeleteFileW(PCWSTR(wide.as_ptr())) }.map_err(io::Error::from)
}

/// `RemoveDirectoryW`. On a junction or directory symlink this removes the link
/// and leaves its target alone; a real directory must already be empty.
pub(crate) fn remove_directory(p: &Path) -> io::Result<()> {
    let wide = to_long_path(p);
    unsafe { RemoveDirectoryW(PCWSTR(wide.as_ptr())) }.map_err(io::Error::from)
}

/// `SetFileAttributesW`, which sets the attributes of a link rather than of what
/// it points at. Bits it cannot set, such as the directory bit, are ignored.
pub(crate) fn set_attributes(p: &Path, attrs: u32) -> io::Result<()> {
    let wide = to_long_path(p);
    // Zero is not a valid request; the API spells "no attributes" this way.
    let attrs = if attrs == 0 {
        FILE_ATTRIBUTE_NORMAL
    } else {
        FILE_FLAGS_AND_ATTRIBUTES(attrs)
    };
    unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), attrs) }.map_err(io::Error::from)
}

/// `CreateHardLinkW`: a second name for the file at `existing`.
pub(crate) fn create_hard_link(link: &Path, existing: &Path) -> io::Result<()> {
    let (link, existing) = (to_long_path(link), to_long_path(existing));
    unsafe { CreateHardLinkW(PCWSTR(link.as_ptr()), PCWSTR(existing.as_ptr()), None) }
        .map_err(io::Error::from)
}

/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, so it never overwrites.
pub(crate) fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (to_long_path(from), to_long_path(to));
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }
    .map_err(io::Error::from)
}

fn open_for_delete(p: &Path) -> io::Result<HANDLE> {
    let wide = to_long_path(p);
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(io::Error::from)
}

/// Removes one name of a file. The read-only attribute belongs to the file and not
/// to the name, so it is ignored rather than cleared: any other name of the file
/// keeps it.
pub(crate) fn delete_name(p: &Path) -> io::Result<()> {
    let handle = open_for_delete(p)?;
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
            FILE_DISPOSITION_FLAG_DELETE.0
                | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0
                | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE.0,
        ),
    };
    let result = unsafe {
        SetFileInformationByHandle(
            handle,
            FileDispositionInfoEx,
            &info as *const _ as *const c_void,
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    // Not every file system takes those flags. A plain delete only falls short on
    // a read-only file.
    result
        .or_else(|_| unsafe { DeleteFileW(PCWSTR(to_long_path(p).as_ptr())) })
        .map_err(io::Error::from)
}

/// Whether another handle stops this file from being deleted right now.
pub(crate) fn is_locked_against_delete(p: &Path) -> bool {
    match open_for_delete(p) {
        Ok(handle) => {
            let _ = unsafe { CloseHandle(handle) };
            false
        }
        Err(e) => e.raw_os_error() == Some(ERROR_SHARING_VIOLATION.to_hresult().0),
    }
}

/// The `{guid}` of the volume holding `p`. `None` where there is no such thing to
/// ask about, such as a network share.
pub(crate) fn volume_guid(p: &Path) -> Option<String> {
    let wide = to_long_path(p);
    let mut root = vec![0u16; 1024];
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut root) }.ok()?;
    let mut name = [0u16; 64];
    unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root.as_ptr()), &mut name) }.ok()?;
    let len = name.iter().position(|&c| c == 0)?;
    let name = String::from_utf16(&name[..len]).ok()?;
    let start = name.find('{')?;
    let end = name.find('}')?;
    Some(name.get(start..=end)?.to_string())
}

/// Length of the file's content, sparse or not. Asks the file rather than the
/// directory, whose copy can be stale when a file has several hard links. Does not
/// recall anything from the cloud.
pub(crate) fn logical_len(p: &Path) -> io::Result<u64> {
    let info = file_info(p)?;
    Ok((u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow))
}

/// Size on disk as reported by `GetCompressedFileSizeW`: the compressed size for
/// compressed or sparse files, the plain length otherwise.
pub fn allocated_size(p: &Path) -> io::Result<u64> {
    let wide = to_long_path(p);
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&mut high)) };
    if low == u32::MAX {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(0) {
            return Err(err);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

/// Opens with `FILE_READ_ATTRIBUTES` only and without following reparse points, so
/// nothing is recalled from the cloud.
fn file_info(p: &Path) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let wide = to_long_path(p);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    }
    .map_err(io::Error::from)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(io::Error::from);
    let _ = unsafe { CloseHandle(handle) };
    result.map(|()| info)
}

/// `(volume serial, file id, link count)`, by way of [`file_info`].
pub fn link_info(p: &Path) -> io::Result<(u64, u64, u32)> {
    let info = file_info(p)?;
    let id = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        id,
        info.nNumberOfLinks,
    ))
}

/// `(total bytes, free bytes available to the caller)` of the volume holding `p`.
pub fn volume_space(p: &Path) -> io::Result<(u64, u64)> {
    let wide = to_long_path(p);
    let (mut free, mut total) = (0u64, 0u64);
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free),
            Some(&mut total),
            None,
        )
    }
    .map_err(io::Error::from)?;
    Ok((total, free))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long(p: &str) -> String {
        let wide = to_long_path(Path::new(p));
        assert_eq!(wide.last(), Some(&0), "{p} is not null-terminated");
        String::from_utf16(&wide[..wide.len() - 1]).unwrap()
    }

    #[test]
    fn prefixes_absolute_paths() {
        assert_eq!(long(r"C:\a\b"), r"\\?\C:\a\b");
        assert_eq!(long("C:/a/b"), r"\\?\C:\a\b");
        assert_eq!(long(r"C:\"), r"\\?\C:\");
        assert_eq!(long(r"C:\a\"), r"\\?\C:\a");
        assert_eq!(long(r"\\srv\share\a"), r"\\?\UNC\srv\share\a");
        assert_eq!(long(r"\\srv\share"), r"\\?\UNC\srv\share\");
    }

    #[test]
    fn resolves_dots_because_the_prefix_disables_win32_normalization() {
        assert_eq!(long(r"C:\a\.\b\..\c"), r"\\?\C:\a\c");
        assert_eq!(long(r"C:\..\..\a"), r"\\?\C:\a");
        assert_eq!(long(r"\\srv\share\..\a"), r"\\?\UNC\srv\share\a");
    }

    #[test]
    fn keeps_literal_trailing_dots_and_spaces() {
        assert_eq!(long(r"C:\a.\b "), r"\\?\C:\a.\b ");
    }

    #[test]
    fn leaves_prefixed_and_relative_paths_alone() {
        assert_eq!(long(r"\\?\C:\a\..\b"), r"\\?\C:\a\..\b");
        assert_eq!(long(r"\\?\UNC\srv\share\a"), r"\\?\UNC\srv\share\a");
        assert_eq!(long(r"a\b"), r"a\b");
        assert_eq!(long(r"C:a"), r"C:a");
        assert_eq!(long(r"\a"), r"\a");
    }

    #[test]
    fn list_dir_skips_dot_entries_and_reports_attrs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let mut entries = list_dir(dir.path()).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["d", "f"]);
        assert_ne!(entries[0].attrs & 0x10, 0);
        assert_eq!(entries[1].attrs & 0x10, 0);
        assert!(entries[1].modified.is_some());
        assert!(list_dir(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn size_link_and_volume_queries() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(allocated_size(&file).unwrap(), 5);
        assert_eq!(link_info(&file).unwrap().2, 1);
        std::fs::hard_link(&file, dir.path().join("g")).unwrap();
        let (vol, id, links) = link_info(&file).unwrap();
        assert_eq!(links, 2);
        assert_eq!(
            (vol, id),
            link_info(&dir.path().join("g"))
                .map(|i| (i.0, i.1))
                .unwrap()
        );
        let (total, free) = volume_space(dir.path()).unwrap();
        assert!(total > 0 && free <= total);
    }
}
