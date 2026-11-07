//! Files not changed since a threshold: 6 months, 1 year or 2 years, oldest first.
//! One year is where it starts. The rows come from the shared tree.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use katharistis_core::analysis::stale_files;
use katharistis_core::protect::ProtectContext;
use katharistis_core::scan::{NodeId, ScanTree};

use crate::i18n::l;
use crate::views::filelist::{show_list, FileList};
use crate::views::home::{area_of, Area, HomeAction, HomeState};

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
    /// Off at first: on a whole drive the oldest files are Windows' own and those of
    /// installed programs, which have no checkbox and would bury everything else.
    show_system: bool,
    list: FileList,
}

impl Default for StaleState {
    fn default() -> Self {
        StaleState {
            age: Age::OneYear,
            show_system: false,
            list: FileList::default(),
        }
    }
}

/// The rows for one age. `now` is read once per build. Runs in the list's job, since
/// telling the areas apart takes a path for every old file.
pub fn rows(
    tree: &ScanTree,
    age: Age,
    now: SystemTime,
    show_system: bool,
    protect: &ProtectContext,
) -> Vec<NodeId> {
    let mut ids = stale_files(tree, age.duration(), now);
    if !show_system {
        ids.retain(|&id| {
            !matches!(
                area_of(&tree.path_of(id), protect),
                Area::Windows | Area::ProgramFiles
            )
        });
    }
    ids
}

/// What the rows depend on besides the tree.
fn list_key(age: Age, show_system: bool) -> u64 {
    age.key() * 2 + u64::from(show_system)
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
    ui.checkbox(
        &mut st.show_system,
        l(
            "Show files in Windows and Program Files",
            "Tampilkan file di Windows dan Program Files",
        ),
    );
    ui.add_space(6.0);
    let age = st.age;
    let show_system = st.show_system;
    let area_context = protect.clone();
    show_list(
        ui,
        "stale-list",
        tree,
        home,
        protect,
        &mut st.list,
        list_key(age, show_system),
        l(
            "No file has gone unchanged that long.",
            "Tidak ada file yang selama itu tidak diubah.",
        ),
        {
            let now = SystemTime::now();
            move |tree| rows(tree, age, now, show_system, &area_context)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::test_support::{file, tree_of};
    use std::path::PathBuf;

    fn ctx() -> ProtectContext {
        ProtectContext {
            windows_dir: r"C:\Windows".into(),
            program_files: vec![r"C:\Program Files".into(), r"C:\Program Files (x86)".into()],
            program_data: r"C:\ProgramData".into(),
            users_dir: r"C:\Users".into(),
            profile: r"C:\Users\me".into(),
            own_dirs: vec![r"C:\Users\me\own".into()],
        }
    }

    #[test]
    fn windows_and_program_files_stay_out_until_asked_for() {
        let now = SystemTime::now();
        let old = |name: &str| {
            let mut f = file(name, 1);
            f.modified = Some(now - Duration::from_secs(1000 * 86_400));
            f
        };
        let mut tree = tree_of(vec![
            old(r"Windows\System32\old.dll"),
            old(r"Program Files\App\old.exe"),
            old(r"Program Files (x86)\App\old.exe"),
            old(r"Users\me\old.zip"),
            old(r"Games\old.iso"),
        ]);
        tree.root_path = PathBuf::from(r"C:\");
        let names = |show| -> Vec<String> {
            rows(&tree, Age::OneYear, now, show, &ctx())
                .into_iter()
                .map(|id| tree.nodes[id].name.clone())
                .collect()
        };
        assert_eq!(names(false), [r"Games\old.iso", r"Users\me\old.zip"]);
        assert_eq!(names(true).len(), 5);
    }

    #[test]
    fn the_toggle_and_the_age_each_make_the_list_again() {
        let mut keys: Vec<u64> = Age::ALL
            .iter()
            .flat_map(|&a| [list_key(a, false), list_key(a, true)])
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 6);
    }

    #[test]
    fn the_default_is_one_year_and_the_choices_are_the_three_asked_for() {
        assert_eq!(StaleState::default().age, Age::OneYear);
        assert!(!StaleState::default().show_system);
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
            rows(&tree, age, now, true, &ctx())
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
