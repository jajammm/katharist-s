use std::path::PathBuf;

use windows::core::PCWSTR;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use winreg::enums::{
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, REG_EXPAND_SZ, REG_SZ,
};
use winreg::RegKey;

pub type NaiveDateLike = (u16, u8, u8);

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const UNINSTALL_WOW: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Str(String),
    Dword(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    /// Name of the registry subkey, which is what `still_registered` looks up.
    pub key: String,
    pub name: String,
    pub publisher: Option<String>,
    pub version: Option<String>,
    pub install_date: Option<NaiveDateLike>,
    pub size: u64,
    pub install_location: Option<PathBuf>,
    pub uninstall: Option<String>,
}

fn text(get: &dyn Fn(&str) -> Option<RegValue>, name: &str) -> Option<String> {
    match get(name)? {
        RegValue::Str(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        RegValue::Dword(_) => None,
    }
}

fn dword(get: &dyn Fn(&str) -> Option<RegValue>, name: &str) -> Option<u32> {
    match get(name)? {
        RegValue::Dword(d) => Some(d),
        RegValue::Str(s) => s.trim().parse().ok(),
    }
}

fn parse_date(s: &str) -> Option<NaiveDateLike> {
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let y: u16 = s[..4].parse().ok()?;
    let m: u8 = s[4..6].parse().ok()?;
    let d: u8 = s[6..].parse().ok()?;
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// Builds an entry from one Uninstall subkey's values, or `None` for entries
/// that are not user-facing apps. `QuietUninstallString` is ignored on purpose.
pub fn entry_from_values(key: &str, get: &dyn Fn(&str) -> Option<RegValue>) -> Option<AppEntry> {
    let name = text(get, "DisplayName")?;
    if dword(get, "SystemComponent") == Some(1) || text(get, "ParentKeyName").is_some() {
        return None;
    }
    Some(AppEntry {
        key: key.to_string(),
        name,
        publisher: text(get, "Publisher"),
        version: text(get, "DisplayVersion"),
        install_date: text(get, "InstallDate").and_then(|s| parse_date(&s)),
        size: u64::from(dword(get, "EstimatedSize").unwrap_or(0)) * 1024,
        install_location: text(get, "InstallLocation").map(PathBuf::from),
        uninstall: text(get, "UninstallString"),
    })
}

fn expand(s: &str) -> String {
    let src: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    // SAFETY: `src` is null-terminated and `buf` is sized by the first call.
    unsafe {
        let n = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None);
        if n == 0 {
            return s.to_string();
        }
        let mut buf = vec![0u16; n as usize];
        let n = ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) as usize;
        if n == 0 || n > buf.len() {
            return s.to_string();
        }
        String::from_utf16_lossy(&buf[..n - 1])
    }
}

fn read_value(key: &RegKey, name: &str) -> Option<RegValue> {
    let raw = key.get_raw_value(name).ok()?;
    if raw.vtype == REG_DWORD {
        let b: [u8; 4] = raw.bytes.get(..4)?.try_into().ok()?;
        return Some(RegValue::Dword(u32::from_le_bytes(b)));
    }
    if raw.vtype != REG_SZ && raw.vtype != REG_EXPAND_SZ {
        return None;
    }
    let units: Vec<u16> = raw
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    let s = String::from_utf16_lossy(&units);
    Some(RegValue::Str(if raw.vtype == REG_EXPAND_SZ {
        expand(&s)
    } else {
        s
    }))
}

fn locations() -> [(RegKey, &'static str); 3] {
    [
        (RegKey::predef(HKEY_LOCAL_MACHINE), UNINSTALL),
        (RegKey::predef(HKEY_CURRENT_USER), UNINSTALL),
        (RegKey::predef(HKEY_LOCAL_MACHINE), UNINSTALL_WOW),
    ]
}

/// Installed apps from HKLM, HKCU and the 32-bit HKLM view. The same app in
/// several hives (same name and version) appears once, under the first key found.
pub fn list_installed() -> Vec<AppEntry> {
    let mut out: Vec<AppEntry> = Vec::new();
    for (root, path) in locations() {
        let Ok(parent) = root.open_subkey_with_flags(path, KEY_READ) else {
            continue;
        };
        for sub in parent.enum_keys().flatten() {
            let Ok(k) = parent.open_subkey_with_flags(&sub, KEY_READ) else {
                continue;
            };
            let Some(e) = entry_from_values(&sub, &|n| read_value(&k, n)) else {
                continue;
            };
            if !out
                .iter()
                .any(|o| o.name == e.name && o.version == e.version)
            {
                out.push(e);
            }
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    out
}

/// Only a key that is not found is gone. Any other failure, such as access denied,
/// says nothing about the app, and calling it gone would offer the leftovers of an
/// app that is still installed.
fn answers_registered<T>(opened: &std::io::Result<T>) -> bool {
    !matches!(opened, Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Whether a subkey with this name still exists in any of the three locations, or
/// could not be ruled out.
pub fn still_registered(key: &str) -> bool {
    locations().iter().any(|(root, path)| {
        answers_registered(
            &root
                .open_subkey_with_flags(path, KEY_READ)
                .and_then(|p| p.open_subkey_with_flags(key, KEY_READ)),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn values(pairs: &[(&str, RegValue)]) -> HashMap<String, RegValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn s(v: &str) -> RegValue {
        RegValue::Str(v.to_string())
    }

    #[test]
    fn filters_system_and_nameless_entries() {
        let entry = |pairs: &[(&str, RegValue)]| {
            let m = values(pairs);
            entry_from_values("k", &|n| m.get(n).cloned())
        };
        assert!(entry(&[("Publisher", s("P"))]).is_none());
        assert!(entry(&[("DisplayName", s("   "))]).is_none());
        assert!(entry(&[
            ("DisplayName", s("A")),
            ("SystemComponent", RegValue::Dword(1))
        ])
        .is_none());
        assert!(entry(&[("DisplayName", s("A")), ("ParentKeyName", s("Parent"))]).is_none());
        let e = entry(&[
            ("DisplayName", s("A")),
            ("SystemComponent", RegValue::Dword(0)),
            ("QuietUninstallString", s("quiet.exe")),
        ])
        .unwrap();
        assert_eq!((e.key.as_str(), e.name.as_str()), ("k", "A"));
        assert_eq!(e.uninstall, None);
    }

    #[test]
    fn converts_size_and_date() {
        let m = values(&[
            ("DisplayName", s("A")),
            ("EstimatedSize", RegValue::Dword(2048)),
            ("InstallDate", s("20240131")),
            ("UninstallString", s("u.exe /S")),
        ]);
        let e = entry_from_values("k", &|n| m.get(n).cloned()).unwrap();
        assert_eq!(e.size, 2_097_152);
        assert_eq!(e.install_date, Some((2024, 1, 31)));
        assert_eq!(e.uninstall.as_deref(), Some("u.exe /S"));
        let m = values(&[("DisplayName", s("A")), ("InstallDate", s("abc"))]);
        let e = entry_from_values("k", &|n| m.get(n).cloned()).unwrap();
        assert_eq!(e.install_date, None);
        assert_eq!(e.size, 0);
        assert_eq!(parse_date("20241345"), None);
    }

    #[test]
    fn only_a_key_that_is_not_found_counts_as_gone() {
        let err = |kind| Err::<(), _>(std::io::Error::from(kind));
        assert!(!answers_registered(&err(std::io::ErrorKind::NotFound)));
        assert!(answers_registered(&err(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(answers_registered(&err(std::io::ErrorKind::Other)));
        assert!(answers_registered(&Ok(())));
        // What winreg hands back for a missing key, ERROR_FILE_NOT_FOUND.
        assert!(!answers_registered(&Err::<(), _>(
            std::io::Error::from_raw_os_error(2)
        )));
    }

    #[test]
    fn list_installed_on_this_machine_returns_entries() {
        let apps = list_installed();
        assert!(!apps.is_empty());
        assert!(apps.iter().all(|a| !a.name.is_empty()));
        assert!(still_registered(&apps[0].key));
        assert!(!still_registered("no-such-key-{00000000-katharistis}"));
    }

    #[test]
    fn expands_environment_variables() {
        let out = expand(r"%SystemRoot%\x.exe");
        assert!(!out.contains('%'));
        assert!(out.ends_with(r"\x.exe"));
    }
}
