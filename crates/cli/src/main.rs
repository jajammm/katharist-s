mod render;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use katharistis_core::scan::{scan, ScanOptions, ScanProgress};
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
    let tree = match scan(&root, &opts, &ScanProgress::default()) {
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
