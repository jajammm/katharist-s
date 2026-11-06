use std::ffi::OsStr;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallCmd {
    pub file: String,
    pub args: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallEnd {
    Exited,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftoverState {
    Ready,
    StillInstalled,
}

/// Splits an `UninstallString` into the program and its arguments. A quoted
/// path is taken as written. An unquoted one is grown token by token until
/// `exists` accepts it, and `MsiExec` is accepted by name. Returns `None` when
/// no program can be established.
pub fn parse_uninstall(s: &str, exists: &dyn Fn(&str) -> bool) -> Option<UninstallCmd> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        let file = &rest[..end];
        if file.is_empty() {
            return None;
        }
        return Some(UninstallCmd {
            file: file.to_string(),
            args: rest[end + 1..].trim().to_string(),
        });
    }
    let mut ends: Vec<usize> = s
        .char_indices()
        .filter(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .collect();
    ends.push(s.len());
    let split = |end: usize| UninstallCmd {
        file: s[..end].to_string(),
        args: s[end..].trim().to_string(),
    };
    for &end in &ends {
        if end > 0 && exists(&s[..end]) {
            return Some(split(end));
        }
    }
    let first = ends[0];
    let name = s[..first].to_ascii_lowercase();
    if name == "msiexec" || name == "msiexec.exe" {
        return Some(split(first));
    }
    None
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Starts the app's own uninstaller through the shell, so its UAC prompt shows,
/// and waits for that process. The result only says the process ended: many
/// uninstallers copy themselves elsewhere and exit at once, so callers decide
/// completion with `leftover_state`.
pub fn run_uninstaller(cmd: &UninstallCmd) -> io::Result<UninstallEnd> {
    let file = wide(&cmd.file);
    let args = wide(&cmd.args);
    let verb = wide("open");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: if cmd.args.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(args.as_ptr())
        },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` and the wide strings it points to outlive the call.
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        if e.code() == ERROR_CANCELLED.to_hresult() {
            return Ok(UninstallEnd::Cancelled);
        }
        return Err(io::Error::from_raw_os_error(e.code().0 & 0xFFFF));
    }
    let process: HANDLE = info.hProcess;
    if !process.is_invalid() {
        // SAFETY: NOCLOSEPROCESS hands us this handle, so we wait on it and close it.
        unsafe {
            WaitForSingleObject(process, INFINITE);
            let _ = CloseHandle(process);
        }
    }
    Ok(UninstallEnd::Exited)
}

/// Leftovers are only offered once the app has left the registry.
pub fn leftover_state(key: &str, registered: &dyn Fn(&str) -> bool) -> LeftoverState {
    if registered(key) {
        LeftoverState::StillInstalled
    } else {
        LeftoverState::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quoted_msi_and_unquoted_paths() {
        let e = |p: &str| p == r"C:\Program Files\X\unins000.exe";
        let c = parse_uninstall(r"C:\Program Files\X\unins000.exe /S", &e).unwrap();
        assert_eq!(c.file, r"C:\Program Files\X\unins000.exe");
        assert_eq!(c.args, "/S");
        let c = parse_uninstall(r#""C:\a b\u.exe" --x"#, &|_| true).unwrap();
        assert_eq!(c.file, r"C:\a b\u.exe");
        assert_eq!(c.args, "--x");
        let c = parse_uninstall("MsiExec.exe /X{ABC}", &|_| false).unwrap();
        assert_eq!(c.file, "MsiExec.exe");
        assert_eq!(c.args, "/X{ABC}");
    }

    #[test]
    fn unresolvable_strings_give_none() {
        assert!(parse_uninstall(r"C:\nowhere\u.exe /S", &|_| false).is_none());
        assert!(parse_uninstall("", &|_| true).is_none());
        assert!(parse_uninstall(r#""unterminated"#, &|_| true).is_none());
    }

    #[test]
    fn no_leftovers_while_still_registered() {
        assert!(matches!(
            leftover_state("k", &|_| true),
            LeftoverState::StillInstalled
        ));
        assert!(matches!(
            leftover_state("k", &|_| false),
            LeftoverState::Ready
        ));
    }
}
