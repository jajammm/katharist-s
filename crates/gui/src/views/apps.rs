//! The installed-apps screen. Opening it reads only the registry. winget runs only
//! when the user asks, and an uninstall starts only from a confirmation that names
//! the app and what runs. Leftovers are offered after the app left the registry,
//! unticked, and are deleted through the shared flow in `confirm`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use katharistis_core::apps::leftovers::{find_leftovers, Certainty, Leftover, LeftoverRoots};
use katharistis_core::apps::registry::{list_installed, still_registered, NaiveDateLike};
use katharistis_core::apps::uninstall::{
    parse_uninstall, run_uninstaller, UninstallCmd, UninstallEnd,
};
use katharistis_core::apps::winget::{
    self, merge, run_visible, AppRow, WingetOutcome, WingetPackage,
};
use katharistis_core::apps::AppEntry;
use katharistis_core::delete::{DeleteMode, DeleteReport, Target};
use katharistis_core::format::format_size;
use katharistis_core::protect::{canonical_key, ProtectContext};

use crate::confirm::{ConfirmContext, ConfirmRequest};
use crate::i18n::l;
use crate::jobs::{Job, JobPanicked};
use crate::views::home::cell;

/// What one confirmed click runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Uninstaller(UninstallCmd),
    WingetUninstall(String),
    WingetUpgrade(String),
}

/// One app and the one thing to run for it, copied when the button was pressed, so
/// a list that changes afterwards cannot change what the confirmation started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pending {
    pub name: String,
    /// `None` for an app only winget knows: there is no registry key to check.
    pub entry: Option<AppEntry>,
    pub method: Method,
    /// winget's package for this app, whichever method runs: the list winget gave
    /// stays in memory and has to hear about what this run did to the app.
    pub winget_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AppsPhase {
    #[default]
    Idle,
    /// Waiting for the user to confirm which app and which uninstaller runs.
    Confirming(Box<Pending>),
    /// The process runs; holds the app's name.
    Uninstalling(String),
    /// The process ended but the app is still registered; holds its key.
    Waiting(String),
    /// The app is gone. Empty until the search finishes.
    Leftovers(Vec<Leftover>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnd {
    Cancelled,
    Done(Option<i32>),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WingetCheck {
    Ok(Vec<WingetPackage>, Option<String>),
    NeedsAgreement(String),
    NotInstalled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    pub publisher: Option<String>,
    pub size: u64,
    pub date: Option<NaiveDateLike>,
    pub version: Option<String>,
    pub available: Option<String>,
    pub entry: Option<AppEntry>,
    /// Set only when the registry's uninstall string parsed to a real program.
    pub official: Option<UninstallCmd>,
    /// Set only when winget's package is this app's, not an ambiguous guess.
    pub winget_id: Option<String>,
    /// Installed somewhere other than Program Files. A mark only, nothing is refused.
    pub outside_program_files: bool,
}

/// Whether an install location lies outside Program Files. A row with no location
/// says nothing about where the app is, and neither does one that names no place.
pub fn outside_program_files(location: Option<&Path>, program_files: &[PathBuf]) -> bool {
    let Some(location) = location else {
        return false;
    };
    // Installers do write this value with quotes around it.
    let text = location.to_string_lossy();
    let Some(key) = canonical_key(Path::new(text.trim().trim_matches('"'))) else {
        return false;
    };
    !program_files
        .iter()
        .filter_map(|dir| canonical_key(dir))
        .any(|dir| Path::new(&key).starts_with(dir))
}

fn drop_package(packages: &mut Vec<WingetPackage>, id: &str) {
    packages.retain(|p| p.id != id);
}

/// Brings what winget said earlier in line with a finished run, so the next merge
/// shows the app as it is now. A package of an app that is gone would come back as a
/// row of its own; one that was updated would still offer the update.
pub fn packages_after_run(
    packages: &mut Vec<WingetPackage>,
    p: &Pending,
    end: &RunEnd,
    registered: bool,
) {
    let (RunEnd::Done(code), Some(id)) = (end, &p.winget_id) else {
        return;
    };
    let succeeded = *code == Some(0);
    match p.method {
        Method::WingetUpgrade(_) => {
            if let Some(package) = packages
                .iter_mut()
                .find(|q| &q.id == id)
                .filter(|_| succeeded)
            {
                if let Some(version) = package.available.take() {
                    package.version = version;
                }
            }
        }
        Method::WingetUninstall(_) | Method::Uninstaller(_) => {
            let key_gone = p.entry.is_some() && !registered;
            let winget_done = succeeded && matches!(p.method, Method::WingetUninstall(_));
            if key_gone || winget_done {
                drop_package(packages, id);
            }
        }
    }
}

/// The phase once the process ended. The app is gone from the registry only when
/// `registered` says so; the leftover list itself is found afterwards.
pub fn next_after_exit(key: &str, registered: &dyn Fn(&str) -> bool) -> AppsPhase {
    if registered(key) {
        AppsPhase::Waiting(key.to_string())
    } else {
        AppsPhase::Leftovers(Vec::new())
    }
}

impl AppsPhase {
    /// Opens the confirmation, only from idle.
    pub fn ask(self, p: Pending) -> AppsPhase {
        match self {
            AppsPhase::Idle => AppsPhase::Confirming(Box::new(p)),
            other => other,
        }
    }

    pub fn cancel_confirm(self) -> AppsPhase {
        match self {
            AppsPhase::Confirming(_) => AppsPhase::Idle,
            other => other,
        }
    }

    /// The confirmed one is handed out to run; any other phase runs nothing.
    pub fn accept(self) -> (AppsPhase, Option<Pending>) {
        match self {
            AppsPhase::Confirming(p) => (AppsPhase::Uninstalling(p.name.clone()), Some(*p)),
            other => (other, None),
        }
    }

    /// Something is open or running, so no row may start another.
    pub fn busy(&self) -> bool {
        !matches!(self, AppsPhase::Idle)
    }
}

/// What `parse_uninstall` takes as an existing program: an absolute path to a file.
pub fn is_abs_file(s: &str) -> bool {
    let p = std::path::Path::new(s);
    p.is_absolute() && p.is_file()
}

fn code_line(p: &Pending, end: &RunEnd) -> String {
    match (&p.method, end) {
        (Method::Uninstaller(_), _) | (_, RunEnd::Done(None)) => String::new(),
        (_, RunEnd::Done(Some(c))) => format!(
            "{} {c}.",
            l(
                "winget finished with exit code",
                "winget selesai dengan kode keluar"
            )
        ),
        _ => String::new(),
    }
}

/// What follows a finished process, and the plain line to show about it.
pub fn after_run(
    p: &Pending,
    end: &RunEnd,
    registered: &dyn Fn(&str) -> bool,
) -> (AppsPhase, String) {
    match end {
        RunEnd::Cancelled => {
            return (
                AppsPhase::Idle,
                l(
                    "Cancelled before it started. Nothing was changed.",
                    "Dibatalkan sebelum mulai. Tidak ada yang diubah.",
                )
                .to_string(),
            )
        }
        RunEnd::Failed(e) => {
            return (
                AppsPhase::Idle,
                format!(
                    "{} {e}",
                    l("It could not be started:", "Tidak bisa dimulai:")
                ),
            )
        }
        RunEnd::Done(_) => {}
    }
    let line = code_line(p, end);
    if matches!(p.method, Method::WingetUpgrade(_)) {
        return (AppsPhase::Idle, line);
    }
    match &p.entry {
        Some(e) => (next_after_exit(&e.key, registered), line),
        None => (
            AppsPhase::Idle,
            format!(
                "{line} {}",
                l(
                    "This app is not in the registry list, so Katharistís cannot tell whether it is gone and offers no leftovers for it.",
                    "Aplikasi ini tidak ada di daftar registry, jadi Katharistís tidak bisa memastikan sudah hilang dan tidak menawarkan sisa file untuknya."
                )
            )
            .trim()
            .to_string(),
        ),
    }
}

/// winget's arguments for a method, `None` for the app's own uninstaller.
pub fn winget_args(m: &Method) -> Option<Vec<String>> {
    let (verb, id) = match m {
        Method::Uninstaller(_) => return None,
        Method::WingetUninstall(id) => ("uninstall", id),
        Method::WingetUpgrade(id) => ("upgrade", id),
    };
    Some(vec![
        verb.into(),
        "--id".into(),
        id.clone(),
        "--exact".into(),
    ])
}

/// Runs a confirmed method. Blocks until the process ends: only a job calls it.
fn run_pending(p: &Pending) -> RunEnd {
    match &p.method {
        Method::Uninstaller(cmd) => match run_uninstaller(cmd) {
            Ok(UninstallEnd::Exited) => RunEnd::Done(None),
            Ok(UninstallEnd::Cancelled) => RunEnd::Cancelled,
            Err(e) => RunEnd::Failed(e.to_string()),
        },
        m => {
            let args = winget_args(m).unwrap_or_default();
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            match run_visible(&refs) {
                Ok(code) => RunEnd::Done(Some(code)),
                Err(e) => RunEnd::Failed(e.to_string()),
            }
        }
    }
}

/// The installed list with winget's available versions, from `list` then `upgrades`.
/// The second call is skipped unless the first gave a list, and both get `agree`
/// exactly as given: the caller passes true only after the user pressed Agree.
pub fn check_winget(
    agree: bool,
    list: &dyn Fn(bool) -> WingetOutcome,
    upgrades: &dyn Fn(bool) -> WingetOutcome,
) -> WingetCheck {
    let mut packages = match list(agree) {
        WingetOutcome::Ok(p) => p,
        WingetOutcome::NeedsAgreement(t) => return WingetCheck::NeedsAgreement(t),
        WingetOutcome::NotInstalled => return WingetCheck::NotInstalled,
        WingetOutcome::Failed(t) => return WingetCheck::Failed(t),
    };
    let note = match upgrades(agree) {
        WingetOutcome::Ok(ups) => {
            for p in &mut packages {
                if let Some(u) = ups.iter().find(|u| u.id == p.id && u.available.is_some()) {
                    p.available = u.available.clone();
                }
            }
            None
        }
        WingetOutcome::NeedsAgreement(t) | WingetOutcome::Failed(t) => Some(t),
        WingetOutcome::NotInstalled => {
            Some(l("winget is not installed.", "winget tidak terpasang.").to_string())
        }
    };
    WingetCheck::Ok(packages, note)
}

pub fn build_rows(
    rows: Vec<AppRow>,
    exists: &dyn Fn(&str) -> bool,
    program_files: &[PathBuf],
) -> Vec<Row> {
    rows.into_iter()
        .map(|r| match r {
            AppRow::Registry(e, p) => Row {
                outside_program_files: outside_program_files(
                    e.install_location.as_deref(),
                    program_files,
                ),
                name: e.name.clone(),
                publisher: e.publisher.clone(),
                size: e.size,
                date: e.install_date,
                version: e.version.clone().or_else(|| {
                    p.as_ref()
                        .map(|p| p.version.clone())
                        .filter(|v| !v.is_empty())
                }),
                available: p.as_ref().and_then(|p| p.available.clone()),
                official: e
                    .uninstall
                    .as_deref()
                    .and_then(|s| parse_uninstall(s, exists)),
                winget_id: p.map(|p| p.id),
                entry: Some(e),
            },
            AppRow::WingetOnly(p) => Row {
                name: p.name,
                publisher: None,
                size: 0,
                date: None,
                version: Some(p.version).filter(|v| !v.is_empty()),
                available: p.available,
                entry: None,
                official: None,
                winget_id: Some(p.id),
                outside_program_files: false,
            },
        })
        .collect()
}

impl Row {
    fn pending(&self, method: Method) -> Pending {
        Pending {
            name: self.name.clone(),
            entry: self.entry.clone(),
            method,
            winget_id: self.winget_id.clone(),
        }
    }
}

/// Indices of the rows that match `query` (in name or publisher), by size.
pub fn view_order(rows: &[Row], query: &str, biggest_first: bool) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<usize> = (0..rows.len())
        .filter(|&i| {
            q.is_empty()
                || rows[i].name.to_lowercase().contains(&q)
                || rows[i]
                    .publisher
                    .as_deref()
                    .is_some_and(|p| p.to_lowercase().contains(&q))
        })
        .collect();
    out.sort_by(|&a, &b| {
        let by_size = rows[a].size.cmp(&rows[b].size);
        let by_size = if biggest_first {
            by_size.reverse()
        } else {
            by_size
        };
        by_size.then_with(|| {
            rows[a]
                .name
                .to_lowercase()
                .cmp(&rows[b].name.to_lowercase())
        })
    });
    out
}

type Finder<'a> = dyn Fn(&AppEntry, &LeftoverRoots, &[AppEntry]) -> Vec<Leftover> + 'a;

/// Leftovers of `app`, `None` while the registry still has it. `installed` is the
/// list read after the uninstall; the app itself is taken out of it, otherwise the
/// search would hold its own name against itself.
pub fn find_after(
    app: &AppEntry,
    installed: Vec<AppEntry>,
    roots: &LeftoverRoots,
    registered: &dyn Fn(&str) -> bool,
    finder: &Finder<'_>,
) -> Option<Vec<Leftover>> {
    if registered(&app.key) {
        return None;
    }
    let others: Vec<AppEntry> = installed
        .into_iter()
        .filter(|o| !o.key.eq_ignore_ascii_case(&app.key))
        .collect();
    Some(finder(app, roots, &others))
}

fn leftover_roots() -> Option<LeftoverRoots> {
    let var = |n: &str| std::env::var_os(n).map(PathBuf::from);
    Some(LeftoverRoots {
        roaming: var("APPDATA")?,
        local: var("LOCALAPPDATA")?,
        program_data: var("ProgramData")?,
    })
}

/// The Recycle Bin request for the ticked leftovers. `None` when nothing is ticked.
pub fn request_from(items: &[Leftover], ticked: &[PathBuf]) -> Option<ConfirmRequest> {
    let targets: Vec<Target> = items
        .iter()
        .filter(|i| ticked.contains(&i.path))
        .map(|i| Target {
            path: i.path.clone(),
            size: i.size,
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    Some(ConfirmRequest {
        items: targets,
        mode: DeleteMode::RecycleBin,
        warn_app_owned: 0,
        warn_holds_more: 0,
        context: ConfirmContext::Leftovers,
        empty_bin: None,
        bin_load: None,
        guards: Vec::new(),
    })
}

/// What the confirmation says: which app, and which program or winget id runs.
pub fn confirm_text(p: &Pending) -> String {
    match &p.method {
        Method::Uninstaller(cmd) => format!(
            "{} {}\n{} {} {}",
            l("Start the uninstaller of", "Jalankan uninstaller milik"),
            p.name,
            l("It runs:", "Yang dijalankan:"),
            cmd.file,
            cmd.args
        )
        .trim_end()
        .to_string(),
        Method::WingetUninstall(id) => format!(
            "{} {}\n{} winget uninstall --id {id} --exact",
            l("Uninstall with winget:", "Uninstall dengan winget:"),
            p.name,
            l("It runs:", "Yang dijalankan:"),
        ),
        Method::WingetUpgrade(id) => format!(
            "{} {}\n{} winget upgrade --id {id} --exact",
            l("Update with winget:", "Perbarui dengan winget:"),
            p.name,
            l("It runs:", "Yang dijalankan:"),
        ),
    }
}

pub fn date_text(d: Option<NaiveDateLike>) -> String {
    match d {
        Some((y, m, d)) => format!("{y:04}-{m:02}-{d:02}"),
        None => "-".to_string(),
    }
}

/// Said while an uninstaller runs: closing the window is allowed and does not stop it.
pub fn close_note() -> &'static str {
    l(
        "You can close Katharistís meanwhile. The uninstaller keeps running on its own, but leftovers will not be offered afterwards.",
        "Anda boleh menutup Katharistís. Uninstaller tetap berjalan sendiri, tetapi sisa file tidak akan ditawarkan setelahnya.",
    )
}

struct RunDone {
    end: RunEnd,
    registered: bool,
}

struct Found {
    leftovers: Option<Vec<Leftover>>,
}

#[derive(Default)]
enum WingetUi {
    #[default]
    Idle,
    Checking(Job<WingetCheck>),
    Agreement(String),
    Failed(String),
    Checked(Option<String>),
}

#[derive(Default)]
pub struct AppsState {
    started: bool,
    packages: Vec<WingetPackage>,
    rows: Vec<Row>,
    /// Counts every new `rows`, which is what `order` was made from.
    rows_gen: u64,
    load: Option<Job<Vec<Row>>>,
    reload_again: bool,
    load_failed: bool,
    query: String,
    smallest_first: bool,
    order: Vec<usize>,
    order_key: Option<(u64, String, bool)>,
    winget_hidden: Option<bool>,
    winget: WingetUi,
    phase: AppsPhase,
    /// The app of the running or finished uninstall, for the checks that follow.
    current: Option<Pending>,
    run: Option<Job<RunDone>>,
    recheck: Option<Job<bool>>,
    find: Option<Job<Found>>,
    ticked: HashSet<PathBuf>,
    note: Option<String>,
    marks_dirty: bool,
}

impl AppsState {
    fn phase(&self) -> &AppsPhase {
        &self.phase
    }

    fn set_phase(&mut self, p: AppsPhase) {
        self.phase = p;
    }

    fn take_phase(&mut self) -> AppsPhase {
        std::mem::take(&mut self.phase)
    }

    /// Reads the registry in a job and merges what winget said so far.
    fn reload(&mut self, ctx: &egui::Context) {
        if self.load.is_some() {
            self.reload_again = true;
            return;
        }
        let packages = self.packages.clone();
        self.load = Some(Job::spawn(ctx.clone(), move || {
            let program_files = ProtectContext::from_env().program_files;
            build_rows(
                merge(list_installed(), packages),
                &is_abs_file,
                &program_files,
            )
        }));
    }

    /// Set once an uninstall finished, so Home's "used by an app" marks get read again.
    pub fn take_marks_dirty(&mut self) -> bool {
        std::mem::take(&mut self.marks_dirty)
    }

    /// A delete of leftovers finished: what went leaves the list.
    pub fn after_report(&mut self, report: &DeleteReport) {
        let gone: HashSet<&PathBuf> = report.removed.iter().chain(&report.recycled).collect();
        if let AppsPhase::Leftovers(items) = &mut self.phase {
            items.retain(|i| !gone.contains(&i.path));
        }
        self.ticked.retain(|p| !gone.contains(p));
    }

    fn start_check(&mut self, ctx: &egui::Context, agree: bool) {
        self.winget = WingetUi::Checking(Job::spawn(ctx.clone(), move || {
            check_winget(agree, &winget::list, &winget::upgrades)
        }));
    }

    fn start_find(&mut self, ctx: &egui::Context) {
        let Some(app) = self.current.as_ref().and_then(|p| p.entry.clone()) else {
            return;
        };
        self.ticked.clear();
        self.find = Some(Job::spawn(ctx.clone(), move || {
            let Some(roots) = leftover_roots() else {
                return Found {
                    leftovers: Some(Vec::new()),
                };
            };
            let leftovers = find_after(
                &app,
                list_installed(),
                &roots,
                &still_registered,
                &find_leftovers,
            );
            Found { leftovers }
        }));
    }

    fn start_recheck(&mut self, ctx: &egui::Context, key: String) {
        self.recheck = Some(Job::spawn(ctx.clone(), move || still_registered(&key)));
    }

    /// Enters `phase` and starts the leftover search when it is the open one.
    fn enter(&mut self, ctx: &egui::Context, phase: AppsPhase) {
        let search = phase == AppsPhase::Leftovers(Vec::new());
        self.set_phase(phase);
        if search {
            self.start_find(ctx);
        }
    }

    /// Takes in finished work. Runs every frame, so an uninstall that ends while
    /// another tool is open is not lost.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Some(outcome) = self.load.as_mut().and_then(Job::poll) {
            self.load = None;
            match outcome {
                Ok(rows) => {
                    self.rows = rows;
                    self.rows_gen += 1;
                    self.load_failed = false;
                }
                Err(JobPanicked) => self.load_failed = true,
            }
            if std::mem::take(&mut self.reload_again) {
                self.reload(ctx);
            }
        }
        if let WingetUi::Checking(job) = &mut self.winget {
            if let Some(outcome) = job.poll() {
                self.winget = match outcome {
                    Ok(WingetCheck::Ok(packages, note)) => {
                        self.packages = packages;
                        self.reload(ctx);
                        WingetUi::Checked(note)
                    }
                    Ok(WingetCheck::NeedsAgreement(t)) => WingetUi::Agreement(t),
                    Ok(WingetCheck::NotInstalled) => {
                        self.winget_hidden = Some(true);
                        WingetUi::Idle
                    }
                    Ok(WingetCheck::Failed(t)) => WingetUi::Failed(t),
                    Err(JobPanicked) => WingetUi::Failed(
                        l("It stopped unexpectedly.", "Berhenti tiba-tiba.").to_string(),
                    ),
                };
            }
        }
        if let Some(outcome) = self.run.as_mut().and_then(Job::poll) {
            self.run = None;
            match (outcome, self.current.clone()) {
                (Ok(done), Some(p)) => {
                    let (phase, line) = after_run(&p, &done.end, &|_| done.registered);
                    let ran = matches!(done.end, RunEnd::Done(_));
                    self.note = (!line.is_empty()).then_some(line);
                    if ran {
                        packages_after_run(&mut self.packages, &p, &done.end, done.registered);
                        self.marks_dirty = !matches!(p.method, Method::WingetUpgrade(_));
                        self.reload(ctx);
                    }
                    if phase == AppsPhase::Idle {
                        self.current = None;
                    }
                    self.enter(ctx, phase);
                }
                _ => {
                    self.note = Some(
                        l(
                            "The process stopped unexpectedly. Check the list for the app.",
                            "Proses berhenti tiba-tiba. Periksa daftar untuk aplikasi itu.",
                        )
                        .to_string(),
                    );
                    self.current = None;
                    self.set_phase(AppsPhase::Idle);
                    self.reload(ctx);
                }
            }
        }
        if let Some(outcome) = self.recheck.as_mut().and_then(Job::poll) {
            self.recheck = None;
            if let (Ok(registered), Some(key)) = (
                outcome,
                self.current
                    .as_ref()
                    .and_then(|p| p.entry.as_ref())
                    .map(|e| e.key.clone()),
            ) {
                let phase = next_after_exit(&key, &|_| registered);
                if phase != AppsPhase::Waiting(key) {
                    if let Some(id) = self.current.as_ref().and_then(|p| p.winget_id.as_ref()) {
                        drop_package(&mut self.packages, id);
                    }
                    self.marks_dirty = true;
                    self.reload(ctx);
                }
                self.enter(ctx, phase);
            }
        }
        if let Some(outcome) = self.find.as_mut().and_then(Job::poll) {
            self.find = None;
            match outcome {
                Ok(Found {
                    leftovers: Some(items),
                }) => self.set_phase(AppsPhase::Leftovers(items)),
                Ok(Found { leftovers: None }) => {
                    if let Some(e) = self.current.as_ref().and_then(|p| p.entry.as_ref()) {
                        self.set_phase(AppsPhase::Waiting(e.key.clone()));
                    }
                }
                Err(JobPanicked) => {
                    self.note = Some(
                        l(
                            "The search for leftovers stopped unexpectedly.",
                            "Pencarian sisa file berhenti tiba-tiba.",
                        )
                        .to_string(),
                    );
                    self.set_phase(AppsPhase::Leftovers(Vec::new()));
                }
            }
        }
    }

    fn accept(&mut self, ctx: &egui::Context) {
        let (phase, run) = self.take_phase().accept();
        self.set_phase(phase);
        let Some(p) = run else {
            return;
        };
        self.note = None;
        self.current = Some(p.clone());
        self.run = Some(Job::spawn(ctx.clone(), move || {
            let end = run_pending(&p);
            let registered = p.entry.as_ref().is_some_and(|e| still_registered(&e.key));
            RunDone { end, registered }
        }));
    }

    fn finish(&mut self) {
        self.find = None;
        self.recheck = None;
        self.set_phase(AppsPhase::Idle);
        self.current = None;
        self.ticked.clear();
    }
}

/// The buttons come first so a row can always be acted on: the default window shows
/// about 740 points of this table and the rest scrolls sideways.
const ACTIONS_W: f32 = 250.0;
const NAME_W: f32 = 190.0;
const WHERE_W: f32 = 130.0;
const PUBLISHER_W: f32 = 120.0;
const SIZE_W: f32 = 70.0;
const DATE_W: f32 = 80.0;
const VERSION_W: f32 = 70.0;
const UPDATE_W: f32 = 70.0;
const TABLE_W: f32 =
    ACTIONS_W + NAME_W + WHERE_W + PUBLISHER_W + SIZE_W + DATE_W + VERSION_W + UPDATE_W + 7.0 * 8.0;

fn outside_mark() -> &'static str {
    l("outside Program Files", "di luar Program Files")
}

fn header(ui: &mut egui::Ui, h: f32) {
    ui.horizontal(|ui| {
        for (w, text, right) in [
            (ACTIONS_W, "", false),
            (NAME_W, l("Name", "Nama"), false),
            (WHERE_W, l("Location", "Lokasi"), false),
            (PUBLISHER_W, l("Publisher", "Penerbit"), false),
            (SIZE_W, l("Size", "Ukuran"), true),
            (DATE_W, l("Installed", "Dipasang"), false),
            (VERSION_W, l("Version", "Versi"), false),
            (UPDATE_W, l("Update", "Pembaruan"), false),
        ] {
            cell(ui, w, h, right, |ui| {
                ui.weak(text);
            });
        }
    });
}

fn leftovers_panel(
    ui: &mut egui::Ui,
    state: &mut AppsState,
    items: &[Leftover],
    flow_busy: bool,
) -> Option<ConfirmRequest> {
    let mut request = None;
    let searching = state.find.is_some();
    ui.group(|ui| {
        ui.strong(l("Leftover files", "Sisa file"));
        if searching {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(l("Looking for leftovers...", "Mencari sisa file..."));
            });
        } else if items.is_empty() {
            ui.label(l(
                "No leftovers found.",
                "Tidak ditemukan sisa file.",
            ));
        } else {
            ui.weak(l(
                "Nothing is ticked. Ticked items go to the Recycle Bin after you confirm. \"Maybe\" means the name only looks like the app's.",
                "Tidak ada yang tercentang. Yang dicentang masuk ke Recycle Bin setelah Anda konfirmasi. \"Mungkin\" berarti namanya hanya mirip milik aplikasi.",
            ));
            egui::ScrollArea::vertical()
                .id_salt("apps-leftovers")
                .max_height(180.0)
                .show(ui, |ui| {
                    for item in items {
                        ui.horizontal(|ui| {
                            let mut on = state.ticked.contains(&item.path);
                            if ui.checkbox(&mut on, "").changed() {
                                if on {
                                    state.ticked.insert(item.path.clone());
                                } else {
                                    state.ticked.remove(&item.path);
                                }
                            }
                            ui.add(
                                egui::Label::new(item.path.display().to_string()).truncate(),
                            );
                            ui.weak(format_size(item.size));
                            if item.certainty == Certainty::Maybe {
                                ui.weak(l("maybe", "mungkin"));
                            }
                        });
                    }
                });
        }
        ui.horizontal(|ui| {
            let ticked: Vec<PathBuf> = state.ticked.iter().cloned().collect();
            let pending = request_from(items, &ticked);
            let go = ui.add_enabled(
                !flow_busy && !searching && pending.is_some(),
                egui::Button::new(l("Move ticked to Recycle Bin...", "Pindahkan yang dicentang ke Recycle Bin...")),
            );
            if go.clicked() {
                request = pending;
            }
            if ui
                .add_enabled(!flow_busy, egui::Button::new(l("Done", "Selesai")))
                .clicked()
            {
                state.finish();
            }
        });
    });
    request
}

fn confirm_dialog(ctx: &egui::Context, state: &mut AppsState) {
    let AppsPhase::Confirming(p) = state.phase().clone() else {
        return;
    };
    let mut accept = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::unique("apps-confirm")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        ui.heading(match p.method {
            Method::WingetUpgrade(_) => l("Update this app?", "Perbarui aplikasi ini?"),
            _ => l("Uninstall this app?", "Uninstall aplikasi ini?"),
        });
        ui.add_space(6.0);
        ui.label(confirm_text(&p));
        ui.add_space(4.0);
        ui.weak(l(
            "The program opens its own window and may ask for permission. Katharistís deletes nothing in this step.",
            "Program membuka jendelanya sendiri dan mungkin meminta izin. Katharistís tidak menghapus apa pun pada langkah ini.",
        ));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(l("Run", "Jalankan")).clicked() {
                accept = true;
            }
            if ui.button(l("Cancel", "Batal")).clicked() {
                cancel = true;
            }
        });
    });
    if accept {
        state.accept(ctx);
    } else if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        let phase = state.take_phase().cancel_confirm();
        state.set_phase(phase);
    }
}

fn winget_controls(ui: &mut egui::Ui, state: &mut AppsState) {
    let hidden = *state
        .winget_hidden
        .get_or_insert_with(|| !winget::is_available());
    if hidden {
        return;
    }
    let busy_run = state.phase().busy();
    match &state.winget {
        WingetUi::Checking(_) => {
            ui.spinner();
            ui.label(l("Asking winget...", "Menanyakan winget..."));
        }
        WingetUi::Agreement(text) => {
            let text = text.clone();
            let mut agree = false;
            let mut dismiss = false;
            ui.group(|ui| {
                ui.weak(l(
                    "winget asks you to agree to its source terms. Read what it says:",
                    "winget meminta Anda menyetujui ketentuan sumbernya. Baca isinya:",
                ));
                ui.label(text);
                ui.horizontal(|ui| {
                    agree = ui.button(l("Agree", "Setujui")).clicked();
                    dismiss = ui.button(l("Not now", "Nanti saja")).clicked();
                });
            });
            if agree {
                state.start_check(ui.ctx(), true);
            } else if dismiss {
                state.winget = WingetUi::Idle;
            }
        }
        _ => {
            if ui
                .add_enabled(
                    !busy_run,
                    egui::Button::new(l("Check winget", "Periksa winget")),
                )
                .clicked()
            {
                state.start_check(ui.ctx(), false);
            }
        }
    }
}

/// Draws the screen. Returns the request when the user asks to delete the ticked
/// leftovers; the caller hands it to the shared confirmation flow. `flow_busy` is a
/// delete in progress.
pub fn show(ui: &mut egui::Ui, state: &mut AppsState, flow_busy: bool) -> Option<ConfirmRequest> {
    let ctx = ui.ctx().clone();
    if !state.started {
        state.started = true;
        state.reload(&ctx);
    }
    let mut request = None;
    ui.heading(l("Apps", "Aplikasi"));
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.query)
                .hint_text(l("Search apps", "Cari aplikasi"))
                .desired_width(220.0),
        );
        let arrow = if state.smallest_first { "^" } else { "v" };
        if ui
            .button(format!("{} {arrow}", l("Size", "Ukuran")))
            .clicked()
        {
            state.smallest_first = !state.smallest_first;
        }
        winget_controls(ui, state);
    });
    if let WingetUi::Failed(t) = &state.winget {
        ui.label(format!("winget: {t}"));
    }
    if let WingetUi::Checked(note) = &state.winget {
        ui.weak(l(
            "winget was checked. Update versions are in the list.",
            "winget sudah diperiksa. Versi pembaruan ada di daftar.",
        ));
        if let Some(n) = note {
            ui.weak(format!(
                "{} {n}",
                l("Updates could not be read:", "Pembaruan tidak terbaca:")
            ));
        }
    }
    if let Some(note) = &state.note {
        ui.label(note.as_str());
    }
    ui.add_space(6.0);

    match state.phase().clone() {
        AppsPhase::Uninstalling(name) => {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!(
                        "{} {name}",
                        l("Waiting for the process of", "Menunggu proses milik")
                    ));
                });
                ui.weak(close_note());
            });
            ui.add_space(6.0);
        }
        AppsPhase::Waiting(key) => {
            let checking = state.recheck.is_some();
            ui.group(|ui| {
                ui.label(l(
                    "Uninstall not finished or cancelled.",
                    "Uninstall belum selesai atau dibatalkan.",
                ));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !checking,
                            egui::Button::new(l("Check again", "Periksa lagi")),
                        )
                        .clicked()
                    {
                        state.start_recheck(&ctx, key.clone());
                    }
                    if ui.button(l("Close", "Tutup")).clicked() {
                        state.finish();
                    }
                    if checking {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(6.0);
        }
        AppsPhase::Leftovers(items) => {
            request = leftovers_panel(ui, state, &items, flow_busy);
            ui.add_space(6.0);
        }
        AppsPhase::Idle | AppsPhase::Confirming(_) => {}
    }

    list(ui, state);
    confirm_dialog(&ctx, state);
    request
}

fn list(ui: &mut egui::Ui, state: &mut AppsState) {
    if state.load.is_some() && state.rows.is_empty() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(l(
                "Reading installed apps...",
                "Membaca aplikasi terpasang...",
            ));
        });
        return;
    }
    if state.load_failed {
        ui.label(l(
            "The list could not be read.",
            "Daftar tidak bisa dibaca.",
        ));
    }
    let key = (state.rows_gen, state.query.clone(), state.smallest_first);
    if state.order_key.as_ref() != Some(&key) {
        state.order = view_order(&state.rows, &key.1, !key.2);
        state.order_key = Some(key);
    }
    let h = ui.spacing().interact_size.y + 4.0;
    // The header's place is taken now, and it is drawn after the rows, when it is known
    // how far they scrolled sideways, so the two never part by a frame.
    let (header_rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    ui.separator();
    let busy = state.phase().busy() || state.run.is_some() || state.find.is_some();
    let mut chosen: Option<Pending> = None;
    let order = &state.order;
    let rows = &state.rows;
    let scrolled = egui::ScrollArea::both()
        .id_salt("apps-list")
        .auto_shrink([false, false])
        .show_rows(ui, h, order.len(), |ui, range| {
            for &i in &order[range] {
                let row = &rows[i];
                ui.horizontal(|ui| {
                    cell(ui, ACTIONS_W, h, false, |ui| {
                        if let Some(cmd) = &row.official {
                            if ui
                                .add_enabled(!busy, egui::Button::new(l("Uninstall", "Uninstall")))
                                .clicked()
                            {
                                chosen = Some(row.pending(Method::Uninstaller(cmd.clone())));
                            }
                        }
                        if let Some(id) = &row.winget_id {
                            if ui
                                .add_enabled(
                                    !busy,
                                    egui::Button::new(l("via winget", "lewat winget")),
                                )
                                .on_hover_text(l(
                                    "Uninstall with winget",
                                    "Uninstall dengan winget",
                                ))
                                .clicked()
                            {
                                chosen = Some(row.pending(Method::WingetUninstall(id.clone())));
                            }
                            if row.available.is_some()
                                && ui
                                    .add_enabled(!busy, egui::Button::new(l("Update", "Perbarui")))
                                    .clicked()
                            {
                                chosen = Some(row.pending(Method::WingetUpgrade(id.clone())));
                            }
                        }
                        if row.official.is_none() && row.winget_id.is_none() {
                            let text = egui::RichText::new(l(
                                "Cannot be uninstalled from here.",
                                "Tidak bisa di-uninstall dari sini.",
                            ))
                            .weak();
                            ui.add(egui::Label::new(text).truncate());
                        }
                    });
                    cell(ui, NAME_W, h, false, |ui| {
                        ui.add(egui::Label::new(row.name.as_str()).truncate());
                    });
                    cell(ui, WHERE_W, h, false, |ui| {
                        if row.outside_program_files {
                            let text = egui::RichText::new(outside_mark()).weak();
                            ui.add(egui::Label::new(text).truncate());
                        }
                    });
                    cell(ui, PUBLISHER_W, h, false, |ui| {
                        ui.add(
                            egui::Label::new(row.publisher.as_deref().unwrap_or("-")).truncate(),
                        );
                    });
                    cell(ui, SIZE_W, h, true, |ui| {
                        ui.label(if row.size > 0 {
                            format_size(row.size)
                        } else {
                            "-".to_string()
                        });
                    });
                    cell(ui, DATE_W, h, false, |ui| {
                        ui.label(date_text(row.date));
                    });
                    cell(ui, VERSION_W, h, false, |ui| {
                        ui.add(egui::Label::new(row.version.as_deref().unwrap_or("-")).truncate());
                    });
                    cell(ui, UPDATE_W, h, false, |ui| {
                        ui.add(
                            egui::Label::new(row.available.as_deref().unwrap_or("-")).truncate(),
                        );
                    });
                });
            }
        });
    let offset = egui::vec2(scrolled.state.unclamped_offset().x, 0.0);
    let mut head = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
        header_rect.min - offset,
        egui::vec2(TABLE_W, h),
    )));
    head.set_clip_rect(header_rect.intersect(ui.clip_rect()));
    header(&mut head, h);
    if let Some(p) = chosen {
        let phase = state.take_phase().ask(p);
        state.set_phase(phase);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confirm::ConfirmContext;
    use crate::views::test_support::text_rect;
    use katharistis_core::apps::leftovers::Certainty;
    use katharistis_core::delete::DeleteMode;
    use std::cell::{Cell, RefCell};

    fn app(key: &str, name: &str) -> AppEntry {
        AppEntry {
            key: key.into(),
            name: name.into(),
            publisher: Some("Pub".into()),
            version: Some("1.0".into()),
            install_date: Some((2024, 3, 9)),
            size: 1000,
            install_location: None,
            uninstall: Some(r#""C:\x\u.exe" /S"#.into()),
        }
    }

    fn pkg(name: &str, id: &str, available: Option<&str>) -> WingetPackage {
        WingetPackage {
            name: name.into(),
            id: id.into(),
            version: "1.0".into(),
            available: available.map(Into::into),
        }
    }

    fn cmd() -> UninstallCmd {
        UninstallCmd {
            file: r"C:\x\u.exe".into(),
            args: "/S".into(),
        }
    }

    fn pending(entry: Option<AppEntry>, method: Method) -> Pending {
        Pending {
            name: "Alpha".into(),
            entry,
            winget_id: None,
            method,
        }
    }

    fn left(path: &str) -> Leftover {
        Leftover {
            path: PathBuf::from(path),
            size: 5,
            certainty: Certainty::Sure,
        }
    }

    #[test]
    fn stays_waiting_while_registered() {
        assert!(matches!(
            next_after_exit("k", &|_| true),
            AppsPhase::Waiting(_)
        ));
    }

    #[test]
    fn moves_on_when_unregistered() {
        assert_eq!(
            next_after_exit("k", &|_| false),
            AppsPhase::Leftovers(Vec::new())
        );
        // The key that is looked up is the one given.
        let seen = RefCell::new(String::new());
        let _ = next_after_exit("the-key", &|k| {
            *seen.borrow_mut() = k.to_string();
            true
        });
        assert_eq!(*seen.borrow(), "the-key");
        assert_eq!(
            next_after_exit("the-key", &|_| true),
            AppsPhase::Waiting("the-key".into())
        );
    }

    #[test]
    fn exists_means_absolute_path_and_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("u.exe");
        std::fs::write(&file, b"x").unwrap();
        assert!(is_abs_file(file.to_str().unwrap()));
        assert!(!is_abs_file(dir.path().to_str().unwrap()));
        assert!(!is_abs_file(dir.path().join("none.exe").to_str().unwrap()));
        assert!(!is_abs_file("u.exe"));
        assert!(!is_abs_file(""));
    }

    #[test]
    fn confirming_needs_idle_and_only_accept_starts_it() {
        let p = pending(Some(app("k", "Alpha")), Method::Uninstaller(cmd()));
        let phase = AppsPhase::Idle.ask(p.clone());
        assert_eq!(phase, AppsPhase::Confirming(Box::new(p.clone())));
        assert!(phase.busy());
        // Nothing replaces a running uninstall or an open confirmation.
        let other = pending(None, Method::WingetUninstall("Other.Id".into()));
        assert_eq!(
            AppsPhase::Uninstalling("Alpha".into()).ask(other.clone()),
            AppsPhase::Uninstalling("Alpha".into())
        );
        assert_eq!(
            AppsPhase::Confirming(Box::new(p.clone())).ask(other),
            AppsPhase::Confirming(Box::new(p.clone()))
        );
        // Cancel goes back with nothing to run.
        assert_eq!(phase.clone().cancel_confirm(), AppsPhase::Idle);
        // Accept hands out exactly the confirmed one.
        let (next, run) = phase.accept();
        assert_eq!(next, AppsPhase::Uninstalling("Alpha".into()));
        assert_eq!(run, Some(p));
        // Accept with nothing to confirm runs nothing.
        let (next, run) = AppsPhase::Idle.accept();
        assert_eq!(next, AppsPhase::Idle);
        assert!(run.is_none());
        let (next, run) = AppsPhase::Leftovers(vec![left(r"C:\a")]).accept();
        assert!(run.is_none());
        assert_eq!(next, AppsPhase::Leftovers(vec![left(r"C:\a")]));
        assert!(!AppsPhase::Idle.busy());
    }

    #[test]
    fn a_cancelled_uninstaller_goes_back_to_idle() {
        let p = pending(Some(app("k", "Alpha")), Method::Uninstaller(cmd()));
        let (phase, note) = after_run(&p, &RunEnd::Cancelled, &|_| true);
        assert_eq!(phase, AppsPhase::Idle);
        assert!(!note.is_empty());
    }

    #[test]
    fn after_an_uninstall_the_registry_decides() {
        let p = pending(Some(app("k", "Alpha")), Method::Uninstaller(cmd()));
        let (phase, _) = after_run(&p, &RunEnd::Done(None), &|k| k == "k");
        assert_eq!(phase, AppsPhase::Waiting("k".into()));
        let (phase, _) = after_run(&p, &RunEnd::Done(None), &|_| false);
        assert_eq!(phase, AppsPhase::Leftovers(Vec::new()));
        let w = pending(
            Some(app("k", "Alpha")),
            Method::WingetUninstall("A.B".into()),
        );
        let (phase, note) = after_run(&w, &RunEnd::Done(Some(3)), &|_| true);
        assert_eq!(phase, AppsPhase::Waiting("k".into()));
        assert!(note.contains('3'));
    }

    #[test]
    fn a_winget_only_app_gets_no_leftovers_and_says_so() {
        let w = pending(None, Method::WingetUninstall("A.B".into()));
        let called = Cell::new(false);
        let (phase, note) = after_run(&w, &RunEnd::Done(Some(0)), &|_| {
            called.set(true);
            false
        });
        assert_eq!(phase, AppsPhase::Idle);
        assert!(!called.get());
        assert!(note.contains('0'));
        assert!(note.to_lowercase().contains("leftover") || note.contains("sisa"));
    }

    #[test]
    fn an_update_never_looks_for_leftovers() {
        let u = pending(Some(app("k", "Alpha")), Method::WingetUpgrade("A.B".into()));
        let (phase, note) = after_run(&u, &RunEnd::Done(Some(0)), &|_| false);
        assert_eq!(phase, AppsPhase::Idle);
        assert!(note.contains('0'));
        let (phase, note) = after_run(&u, &RunEnd::Failed("boom".into()), &|_| false);
        assert_eq!(phase, AppsPhase::Idle);
        assert!(note.contains("boom"));
    }

    #[test]
    fn winget_arguments_are_exact_and_the_uninstaller_has_none() {
        assert_eq!(
            winget_args(&Method::WingetUninstall("A.B".into())).unwrap(),
            ["uninstall", "--id", "A.B", "--exact"]
        );
        assert_eq!(
            winget_args(&Method::WingetUpgrade("A.B".into())).unwrap(),
            ["upgrade", "--id", "A.B", "--exact"]
        );
        assert!(winget_args(&Method::Uninstaller(cmd())).is_none());
    }

    #[test]
    fn the_confirmation_names_the_app_and_what_runs() {
        let a = confirm_text(&pending(
            Some(app("k", "Alpha")),
            Method::Uninstaller(cmd()),
        ));
        assert!(a.contains("Alpha") && a.contains(r"C:\x\u.exe"));
        let b = confirm_text(&pending(None, Method::WingetUninstall("A.B".into())));
        assert!(b.contains("Alpha") && b.contains("A.B"));
    }

    #[test]
    fn winget_asks_before_the_user_agrees_and_never_agrees_alone() {
        let calls = RefCell::new(Vec::new());
        let list = |a: bool| {
            calls.borrow_mut().push(("list", a));
            WingetOutcome::NeedsAgreement("agree?".into())
        };
        let up = |a: bool| {
            calls.borrow_mut().push(("up", a));
            WingetOutcome::Ok(Vec::new())
        };
        assert_eq!(
            check_winget(false, &list, &up),
            WingetCheck::NeedsAgreement("agree?".into())
        );
        assert_eq!(*calls.borrow(), [("list", false)]);
    }

    #[test]
    fn the_agree_button_passes_true_to_both_calls() {
        let calls = RefCell::new(Vec::new());
        let list = |a: bool| {
            calls.borrow_mut().push(("list", a));
            WingetOutcome::Ok(vec![pkg("Alpha", "A.B", None), pkg("Beta", "C.D", None)])
        };
        let up = |a: bool| {
            calls.borrow_mut().push(("up", a));
            WingetOutcome::Ok(vec![pkg("Alpha", "A.B", Some("2.0"))])
        };
        let WingetCheck::Ok(pkgs, note) = check_winget(true, &list, &up) else {
            panic!("expected Ok");
        };
        assert_eq!(*calls.borrow(), [("list", true), ("up", true)]);
        assert!(note.is_none());
        assert_eq!(pkgs[0].available.as_deref(), Some("2.0"));
        assert_eq!(pkgs[1].available, None);
    }

    #[test]
    fn a_missing_or_failed_winget_skips_the_second_call() {
        let up = |_: bool| -> WingetOutcome { panic!("upgrades must not run") };
        assert_eq!(
            check_winget(false, &|_| WingetOutcome::NotInstalled, &up),
            WingetCheck::NotInstalled
        );
        assert_eq!(
            check_winget(false, &|_| WingetOutcome::Failed("x".into()), &up),
            WingetCheck::Failed("x".into())
        );
    }

    #[test]
    fn a_failed_upgrade_list_keeps_the_installed_list_and_says_so() {
        let list = |_: bool| WingetOutcome::Ok(vec![pkg("Alpha", "A.B", None)]);
        let up = |_: bool| WingetOutcome::Failed("net".into());
        let WingetCheck::Ok(pkgs, note) = check_winget(false, &list, &up) else {
            panic!("expected Ok");
        };
        assert_eq!(pkgs.len(), 1);
        assert!(note.unwrap().contains("net"));
    }

    #[test]
    fn rows_offer_what_the_match_allows() {
        let rows = build_rows(
            vec![
                AppRow::Registry(app("k1", "Alpha"), Some(pkg("Alpha", "A.B", Some("2.0")))),
                // Ambiguous match: no package, so no winget uninstall.
                AppRow::Registry(app("k2", "Beta"), None),
                // No uninstall string at all.
                AppRow::Registry(
                    AppEntry {
                        uninstall: None,
                        ..app("k3", "Gamma")
                    },
                    None,
                ),
                AppRow::WingetOnly(pkg("Delta", "D.E", None)),
            ],
            &|_| true,
            &program_files(),
        );
        assert_eq!(rows[0].winget_id.as_deref(), Some("A.B"));
        assert_eq!(rows[0].available.as_deref(), Some("2.0"));
        assert_eq!(rows[0].official, Some(cmd()));
        assert!(rows[1].winget_id.is_none() && rows[1].official.is_some());
        assert!(rows[2].winget_id.is_none() && rows[2].official.is_none());
        assert!(rows[3].entry.is_none());
        assert_eq!(rows[3].winget_id.as_deref(), Some("D.E"));
        assert!(rows[3].official.is_none());
    }

    #[test]
    fn an_unquoted_path_must_be_an_existing_file_to_parse() {
        let mut e = app("k", "Alpha");
        e.uninstall = Some(r"C:\nowhere\u.exe /S".into());
        let rows = build_rows(
            vec![AppRow::Registry(e, None)],
            &|_| false,
            &program_files(),
        );
        assert!(rows[0].official.is_none());
    }

    fn row(name: &str, size: u64) -> Row {
        Row {
            name: name.into(),
            publisher: None,
            size,
            date: None,
            version: None,
            available: None,
            entry: None,
            official: None,
            winget_id: None,
            outside_program_files: false,
        }
    }

    fn program_files() -> Vec<PathBuf> {
        vec![r"C:\Program Files".into(), r"C:\Program Files (x86)".into()]
    }

    #[test]
    fn only_a_location_outside_program_files_is_marked() {
        let pf = program_files();
        let outside = |p: &str| outside_program_files(Some(Path::new(p)), &pf);
        assert!(outside(r"D:\Games\App"));
        assert!(outside(r"C:\Users\me\AppData\Local\Programs\App"));
        assert!(outside(r"C:\Program Files Extra\App"));
        assert!(!outside(r"C:\Program Files\App"));
        assert!(!outside(r"c:\program files (x86)\App\"));
        assert!(!outside(r#""C:\Program Files\App""#));
        // Nothing to go by, so nothing to say.
        assert!(!outside_program_files(None, &pf));
        assert!(!outside("not a place"));
    }

    #[test]
    fn the_row_takes_its_mark_from_the_install_location() {
        let located = |location: Option<&str>| AppEntry {
            install_location: location.map(PathBuf::from),
            ..app("k", "Alpha")
        };
        let rows = build_rows(
            vec![
                AppRow::Registry(located(Some(r"D:\Games\App")), None),
                AppRow::Registry(located(Some(r"C:\Program Files\App")), None),
                AppRow::Registry(located(None), None),
                AppRow::WingetOnly(pkg("Delta", "D.E", None)),
            ],
            &|_| true,
            &program_files(),
        );
        let marks: Vec<bool> = rows.iter().map(|r| r.outside_program_files).collect();
        assert_eq!(marks, [true, false, false, false]);
        // A mark, and the buttons stay.
        assert!(rows[0].official.is_some());
    }

    fn with_id(method: Method, entry: Option<AppEntry>) -> Pending {
        Pending {
            winget_id: Some("A.B".into()),
            ..pending(entry, method)
        }
    }

    #[test]
    fn a_winget_uninstall_that_worked_takes_the_package_out_of_the_next_merge() {
        let mut packages = vec![pkg("Alpha", "A.B", None), pkg("Beta", "C.D", None)];
        let p = with_id(Method::WingetUninstall("A.B".into()), None);
        packages_after_run(&mut packages, &p, &RunEnd::Done(Some(0)), false);
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].id, "C.D");
        let rows = build_rows(merge(Vec::new(), packages), &|_| true, &program_files());
        assert!(rows.iter().all(|r| r.name != "Alpha"));
    }

    #[test]
    fn a_failed_winget_uninstall_keeps_the_package_unless_the_key_is_gone() {
        let start = || vec![pkg("Alpha", "A.B", None)];
        let entry = Some(app("k", "Alpha"));
        let p = with_id(Method::WingetUninstall("A.B".into()), entry.clone());
        let mut packages = start();
        packages_after_run(&mut packages, &p, &RunEnd::Done(Some(3)), true);
        assert_eq!(packages.len(), 1);
        packages_after_run(&mut packages, &p, &RunEnd::Done(Some(3)), false);
        assert!(packages.is_empty());
        // With no key to look at, a failure proves nothing.
        let p = with_id(Method::WingetUninstall("A.B".into()), None);
        let mut packages = start();
        packages_after_run(&mut packages, &p, &RunEnd::Done(Some(3)), false);
        assert_eq!(packages.len(), 1);
    }

    #[test]
    fn the_official_uninstaller_drops_the_package_once_the_key_is_gone() {
        let p = with_id(Method::Uninstaller(cmd()), Some(app("k", "Alpha")));
        let mut packages = vec![pkg("Alpha", "A.B", None)];
        packages_after_run(&mut packages, &p, &RunEnd::Done(None), true);
        assert_eq!(packages.len(), 1);
        packages_after_run(&mut packages, &p, &RunEnd::Done(None), false);
        assert!(packages.is_empty());
    }

    #[test]
    fn a_run_that_never_ran_changes_nothing() {
        let p = with_id(Method::Uninstaller(cmd()), Some(app("k", "Alpha")));
        let mut packages = vec![pkg("Alpha", "A.B", None)];
        packages_after_run(&mut packages, &p, &RunEnd::Cancelled, false);
        packages_after_run(&mut packages, &p, &RunEnd::Failed("x".into()), false);
        assert_eq!(packages.len(), 1);
    }

    #[test]
    fn an_update_that_worked_stops_offering_the_update() {
        let p = with_id(Method::WingetUpgrade("A.B".into()), Some(app("k", "Alpha")));
        let mut packages = vec![pkg("Alpha", "A.B", Some("2.0"))];
        packages_after_run(&mut packages, &p, &RunEnd::Done(Some(7)), true);
        assert_eq!(packages[0].available.as_deref(), Some("2.0"));
        packages_after_run(&mut packages, &p, &RunEnd::Done(Some(0)), true);
        assert_eq!(packages[0].available, None);
        assert_eq!(packages[0].version, "2.0");
    }

    /// A list with every button a row can have, in a window `width` points wide.
    fn drawn_list(width: f32, wheel: Option<egui::Vec2>) -> egui::FullOutput {
        let ctx = egui::Context::default();
        let mut state = AppsState {
            started: true,
            rows: vec![Row {
                official: Some(cmd()),
                winget_id: Some("A.B".into()),
                available: Some("2.0".into()),
                outside_program_files: true,
                ..row("Alpha", 5)
            }],
            ..Default::default()
        };
        let frame = |events: Vec<egui::Event>, state: &mut AppsState| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 400.0),
                )),
                events,
                predicted_dt: 1.0,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| list(ui, state));
            out.textures_delta.clear();
            out
        };
        for _ in 0..3 {
            frame(Vec::new(), &mut state);
        }
        if let Some(delta) = wheel {
            frame(
                vec![
                    egui::Event::PointerMoved(egui::pos2(100.0, 100.0)),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta,
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::NONE,
                        source: egui::MouseWheelSource::Unknown,
                    },
                ],
                &mut state,
            );
        }
        for _ in 0..5 {
            frame(Vec::new(), &mut state);
        }
        frame(Vec::new(), &mut state)
    }

    #[test]
    fn every_button_is_on_screen_in_the_default_window() {
        let out = drawn_list(740.0, None);
        for button in ["Uninstall", "via winget", "Update"] {
            let at = text_rect(&out, button).unwrap_or_else(|| panic!("{button} is drawn"));
            assert!(at.max.x <= ACTIONS_W, "{button} at {at:?}");
        }
    }

    #[test]
    fn the_buttons_fit_their_column_in_indonesian_too() {
        let ctx = egui::Context::default();
        let mut width = 0.0;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let spacing = ui.spacing().clone();
            let body = egui::TextStyle::Button.resolve(ui.style());
            let labels = ["Uninstall", "lewat winget", "Perbarui"];
            for text in labels {
                let galley = ui
                    .fonts_mut(|f| f.layout_no_wrap(text.into(), body.clone(), egui::Color32::RED));
                width += galley.size().x + 2.0 * spacing.button_padding.x;
            }
            width += (labels.len() - 1) as f32 * spacing.item_spacing.x;
        });
        out.textures_delta.clear();
        assert!(width <= ACTIONS_W, "{width}");
    }

    #[test]
    fn the_header_stays_over_its_column_when_the_rows_scroll_sideways() {
        for wheel in [None, Some(egui::vec2(-120.0, 0.0))] {
            let out = drawn_list(740.0, wheel);
            let head = text_rect(&out, "Name").expect("the header is drawn");
            let cell = text_rect(&out, "Alpha").expect("the row is drawn");
            assert!((head.min.x - cell.min.x).abs() < 0.5, "{head:?} {cell:?}");
            let start = 250.0 + 8.0;
            if wheel.is_some() {
                assert!(cell.min.x < start, "it scrolled: {cell:?}");
            } else {
                assert!((cell.min.x - start).abs() < 0.5, "{cell:?}");
            }
        }
    }

    #[test]
    fn rows_filter_by_query_and_sort_by_size() {
        let rows = vec![row("Zed", 5), row("Alpha", 50), row("alphabet", 20)];
        assert_eq!(view_order(&rows, "", true), [1, 2, 0]);
        assert_eq!(view_order(&rows, "", false), [0, 2, 1]);
        assert_eq!(view_order(&rows, "  ALPHA ", true), [1, 2]);
        assert!(view_order(&rows, "nothing", true).is_empty());
    }

    fn roots() -> LeftoverRoots {
        LeftoverRoots {
            roaming: PathBuf::from(r"C:\r"),
            local: PathBuf::from(r"C:\l"),
            program_data: PathBuf::from(r"C:\p"),
        }
    }

    #[test]
    fn leftovers_search_leaves_the_app_out_of_the_others() {
        let target = app("k", "Alpha");
        let installed = vec![app("k", "Alpha"), app("j", "Beta")];
        let seen = RefCell::new(Vec::new());
        let found = find_after(&target, installed, &roots(), &|_| false, &|a, _, others| {
            assert_eq!(a.key, "k");
            *seen.borrow_mut() = others.iter().map(|o| o.key.clone()).collect::<Vec<_>>();
            vec![left(r"C:\l\Alpha")]
        });
        assert_eq!(*seen.borrow(), ["j"]);
        assert_eq!(found, Some(vec![left(r"C:\l\Alpha")]));
    }

    #[test]
    fn no_leftovers_while_the_app_is_still_registered() {
        let target = app("k", "Alpha");
        let found = find_after(&target, Vec::new(), &roots(), &|_| true, &|_, _, _| {
            panic!("must not search while registered")
        });
        assert!(found.is_none());
    }

    #[test]
    fn nothing_is_ticked_at_first_and_only_ticks_are_deleted_to_the_bin() {
        let items = vec![left(r"C:\a"), left(r"C:\b")];
        assert!(request_from(&items, &[]).is_none());
        let req = request_from(&items, &[PathBuf::from(r"C:\b")]).unwrap();
        assert_eq!(req.items.len(), 1);
        assert_eq!(req.items[0].path, PathBuf::from(r"C:\b"));
        assert_eq!(req.mode, DeleteMode::RecycleBin);
        assert_eq!(req.context, ConfirmContext::Leftovers);
        assert!(req.empty_bin.is_none() && req.guards.is_empty());
        // A tick for something not in the list is not a target.
        assert!(request_from(&items, &[PathBuf::from(r"C:\zzz")]).is_none());
    }

    #[test]
    fn dates_read_year_month_day() {
        assert_eq!(date_text(Some((2024, 3, 9))), "2024-03-09");
        assert_eq!(date_text(None), "-");
    }
}
