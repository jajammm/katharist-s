mod render;

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use clap::Parser;
use katharistis_core::format::format_size;
use katharistis_core::scan::{scan, ScanOptions, ScanProgress, ScanTree};
use katharistis_core::win::volume_space;

use render::{render, RenderOpts};

/// Read-only disk usage report. Never deletes anything.
#[derive(Parser)]
#[command(name = "katharistis", version)]
struct Cli {
    /// Folder to scan (default: user profile)
    path: Option<PathBuf>,
    /// Tree depth shown
    #[arg(short = 'd', default_value_t = 1)]
    depth: usize,
    /// Top folders per level (0 = all)
    #[arg(short = 'n', default_value_t = 20)]
    top: usize,
    /// Largest files listed (0 = off)
    #[arg(short = 'f', default_value_t = 15)]
    files: usize,
    /// Accepted for compatibility: links are never followed, so a scan never leaves its volume
    #[arg(short = 'x')]
    _one_volume: bool,
    /// Scan threads (default: CPU count x 4)
    #[arg(short = 'j')]
    threads: Option<usize>,
}

#[cfg(windows)]
fn use_utf8_console() {
    // SAFETY: plain Win32 call with a constant code page.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }
}

#[cfg(not(windows))]
fn use_utf8_console() {}

/// How often the counter changes. A terminal has nothing to gain from more.
const COUNTER_EVERY: Duration = Duration::from_millis(200);

/// The counter's line, to be written over itself.
fn counter_line(files: u64, bytes: u64) -> String {
    format!("Scanning... {files} files, {}", format_size(bytes))
}

/// Writes `line` over one of `previous` characters, and returns its length.
fn write_counter(out: &mut impl Write, line: &str, previous: usize) -> usize {
    let len = line.chars().count();
    let tail = previous.saturating_sub(len);
    let _ = write!(out, "\r{line}{:tail$}", "");
    let _ = out.flush();
    len
}

/// Leaves the terminal line as it was, so the results start clean.
fn clear_counter(out: &mut impl Write, previous: usize) {
    let _ = write!(out, "\r{:previous$}\r", "");
    let _ = out.flush();
}

/// Scans with a live counter on stderr. The counter is gone before this returns, and
/// stdout never sees any of it.
fn scan_with_counter(
    root: &Path,
    opts: &ScanOptions,
    progress: &ScanProgress,
) -> std::io::Result<ScanTree> {
    let (done, finished) = mpsc::channel::<()>();
    std::thread::scope(|threads| {
        let counter = threads.spawn(move || {
            let mut err = std::io::stderr();
            let mut shown = 0;
            while let Err(RecvTimeoutError::Timeout) = finished.recv_timeout(COUNTER_EVERY) {
                let line = counter_line(progress.files(), progress.bytes());
                shown = write_counter(&mut err, &line, shown);
            }
            clear_counter(&mut err, shown);
        });
        let result = scan(root, opts, progress);
        drop(done);
        let _ = counter.join();
        result
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    use_utf8_console();

    let root = cli
        .path
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    if !root.exists() {
        eprintln!("error: path not found: {}", root.display());
        return ExitCode::from(1);
    }

    let threads = cli
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()) * 4);
    let opts = ScanOptions {
        threads: threads.max(1),
        excluded: vec![],
    };
    let start = Instant::now();
    let progress = ScanProgress::default();
    let scanned = if std::io::stderr().is_terminal() {
        scan_with_counter(&root, &opts, &progress)
    } else {
        scan(&root, &opts, &progress)
    };
    let tree = match scanned {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let elapsed: Duration = start.elapsed();

    let volume = volume_space(&tree.root_path).ok();
    let ropts = RenderOpts {
        depth: cli.depth,
        top: cli.top,
        files: cli.files,
    };
    print!("{}", render(&tree, &ropts, volume, elapsed));
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_says_files_and_size() {
        let line = counter_line(1234, 2048);
        assert!(line.contains("1234 files"), "{line}");
        assert!(line.contains("2.0 KiB"), "{line}");
    }

    #[test]
    fn a_shorter_line_covers_the_longer_one_before_it() {
        let mut out = Vec::new();
        let shown = write_counter(&mut out, "long line", 0);
        assert_eq!(shown, 9);
        let shown = write_counter(&mut out, "short", shown);
        assert_eq!(shown, 5);
        assert_eq!(String::from_utf8(out).unwrap(), "\rlong line\rshort    ");
    }

    #[test]
    fn clearing_leaves_a_blank_line_with_the_cursor_at_its_start() {
        let mut out = Vec::new();
        clear_counter(&mut out, 4);
        assert_eq!(String::from_utf8(out).unwrap(), "\r    \r");
    }

    #[test]
    fn the_counter_ends_with_a_scan_that_fails() {
        let dir = std::env::temp_dir();
        let opts = ScanOptions {
            threads: 2,
            excluded: Vec::new(),
        };
        let missing = dir.join("katharistis-no-such-folder-for-the-counter");
        let progress = ScanProgress::default();
        assert!(scan_with_counter(&missing, &opts, &progress).is_err());
    }
}
