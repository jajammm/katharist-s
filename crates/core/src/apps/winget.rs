//! winget integration: parse its localized table output, run it, and merge
//! its packages with the registry app list.
//!
//! Nothing here accepts a source agreement on its own. `list(false)` and
//! `upgrades(false)` never pass `--accept-source-agreements`; the caller shows
//! the text of `NeedsAgreement` and only then calls the `true` variant.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use unicode_width::UnicodeWidthChar;

use super::AppEntry;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WingetPackage {
    pub name: String,
    pub id: String,
    pub version: String,
    pub available: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WingetOutcome {
    Ok(Vec<WingetPackage>),
    NeedsAgreement(String),
    NotInstalled,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppRow {
    Registry(AppEntry, Option<WingetPackage>),
    WingetOnly(WingetPackage),
}

/// Drops carriage-return spinner frames: keeps the last non-blank segment.
fn clean_line(line: &str) -> &str {
    line.rsplit('\r')
        .find(|s| !s.trim().is_empty())
        .unwrap_or("")
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3 && t.bytes().all(|b| b == b'-')
}

/// Display-column starts of each header cell. Winget pads columns with at
/// least two spaces, so normally a single space stays inside a multi-word
/// header. Some layouts are tighter than that: when the two-space split finds
/// fewer than 4 cells but the header has 3 to 5 words, every word is a cell
/// (winget has at most 5 columns).
fn column_starts(header: &str) -> Vec<usize> {
    let mut by_gap = Vec::new();
    let mut by_word = Vec::new();
    let mut col = 0;
    let mut spaces = 2;
    let mut prev_space = true;
    for ch in header.chars() {
        if ch == ' ' {
            spaces += 1;
            prev_space = true;
        } else {
            if spaces >= 2 {
                by_gap.push(col);
            }
            if prev_space {
                by_word.push(col);
            }
            spaces = 0;
            prev_space = false;
        }
        col += ch.width().unwrap_or(0);
    }
    if by_gap.len() < 4 && (3..=5).contains(&by_word.len()) {
        by_word
    } else {
        by_gap
    }
}

/// Text whose first display column lies in `[start, end)`, trimmed.
fn cell(line: &str, start: usize, end: Option<usize>) -> String {
    let mut col = 0;
    let mut out = String::new();
    for ch in line.chars() {
        if col >= start && end.is_none_or(|e| col < e) {
            out.push(ch);
        }
        col += ch.width().unwrap_or(0);
    }
    out.trim().to_string()
}

/// Parses the first table in winget's output. Column names are ignored (they
/// are localized): the `---` rule marks the table, the line above it gives the
/// column positions. Columns are name, id, version, then available (only when
/// there are five) and source. Reading stops at the first blank line after the
/// rows begin, so footers and any second table are skipped. Rows need a
/// non-empty id (ARP keys may contain spaces).
pub fn parse_table(output: &str) -> Vec<WingetPackage> {
    let lines: Vec<&str> = output.lines().map(clean_line).collect();
    for i in 1..lines.len() {
        if !is_separator(lines[i]) || lines[i - 1].trim().is_empty() {
            continue;
        }
        let starts = column_starts(lines[i - 1]);
        if starts.len() < 3 {
            continue;
        }
        let end_of = |n: usize| starts.get(n + 1).copied();
        let mut out = Vec::new();
        for line in &lines[i + 1..] {
            if line.trim().is_empty() {
                break;
            }
            let id = cell(line, starts[1], end_of(1));
            if id.is_empty() {
                continue;
            }
            let available = if starts.len() >= 5 {
                Some(cell(line, starts[3], end_of(3))).filter(|s| !s.is_empty())
            } else {
                None
            };
            out.push(WingetPackage {
                name: cell(line, starts[0], end_of(0)),
                id,
                version: cell(line, starts[2], end_of(2)),
                available,
            });
        }
        return out;
    }
    Vec::new()
}

/// Windows components that are not user apps. A prefix counts only when it
/// ends the id or is followed by `.` or `_`, so `Microsoft.WindowsTerminal`
/// is not caught by `Microsoft.Windows`.
pub fn is_framework(id: &str) -> bool {
    const PREFIXES: [&str; 6] = [
        "Microsoft.VCLibs",
        "Microsoft.NET",
        "Microsoft.UI.Xaml",
        "Microsoft.WindowsStore",
        "Microsoft.Windows",
        "Microsoft.DesktopAppInstaller",
    ];
    let id = id.strip_prefix(r"MSIX\").unwrap_or(id);
    PREFIXES.iter().any(|p| {
        id.get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
            && matches!(id[p.len()..].chars().next(), None | Some('.') | Some('_'))
    })
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn arp_key(id: &str) -> Option<&str> {
    id.strip_prefix(r"ARP\")?.rsplit('\\').next()
}

/// Matches registry entries to winget packages: first by `ARP\...\<key>`, then
/// by normalized name, then by a `…`-truncated name used as a prefix (at
/// least 4 characters). The two name passes accept a pair only when each side
/// has exactly one candidate, so ambiguous entries stay unmatched. A package is used once. Unmatched packages that are
/// not frameworks come last as `WingetOnly`.
pub fn merge(entries: Vec<AppEntry>, packages: Vec<WingetPackage>) -> Vec<AppRow> {
    let mut used = vec![false; packages.len()];
    let mut matched: Vec<Option<usize>> = vec![None; entries.len()];

    // Pass 1: the ARP key is unique per entry, so the first hit is enough.
    for (ei, e) in entries.iter().enumerate() {
        let hit = (0..packages.len()).find(|&pi| {
            !used[pi] && arp_key(&packages[pi].id).is_some_and(|k| k.eq_ignore_ascii_case(&e.key))
        });
        if let Some(pi) = hit {
            used[pi] = true;
            matched[ei] = Some(pi);
        }
    }

    // Passes 2 and 3 are fuzzy: a pair is accepted only when it is the sole
    // candidate in both directions, so list order never decides a match.
    let mut unique_pass = |f: &dyn Fn(&AppEntry, &WingetPackage) -> bool| {
        let mut pairs = Vec::new();
        for (ei, e) in entries.iter().enumerate() {
            if matched[ei].is_some() {
                continue;
            }
            let mut hits = (0..packages.len()).filter(|&pi| !used[pi] && f(e, &packages[pi]));
            let (Some(pi), None) = (hits.next(), hits.next()) else {
                continue;
            };
            let rivals = entries
                .iter()
                .enumerate()
                .filter(|(oi, o)| matched[*oi].is_none() && f(o, &packages[pi]))
                .count();
            if rivals == 1 {
                pairs.push((ei, pi));
            }
        }
        for (ei, pi) in pairs {
            used[pi] = true;
            matched[ei] = Some(pi);
        }
    };
    unique_pass(&|e, p| {
        let n = normalize(&e.name);
        !n.is_empty() && n == normalize(&p.name)
    });
    unique_pass(&|e, p| {
        p.name.strip_suffix('…').is_some_and(|stem| {
            let stem = normalize(stem);
            stem.chars().count() >= 4 && normalize(&e.name).starts_with(&stem)
        })
    });

    let mut rows: Vec<AppRow> = entries
        .into_iter()
        .zip(matched)
        .map(|(e, m)| AppRow::Registry(e, m.map(|pi| packages[pi].clone())))
        .collect();
    rows.extend(
        packages
            .into_iter()
            .zip(used)
            .filter(|(p, used)| !used && !is_framework(&p.id))
            .map(|(p, _)| AppRow::WingetOnly(p)),
    );
    rows
}

/// `APPINSTALLER_CLI_ERROR_SOURCE_AGREEMENTS_NOT_ACCEPTED` is 0x8A150046
/// (-1978335162 as i32), from winget-cli's doc/windows/package-manager/winget/returnCodes.md
/// (the source of Microsoft's published winget return codes).
const SOURCE_AGREEMENTS_NOT_ACCEPTED: i32 = 0x8A15_0046_u32 as i32;

fn mentions_agreement(text: &str) -> bool {
    let t = text.to_lowercase();
    [
        "--accept-source-agreements",
        "agreement",
        "perjanjian",
        "persetujuan",
    ]
    .iter()
    .any(|k| t.contains(k))
}

fn clean_text(text: &str) -> String {
    text.lines()
        .map(clean_line)
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.chars().all(|c| matches!(c, '-' | '\\' | '|' | '/')))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Maps a finished `winget list/upgrade` run to an outcome without running anything.
fn classify(exit: Option<i32>, stdout: &str, stderr: &str) -> WingetOutcome {
    let rows = parse_table(stdout);
    if exit == Some(0) || !rows.is_empty() {
        return WingetOutcome::Ok(rows);
    }
    let out = clean_text(stdout);
    let err = clean_text(stderr);
    let combined = [out.as_str(), err.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if exit == Some(SOURCE_AGREEMENTS_NOT_ACCEPTED) || mentions_agreement(&combined) {
        return WingetOutcome::NeedsAgreement(combined);
    }
    if !err.is_empty() {
        WingetOutcome::Failed(err)
    } else if !out.is_empty() {
        WingetOutcome::Failed(out)
    } else {
        match exit {
            Some(c) => WingetOutcome::Failed(format!("winget exited with code {c}")),
            None => WingetOutcome::Failed("winget ended without an exit code".into()),
        }
    }
}

fn outcome_from_spawn_error(e: &io::Error) -> WingetOutcome {
    if e.kind() == io::ErrorKind::NotFound {
        WingetOutcome::NotInstalled
    } else {
        WingetOutcome::Failed(e.to_string())
    }
}

/// Read-only `winget <sub>`. Stdin is empty and no window is shown.
fn run_table(sub: &str, accept_agreements: bool) -> WingetOutcome {
    let mut cmd = Command::new("winget");
    cmd.args([sub, "--disable-interactivity"]);
    if accept_agreements {
        cmd.arg("--accept-source-agreements");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);
    match cmd.output() {
        Ok(o) => classify(
            o.status.code(),
            &String::from_utf8_lossy(&o.stdout),
            &String::from_utf8_lossy(&o.stderr),
        ),
        Err(e) => outcome_from_spawn_error(&e),
    }
}

pub fn list(accept_agreements: bool) -> WingetOutcome {
    run_table("list", accept_agreements)
}

pub fn upgrades(accept_agreements: bool) -> WingetOutcome {
    run_table("upgrade", accept_agreements)
}

/// Runs winget (uninstall or upgrade) in its own visible console window and
/// returns its exit code.
pub fn run_visible(args: &[&str]) -> io::Result<i32> {
    let status = Command::new("winget")
        .args(args)
        .creation_flags(CREATE_NEW_CONSOLE)
        .status()?;
    Ok(status.code().unwrap_or(-1))
}

fn exists_in_any(dirs: impl IntoIterator<Item = PathBuf>) -> bool {
    dirs.into_iter().any(|d| has_winget(&d))
}

/// `symlink_metadata`, because `WindowsApps\winget.exe` is a reparse-point
/// alias that `Path::exists` can report as missing.
fn has_winget(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join("winget.exe")).is_ok()
}

/// Whether a `winget.exe` file is on PATH or in the WindowsApps alias folder.
/// Only looks at files; never runs winget.
pub fn is_available() -> bool {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(r"Microsoft\WindowsApps"));
    }
    exists_in_any(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppEntry;

    fn pkg(name: &str, id: &str) -> WingetPackage {
        WingetPackage {
            name: name.into(),
            id: id.into(),
            version: "1.0".into(),
            available: None,
        }
    }

    fn entry(key: &str, name: &str) -> AppEntry {
        AppEntry {
            key: key.into(),
            name: name.into(),
            publisher: None,
            version: None,
            install_date: None,
            size: 0,
            install_location: None,
            uninstall: None,
        }
    }

    #[test]
    fn parses_recorded_output() {
        let p = parse_table(include_str!("../../tests/fixtures/winget_list.txt"));
        assert!(p.len() > 3);
        assert_eq!(p.len(), 13);
    }

    #[test]
    fn parses_localized_headers_and_truncated_names() {
        let s = "\r- \rNama    Id       Versi Tersedia Sumber\n-----------------------------------\nVisual…  Foo.Bar  1.0   1.1      winget\n";
        let p = &parse_table(s)[0];
        assert_eq!(p.name, "Visual…");
        assert_eq!(p.id, "Foo.Bar");
        assert_eq!(p.version, "1.0");
        assert_eq!(p.available.as_deref(), Some("1.1"));
    }

    #[test]
    fn recorded_list_has_arp_msix_truncated_and_optional_available() {
        let p = parse_table(include_str!("../../tests/fixtures/winget_list.txt"));
        let ff = p.iter().find(|x| x.id == "Mozilla.Firefox").unwrap();
        assert_eq!(ff.available.as_deref(), Some("127.0"));
        let z = p.iter().find(|x| x.id == "7zip.7zip").unwrap();
        assert_eq!(z.available, None);
        assert_eq!(z.version, "23.01");
        let arp = p.iter().find(|x| x.id.starts_with(r"ARP\Machine")).unwrap();
        assert!(arp.name.ends_with('…'));
        assert_eq!(arp.version, "3.12.3150.0");
        assert!(p.iter().any(|x| x.id.starts_with(r"MSIX\")));
    }

    #[test]
    fn wide_characters_do_not_shift_columns() {
        let p = parse_table(include_str!("../../tests/fixtures/winget_upgrade.txt"));
        let cn = p.iter().find(|x| x.id == "Example.TextEditorCN").unwrap();
        assert_eq!(cn.name, "示例文本编辑器");
        assert_eq!(cn.version, "2.0.1");
        assert_eq!(cn.available.as_deref(), Some("2.0.2"));
    }

    #[test]
    fn upgrade_fixture_takes_only_first_table() {
        let p = parse_table(include_str!("../../tests/fixtures/winget_upgrade.txt"));
        assert_eq!(p.len(), 4);
        assert!(p.iter().all(|x| x.available.is_some()));
        assert!(p.iter().all(|x| x.id != "Example.PinnedTool"));
    }

    #[test]
    fn four_column_table_has_no_available() {
        let p = parse_table(include_str!(
            "../../tests/fixtures/winget_list_no_available.txt"
        ));
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].id, "VideoLAN.VLC");
        assert_eq!(p[0].version, "3.0.20");
        assert_eq!(p[0].available, None);
    }

    #[test]
    fn empty_or_garbage_output_yields_no_packages() {
        for s in [
            "",
            "\n\n",
            "\r",
            "-----",
            "---\n---\n---",
            "Name Id\n---",
            "No installed package found matching input criteria.",
            "日本語だけ\n----\n日本語",
            "A  B  C\n------\nx",
            "…\n…\n…",
        ] {
            assert!(parse_table(s).is_empty(), "input {s:?}");
        }
    }

    #[test]
    fn short_and_odd_lines_do_not_panic() {
        let s = "Name   Id     Version\n----------------\nab\n\u{0}\n  \nx  y  z\n";
        assert!(parse_table(s).iter().all(|p| !p.id.is_empty()));
        let s = "Nama  Id  Versi\n------\n示例  Foo  1\n示 例 Bar\n";
        assert!(parse_table(s).iter().all(|p| !p.id.is_empty()));
    }

    #[test]
    fn framework_filter_handles_msix_prefix() {
        assert!(is_framework(r"MSIX\Microsoft.VCLibs.x64_1"));
        assert!(!is_framework("Mozilla.Firefox"));
    }

    #[test]
    fn framework_filter_covers_listed_prefixes() {
        for id in [
            "Microsoft.VCLibs.140.00",
            r"MSIX\Microsoft.UI.Xaml.2.8_8.2310.30001.0_x64__8wekyb3d8bbwe",
            "Microsoft.NET.Runtime.8",
            r"MSIX\Microsoft.NET.Native.Framework.2.2_2.2.0.0_x64__8wekyb3d8bbwe",
            "Microsoft.WindowsStore",
            r"MSIX\Microsoft.WindowsStore_22.0_x64__8wekyb3d8bbwe",
            "Microsoft.Windows.Photos",
            "Microsoft.DesktopAppInstaller",
            "microsoft.vclibs.140.00",
        ] {
            assert!(is_framework(id), "{id}");
        }
        for id in [
            "Microsoft.WindowsTerminal",
            "Microsoft.PowerToys",
            "Microsoft.VisualStudioCode",
            "",
        ] {
            assert!(!is_framework(id), "{id}");
        }
    }

    #[test]
    fn merge_by_arp_key_then_name() {
        let entries = vec![
            entry("{AAAA-1}", "Totally Different Registry Name"),
            entry("Plain App", "Plain App"),
            entry("k3", "Visual Studio Community 2022"),
            entry("k4", "Unrelated"),
        ];
        let packages = vec![
            pkg("Something Else", r"ARP\Machine\X64\{AAAA-1}"),
            pkg("plain  app!", "Vendor.PlainApp"),
            pkg(
                "Visual Studio Commun…",
                "Microsoft.VisualStudio.2022.Community",
            ),
            pkg("Brand New", "Vendor.BrandNew"),
            pkg(
                "VCLibs",
                r"MSIX\Microsoft.VCLibs.140.00_14.0_x64__8wekyb3d8bbwe",
            ),
        ];
        let rows = merge(entries, packages);
        assert_eq!(rows.len(), 5);
        match &rows[0] {
            AppRow::Registry(e, Some(p)) => {
                assert_eq!(e.key, "{AAAA-1}");
                assert_eq!(p.id, r"ARP\Machine\X64\{AAAA-1}");
            }
            r => panic!("{r:?}"),
        }
        match &rows[1] {
            AppRow::Registry(_, Some(p)) => assert_eq!(p.id, "Vendor.PlainApp"),
            r => panic!("{r:?}"),
        }
        match &rows[2] {
            AppRow::Registry(_, Some(p)) => assert!(p.name.ends_with('…')),
            r => panic!("{r:?}"),
        }
        assert!(matches!(&rows[3], AppRow::Registry(e, None) if e.key == "k4"));
        assert!(matches!(&rows[4], AppRow::WingetOnly(p) if p.id == "Vendor.BrandNew"));
    }

    #[test]
    fn ambiguous_name_matches_stay_unmatched() {
        let entries = vec![
            entry("a", "Foo"),
            entry("b", "Foo"),
            entry("c", "Foobar Studio"),
        ];
        let packages = vec![pkg("Fo…", "Vendor.Fo"), pkg("Foo", "Vendor.Foo")];
        let rows = merge(entries, packages);
        assert_eq!(rows.len(), 5);
        assert!(rows[..3]
            .iter()
            .all(|r| matches!(r, AppRow::Registry(_, None))));
        assert!(matches!(&rows[3], AppRow::WingetOnly(p) if p.id == "Vendor.Fo"));
        assert!(matches!(&rows[4], AppRow::WingetOnly(p) if p.id == "Vendor.Foo"));
    }

    #[test]
    fn duplicate_full_names_do_not_swap_packages() {
        let name = "Microsoft Visual C++ 2015-2022 Redistributable";
        let entries = vec![entry("x64key", name), entry("x86key", name)];
        let packages = vec![
            pkg(name, "Microsoft.VCRedist.2015+.x64"),
            pkg(name, "Microsoft.VCRedist.2015+.x86"),
        ];
        let rows = merge(entries.clone(), packages.clone());
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0], AppRow::Registry(_, None)));
        assert!(matches!(&rows[1], AppRow::Registry(_, None)));
        assert!(matches!(&rows[2], AppRow::WingetOnly(_)));
        assert!(matches!(&rows[3], AppRow::WingetOnly(_)));
        // Reversed package order gives the same result.
        let mut rev = packages;
        rev.reverse();
        let rows = merge(entries, rev);
        assert!(rows[..2]
            .iter()
            .all(|r| matches!(r, AppRow::Registry(_, None))));
    }

    #[test]
    fn duplicate_full_names_still_match_by_arp_key() {
        let name = "Redistributable Tool";
        let entries = vec![entry("k64", name), entry("k86", name)];
        let packages = vec![
            pkg(name, r"ARP\Machine\X86\k86"),
            pkg(name, r"ARP\Machine\X64\k64"),
        ];
        let rows = merge(entries, packages);
        assert!(matches!(&rows[0], AppRow::Registry(_, Some(p)) if p.id.ends_with("k64")));
        assert!(matches!(&rows[1], AppRow::Registry(_, Some(p)) if p.id.ends_with("k86")));
    }

    #[test]
    fn duplicate_truncated_names_stay_unmatched() {
        let entries = vec![entry("a", "Visual Studio Community 2022")];
        let packages = vec![
            pkg(
                "Visual Studio Commun…",
                "Microsoft.VisualStudio.2022.Community",
            ),
            pkg(
                "Visual Studio Commun…",
                "Microsoft.VisualStudio.2019.Community",
            ),
        ];
        let rows = merge(entries, packages);
        assert_eq!(rows.len(), 3);
        assert!(matches!(&rows[0], AppRow::Registry(_, None)));

        // One truncated package, two entries sharing the prefix.
        let entries = vec![
            entry("a", "Visual Studio Community 2022"),
            entry("b", "Visual Studio Community 2019"),
        ];
        let packages = vec![pkg(
            "Visual Studio Commun…",
            "Microsoft.VisualStudio.2022.Community",
        )];
        let rows = merge(entries, packages);
        assert!(rows[..2]
            .iter()
            .all(|r| matches!(r, AppRow::Registry(_, None))));
        assert!(matches!(&rows[2], AppRow::WingetOnly(_)));
    }

    #[test]
    fn short_prefix_is_ignored() {
        let rows = merge(vec![entry("a", "Foobar")], vec![pkg("Fo…", "Vendor.Fo")]);
        assert!(matches!(&rows[0], AppRow::Registry(_, None)));
        assert!(matches!(&rows[1], AppRow::WingetOnly(_)));
    }

    #[test]
    fn merge_with_empty_inputs() {
        assert!(merge(vec![], vec![]).is_empty());
        assert_eq!(merge(vec![entry("a", "A")], vec![]).len(), 1);
    }

    #[test]
    fn outcome_ok_parses_table() {
        let out = include_str!("../../tests/fixtures/winget_upgrade.txt");
        match classify(Some(0), out, "") {
            WingetOutcome::Ok(p) => assert_eq!(p.len(), 4),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn outcome_ok_for_empty_success() {
        let o = classify(Some(0), "No installed package found.\r\n", "");
        assert!(matches!(o, WingetOutcome::Ok(p) if p.is_empty()));
    }

    #[test]
    fn outcome_needs_agreement_carries_text() {
        let text = "The source requires that you view the following agreements before using.\nTo proceed, pass --accept-source-agreements.\n";
        match classify(Some(1), text, "") {
            WingetOutcome::NeedsAgreement(t) => assert!(t.contains("agreements")),
            o => panic!("{o:?}"),
        }
        let id = "Sumber memerlukan persetujuan. Gunakan --accept-source-agreements";
        assert!(matches!(
            classify(Some(1), "", id),
            WingetOutcome::NeedsAgreement(_)
        ));
    }

    #[test]
    fn outcome_needs_agreement_by_exit_code_alone() {
        assert!(matches!(
            classify(Some(0x8A15_0046_u32 as i32), "", ""),
            WingetOutcome::NeedsAgreement(_)
        ));
        assert_eq!(0x8A15_0046_u32 as i32, -1978335162);
        assert!(matches!(
            classify(Some(-1978335163), "", ""),
            WingetOutcome::Failed(_)
        ));
    }

    #[test]
    fn outcome_failed_has_message() {
        match classify(Some(1), "", "boom\n") {
            WingetOutcome::Failed(m) => assert_eq!(m, "boom"),
            o => panic!("{o:?}"),
        }
        match classify(Some(5), "", "") {
            WingetOutcome::Failed(m) => assert!(m.contains('5')),
            o => panic!("{o:?}"),
        }
        assert!(matches!(classify(None, "", ""), WingetOutcome::Failed(_)));
    }

    #[test]
    fn outcome_nonzero_with_rows_is_still_ok() {
        let out = include_str!("../../tests/fixtures/winget_list.txt");
        assert!(matches!(classify(Some(1), out, ""), WingetOutcome::Ok(p) if p.len() == 13));
    }

    #[test]
    fn spawn_error_mapping() {
        use std::io::{Error, ErrorKind};
        assert!(matches!(
            outcome_from_spawn_error(&Error::from(ErrorKind::NotFound)),
            WingetOutcome::NotInstalled
        ));
        assert!(matches!(
            outcome_from_spawn_error(&Error::from(ErrorKind::PermissionDenied)),
            WingetOutcome::Failed(_)
        ));
    }

    #[test]
    fn availability_lookup_finds_file_without_running_it() {
        let d = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("winget.exe"), b"not an exe").unwrap();
        assert!(exists_in_any([
            other.path().to_path_buf(),
            d.path().to_path_buf()
        ]));
        assert!(!exists_in_any([other.path().to_path_buf()]));
        assert!(!exists_in_any(Vec::<std::path::PathBuf>::new()));
        assert!(!exists_in_any([d.path().join("missing")]));
    }

    #[test]
    fn arp_ids_may_contain_spaces() {
        let s = r"Name      Id                           Version  Source
------------------------------------------------------
Some App  ARP\Machine\X64\Some App     1.0
";
        let p = parse_table(s);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].id, r"ARP\Machine\X64\Some App");
        assert_eq!(p[0].version, "1.0");
    }
}
