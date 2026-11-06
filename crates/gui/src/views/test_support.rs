//! Small hand-made trees for the tests of the list screens.

use std::path::PathBuf;

use katharistis_core::scan::{EntryClass, Node, ScanTree};

pub fn file(name: &str, size: u64) -> Node {
    Node {
        name: name.into(),
        size,
        modified: None,
        class: EntryClass::File,
        cloud: false,
        parent: None,
        children: Vec::new(),
    }
}

/// A root `C:\t` holding `files` directly.
pub fn tree_of(files: Vec<Node>) -> ScanTree {
    let mut nodes = vec![Node {
        name: r"C:\t".into(),
        size: files.iter().map(|f| f.size).sum(),
        modified: None,
        class: EntryClass::Dir,
        cloud: false,
        parent: None,
        children: Vec::new(),
    }];
    for mut f in files {
        f.parent = Some(0);
        nodes.push(f);
        let id = nodes.len() - 1;
        nodes[0].children.push(id);
    }
    ScanTree {
        nodes,
        root: 0,
        root_path: PathBuf::from(r"C:\t"),
        errors: Vec::new(),
        files: 0,
        dirs: 0,
        cancelled: false,
    }
}
