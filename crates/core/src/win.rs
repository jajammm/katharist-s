use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, Prefix};

use windows::core::PCWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, GetLongPathNameW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GETFINALPATHNAMEBYHANDLE_FLAGS, OPEN_EXISTING, VOLUME_NAME_DOS,
};

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
}
