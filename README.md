# Katharistís

A disk cleaner for Windows, written in Rust. One scan shows what is taking up space on a disk map, so you don't have to open folders one by one. You decide what gets deleted: personal files are never ticked automatically.

There are two programs, both portable (a single `.exe`, no installer, no admin rights), for Windows 10 and later:

- `katharistis-gui.exe`: the graphical app, in English and Indonesian.
- `katharistis.exe`: a disk usage report in the terminal. It only reads and never deletes.

## Status

Version 0.1.0, with no downloadable release yet. The automated tests pass, but the graphical app's flow with real data is still waiting on manual testing, so try it on a test folder before deleting anything important.

## Building

```
cargo build --release --workspace
cargo test --workspace
```

This needs stable Rust with the `x86_64-pc-windows-msvc` target and `rc.exe` from the Windows SDK. The build output is in `target\release\`.

The normal tests don't touch the Recycle Bin. Tests that use the real Recycle Bin are marked `#[ignore]` and put `katharistis-test-*` files in it, so run them only when needed:

```
cargo test -p katharistis-core -- --ignored --test-threads=1
```

## Features

### Home

Pick a drive or type a folder, then press **Scan**. The disk map shows the contents of the location as boxes. Click a box to go deeper, and use the breadcrumb to go back. Box colors tell kinds of area apart, and there are no danger scores.

The panel beside it lists the current location from largest to smallest, with the last modified date. Items that are old get a color mark (the default threshold is 1 year). Ticked items move to the Recycle Bin, and permanent deletion has to be chosen separately.

On a whole-drive scan, the "Other" panel explains the gap between the scanned total and the space in use: `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`, System Volume Information, and unreadable folders with the reason for each.

### Cleanup

Items on this screen are deleted permanently because their apps can recreate them, and the confirmation says so. Only locations under your own user account are cleaned. `C:\Windows\Temp` and the Windows Update cache are left out because they need admin rights.

| Category | Location | Ticked at start |
|---|---|---|
| Temporary files | Contents of `%TEMP%` | Yes, except items changed in the last 24 hours |
| Browser cache | `Cache` and `Code Cache` per Chrome and Edge profile, `cache2` per Firefox profile. Profiles, passwords, and bookmarks are not touched | Yes |
| Crash dumps and logs | `%LOCALAPPDATA%\CrashDumps`, `%LOCALAPPDATA%\Microsoft\Windows\WER` | Yes |
| Developer data | npm cache, pip cache, Gradle caches, NuGet `v3-cache` | Yes |
| General cache | `%USERPROFILE%\.cache` | No |
| Incomplete downloads | `.crdownload`, `.part`, `.partial` in `Downloads` | Yes, except items changed in the last 24 hours |
| Recycle Bin | Emptied through the Windows API | No |

Files in use are skipped and reported. Folders excluded in Settings are not offered.

### Apps

The list of installed apps comes from the registry, with columns for name, publisher, size, install date, version, and update. Apps outside `Program Files` get a mark but are not blocked.

Uninstalling runs the app's official uninstaller, so you can see the process. Once the app is no longer registered, its leftover folders in `%APPDATA%`, `%LOCALAPPDATA%`, and `%PROGRAMDATA%` are offered with nothing ticked, because they can hold personal data. Publisher folders such as `%LOCALAPPDATA%\Google` are never offered.

The **Check winget** button adds winget marks, Microsoft Store apps, and the list of updates, which run per app when you ask. If winget asks for source agreement, the text is shown and accepted only after you agree.

### Analysis

- **Largest:** files of 100 MB and up in the scanned location.
- **Not changed in a while:** files unchanged for 6 months, 1 year, or 2 years. The Windows and Program Files areas stay hidden unless you show them.
- **Duplicates:** files of 1 MiB and up are grouped by size, then by SHA-256 hash. One copy in each group always stays and is checked again right before deletion. Hard links do not count as duplicates.

### Settings

Language, theme (light, dark, or follow the system), the "old" threshold, and folders excluded from scans. Stored in `%APPDATA%\Katharistis\settings.toml`.

### CLI

```
katharistis [options] [path]
```

Without a `path`, it scans your user profile folder.

| Option | Meaning | Default |
|---|---|---|
| `-d <DEPTH>` | Depth of the tree shown | 1 |
| `-n <TOP>` | Number of top folders per level (0 = all) | 20 |
| `-f <FILES>` | Number of largest files listed (0 = off) | 15 |
| `-x` | Accepted but changes nothing, because scanning never follows links | |
| `-j <THREADS>` | Number of scan threads | CPU count x 4 |

## Scanning rules

- Sizes are real disk space, read from the directory listing without opening each file, so compressed and sparse files count correctly.
- Hard links count once. Junctions, symlinks, and mount points are not followed.
- OneDrive folders are scanned without triggering downloads. Files still in the cloud get a mark and their contents are never read, including when looking for duplicates.
- A folder's date is the newest date among everything inside it.
- A folder that can't be read is recorded with the reason, and the scan continues.
- Scans can be cancelled, and paths over 260 characters are supported.

A full scan of a 512 GB NVMe drive with about 3.6 million files took about 21 seconds and 1.3 GB of memory on the developer's machine, with the file system cache already warm. The first scan after a restart can take longer.

## Deletion rules

All deletion goes through one function in `katharistis-core`, and there is always a confirmation with the item count, total size, and delete mode. The report separates files moved to the Recycle Bin, which have not given disk space back yet, from space actually gained. A button in that report empties the Recycle Bin, with no extra Windows prompt.

A file or folder that can't enter the Recycle Bin (over the quota, a drive with no Recycle Bin, or a folder containing a link) is never just deleted. Permanent deletion happens only after a separate confirmation.

These can't be deleted from this app:

- Drive roots, `C:\Windows` and its contents, `Program Files` and its contents, `C:\ProgramData` and `C:\Users` as folders, and the parent folders of those locations.
- `System Volume Information`, `$Recycle.Bin`, `pagefile.sys`, `hiberfil.sys`, `swapfile.sys`.
- Boot files at the root of the Windows drive: `bootmgr`, `BOOTNXT`, `Boot`, `Recovery`, `EFI`.
- Each user's profile folder and its standard folders as whole folders (`Documents`, `Desktop`, `Downloads`, `Pictures`, `Music`, `Videos`, `AppData`, `AppData\Local`, `AppData\Roaming`). Their contents can be deleted.
- `.ssh` and `.gnupg` with their contents, and Katharistís's own folder.

Paths are normalized before the check, including `\\?\`, letter case, and 8.3 short names. A link is deleted as a link, and its target is not touched. Items inside a registered app's folder and browser profiles can be deleted, but they carry an "used by an app" mark and a warning in the confirmation.

## Not included

Registry cleaning, "optimizers", PC health scores, scheduled or automatic deletion, automatic updates of this app, and macOS or Linux support.

## Network

Katharistís does not use the network and has no telemetry. The one exception is `winget` on the Apps screen, which runs only after you press its button.

## SmartScreen warning

The binaries are not signed, so Windows SmartScreen may show a warning on first run. Choose "More info" and then "Run anyway" if you trust the source.

## Structure

A Cargo workspace with three crates:

- `crates/core` (`katharistis-core`): scanning, treemap, path rules, deletion, cleanup, apps, and duplicates. It does not depend on the GUI.
- `crates/gui` (`katharistis-gui`): the `egui` interface.
- `crates/cli` (`katharistis-cli`): the terminal program.

The design document is at [`docs/superpowers/specs/2026-11-06-katharistis-design.md`](docs/superpowers/specs/2026-11-06-katharistis-design.md), and the implementation plan is at [`docs/superpowers/plans/2026-11-06-katharistis.md`](docs/superpowers/plans/2026-11-06-katharistis.md). Both are in Indonesian.

## Origin and license

A port of [CleanBro](https://github.com/jetorbitdev/cleanbro), Copyright (C) 2026 gilang. Its behavior and structure follow that project.

Licensed under GPL-3.0. See the `LICENSE` file.
