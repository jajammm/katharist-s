//! Items that belong to an installed app or a browser profile are flagged
//! "dipakai aplikasi" in the map and lists. The flag warns; it never blocks.

use std::path::{Path, PathBuf};

use super::AppEntry;
use crate::protect::{canonical_key, is_protected, ProtectContext};

#[derive(Debug, Clone, Default)]
pub struct UsageMarks {
    /// Canonical keys of the folders whose contents count as in use.
    roots: Vec<String>,
}

/// `key` is `base` or lies below it, compared by whole path component so
/// `c:\apps\foo` does not cover `c:\apps\foobar`.
pub(crate) fn key_at_or_under(key: &str, base: &str) -> bool {
    let base = base.trim_end_matches('\\');
    key.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// A drive root (`c:\`) or a share root (`\\server\share`), as a canonical key.
pub(crate) fn is_root_key(key: &str) -> bool {
    match key.strip_prefix(r"\\") {
        Some(unc) => unc.trim_end_matches('\\').split('\\').count() <= 2,
        None => key.trim_end_matches('\\').len() <= 2,
    }
}

/// Registry values sometimes arrive quoted.
pub(crate) fn unquote(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) => PathBuf::from(s.trim().trim_matches('"').trim()),
        None => p.to_path_buf(),
    }
}

/// Whether `p` may serve as a mark root. A drive root or a protected folder
/// (the profile, Documents, AppData, ...) would mark far more than one app, so a
/// bad `InstallLocation` is ignored. Folders inside Program Files are protected
/// as a subtree, yet they are exactly where apps live, so they stay usable.
fn usable_root(p: &Path, ctx: &ProtectContext) -> Option<String> {
    let key = canonical_key(p)?;
    if is_root_key(&key) {
        return None;
    }
    if is_protected(p, ctx) {
        let inside_program_files = ctx
            .program_files
            .iter()
            .any(|pf| canonical_key(pf).is_some_and(|pf| key != pf && key_at_or_under(&key, &pf)));
        if !inside_program_files {
            return None;
        }
    }
    Some(key)
}

impl UsageMarks {
    pub fn new(apps: &[AppEntry], browser_profile_roots: Vec<PathBuf>) -> Self {
        Self::with_context(apps, browser_profile_roots, &ProtectContext::from_env())
    }

    pub fn with_context(
        apps: &[AppEntry],
        browser_profile_roots: Vec<PathBuf>,
        ctx: &ProtectContext,
    ) -> Self {
        let mut roots: Vec<String> = Vec::new();
        let candidates = apps
            .iter()
            .filter_map(|a| a.install_location.as_deref().map(unquote))
            .chain(browser_profile_roots);
        for p in candidates {
            if let Some(key) = usable_root(&p, ctx) {
                if !roots.contains(&key) {
                    roots.push(key);
                }
            }
        }
        UsageMarks { roots }
    }

    /// `false` for a path that has no canonical key.
    pub fn is_app_owned(&self, p: &Path) -> bool {
        canonical_key(p).is_some_and(|key| self.roots.iter().any(|r| key_at_or_under(&key, r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn app(loc: Option<&Path>) -> AppEntry {
        AppEntry {
            key: "k".into(),
            name: "Foo".into(),
            publisher: None,
            version: None,
            install_date: None,
            size: 0,
            install_location: loc.map(Path::to_path_buf),
            uninstall: None,
        }
    }

    fn ctx(base: &Path) -> ProtectContext {
        ProtectContext {
            windows_dir: base.join("Windows"),
            program_files: vec![base.join("Program Files")],
            program_data: base.join("ProgramData"),
            users_dir: base.join("Users"),
            profile: base.join("Users").join("me"),
            own_dirs: vec![base.join("Users").join("me").join("own")],
        }
    }

    #[test]
    fn marks_inside_install_location_and_browser_profiles() {
        let t = tempdir().unwrap();
        let ctx = ctx(t.path());
        let other_drive_app = t.path().join("Games").join("Foo");
        let pf_app = t.path().join("Program Files").join("Bar");
        let local_app = t
            .path()
            .join("Users")
            .join("me")
            .join("AppData")
            .join("Local")
            .join("Programs")
            .join("Baz");
        let chrome = t
            .path()
            .join("Users")
            .join("me")
            .join("Chrome")
            .join("User Data");
        let apps = [
            app(Some(&other_drive_app)),
            app(Some(&pf_app)),
            app(Some(&local_app)),
            app(None),
        ];
        let m = UsageMarks::with_context(&apps, vec![chrome.clone()], &ctx);

        assert!(m.is_app_owned(&other_drive_app));
        assert!(m.is_app_owned(&other_drive_app.join("data").join("x.bin")));
        assert!(m.is_app_owned(&pf_app.join("bin")));
        assert!(m.is_app_owned(&local_app.join("a.exe")));
        assert!(m.is_app_owned(&chrome.join("Default").join("Cache")));
        // Case and slash style do not matter.
        let shouty = other_drive_app
            .to_string_lossy()
            .to_uppercase()
            .replace('\\', "/");
        assert!(m.is_app_owned(Path::new(&shouty)));
        // Unrelated and relative paths are not marked.
        assert!(!m.is_app_owned(&t.path().join("Games")));
        assert!(!m.is_app_owned(&t.path().join("Elsewhere").join("x")));
        assert!(!m.is_app_owned(Path::new(r"relative\Foo")));
        assert!(!m.is_app_owned(&t.path().join("Program Files")));
    }

    #[test]
    fn marks_do_not_match_prefix_siblings() {
        let t = tempdir().unwrap();
        let foo = t.path().join("Apps").join("Foo");
        let m = UsageMarks::with_context(&[app(Some(&foo))], vec![], &ctx(t.path()));
        assert!(m.is_app_owned(&foo));
        assert!(m.is_app_owned(&foo.join("x")));
        assert!(!m.is_app_owned(&t.path().join("Apps").join("FooBar")));
        assert!(!m.is_app_owned(&t.path().join("Apps").join("FooBar").join("x")));
    }

    #[test]
    fn bad_install_locations_mark_nothing() {
        let t = tempdir().unwrap();
        let c = ctx(t.path());
        let docs = c.profile.join("Documents");
        fs::create_dir_all(&docs).unwrap();
        let bad = [
            Some(PathBuf::from(r"C:\")),
            Some(PathBuf::from("C:")),
            Some(PathBuf::from(r"\\server\share")),
            Some(c.profile.clone()),
            Some(docs.clone()),
            Some(c.users_dir.clone()),
            Some(c.program_files[0].clone()),
            Some(c.windows_dir.clone()),
            Some(PathBuf::from(r"relative\Foo")),
            Some(PathBuf::new()),
            None,
        ];
        let apps: Vec<AppEntry> = bad.iter().map(|l| app(l.as_deref())).collect();
        let m = UsageMarks::with_context(&apps, vec![PathBuf::from(r"D:\")], &c);
        assert!(m.roots.is_empty(), "{:?}", m.roots);
        assert!(!m.is_app_owned(Path::new(r"C:\Anything\x")));
        assert!(!m.is_app_owned(&docs.join("save.dat")));
    }

    #[test]
    fn quoted_install_location_still_marks() {
        let t = tempdir().unwrap();
        let foo = t.path().join("Apps").join("Foo");
        let quoted = PathBuf::from(format!("\"{}\"", foo.display()));
        let m = UsageMarks::with_context(&[app(Some(&quoted))], vec![], &ctx(t.path()));
        assert!(m.is_app_owned(&foo.join("x")));
    }
}
