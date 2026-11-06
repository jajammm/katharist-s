//! The largest files of the scanned location: 100 MB and up, biggest first. The rows
//! come from the shared tree; a delete goes through the same flow as Home's.

use katharistis_core::analysis::{large_files, LARGE_MIN};
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::{NodeId, ScanTree};

use crate::i18n::l;
use crate::views::filelist::{show_list, FileList};
use crate::views::home::{HomeAction, HomeState};

#[derive(Default)]
pub struct LargeState {
    list: FileList,
}

/// The rows of the screen: files from the threshold up.
pub fn rows(tree: &ScanTree) -> Vec<NodeId> {
    large_files(tree, LARGE_MIN)
}

pub fn show(
    ui: &mut egui::Ui,
    st: &mut LargeState,
    tree: Option<&ScanTree>,
    home: &HomeState,
    protect: &ProtectContext,
) -> Option<HomeAction> {
    ui.heading(l("Largest", "Terbesar"));
    ui.weak(l(
        "Files of 100 MB and up in the scanned location.",
        "File 100 MB ke atas di lokasi yang dipindai.",
    ));
    ui.add_space(8.0);
    show_list(
        ui,
        "large-list",
        tree,
        home,
        protect,
        &mut st.list,
        0,
        l(
            "No file of 100 MB or more here.",
            "Tidak ada file 100 MB ke atas di sini.",
        ),
        rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::test_support::{file, tree_of};

    #[test]
    fn only_files_from_100_mb_up_biggest_first() {
        const MB: u64 = 1024 * 1024;
        let tree = tree_of(vec![
            file("small.bin", 99 * MB + 1),
            file("exact.bin", 100 * MB),
            file("big.bin", 300 * MB),
        ]);
        let names: Vec<_> = rows(&tree)
            .into_iter()
            .map(|id| tree.nodes[id].name.clone())
            .collect();
        assert_eq!(names, ["big.bin", "exact.bin"]);
    }
}
