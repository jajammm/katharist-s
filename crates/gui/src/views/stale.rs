//! Files not changed since a threshold: 6 months, 1 year or 2 years, oldest first.
//! One year is where it starts. The rows come from the shared tree.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use katharistis_core::analysis::stale_files;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::{NodeId, ScanTree};

use crate::i18n::l;
use crate::views::filelist::{show_list, FileList};
use crate::views::home::{HomeAction, HomeState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Age {
    SixMonths,
    OneYear,
    TwoYears,
}

impl Age {
    pub const ALL: [Age; 3] = [Age::SixMonths, Age::OneYear, Age::TwoYears];

    pub fn duration(self) -> Duration {
        let days = match self {
            Age::SixMonths => 183,
            Age::OneYear => 365,
            Age::TwoYears => 730,
        };
        Duration::from_secs(days * 86_400)
    }

    pub fn label(self) -> &'static str {
        match self {
            Age::SixMonths => l("6 months", "6 bulan"),
            Age::OneYear => l("1 year", "1 tahun"),
            Age::TwoYears => l("2 years", "2 tahun"),
        }
    }

    fn key(self) -> u64 {
        self as u64
    }
}

pub struct StaleState {
    age: Age,
    list: FileList,
}

impl Default for StaleState {
    fn default() -> Self {
        StaleState {
            age: Age::OneYear,
            list: FileList::default(),
        }
    }
}

/// The rows for one age. `now` is read once per build.
pub fn rows(tree: &ScanTree, age: Age, now: SystemTime) -> Vec<NodeId> {
    stale_files(tree, age.duration(), now)
}

pub fn show(
    ui: &mut egui::Ui,
    st: &mut StaleState,
    tree: Option<&Arc<ScanTree>>,
    home: &HomeState,
    protect: &ProtectContext,
) -> Option<HomeAction> {
    ui.heading(l("Not changed in a while", "Lama Tak Diubah"));
    ui.weak(l(
        "Files not changed since the age you pick, in the scanned location.",
        "File yang tidak diubah sejak usia yang dipilih, di lokasi yang dipindai.",
    ));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(l("Not changed for", "Tidak diubah selama"));
        for age in Age::ALL {
            ui.selectable_value(&mut st.age, age, age.label());
        }
    });
    ui.add_space(6.0);
    let age = st.age;
    show_list(
        ui,
        "stale-list",
        tree,
        home,
        protect,
        &mut st.list,
        age.key(),
        l(
            "No file has gone unchanged that long.",
            "Tidak ada file yang selama itu tidak diubah.",
        ),
        {
            let now = SystemTime::now();
            move |tree| rows(tree, age, now)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::test_support::{file, tree_of};

    #[test]
    fn the_default_is_one_year_and_the_choices_are_the_three_asked_for() {
        assert_eq!(StaleState::default().age, Age::OneYear);
        let days: Vec<u64> = Age::ALL
            .iter()
            .map(|a| a.duration().as_secs() / 86_400)
            .collect();
        assert_eq!(days, [183, 365, 730]);
    }

    #[test]
    fn each_age_keeps_only_files_older_than_it_oldest_first() {
        let now = SystemTime::now();
        let day = Duration::from_secs(86_400);
        let mut a = file("eight_months.txt", 1);
        a.modified = Some(now - day * 240);
        let mut b = file("eighteen_months.txt", 1);
        b.modified = Some(now - day * 550);
        let mut c = file("three_years.txt", 1);
        c.modified = Some(now - day * 1100);
        let mut d = file("recent.txt", 1);
        d.modified = Some(now - day * 10);
        let unknown = file("unknown.txt", 1);
        let tree = tree_of(vec![a, b, c, d, unknown]);
        let names = |age| -> Vec<String> {
            rows(&tree, age, now)
                .into_iter()
                .map(|id| tree.nodes[id].name.clone())
                .collect()
        };
        assert_eq!(
            names(Age::SixMonths),
            ["three_years.txt", "eighteen_months.txt", "eight_months.txt"]
        );
        assert_eq!(
            names(Age::OneYear),
            ["three_years.txt", "eighteen_months.txt"]
        );
        assert_eq!(names(Age::TwoYears), ["three_years.txt"]);
    }
}
