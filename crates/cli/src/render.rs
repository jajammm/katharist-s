//! Text output of a finished scan, in the shape of the Go reference.

use std::fmt::Write;
use std::time::Duration;

use katharistis_core::format::format_size;
use katharistis_core::scan::{EntryClass, NodeId, ScanTree};

const BAR_WIDTH: usize = 20;

pub struct RenderOpts {
    /// Folder levels shown below the root.
    pub depth: usize,
    /// Folders shown per level, 0 = all.
    pub top: usize,
    /// Largest files listed, 0 = none.
    pub files: usize,
}

fn bar(frac: f64) -> String {
    let n = ((frac * BAR_WIDTH as f64 + 0.5) as usize).min(BAR_WIDTH);
    format!("{}{}", "█".repeat(n), "░".repeat(BAR_WIDTH - n))
}

fn write_level(out: &mut String, tree: &ScanTree, id: NodeId, level: usize, opts: &RenderOpts) {
    let parent_size = tree.nodes[id].size;
    let dirs: Vec<NodeId> = tree
        .children_by_size(id)
        .into_iter()
        .filter(|&c| tree.nodes[c].class == EntryClass::Dir)
        .collect();
    let shown = if opts.top > 0 {
        opts.top.min(dirs.len())
    } else {
        dirs.len()
    };
    let indent = "  ".repeat(level);
    for &c in &dirs[..shown] {
        let node = &tree.nodes[c];
        let frac = if parent_size > 0 {
            node.size as f64 / parent_size as f64
        } else {
            0.0
        };
        let _ = writeln!(
            out,
            "{:>10}  {} {:>5.1}%  {}{}/",
            format_size(node.size),
            bar(frac),
            frac * 100.0,
            indent,
            node.name
        );
        if level + 1 < opts.depth {
            write_level(out, tree, c, level + 1, opts);
        }
    }
    if shown < dirs.len() {
        let rest: u64 = dirs[shown..].iter().map(|&c| tree.nodes[c].size).sum();
        let _ = writeln!(
            out,
            "{:>10}  {}         {}… {} folder lainnya",
            format_size(rest),
            " ".repeat(BAR_WIDTH),
            indent,
            dirs.len() - shown
        );
    }
}

pub fn render(
    tree: &ScanTree,
    opts: &RenderOpts,
    volume: Option<(u64, u64)>,
    elapsed: Duration,
) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "\n📁 {}  —  total {}, {} file, {} folder  ({:.2}s)\n\n",
        tree.root_path.display(),
        format_size(tree.nodes[tree.root].size),
        tree.files,
        tree.dirs,
        elapsed.as_secs_f64()
    );
    write_level(&mut out, tree, tree.root, 0, opts);

    if opts.files > 0 {
        let mut files: Vec<NodeId> = (0..tree.nodes.len())
            .filter(|&i| tree.nodes[i].class == EntryClass::File && is_attached(tree, i))
            .collect();
        files.sort_by(|&a, &b| {
            tree.nodes[b]
                .size
                .cmp(&tree.nodes[a].size)
                .then_with(|| a.cmp(&b))
        });
        files.truncate(opts.files);
        if !files.is_empty() {
            let _ = write!(out, "\n📄 {} file terbesar:\n", files.len());
            for id in files {
                let _ = writeln!(
                    out,
                    "{:>10}  {}",
                    format_size(tree.nodes[id].size),
                    tree.path_of(id).display()
                );
            }
        }
    }

    if let Some((total, free)) = volume {
        let _ = write!(
            out,
            "\n💾 Volume: {} terpakai dari {}, sisa {}\n",
            format_size(total.saturating_sub(free)),
            format_size(total),
            format_size(free)
        );
    }
    let restricted = tree.errors.iter().filter(|e| e.is_access_denied()).count();
    let problems = tree.errors.len() - restricted;
    if restricted > 0 {
        let _ = writeln!(
            out,
            "ℹ️  {restricted} folder butuh hak administrator, jadi isinya tidak terhitung."
        );
    }
    if problems > 0 {
        let _ = writeln!(out, "⚠️  {problems} item gagal dibaca.");
    }
    out
}

/// Nodes removed with `remove_subtree` keep their slot but lose their parent.
fn is_attached(tree: &ScanTree, mut id: NodeId) -> bool {
    while let Some(p) = tree.nodes[id].parent {
        id = p;
    }
    id == tree.root
}

#[cfg(test)]
mod tests {
    use super::*;
    use katharistis_core::scan::{EntryClass, Node, ScanError};
    use std::path::PathBuf;

    const GIB: u64 = 1 << 30;

    fn node(name: &str, size: u64, class: EntryClass, parent: Option<usize>) -> Node {
        Node {
            name: name.into(),
            size,
            modified: None,
            class,
            cloud: false,
            parent,
            children: vec![],
        }
    }

    /// root 100 GiB: big 69 GiB (holds huge.iso 60 GiB, small.txt 9 GiB), mid 20 GiB, tiny 11 GiB.
    fn fixture() -> ScanTree {
        let mut nodes = vec![
            node("root", 100 * GIB, EntryClass::Dir, None),
            node("big", 69 * GIB, EntryClass::Dir, Some(0)),
            node("mid", 20 * GIB, EntryClass::Dir, Some(0)),
            node("tiny", 11 * GIB, EntryClass::Dir, Some(0)),
            node("huge.iso", 60 * GIB, EntryClass::File, Some(1)),
            node("small.txt", 9 * GIB, EntryClass::File, Some(1)),
            node("cache", 8 * GIB, EntryClass::Dir, Some(2)),
        ];
        nodes[0].children = vec![1, 2, 3];
        nodes[1].children = vec![4, 5];
        nodes[2].children = vec![6];
        ScanTree {
            nodes,
            root: 0,
            root_path: PathBuf::from("C:/data"),
            errors: vec![],
            files: 2,
            dirs: 4,
            cancelled: false,
        }
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn renders_bar_percent_and_rest() {
        let tree = fixture();
        let opts = RenderOpts {
            depth: 1,
            top: 1,
            files: 0,
        };
        let s = render(&tree, &opts, Some((228 * GIB, 25 * GIB)), secs(2));
        assert!(s.contains("69.0%"), "{s}");
        assert!(s.contains("██████████████░░░░░░"), "{s}");
        assert!(s.contains("2 folder lainnya"), "{s}");
        assert!(s.contains("big/"), "{s}");
        assert!(!s.contains("mid/"), "{s}");
        assert!(s.contains("100.0 GiB"), "{s}");
    }

    #[test]
    fn top_zero_shows_all_and_files_zero_hides_files() {
        let tree = fixture();
        let opts = RenderOpts {
            depth: 1,
            top: 0,
            files: 0,
        };
        let s = render(&tree, &opts, None, secs(1));
        for name in ["big/", "mid/", "tiny/"] {
            assert!(s.contains(name), "{s}");
        }
        assert!(!s.contains("lainnya"), "{s}");
        assert!(!s.contains("huge.iso"), "{s}");
        assert!(!s.contains("terbesar"), "{s}");
        assert!(!s.contains("Volume"), "{s}");

        let opts = RenderOpts {
            depth: 1,
            top: 0,
            files: 1,
        };
        let s = render(&tree, &opts, None, secs(1));
        assert!(s.contains("huge.iso"), "{s}");
        assert!(!s.contains("small.txt"), "{s}");
    }

    #[test]
    fn depth_two_nests_children() {
        let tree = fixture();
        let opts = RenderOpts {
            depth: 2,
            top: 0,
            files: 0,
        };
        let s = render(&tree, &opts, None, secs(1));
        // Files are not listed in the folder tree.
        assert!(!s.contains("huge.iso"), "{s}");
        assert!(s.contains("  cache/"), "{s}");
        assert!(s.contains("40.0%"), "{s}");
        let flat = RenderOpts {
            depth: 1,
            top: 0,
            files: 0,
        };
        assert!(!render(&tree, &flat, None, secs(1)).contains("cache/"));
    }

    #[test]
    fn warns_about_denied_folders() {
        let mut tree = fixture();
        let opts = RenderOpts {
            depth: 1,
            top: 0,
            files: 0,
        };
        let clean = render(&tree, &opts, None, secs(1));
        assert!(!clean.contains("administrator"), "{clean}");
        assert!(!clean.contains("gagal dibaca"), "{clean}");

        let error = |path: &str, code: i32| ScanError {
            path: path.into(),
            reason: std::io::Error::from_raw_os_error(code).to_string(),
        };
        tree.errors = vec![error("C:/data/a", 5), error("C:/data/b", -2147024891)];
        let s = render(&tree, &opts, None, secs(1));
        assert!(s.contains("2 folder butuh hak administrator"), "{s}");
        assert!(!s.contains("gagal dibaca"), "{s}");

        tree.errors.push(error("C:/data/c", 1117));
        let s = render(&tree, &opts, None, secs(1));
        assert!(s.contains("2 folder butuh hak administrator"), "{s}");
        assert!(s.contains("1 item gagal dibaca"), "{s}");
    }

    #[test]
    fn shows_volume_line() {
        let tree = fixture();
        let opts = RenderOpts {
            depth: 1,
            top: 0,
            files: 0,
        };
        let s = render(&tree, &opts, Some((228 * GIB, 25 * GIB)), secs(1));
        assert!(s.contains("💾"), "{s}");
        assert!(
            s.contains("203.0 GiB terpakai dari 228.0 GiB, sisa 25.0 GiB"),
            "{s}"
        );
    }
}
