//! Folders an uninstalled app may have left behind. These can hold saves and
//! projects, so the list errs towards offering too little, and nothing in it is
//! ever pre-selected.

use std::path::{Path, PathBuf};

use super::marks::{is_root_key, key_at_or_under, unquote};
use super::AppEntry;
use crate::protect::{canonical_for_delete, canonical_key, is_protected, ProtectContext};
use crate::scan::{self, EntryClass, ScanOptions, ScanProgress};
use crate::win;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Certainty {
    Sure,
    Maybe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    pub path: PathBuf,
    pub size: u64,
    pub certainty: Certainty,
}

#[derive(Debug, Clone)]
pub struct LeftoverRoots {
    pub roaming: PathBuf,
    pub local: PathBuf,
    pub program_data: PathBuf,
}

/// Shorter names match too many unrelated folders to be offered on name alone.
const MIN_NAME_ONLY: usize = 4;
const NEVER_OFFERED: [&str; 9] = [
    "microsoft",
    "google",
    "mozilla",
    "packages",
    "programs",
    "temp",
    "local",
    "roaming",
    "locallow",
];
const ARCH_MARKERS: [&str; 9] = [
    "x64", "x86", "x86_64", "amd64", "win64", "win32", "64-bit", "32-bit", "64bit",
];
const COMPANY_SUFFIXES: [&str; 14] = [
    "inc",
    "llc",
    "ltd",
    "corp",
    "corporation",
    "co",
    "gmbh",
    "limited",
    "company",
    "plc",
    "ag",
    "sa",
    "bv",
    "oy",
];

fn is_version(token: &str) -> bool {
    let t = token.strip_prefix('v').unwrap_or(token);
    t.chars().next().is_some_and(|c| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Lowercase name without architecture markers, empty brackets and trailing
/// version numbers: `7-Zip 23.01 (x64)` becomes `7-zip`. Empty when nothing else
/// is left, which callers treat as "no match".
pub fn normalize_app_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let brackets = |c: char| matches!(c, '(' | ')' | '[' | ']' | ',' | ';');
    let mut tokens: Vec<&str> = lower
        .split_whitespace()
        .filter(|t| {
            let core = t.trim_matches(brackets);
            let only_brackets = core.is_empty() && !t.is_empty();
            !(only_brackets || ARCH_MARKERS.contains(&core))
        })
        .collect();
    while tokens.len() > 1 && tokens.last().is_some_and(|t| is_version(t) || *t == "-") {
        tokens.pop();
    }
    while tokens.len() > 1 && tokens.last() == Some(&"-") {
        tokens.pop();
    }
    tokens.join(" ")
}

/// Lowercase publisher with company suffixes (`Inc.`, `LLC`, `Corporation`) cut off.
fn normalize_publisher(publisher: &str) -> String {
    let mut s = publisher.trim().to_lowercase();
    loop {
        let trimmed = s.trim_end_matches([',', '.', ' ', ';']).to_string();
        let cut = trimmed
            .rsplit_once([' ', ','])
            .filter(|(head, last)| !head.trim().is_empty() && COMPANY_SUFFIXES.contains(last))
            .map(|(head, _)| head.trim_end_matches([',', ' ']).to_string());
        match cut {
            Some(head) => s = head,
            None => return trimmed,
        }
    }
}

/// The exact value and the normalized one, lowercase and without duplicates.
fn publisher_names(publisher: Option<&str>) -> Vec<String> {
    let Some(p) = publisher.map(str::trim).filter(|p| !p.is_empty()) else {
        return Vec::new();
    };
    let mut names = vec![p.to_lowercase()];
    let normalized = normalize_publisher(p);
    if !normalized.is_empty() && !names.contains(&normalized) {
        names.push(normalized);
    }
    names
}

/// An existing, real (not link) folder.
fn is_plain_dir(p: &Path) -> bool {
    win::entry_info(p).is_ok_and(|e| scan::classify(e.attrs, e.reparse_tag) == EntryClass::Dir)
}

fn plain_subdirs(p: &Path) -> Vec<String> {
    win::list_dir(p)
        .map(|entries| {
            entries
                .into_iter()
                .filter(|e| scan::classify(e.attrs, e.reparse_tag) == EntryClass::Dir)
                .map(|e| e.name)
                .collect()
        })
        .unwrap_or_default()
}

struct Guard<'a> {
    ctx: &'a ProtectContext,
    never: Vec<String>,
    other_names: Vec<String>,
    other_locations: Vec<String>,
    root_keys: Vec<String>,
}

impl Guard<'_> {
    /// Everything a folder must satisfy before it may be offered, whichever way
    /// it was found. Returns its canonical key.
    fn allows(&self, p: &Path) -> Option<String> {
        let key = canonical_key(p)?;
        if is_root_key(&key) || is_protected(p, self.ctx) {
            return None;
        }
        // A parent that is a junction or symlink: the folder is not where it seems.
        let real = canonical_for_delete(p).and_then(|r| canonical_key(&r))?;
        if real != key || !is_plain_dir(p) {
            return None;
        }
        let name = p.file_name()?.to_string_lossy().to_lowercase();
        if self.never.contains(&name) || self.other_names.contains(&name) {
            return None;
        }
        // The folder must not contain a scan root or another app's install.
        let covers = |other: &String| key_at_or_under(other, &key);
        if self.root_keys.iter().any(covers) || self.other_locations.iter().any(covers) {
            return None;
        }
        Some(key)
    }
}

pub fn find_leftovers(app: &AppEntry, roots: &LeftoverRoots, others: &[AppEntry]) -> Vec<Leftover> {
    find_leftovers_with(app, roots, others, &ProtectContext::from_env())
}

/// Same as [`find_leftovers`] with the protected locations passed in.
///
/// A publisher folder is matched against the registry value as written and with
/// company suffixes cut (`Google LLC` also matches `Google`).
pub fn find_leftovers_with(
    app: &AppEntry,
    roots: &LeftoverRoots,
    others: &[AppEntry],
    ctx: &ProtectContext,
) -> Vec<Leftover> {
    let name = normalize_app_name(&app.name);
    if name.is_empty() {
        return Vec::new();
    }
    let publishers = publisher_names(app.publisher.as_deref());

    let mut never: Vec<String> = NEVER_OFFERED.iter().map(|s| s.to_string()).collect();
    never.extend(publishers.iter().cloned());
    for other in others {
        never.extend(publisher_names(other.publisher.as_deref()));
    }
    let guard = Guard {
        ctx,
        never,
        other_names: others
            .iter()
            .map(|o| normalize_app_name(&o.name))
            .filter(|n| !n.is_empty())
            .collect(),
        other_locations: others
            .iter()
            .filter_map(|o| o.install_location.as_deref())
            .filter_map(|p| canonical_key(&unquote(p)))
            .collect(),
        root_keys: [&roots.roaming, &roots.local, &roots.program_data]
            .into_iter()
            .filter_map(|p| canonical_key(p))
            .collect(),
    };

    let mut found: Vec<(String, Leftover)> = Vec::new();
    let mut consider = |path: PathBuf, certainty: Certainty| {
        let Some(key) = guard.allows(&path) else {
            return;
        };
        if let Some(i) = found.iter().position(|(k, _)| *k == key) {
            if certainty == Certainty::Sure {
                found[i].1.certainty = Certainty::Sure;
            }
            return;
        }
        let opts = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        // A folder that cannot be sized is not offered: size is what the list shows.
        let Ok(tree) = scan::scan(&path, &opts, &ScanProgress::default()) else {
            return;
        };
        let size = tree.nodes[tree.root].size;
        found.push((
            key,
            Leftover {
                path,
                size,
                certainty,
            },
        ));
    };

    if let Some(loc) = app.install_location.as_deref() {
        consider(unquote(loc), Certainty::Sure);
    }
    for root in [&roots.roaming, &roots.local, &roots.program_data] {
        for entry in plain_subdirs(root) {
            let lower = entry.to_lowercase();
            if publishers.contains(&lower) {
                let publisher_dir = root.join(&entry);
                for child in plain_subdirs(&publisher_dir) {
                    if child.to_lowercase() == name {
                        consider(publisher_dir.join(child), Certainty::Sure);
                    }
                }
            } else if lower == name && name.chars().count() >= MIN_NAME_ONLY {
                consider(root.join(entry), Certainty::Maybe);
            }
        }
    }

    let mut out: Vec<Leftover> = found.into_iter().map(|(_, l)| l).collect();
    out.sort_by(|a, b| {
        (a.certainty == Certainty::Maybe)
            .cmp(&(b.certainty == Certainty::Maybe))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};

    struct Fx {
        _t: TempDir,
        base: PathBuf,
        roots: LeftoverRoots,
        ctx: ProtectContext,
    }

    fn fx() -> Fx {
        let t = tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let base = PathBuf::from(base.to_string_lossy().trim_start_matches(r"\\?\"));
        let roots = LeftoverRoots {
            roaming: base
                .join("Users")
                .join("me")
                .join("AppData")
                .join("Roaming"),
            local: base.join("Users").join("me").join("AppData").join("Local"),
            program_data: base.join("ProgramData"),
        };
        for r in [&roots.roaming, &roots.local, &roots.program_data] {
            fs::create_dir_all(r).unwrap();
        }
        let ctx = ProtectContext {
            windows_dir: base.join("Windows"),
            program_files: vec![base.join("Program Files")],
            program_data: roots.program_data.clone(),
            users_dir: base.join("Users"),
            profile: base.join("Users").join("me"),
            own_dirs: vec![roots.roaming.join("Katharistis")],
        };
        Fx {
            _t: t,
            base,
            roots,
            ctx,
        }
    }

    fn app(name: &str, publisher: Option<&str>, loc: Option<&Path>) -> AppEntry {
        AppEntry {
            key: name.into(),
            name: name.into(),
            publisher: publisher.map(Into::into),
            version: None,
            install_date: None,
            size: 0,
            install_location: loc.map(Path::to_path_buf),
            uninstall: None,
        }
    }

    fn mk(dir: &Path, file: &str, bytes: usize) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), vec![7u8; bytes]).unwrap();
    }

    fn run(f: &Fx, a: &AppEntry, others: &[AppEntry]) -> Vec<Leftover> {
        find_leftovers_with(a, &f.roots, others, &f.ctx)
    }

    fn paths(v: &[Leftover]) -> Vec<PathBuf> {
        v.iter().map(|l| l.path.clone()).collect()
    }

    #[test]
    fn normalizes_version_and_arch() {
        assert_eq!(normalize_app_name("7-Zip 23.01 (x64)"), "7-zip");
        assert_eq!(normalize_app_name("Slack"), "slack");
        assert_eq!(normalize_app_name("Python 3.12.1 (64-bit)"), "python");
        assert_eq!(normalize_app_name("Foo x86"), "foo");
        assert_eq!(normalize_app_name("Foo ()"), "foo");
        assert_eq!(normalize_app_name("Foo v2.0"), "foo");
        assert_eq!(normalize_app_name("  Mixed   CASE  "), "mixed case");
    }

    #[test]
    fn normalization_never_yields_a_name_from_nothing() {
        assert_eq!(normalize_app_name("(x64)"), "");
        assert_eq!(normalize_app_name("x64"), "");
        assert_eq!(normalize_app_name("   "), "");
        // A name that is only digits is still a name.
        assert_eq!(normalize_app_name("2048"), "2048");
        let f = fx();
        mk(&f.roots.local.join("Anything"), "a", 1);
        assert!(run(&f, &app("(x64)", Some("Anything"), None), &[]).is_empty());
    }

    #[test]
    fn publisher_folder_is_never_offered() {
        let f = fx();
        mk(&f.roots.local.join("Google").join("Chrome"), "a", 10);
        mk(&f.roots.local.join("Google"), "profile.dat", 10);
        let foo = app("Foo", Some("Google"), None);
        let out = run(&f, &foo, &[]);
        assert!(out.iter().all(|l| l.path.file_name().unwrap() != "Google"));
        assert!(out.is_empty(), "{out:?}");
        // Even an app literally named like its publisher.
        let google = app("Google", Some("Google"), None);
        assert!(run(&f, &google, &[]).is_empty());
    }

    #[test]
    fn never_offered_names_ignore_case() {
        let f = fx();
        for n in ["MICROSOFT", "mozilla", "Packages", "PROGRAMS", "temp"] {
            mk(&f.roots.local.join(n), "a", 1);
            mk(&f.roots.roaming.join(n), "a", 1);
            let a = app(n, None, None);
            assert!(run(&f, &a, &[]).is_empty(), "{n}");
        }
    }

    #[test]
    fn sure_requires_publisher_path_or_install_location() {
        let f = fx();
        mk(&f.roots.local.join("Acme").join("Foobar"), "a.dat", 100);
        mk(&f.roots.roaming.join("Foobar"), "b.dat", 50);
        mk(
            &f.roots.program_data.join("Acme").join("Foobar"),
            "c.dat",
            5,
        );
        let a = app("Foobar 1.2 (x64)", Some("Acme"), None);
        let out = run(&f, &a, &[]);
        let by = |p: PathBuf| out.iter().find(|l| l.path == p).unwrap().certainty;
        assert_eq!(
            by(f.roots.local.join("Acme").join("Foobar")),
            Certainty::Sure
        );
        assert_eq!(
            by(f.roots.program_data.join("Acme").join("Foobar")),
            Certainty::Sure
        );
        // Name only, no publisher folder above it.
        assert_eq!(by(f.roots.roaming.join("Foobar")), Certainty::Maybe);
        assert_eq!(out.len(), 3);
        // Sizes come from the scanner.
        let tree = crate::scan::scan(
            &f.roots.local.join("Acme").join("Foobar"),
            &crate::scan::ScanOptions {
                threads: 2,
                excluded: vec![],
            },
            &crate::scan::ScanProgress::default(),
        )
        .unwrap();
        let sized = out
            .iter()
            .find(|l| l.path == f.roots.local.join("Acme").join("Foobar"))
            .unwrap();
        assert_eq!(sized.size, tree.nodes[tree.root].size);
        assert!(sized.size > 0);
    }

    #[test]
    fn publisher_matches_exact_and_normalized_ignoring_case() {
        let f = fx();
        mk(&f.roots.local.join("Google").join("Foo"), "a", 1);
        mk(
            &f.roots.roaming.join("MOZILLA CORPORATION").join("Bar"),
            "a",
            1,
        );
        mk(&f.roots.roaming.join("mozilla").join("Baz"), "a", 1);
        let g = run(&f, &app("Foo", Some("Google LLC"), None), &[]);
        assert_eq!(paths(&g), vec![f.roots.local.join("Google").join("Foo")]);
        assert_eq!(g[0].certainty, Certainty::Sure);
        let bar = run(&f, &app("Bar", Some("Mozilla Corporation"), None), &[]);
        assert_eq!(bar.len(), 1);
        assert_eq!(bar[0].certainty, Certainty::Sure);
        let baz = run(&f, &app("Baz", Some("Mozilla Corporation"), None), &[]);
        assert_eq!(baz.len(), 1);
        assert_eq!(baz[0].certainty, Certainty::Sure);
        // Wrong publisher: the folder under another publisher is not Sure.
        let other = run(&f, &app("Foo", Some("Acme"), None), &[]);
        assert!(other.is_empty(), "{other:?}");
    }

    #[test]
    fn install_location_is_sure_even_outside_the_roots() {
        let f = fx();
        let loc = f.base.join("Games").join("Foo");
        mk(&loc, "save.dat", 10);
        let out = run(&f, &app("Foo", None, Some(&loc)), &[]);
        assert_eq!(paths(&out), vec![loc.clone()]);
        assert_eq!(out[0].certainty, Certainty::Sure);
        // A missing install location offers nothing.
        let gone = f.base.join("Games").join("Gone");
        assert!(run(&f, &app("Gone", None, Some(&gone)), &[]).is_empty());
    }

    #[test]
    fn name_only_match_is_maybe_and_short_names_skipped() {
        let f = fx();
        mk(&f.roots.roaming.join("Slack"), "a", 1);
        mk(&f.roots.local.join("VLC"), "a", 1);
        mk(&f.roots.roaming.join("Vlc"), "a", 1);
        let slack = run(&f, &app("Slack", None, None), &[]);
        assert_eq!(paths(&slack), vec![f.roots.roaming.join("Slack")]);
        assert_eq!(slack[0].certainty, Certainty::Maybe);
        assert!(run(&f, &app("VLC", None, None), &[]).is_empty());
        // Short, but under the publisher folder: that is still Sure.
        mk(&f.roots.local.join("VideoLAN").join("VLC"), "a", 1);
        let vlc = run(&f, &app("VLC", Some("VideoLAN"), None), &[]);
        assert_eq!(
            paths(&vlc),
            vec![f.roots.local.join("VideoLAN").join("VLC")]
        );
        assert_eq!(vlc[0].certainty, Certainty::Sure);
    }

    #[test]
    fn other_apps_folders_are_excluded() {
        let f = fx();
        mk(&f.roots.roaming.join("Shared"), "a", 1);
        mk(&f.roots.local.join("Acme").join("Shared"), "a", 1);
        mk(&f.roots.roaming.join("Alpha"), "a", 1);
        let a = app("Shared", Some("Acme"), None);
        let twin = app("Shared 2.0 (x64)", Some("Acme"), None);
        assert!(run(&f, &a, &[twin]).is_empty());
        // Publisher of another app is never offered either.
        mk(&f.roots.roaming.join("Beta Co"), "a", 1);
        let b = app("Beta Co", None, None);
        let c = app("Gamma", Some("Beta Co"), None);
        assert!(run(&f, &b, &[c]).is_empty());
        // Another app installed in the same folder.
        let loc = f.base.join("Games").join("Foo");
        mk(&loc, "a", 1);
        let x = app("Foo", None, Some(&loc));
        let y = app("Foo Add-on", None, Some(&loc));
        assert!(run(&f, &x, &[y]).is_empty());
        // Another app installed inside this one's folder.
        let inner = app("Inner", None, Some(&loc.join("inner")));
        assert!(run(&f, &x, &[inner]).is_empty());
        // Unrelated others change nothing.
        let unrelated = app("Zed", Some("Zed Inc."), None);
        assert_eq!(run(&f, &app("Alpha", None, None), &[unrelated]).len(), 1);
    }

    #[test]
    fn dangerous_install_locations_are_never_offered() {
        let f = fx();
        let drive_root = PathBuf::from(r"C:\");
        let cases = [
            drive_root,
            f.ctx.users_dir.clone(),
            f.ctx.profile.clone(),
            f.ctx.profile.join("Documents"),
            f.ctx.program_files[0].clone(),
            f.ctx.windows_dir.clone(),
            f.roots.local.clone(),
            f.roots.roaming.clone(),
            f.roots.roaming.parent().unwrap().to_path_buf(),
            f.roots.program_data.clone(),
            f.base.clone(),
        ];
        for d in &cases {
            let _ = fs::create_dir_all(d);
        }
        for d in &cases {
            let out = run(&f, &app("Foo", Some("Acme"), Some(d)), &[]);
            assert!(out.is_empty(), "{d:?} offered as {out:?}");
        }
        assert!(run(&f, &app("Foo", None, Some(Path::new(r"relative\Foo"))), &[]).is_empty());
    }

    #[test]
    fn link_folders_are_never_offered() {
        let f = fx();
        let target = f.base.join("target");
        mk(&target, "a", 1);
        let link = f.roots.roaming.join("Linked");
        let out = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(run(&f, &app("Linked", None, None), &[]).is_empty());
        assert!(run(&f, &app("Linked", None, Some(&link)), &[]).is_empty());
        // A folder under a linked publisher folder is skipped too.
        let pub_link = f.roots.local.join("Acme");
        mk(&target.join("Foo"), "a", 1);
        let out = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&pub_link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(run(&f, &app("Foo", Some("Acme"), None), &[]).is_empty());
    }

    #[test]
    fn files_with_the_app_name_are_not_leftovers() {
        let f = fx();
        fs::write(f.roots.roaming.join("Slack"), b"x").unwrap();
        assert!(run(&f, &app("Slack", None, None), &[]).is_empty());
    }
}
