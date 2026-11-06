use std::env;
use std::path::{Path, PathBuf};

use crate::win;

const TRAILING_IGNORED_BY_WIN32: [char; 2] = ['.', ' '];

/// Protected only as the folder itself, so the contents stay deletable.
const PROFILE_FOLDERS: [&str; 9] = [
    "Documents",
    "Desktop",
    "Downloads",
    "Pictures",
    "Music",
    "Videos",
    "AppData",
    r"AppData\Local",
    r"AppData\Roaming",
];
const PROFILE_SECRETS: [&str; 2] = [".ssh", ".gnupg"];
/// Lowercase, because they are compared against canonical keys.
const VOLUME_ROOT_ITEMS: [&str; 5] = [
    "system volume information",
    "$recycle.bin",
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
];

pub struct ProtectContext {
    pub windows_dir: PathBuf,
    pub program_files: Vec<PathBuf>,
    pub program_data: PathBuf,
    pub users_dir: PathBuf,
    pub profile: PathBuf,
    pub own_dirs: Vec<PathBuf>,
}

impl ProtectContext {
    /// Reads the locations from the environment. A missing or relative variable
    /// falls back to the standard location, so a stripped environment cannot
    /// leave the system folders unprotected.
    pub fn from_env() -> Self {
        let var = |name: &str| {
            env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let system_root = env::var_os("SystemDrive")
            .map(|mut drive| {
                drive.push(r"\");
                PathBuf::from(drive)
            })
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from(r"C:\"));

        let windows_dir = var("SystemRoot")
            .or_else(|| var("windir"))
            .unwrap_or_else(|| system_root.join("Windows"));

        let mut program_files: Vec<PathBuf> = Vec::new();
        for name in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(dir) = var(name).filter(|dir| !program_files.contains(dir)) {
                program_files.push(dir);
            }
        }
        if program_files.is_empty() {
            program_files.push(system_root.join("Program Files"));
            program_files.push(system_root.join("Program Files (x86)"));
        }

        let program_data = var("ProgramData").unwrap_or_else(|| system_root.join("ProgramData"));
        let profile = var("USERPROFILE").unwrap_or_else(|| {
            let name = env::var_os("USERNAME").unwrap_or_default();
            system_root.join("Users").join(name)
        });
        // %PUBLIC% first: the profile's parent is wrong for service accounts,
        // whose profile lives under the Windows folder.
        let users_dir = var("PUBLIC")
            .as_deref()
            .or(Some(profile.as_path()))
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| system_root.join("Users"));

        let app_data = var("APPDATA").unwrap_or_else(|| profile.join(r"AppData\Roaming"));
        let mut own_dirs = vec![app_data.join("Katharistis")];
        // The exe itself, not its folder: running from Downloads must not make
        // the rest of Downloads undeletable.
        own_dirs.extend(env::current_exe());

        ProtectContext {
            windows_dir,
            program_files,
            program_data,
            users_dir,
            profile,
            own_dirs,
        }
    }
}

/// An absolute path taken apart: `.` and `..` resolved, separators unified, names
/// otherwise untouched.
struct Lexical {
    /// `C:` or `\\server\share`.
    root: String,
    components: Vec<String>,
}

impl Lexical {
    /// `None` for anything that does not name a place on a drive or share:
    /// relative paths, device namespaces such as `\\.\` and `\\?\GLOBALROOT`,
    /// alternate data streams, and text that is not valid Unicode.
    fn parse(p: &Path) -> Option<Self> {
        let text = p.to_str()?.replace('/', r"\");
        let (root, rest) = split_root(&text)?;
        let mut components = Vec::new();
        for part in rest.split('\\') {
            match part {
                "" | "." => {}
                ".." => {
                    components.pop();
                }
                _ if part.contains(':') => return None,
                _ => components.push(part.to_string()),
            }
        }
        Some(Lexical { root, components })
    }

    /// Asks `resolve` about the first `count` components, then about ever shorter
    /// prefixes until it answers, and appends the unanswered components as
    /// written. A missing tail therefore never stops the existing part from
    /// being resolved.
    fn resolve_longest_prefix(
        &self,
        count: usize,
        resolve: impl Fn(&Path) -> Option<String>,
    ) -> String {
        for known in (0..=count).rev() {
            let prefix = render(&self.root, &self.components[..known]);
            if let Some(base) = resolve(Path::new(&prefix)) {
                return render(&base, &self.components[known..count]);
            }
        }
        render(&self.root, &self.components[..count])
    }
}

fn split_root(text: &str) -> Option<(String, &str)> {
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    if let Some(head) = text.get(..VERBATIM_UNC.len()) {
        if head.eq_ignore_ascii_case(VERBATIM_UNC) {
            return split_unc_root(&text[VERBATIM_UNC.len()..]);
        }
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return split_drive_root(rest);
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        return split_unc_root(rest);
    }
    split_drive_root(text)
}

fn split_drive_root(text: &str) -> Option<(String, &str)> {
    match text.as_bytes() {
        [letter, b':', b'\\', ..] if letter.is_ascii_alphabetic() => {
            Some((text[..2].to_string(), &text[3..]))
        }
        _ => None,
    }
}

fn split_unc_root(text: &str) -> Option<(String, &str)> {
    let (server, rest) = text.trim_start_matches('\\').split_once('\\')?;
    let rest = rest.trim_start_matches('\\');
    let (share, tail) = rest.split_once('\\').unwrap_or((rest, ""));
    // `\\.\` is the device namespace, not a server called `.`.
    if server == "." {
        return None;
    }
    let server = server.trim_end_matches(TRAILING_IGNORED_BY_WIN32);
    let share = share.trim_end_matches(TRAILING_IGNORED_BY_WIN32);
    if server.is_empty() || share.is_empty() || server.contains(':') || share.contains(':') {
        return None;
    }
    Some((format!(r"\\{server}\{share}"), tail))
}

/// `base` may be a root or a path, with or without a trailing `\`.
fn render(base: &str, components: &[String]) -> String {
    let mut out = base.trim_end_matches('\\').to_string();
    for component in components {
        out.push('\\');
        out.push_str(component);
    }
    if out.len() == 2 {
        out.push('\\'); // `C:` alone would mean the current folder of that drive.
    }
    out
}

/// A single spelling for a location, for comparing paths. Junctions are not
/// followed; see [`canonical_for_delete`]. `None` when the path cannot be pinned
/// to a location, which covers relative paths and alternate data streams.
///
/// A drive root keeps its `\` (`c:\`) because `c:` means something else.
pub fn canonical_key(p: &Path) -> Option<String> {
    let mut lexical = Lexical::parse(p)?;
    for component in &mut lexical.components {
        let kept = component.trim_end_matches(TRAILING_IGNORED_BY_WIN32).len();
        // `...` or `.. `: whether Win32 sees a name or a parent here depends on
        // the position in the path, so do not guess.
        if kept == 0 {
            return None;
        }
        component.truncate(kept);
    }
    let long = lexical.resolve_longest_prefix(lexical.components.len(), win::long_path_name);
    Some(long.to_lowercase())
}

/// The path to hand to the deletion code: the parent folder fully resolved, the
/// last component left as a name so a junction is removed rather than entered.
/// Any part that no longer exists stays as written. `None` when the path cannot
/// be pinned to a location; treat that as protected.
pub fn canonical_for_delete(p: &Path) -> Option<PathBuf> {
    let lexical = Lexical::parse(p)?;
    let Some((last, parents)) = lexical.components.split_last() else {
        return Some(PathBuf::from(render(&lexical.root, &[])));
    };
    let parent = lexical.resolve_longest_prefix(parents.len(), win::final_path_name);
    Some(PathBuf::from(render(&parent, std::slice::from_ref(last))))
}

/// Every key a context entry may appear under: as configured and with reparse
/// points followed, since [`canonical_for_delete`] reports resolved paths. This
/// is what makes a profile or own folder behind a junction still match.
fn entry_keys(entry: &Path) -> Vec<String> {
    let Some(key) = canonical_key(entry) else {
        return Vec::new();
    };
    let resolved = Lexical::parse(Path::new(&key))
        .map(|lexical| {
            lexical.resolve_longest_prefix(lexical.components.len(), win::final_path_name)
        })
        .and_then(|path| canonical_key(Path::new(&path)));
    let mut keys = vec![key];
    keys.extend(resolved);
    keys
}

fn is_at_or_under(key: &str, base: &str) -> bool {
    let base = base.trim_end_matches('\\');
    key.strip_prefix(base)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
}

/// Rules that follow from the shape of the key alone, whatever the context.
fn violates_root_rules(key: &str) -> bool {
    let (share, after_root) = match key.strip_prefix(r"\\") {
        Some(unc) => {
            let mut parts = unc.splitn(3, '\\');
            let share = parts.nth(1).unwrap_or("");
            (share, parts.next().unwrap_or(""))
        }
        None => ("", key.get(3..).unwrap_or("")),
    };
    // `\\host\c$` and `\\host\admin$` can be the system drive under another name.
    if share.ends_with('$') {
        return true;
    }
    let first = after_root.split('\\').next().unwrap_or("");
    first.is_empty() || VOLUME_ROOT_ITEMS.contains(&first)
}

/// The safety gate for deletion. Fails closed: a path that cannot be pinned to a
/// location is protected. The checked path counts under each spelling it could
/// be reached by, including with reparse points in its parents followed.
pub fn is_protected(p: &Path, ctx: &ProtectContext) -> bool {
    let Some(key) = canonical_key(p) else {
        return true;
    };
    // Resolving the key as well catches `link.\x`: Win32 would open `link\x`,
    // but `link.` taken literally does not exist and would not resolve.
    let resolved =
        [p, Path::new(&key)].map(|path| canonical_for_delete(path).and_then(|r| canonical_key(&r)));
    let [Some(resolved_literal), Some(resolved_normalized)] = resolved else {
        return true;
    };
    let keys = [key.as_str(), &resolved_literal, &resolved_normalized];
    if keys.iter().any(|key| violates_root_rules(key)) {
        return true;
    }

    let mut exact = vec![
        ctx.windows_dir.clone(),
        ctx.program_data.clone(),
        ctx.users_dir.clone(),
        ctx.profile.clone(),
    ];
    exact.extend(PROFILE_FOLDERS.iter().map(|name| ctx.profile.join(name)));
    let exact: Vec<String> = exact.iter().flat_map(|entry| entry_keys(entry)).collect();
    if keys.iter().any(|key| exact.iter().any(|base| base == key)) {
        return true;
    }

    let mut subtree = vec![ctx.windows_dir.clone()];
    subtree.extend(ctx.program_files.iter().cloned());
    subtree.extend(PROFILE_SECRETS.iter().map(|name| ctx.profile.join(name)));
    subtree.extend(ctx.own_dirs.iter().cloned());
    let subtree: Vec<String> = subtree.iter().flat_map(|entry| entry_keys(entry)).collect();
    if keys
        .iter()
        .any(|key| subtree.iter().any(|base| is_at_or_under(key, base)))
    {
        return true;
    }

    // Deleting a folder deletes what is in it, so the chain leading up to each
    // location is protected too. Siblings along that chain stay deletable.
    let mut chained = vec![
        ctx.windows_dir.clone(),
        ctx.program_data.clone(),
        ctx.users_dir.clone(),
        ctx.profile.clone(),
    ];
    chained.extend(ctx.program_files.iter().cloned());
    chained.extend(ctx.own_dirs.iter().cloned());
    let chained: Vec<String> = chained.iter().flat_map(|entry| entry_keys(entry)).collect();
    keys.iter()
        .any(|key| chained.iter().any(|entry| is_at_or_under(entry, key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::{tempdir, TempDir};
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;

    fn ctx() -> ProtectContext {
        ProtectContext {
            windows_dir: r"C:\Windows".into(),
            program_files: vec![r"C:\Program Files".into(), r"C:\Program Files (x86)".into()],
            program_data: r"C:\ProgramData".into(),
            users_dir: r"C:\Users".into(),
            profile: r"C:\Users\me".into(),
            own_dirs: vec![r"C:\Users\me\AppData\Roaming\Katharistis".into()],
        }
    }

    fn ctx_owning(own: &Path) -> ProtectContext {
        ProtectContext {
            own_dirs: vec![own.to_path_buf()],
            ..ctx()
        }
    }

    fn junction(link: &Path, target: &Path) {
        let out = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("cmd runs");
        assert!(out.status.success(), "mklink failed: {out:?}");
    }

    /// `<tmp>\guarded\file.txt` plus the junction `<tmp>\link` pointing at `guarded`.
    fn guarded_with_link() -> (TempDir, PathBuf) {
        let tmp = tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        fs::create_dir(root.join("guarded")).unwrap();
        fs::write(root.join(r"guarded\file.txt"), "x").unwrap();
        junction(&root.join("link"), &root.join("guarded"));
        (tmp, root)
    }

    fn short_path(p: &Path) -> PathBuf {
        let wide = crate::win::to_long_path(p);
        let mut buf = vec![0u16; 1024];
        let len = unsafe { GetShortPathNameW(PCWSTR(wide.as_ptr()), Some(&mut buf)) } as usize;
        assert!(
            len > 0 && len < buf.len(),
            "GetShortPathNameW failed for {p:?}"
        );
        let text = String::from_utf16(&buf[..len]).unwrap();
        PathBuf::from(text.strip_prefix(r"\\?\").unwrap())
    }

    #[test]
    fn blocks_exact_and_subtree() {
        let c = ctx();
        for p in [
            r"C:\",
            r"D:\",
            r"C:\Windows",
            r"C:\Windows\System32\kernel32.dll",
            r"C:\Program Files\App\a.exe",
            r"C:\Program Files (x86)",
            r"C:\Program Files (x86)\App\a.exe",
            r"C:\ProgramData",
            r"C:\Users",
            r"C:\Users\me",
            r"C:\Users\me\Documents",
            r"C:\Users\me\Desktop",
            r"C:\Users\me\Downloads",
            r"C:\Users\me\Pictures",
            r"C:\Users\me\Music",
            r"C:\Users\me\Videos",
            r"C:\Users\me\AppData",
            r"C:\Users\me\AppData\Local",
            r"C:\Users\me\AppData\Roaming",
            r"C:\Users\me\.ssh",
            r"C:\Users\me\.ssh\id_rsa",
            r"C:\Users\me\.gnupg\key",
            r"D:\System Volume Information",
            r"D:\System Volume Information\x",
            r"D:\$Recycle.Bin",
            r"D:\$Recycle.Bin\S-1\a",
            r"D:\pagefile.sys",
            r"D:\hiberfil.sys",
            r"D:\swapfile.sys",
            r"C:\Users\me\AppData\Roaming\Katharistis",
            r"C:\Users\me\AppData\Roaming\Katharistis\settings.toml",
            "relative\\file",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn allows_contents_of_standard_folders() {
        let c = ctx();
        for p in [
            r"C:\Users\me\Documents\a.txt",
            r"C:\Users\me\Downloads\sub",
            r"C:\Users\me\AppData\Local\Temp\x",
            r"C:\Users\me\AppData\Roaming\Other",
            r"C:\Users\me\file.txt",
            r"C:\ProgramData\Vendor",
            r"D:\Games\old",
        ] {
            assert!(!is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn relocated_profile_protects_its_parent_but_not_lookalikes() {
        let c = ProtectContext {
            profile: r"D:\Profiles\me".into(),
            ..ctx()
        };
        for p in [
            r"D:\Profiles",
            r"D:\Profiles\",
            r"d:/PROFILES",
            r"D:\Profiles.",
            r"D:\Profiles\me",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
        for p in [
            r"D:\Profiles2",
            r"D:\Prof",
            r"D:\Profiles\other",
            r"D:\Other",
        ] {
            assert!(!is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn exe_under_downloads_protects_only_the_chain_up_to_it() {
        let c = ctx_owning(Path::new(r"C:\Users\me\Downloads\tool\k.exe"));
        for p in [
            r"C:\Users\me\Downloads\tool",
            r"C:\Users\me\Downloads\tool\k.exe",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
        for p in [
            r"C:\Users\me\Downloads\other",
            r"C:\Users\me\Downloads\tool2",
            r"C:\Users\me\Downloads\tool\other.txt",
            r"C:\Users\me\Downloads\tool\k.exe.bak",
        ] {
            assert!(!is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn every_ancestor_of_every_context_location_is_protected() {
        let c = ProtectContext {
            windows_dir: r"E:\a\b\Windows".into(),
            program_files: vec![r"F:\c\PF1".into(), r"F:\d\PF2".into()],
            program_data: r"G:\e\Data".into(),
            users_dir: r"H:\f\Users".into(),
            profile: r"I:\g\me".into(),
            own_dirs: vec![r"J:\h\own".into(), r"K:\i\k.exe".into()],
        };
        for p in [
            r"E:\a", r"E:\a\b", r"F:\c", r"F:\d", r"G:\e", r"H:\f", r"I:\g", r"J:\h", r"K:\i",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
        for p in [
            r"E:\a\other",
            r"E:\a\b\other",
            r"E:\ab",
            r"F:\c\other",
            r"G:\e\other",
            r"H:\f\other",
            r"I:\g\other",
            r"J:\h\other",
            r"K:\i\other",
        ] {
            assert!(!is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn ancestors_are_seen_through_a_junction() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(r"real\sub\own")).unwrap();
        fs::create_dir(root.join(r"real\other")).unwrap();
        junction(&root.join("alias"), &root.join("real"));
        let c = ctx_owning(&root.join(r"alias\sub\own"));
        assert!(is_protected(&root.join("real"), &c));
        assert!(is_protected(&root.join(r"real\sub"), &c));
        assert!(is_protected(
            &canonical_for_delete(&root.join(r"alias\sub")).unwrap(),
            &c
        ));
        assert!(!is_protected(&root.join(r"real\other"), &c));
    }

    #[test]
    fn names_that_only_share_a_prefix_are_not_protected() {
        let c = ctx();
        for p in [
            r"C:\Windows.old",
            r"C:\Windows2\x",
            r"C:\Program Files Extra\x",
            r"C:\Users\me\.sshx\a",
            r"C:\Users\me\AppData\Roaming\Katharistis2\a",
        ] {
            assert!(!is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn volume_files_are_only_protected_directly_under_the_root() {
        let c = ctx();
        for p in [
            r"D:\sub\pagefile.sys",
            r"D:\sub\System Volume Information",
            r"D:\sub\$Recycle.Bin\a",
        ] {
            assert!(!is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn sees_through_disguises() {
        let c = ctx();
        for p in [
            r"\\?\C:\Windows\x",
            r"\\?\c:\windows",
            r"//?/C:/Windows/x",
            r"c:/WINDOWS/x",
            r"C:\Windows.\x",
            r"C:\Windows \x",
            r"C:\Windows. . \x",
            r"C:\Windows\",
            r"C:\\Windows\\x",
            r"C:\Users\me\Documents ",
            r"C:\Users\me\Documents.",
            r"C:\Users\me\Documents\",
            r"C:\Users\me\x\..\Documents",
            r"C:\Users\me\.\Documents",
            r"C:\..\..\Windows\x",
            r"C:\tmp\..\Windows\x",
            r"\\?\C:\tmp\..\Windows\x",
            r"C:\Windows\x:stream",
            r"D:\Games\x:stream",
            r"D:\Games\x::$DATA",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn paths_that_cannot_be_pinned_to_a_location_are_protected() {
        let c = ctx();
        for p in [
            "",
            r"C:",
            r"C:Windows\x",
            r"\Windows\x",
            r"\??\C:\Windows\x",
            r"\\.\C:\Windows\x",
            r"\\?\GLOBALROOT\Device\HarddiskVolume3\Windows\x",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\Windows\x",
            r"\\server",
            r"D:\Games\...\x",
            r"D:\Games\.. \x",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn network_shares_follow_the_same_root_rules() {
        let c = ctx();
        for p in [
            r"\\srv\share",
            r"\\srv\share\",
            r"\\?\UNC\srv\share",
            r"\\srv\share\pagefile.sys",
            r"\\srv\share\$Recycle.Bin\a",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
        assert!(!is_protected(Path::new(r"\\srv\share\old"), &c));
    }

    #[test]
    fn hidden_shares_can_alias_the_system_drive() {
        let c = ctx();
        for p in [
            r"\\localhost\C$\Windows\x",
            r"\\?\UNC\127.0.0.1\c$\Users\me\Documents",
            r"\\localhost\ADMIN$\x",
        ] {
            assert!(is_protected(Path::new(p), &c), "{p}");
        }
    }

    #[test]
    fn canonical_key_normalizes_every_spelling_to_one_key() {
        let expected = Some(r"z:\nope\some dir\file.txt".to_string());
        for p in [
            r"Z:\Nope\Some Dir\File.txt",
            r"z:/nope/some dir/file.txt",
            r"\\?\Z:\NOPE\SOME DIR\FILE.TXT",
            r"Z:\nope\.\x\..\some dir\file.txt",
            r"Z:\nope.\some dir \file.txt. ",
            r"Z:\nope\some dir\file.txt\",
            r"Z:\\nope\\some dir\\file.txt",
        ] {
            assert_eq!(canonical_key(Path::new(p)), expected, "{p}");
        }
    }

    #[test]
    fn canonical_key_handles_roots_and_shares() {
        let key = |p: &str| canonical_key(Path::new(p));
        assert_eq!(key(r"Z:\").as_deref(), Some(r"z:\"));
        assert_eq!(key(r"\\?\Z:\").as_deref(), Some(r"z:\"));
        assert_eq!(key(r"Z:\a\..").as_deref(), Some(r"z:\"));
        assert_eq!(key(r"\\Srv\Share\A").as_deref(), Some(r"\\srv\share\a"));
        assert_eq!(
            key(r"\\?\UNC\Srv\Share\A").as_deref(),
            Some(r"\\srv\share\a")
        );
        assert_eq!(key(r"\\?\unc\srv\share\").as_deref(), Some(r"\\srv\share"));
        assert_eq!(key(r"\\srv\share\..\..").as_deref(), Some(r"\\srv\share"));
    }

    #[test]
    fn canonical_key_rejects_relative_and_stream_paths() {
        for p in [
            "a",
            r"a\b",
            r".\a",
            r"..\a",
            "",
            r"C:",
            r"C:a",
            r"\a",
            r"Z:\a:s",
            r"Z:\a:s:$DATA",
            r"Z:\a:\b",
        ] {
            assert_eq!(canonical_key(Path::new(p)), None, "{p}");
        }
    }

    #[test]
    fn canonical_for_delete_is_lexical_when_nothing_exists() {
        let cases = [
            (r"Z:\nope\x\..\file.txt", r"Z:\nope\file.txt"),
            ("Z:/nope/file.txt", r"Z:\nope\file.txt"),
            (r"\\?\Z:\nope\file.txt", r"Z:\nope\file.txt"),
            (r"Z:\", r"Z:\"),
            (r"Z:\a\..", r"Z:\"),
        ];
        for (p, want) in cases {
            assert_eq!(
                canonical_for_delete(Path::new(p)),
                Some(PathBuf::from(want)),
                "{p}"
            );
        }
    }

    #[test]
    fn canonical_for_delete_rejects_relative_and_stream_paths() {
        for p in ["a", r"C:a", r"\a", r"Z:\a:s", r"\\.\C:\a"] {
            assert_eq!(canonical_for_delete(Path::new(p)), None, "{p}");
        }
    }

    #[test]
    fn canonical_for_delete_keeps_a_literal_last_component() {
        let tmp = tempdir().unwrap();
        let got = canonical_for_delete(&tmp.path().join("name. ")).unwrap();
        assert_eq!(got.file_name().unwrap(), "name. ");
    }

    #[test]
    fn junction_into_protected_is_resolved_but_junction_itself_is_not() {
        let (_tmp, root) = guarded_with_link();
        let c = ctx_owning(&root.join("guarded"));
        assert!(is_protected(
            &canonical_for_delete(&root.join(r"link\file.txt")).unwrap(),
            &c
        ));
        assert!(!is_protected(
            &canonical_for_delete(&root.join("link")).unwrap(),
            &c
        ));
    }

    #[test]
    fn canonical_for_delete_names_the_junction_not_its_target() {
        let (_tmp, root) = guarded_with_link();
        let got = canonical_for_delete(&root.join("link")).unwrap();
        assert_eq!(got.file_name().unwrap(), "link");
        let through = canonical_for_delete(&root.join(r"link\file.txt")).unwrap();
        assert!(
            through.parent().unwrap().ends_with("guarded"),
            "{through:?}"
        );
        assert!(
            !through.to_str().unwrap().starts_with(r"\\?\"),
            "{through:?}"
        );
    }

    #[test]
    fn missing_paths_behind_a_junction_are_still_resolved() {
        let (_tmp, root) = guarded_with_link();
        let c = ctx_owning(&root.join("guarded"));
        for rest in [r"link\gone.txt", r"link\gone\deeper\file.txt"] {
            let got = canonical_for_delete(&root.join(rest)).unwrap();
            assert!(is_protected(&got, &c), "{rest}: {got:?}");
        }
    }

    #[test]
    fn is_protected_resolves_junctions_without_help_from_the_caller() {
        let (_tmp, root) = guarded_with_link();
        let c = ctx_owning(&root.join("guarded"));
        assert!(is_protected(&root.join(r"link\file.txt"), &c));
        assert!(!is_protected(&root.join("link"), &c));
    }

    #[test]
    fn trailing_dot_on_a_junction_does_not_hide_its_target() {
        let (_tmp, root) = guarded_with_link();
        let c = ctx_owning(&root.join("guarded"));
        let disguised = root.join(r"link.\file.txt");
        assert!(is_protected(&disguised, &c));
        assert!(is_protected(&canonical_for_delete(&disguised).unwrap(), &c));
    }

    #[test]
    fn own_dir_reached_through_a_junction_protects_the_real_folder() {
        let (_tmp, root) = guarded_with_link();
        let c = ctx_owning(&root.join("link"));
        assert!(is_protected(&root.join(r"guarded\file.txt"), &c));
        assert!(is_protected(&root.join("guarded"), &c));
        assert!(is_protected(
            &canonical_for_delete(&root.join(r"link\file.txt")).unwrap(),
            &c
        ));
    }

    #[test]
    fn exact_rules_also_apply_to_a_profile_behind_a_junction() {
        let tmp = tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(r"real\Documents")).unwrap();
        junction(&root.join("alias"), &root.join("real"));
        let c = ProtectContext {
            profile: root.join("alias"),
            ..ctx()
        };
        assert!(is_protected(&root.join("real"), &c));
        assert!(is_protected(
            &canonical_for_delete(&root.join(r"alias\Documents")).unwrap(),
            &c
        ));
        assert!(!is_protected(&root.join(r"real\Documents\a.txt"), &c));
    }

    #[test]
    fn dropping_the_temp_dir_does_not_delete_through_a_junction() {
        let outside = tempdir().unwrap();
        let sentinel = outside.path().join("keep.txt");
        fs::write(&sentinel, "keep").unwrap();
        let tmp = tempdir().unwrap();
        let link = tmp.path().join("link");
        junction(&link, outside.path());
        assert!(
            link.join("keep.txt").exists(),
            "junction must work for this test to mean anything"
        );
        drop(tmp);
        assert!(!link.exists());
        assert!(sentinel.exists());
    }

    #[test]
    fn short_name_is_expanded() {
        let tmp = tempdir().unwrap();
        let long = tmp.path().join("Long Folder Name For Short Test");
        fs::create_dir(&long).unwrap();
        fs::write(long.join("file.txt"), "x").unwrap();
        let short = short_path(&long);
        // A name with spaces and over eight characters always gets an alias
        // unless the volume does not generate 8.3 names at all.
        if short.file_name() == long.file_name() {
            eprintln!("skipped: this volume has no 8.3 names");
            return;
        }

        assert_eq!(canonical_key(&short), canonical_key(&long));
        assert_eq!(
            canonical_key(&short.join("file.txt")),
            canonical_key(&long.join("file.txt"))
        );
        assert_eq!(
            canonical_key(&short.join(r"gone\deeper.txt")),
            canonical_key(&long.join(r"gone\deeper.txt")),
            "the existing part of a partly missing path must still expand"
        );

        let own_long = ctx_owning(&long);
        assert!(is_protected(&short.join("file.txt"), &own_long));
        assert!(is_protected(&short.join("gone.txt"), &own_long));

        let own_short = ctx_owning(&short);
        assert!(is_protected(&long.join("file.txt"), &own_short));
        assert!(is_protected(
            &canonical_for_delete(&long.join("file.txt")).unwrap(),
            &own_short
        ));
    }

    #[test]
    fn paths_longer_than_the_first_win32_buffer_still_resolve() {
        let tmp = tempdir().unwrap();
        let top = tmp.path().join("top");
        let segment = "d".repeat(200);
        let mut deep = top.clone();
        for _ in 0..7 {
            deep.push(&segment);
        }
        fs::create_dir_all(&deep).unwrap();

        let key = canonical_key(&deep).unwrap();
        assert!(key.len() > 1400, "{}", key.len());
        assert!(key.ends_with(&segment));
        let file = deep.join("file.txt");
        assert!(canonical_for_delete(&file).unwrap().ends_with("file.txt"));
        assert!(is_protected(&file, &ctx_owning(&top)));
        assert!(!is_protected(&file, &ctx()));
    }

    #[test]
    fn from_env_protects_the_exe_and_the_chain_to_it_but_not_siblings() {
        let c = ProtectContext::from_env();
        let exe = std::env::current_exe().unwrap();
        assert!(c.own_dirs.contains(&exe));
        assert!(!c.own_dirs.iter().any(|d| Some(d.as_path()) == exe.parent()));
        assert!(c.own_dirs.iter().any(|d| d.ends_with("Katharistis")));
        assert!(is_protected(&exe, &c));
        assert!(!is_protected(&exe.with_file_name("not-the-exe.txt"), &c));
        assert!(is_protected(exe.parent().unwrap(), &c));
        assert!(!is_protected(
            &exe.parent().unwrap().with_file_name("not-the-exe-folder"),
            &c
        ));
    }

    #[test]
    fn from_env_covers_the_real_system_locations() {
        let c = ProtectContext::from_env();
        assert!(c.windows_dir.is_absolute());
        assert!(c.program_data.is_absolute());
        assert!(c.users_dir.is_absolute());
        assert!(c.profile.is_absolute());
        assert!(!c.program_files.is_empty());
        assert!(is_protected(&c.windows_dir.join("System32"), &c));
        assert!(is_protected(&c.program_files[0].join("App"), &c));
        assert!(is_protected(&c.profile.join("Documents"), &c));
        assert!(is_protected(&c.users_dir, &c));
    }
}
