//! egui window. Verify / install / winetricks run on one worker thread; it streams log lines over a channel.
//! `easy.rs` draws the library, game pages and the settings view; this file holds the state, the jobs and
//! the per-game settings content (`detail`).
mod easy;
mod pad;

use crate::{check, gb, free_space, ProtonRelease, Report, install, run, settings_file, steam, targets, update::{self, Update}, which_path, write_launch_script, Game, Kind, Library, Settings, Status};
use anyhow::Result;
use eframe::egui::{self, RichText, Ui};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{channel, Receiver, Sender},
    time::{Duration, Instant},
};

enum Ev {
    Log(String),
    Status(PathBuf, Status),
    /// An update finished: game dir, new version, history line.
    Applied(PathBuf, String, String),
    /// The newest update was undone: game dir, the version it is at again, the version it was undone from.
    Undone(PathBuf, String, String),
    /// Bytes unpacked / bytes needed.
    Progress(u64, u64),
    /// New artwork for this game's dir: reload just its images.
    ArtReady(PathBuf),
    /// A message for one game's page (or the library when None); errors in red.
    Note(Option<PathBuf>, String, bool),
    /// What the running job is doing now.
    Step(String),
    /// SteamGridDB knows this game's Steam store id.
    StoreId(PathBuf, u32),
    /// ProtonDB rating (tier, reports) for a Steam store id, or why there is none.
    ProtonDb(u32, Rating),
    /// Downloadable Proton releases from GitHub, or why they could not be listed.
    ProtonList(Result<Vec<ProtonRelease>, String>),
    /// A Proton build was installed: list them again.
    ProtonsChanged,
    /// A game stopped right away: its dir, why, and the fix to offer.
    StartFailed(PathBuf, String, Option<crate::Fix>),
    /// A game was played: its dir, when it started (Unix time), for how many seconds.
    Played(PathBuf, u64, u64),
    /// The launch watcher gave up without a game to time (could not start, Steam never showed it).
    NotPlaying(PathBuf),
    /// A game was deleted: drop it from the library (by its dir).
    Removed(PathBuf),
    /// A game's folder was moved: old dir, new dir.
    Moved(PathBuf, PathBuf),
    /// After an install: the type, game file, emulator and launch options found in what was installed.
    SetUp(PathBuf, Kind, Option<PathBuf>, Option<PathBuf>, String),
    /// A game's disk use was measured (see `App::disk`).
    DiskUse(PathBuf, [u64; 4]),
    /// One game's sizes for the Disk usage page: game, prefix, repack.
    DiskRow(PathBuf, [u64; 3]),
    /// Updates found for a game (by its dir): the chain to apply, or why the found ones do not chain.
    UpdatesFound(PathBuf, Result<Vec<Update>, String>),
    /// SteamGridDB search results for a game's artwork picker.
    SgdbHits(PathBuf, Result<Vec<steam::SgdbHit>, String>),
    /// The watch folder holds repacks that finished downloading.
    Watched(Vec<PathBuf>),
    /// "Import a folder" scanned it: what is not in the library yet.
    Scanned(Vec<crate::Found>),
    /// A repack folder was deleted (off the UI thread): the game, where to say so (None: the library), how it went.
    RepackGone(PathBuf, Option<PathBuf>, Result<usize, String>),
    /// The job finished: whether it worked.
    Done(bool),
}

/// A desktop notification, when notify-send is installed.
fn desktop_notify(text: &str) {
    if which_path("notify-send").is_some() {
        let _ = Command::new("notify-send").args(["-a", "repackd", "-i", "applications-games", "repackd", text]).spawn();
    }
}

/// A ProtonDB rating (tier, number of reports), or why there is none.
type Rating = Result<(String, u64), String>;

/// The message shown on one game's page (or in the library when `game` is None) until the user moves on.
#[derive(Clone)]
struct Note {
    game: Option<PathBuf>,
    text: String,
    error: bool,
}

/// Job output to the window: log lines and unpack progress, each waking the UI.
struct UiReport {
    tx: Sender<Ev>,
    ctx: egui::Context,
    /// Set by the Cancel button.
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Jobs wake the window at most this often: Wine prints thousands of lines, one redraw per line would spin
/// the UI at full frame rate (also behind a game) for the whole job.
const JOB_REPAINT: Duration = Duration::from_millis(250);

impl Report for UiReport {
    fn log(&self, line: String) {
        let _ = self.tx.send(Ev::Log(line));
        self.ctx.request_repaint_after(JOB_REPAINT);
    }
    fn progress(&self, done: u64, total: u64) {
        let _ = self.tx.send(Ev::Progress(done, total));
        self.ctx.request_repaint_after(JOB_REPAINT);
    }
    fn step(&self, what: &str) {
        let _ = self.tx.send(Ev::Step(what.to_string()));
        self.ctx.request_repaint_after(JOB_REPAINT);
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A grid row's label, centred on the row's controls (egui grids top-align their cells).
fn row_label(ui: &mut Ui, text: &str) {
    // a fixed-width column (not the text's width), so rows outside grids line up with the grid rows too
    let (rect, _) = ui.allocate_exact_size(egui::vec2(170.0, ui.spacing().interact_size.y.max(34.0)), egui::Sense::hover());
    let font = egui::TextStyle::Body.resolve(ui.style());
    ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER, text, font, ui.visuals().text_color());
}

enum Job {
    Verify,
    Install { verify: bool, unattended: bool },
    /// Imported game: create its prefix with the runtimes it ships.
    Import,
    BackupSaves,
    RestoreSaves(PathBuf),
    Winetricks(Vec<String>),
    /// A mod installer, trainer or patcher, run with the game's Proton in its prefix.
    RunExe(PathBuf),
    Updates { ups: Vec<Update>, verify: bool, backup: bool },
    /// Put back the files of the newest backed-up update.
    UndoUpdate,
    /// Delete the game (saves backed up first), then drop it from the library.
    Delete,
    /// Move the game's folder into this folder.
    Move { to: PathBuf },
}

pub struct App {
    lib: Library,
    sel: Option<usize>,
    log: Vec<String>,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    busy: bool,
    /// Prefix of the running install/update/winetricks job, for Cancel.
    running: Option<PathBuf>,
    progress: Option<(u64, u64)>,
    /// umu-run is required for every Wine job; winetricks only for its button.
    no_umu: bool,
    /// Game dirs whose repack is being deleted in the background.
    deleting_repack: std::collections::HashSet<PathBuf>,
    no_winetricks: bool,
    verbs: String,
    /// Launchable files of the selected game; `None` until the picker first needs them (big dirs are slow to walk).
    exes: Option<Vec<PathBuf>>,
    settings: Settings,
    /// Update chain waiting for "Apply" (already ordered), or why the picked folders do not chain.
    pending: Result<Vec<Update>, String>,
    steam_users: Vec<PathBuf>,
    steam_pick: usize,
    confirm_delete: bool,
    /// Image URIs to drop from egui's cache next frame (new artwork on disk).
    forget: Vec<String>,
    /// Artwork paths per (game dir, kind), so tiles don't stat files every frame.
    art: HashMap<(PathBuf, &'static str), Option<PathBuf>>,
    /// The game page that is open, the focused tile, and whether keys moved the focus (scroll to it).
    easy_open: Option<usize>,
    easy_focus: usize,
    scroll_focus: bool,
    note: Option<Note>,
    /// Which game the running job belongs to, and what it is doing ("Installing", "Checking files", ...).
    job_game: Option<PathBuf>,
    job_label: &'static str,
    /// A game just started: its dir and when, to show "Starting…" and stop double launches.
    launching: Option<(PathBuf, Instant)>,
    /// Game dirs whose launch watcher is running (the game page shows "Stop game"; no /proc scan per frame).
    playing: std::collections::HashSet<PathBuf>,
    /// The store-id field as typed (a pasted store URL is fine), and a save backup waiting for "Yes, restore".
    store_text: String,
    confirm_restore: Option<PathBuf>,
    styled: bool,
    /// The game page shows that game's full settings (gear button) instead of the big Play page.
    easy_gear: bool,
    /// "Show log" on a failed install: the settings page scrolls to its Log section once.
    scroll_to_log: bool,
    /// "SET UP EMULATOR" opened the global Settings: scroll to its Emulators section once.
    scroll_to_emulators: bool,
    /// The global Settings page is open (top bar), and its SteamGridDB key field as typed.
    global: bool,
    /// The Disk usage page (inside the global Settings), its measured rows by game dir, and the repack delete waiting for "Yes".
    disk_page: bool,
    disk_rows: HashMap<PathBuf, [u64; 3]>,
    confirm_disk: Option<PathBuf>,
    /// An installed Proton (its path) waiting for "Yes, delete".
    confirm_proton: Option<String>,
    key_text: String,
    /// Downloadable Proton releases on GitHub (None: not loaded yet).
    releases: Option<Result<Vec<ProtonRelease>, String>>,
    /// The running job's current step, and the first progress sample of that step (for speed and time left).
    job_step: String,
    rate: Option<(Instant, u64)>,
    /// The artwork search box of the open game's settings, and its results (for that game's dir).
    art_query: String,
    sgdb_hits: Option<(PathBuf, Result<Vec<steam::SgdbHit>, String>)>,
    /// ProtonDB ratings by Steam store id (None while being fetched).
    protondb: HashMap<u32, Option<Rating>>,
    /// Proton builds found at start, for the dropdown.
    protons: Vec<(String, String)>,
    /// Updates found lying next to each game (by dir), see `update::find_for`.
    found: HashMap<PathBuf, Result<Vec<Update>, String>>,
    /// The fix offered for the game that stopped right away (its dir).
    fix: Option<(PathBuf, crate::Fix)>,
    /// Jobs waiting for the running one (game dir, job), and when the running one started.
    queue: std::collections::VecDeque<(PathBuf, Job)>,
    job_started: Instant,
    /// The window's context, for starting queued jobs from `poll`.
    ctx: Option<egui::Context>,
    /// The library's search text.
    search: String,
    /// "Delete the repack" waiting for its confirmation, and the open game's disk use (dir; game, prefix,
    /// repack, update backups) once measured.
    confirm_repack: bool,
    /// "Move to another folder" waiting for its confirmation: the picked folder.
    confirm_move: Option<PathBuf>,
    /// The "Move…" drive list is open, with the drives read when it opened (not per frame).
    move_pick: bool,
    move_drives: Vec<crate::Drive>,
    disk: Option<(PathBuf, [u64; 4])>,
    /// Gamepad input, started with the first frame (None: not started, or no gamepad support).
    pad: Option<pad::Gamepad>,
    /// The game name as it was when its field got the cursor (to notice a rename when it loses it).
    name_was: Option<String>,
    /// A game whose tile should get the library's focus next time it is drawn (by game index).
    want_game: Option<usize>,
    /// File checks for the open game's pages, made once instead of every frame (see `side`).
    side: Option<(PathBuf, Side)>,
    /// The CJK fallback font is loaded (only once a game name needs it: it is a 17 MB file).
    cjk: bool,
    /// Images were loading last frame (their caches are dropped once they are on the GPU).
    images_pending: bool,
    /// The running job's Cancel flag (a new one per job).
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// What the watch thread reads each poll: the folder, and the paths it must skip (in the library, dismissed).
    watch: std::sync::Arc<std::sync::Mutex<(Option<PathBuf>, Vec<PathBuf>)>>,
    /// Finished repacks found in the watch folder, waiting for Add / Dismiss.
    watching: Vec<PathBuf>,
    /// "Import a folder": what the scan found with its checkbox, and whether the scan is still running.
    scan: Option<Vec<(crate::Found, bool)>>,
    scanning: bool,
}

/// What the open game's pages show from disk: its newest update backup (from, to), its save backups,
/// whether it has a checksum list, and what its Switch emulator still needs.
#[derive(Default)]
struct Side {
    undo: Option<(String, String)>,
    saves: Vec<PathBuf>,
    checksum: bool,
    emulator: Option<(String, PathBuf)>,
    /// Whether its Wine prefix exists, whether an emulator is set up, and whether a last run's output exists.
    has_prefix: bool,
    has_emulator: bool,
    last_run: bool,
    /// Bytes in its shader cache folder.
    shader_cache: u64,
}

impl App {
    pub fn new(lib: Library) -> Self {
        let (tx, rx) = channel();
        let mut app = Self {
            no_umu: which_path("umu-run").is_none(),
            deleting_repack: Default::default(),
            no_winetricks: which_path("winetricks").is_none(),
            sel: (!lib.games.is_empty()).then_some(0),
            lib,
            log: vec![],
            tx,
            rx,
            busy: false,
            running: None,
            progress: None,
            verbs: String::new(),
            exes: None,
            settings: Settings::load(&settings_file()),
            pending: Ok(vec![]),
            steam_users: steam::users(),
            steam_pick: 0,
            confirm_delete: false,
            forget: vec![],
            art: HashMap::new(),
            easy_open: None,
            easy_focus: 0,
            scroll_focus: false,
            note: None,
            job_game: None,
            job_label: "",
            launching: None,
            playing: Default::default(),
            store_text: String::new(),
            confirm_restore: None,
            styled: false,
            easy_gear: false,
            scroll_to_log: false,
            scroll_to_emulators: false,
            global: false,
            disk_page: false,
            disk_rows: HashMap::new(),
            confirm_disk: None,
            confirm_proton: None,
            key_text: String::new(),
            releases: None,
            job_step: String::new(),
            rate: None,
            protondb: HashMap::new(),
            art_query: String::new(),
            sgdb_hits: None,
            protons: crate::protons(),
            found: HashMap::new(),
            fix: None,
            queue: Default::default(),
            job_started: Instant::now(),
            ctx: None,
            search: String::new(),
            confirm_repack: false,
            confirm_move: None,
            move_pick: false,
            move_drives: vec![],
            disk: None,
            pad: None,
            name_was: None,
            want_game: None,
            side: None,
            cancel: Default::default(),
            watch: Default::default(),
            watching: vec![],
            scan: None,
            scanning: false,
            cjk: false,
            images_pending: false,
        };
        // games left "Installing" by a crash did not finish; their installer may still be running
        for g in &mut app.lib.games {
            if g.status == Status::Installing {
                g.status = Status::Failed;
                crate::kill_prefix(&g.prefix());
            }
        }
        app.key_text = app.settings.sgdb_key.clone();
        app.sync_watch();
        // installed games nobody configured yet: type (Switch/PS4 by their files), game file, emulator
        let mut changed = false;
        for g in app.lib.games.iter_mut().filter(|g| g.status == Status::Installed) {
            changed |= g.auto_setup(&app.settings);
        }
        if changed {
            app.save();
        }
        // scripts from older versions miss later launch fixes (e.g. crack DLL overrides): regenerate them all
        // quietly: a game that can't have one yet (no emulator) says so when Play is pressed
        for g in app.lib.games.iter().filter(|g| g.status == Status::Installed && g.exe.is_some()) {
            let _ = write_launch_script(g, &app.settings);
        }
        // artwork from before it was scaled to its display size: once, off the UI thread
        let (games, tx) = (app.lib.games.clone(), app.tx.clone());
        std::thread::spawn(move || {
            for g in games.iter().filter(|g| g.shrink_art()) {
                let _ = tx.send(Ev::ArtReady(g.dir.clone()));
            }
        });
        app.fetch_missing_covers();
        app.find_updates(app.lib.games.clone());
        let (root, games) = (app.lib.root.clone(), app.lib.games.clone());
        std::thread::spawn(move || crate::clear_trash(&root, &games));
        app.select(app.sel);
        app
    }

    /// Look for updates of these games off the UI thread (folder listings only).
    fn find_updates(&self, games: Vec<Game>) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for g in games.into_iter().filter(|g| g.status == Status::Installed) {
                let _ = tx.send(Ev::UpdatesFound(g.dir.clone(), update::find_for(&g)));
            }
        });
    }

    /// A log line from something the user did; an error also shows on the open game's page.
    fn say(&mut self, s: impl Into<String>) {
        let game = self.easy_open.and_then(|i| self.lib.games.get(i)).map(|g| g.dir.clone());
        self.log_line(s.into(), game);
    }

    /// A log line; an error also goes on `game`'s page.
    fn log_line(&mut self, s: String, game: Option<PathBuf>) {
        if let Some(text) = s.strip_prefix("error: ") {
            self.note = Some(Note { game, text: text.to_string(), error: true });
            self.fix = None; // belongs to the note it came with
        }
        self.log.push(s);
    }

    fn notify(&mut self, game: Option<PathBuf>, text: impl Into<String>, error: bool) {
        let text = text.into();
        self.log.push(if error { format!("error: {text}") } else { text.clone() });
        self.note = Some(Note { game, text, error });
        self.fix = None; // belongs to the note it came with; StartFailed sets it again
    }

    /// New artwork files in `dir`: drop cached paths and images so they show.
    fn art_changed(&mut self, dir: &Path) {
        self.art.retain(|(d, _), _| d != dir);
        for which in ["cover", "wide", "hero", "logo"] {
            for ext in crate::IMAGE_EXTS {
                self.forget.push(format!("file://{}", dir.join(format!("{which}.{ext}")).display()));
            }
        }
    }

    /// The open game's file checks, made on first use and again after anything that changes them.
    fn side(&mut self, g: &Game) -> &Side {
        if self.side.as_ref().is_none_or(|(d, _)| *d != g.dir) {
            let s = Side {
                undo: update::last_backup(g).map(|(b, _)| (b.from, b.to)),
                saves: crate::backups(g),
                checksum: crate::checksum_file(g).is_some(),
                emulator: crate::emulator_setup_problem(g, &self.settings),
                has_prefix: g.prefix().join("drive_c").is_dir(),
                has_emulator: g.effective_emulator(&self.settings).is_some(),
                last_run: g.dir.join("last-run.log").is_file(),
                shader_cache: crate::dir_size(&g.dir.join("shadercache")),
            };
            self.side = Some((g.dir.clone(), s));
        }
        &self.side.as_ref().expect("just set").1
    }

    /// A job for this game waits in the queue.
    fn queued(&self, g: &Game) -> bool {
        self.queue.iter().any(|(d, _)| *d == g.dir)
    }

    /// Is a job running for this game (install, check, update, ...)?
    fn busy_with(&self, g: &Game) -> bool {
        (self.busy && self.job_game.as_ref() == Some(&g.dir)) || self.deleting_repack.contains(&g.dir)
    }

    /// The game's artwork file, looked up once and then cached.
    fn art_of(&mut self, i: usize, which: &'static str) -> Option<PathBuf> {
        let g = &self.lib.games[i];
        self.art.entry((g.dir.clone(), which)).or_insert_with(|| g.art(which)).clone()
    }

    fn save(&mut self) {
        if let Err(e) = self.lib.save() {
            self.say(format!("error: save library: {e:#}"));
        }
        self.sync_watch(); // the library changed: what counts as new did too
    }

    /// Hand the watch thread the folder and the skip list (here, not per frame: canonicalizing touches the disk).
    fn sync_watch(&self) {
        let mut skip = crate::known_dirs(&self.lib);
        skip.extend(self.settings.watch_dismissed.iter().map(|p| std::fs::canonicalize(p).unwrap_or(p.clone())));
        if let Ok(mut w) = self.watch.lock() {
            *w = (self.settings.watch_dir.clone(), skip);
        }
    }

    /// Every 60 s, while a watch folder is set: tell the UI about repacks whose size stopped changing.
    fn start_watch(&self, ctx: egui::Context) {
        let (shared, tx) = (self.watch.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let (mut last, mut told): (HashMap<PathBuf, u64>, Vec<PathBuf>) = Default::default();
            loop {
                std::thread::sleep(Duration::from_secs(60));
                let (dir, skip) = shared.lock().map(|w| w.clone()).unwrap_or_default();
                let Some(dir) = dir else { continue };
                let mut done = vec![];
                for f in crate::scan_known(&dir, &skip).into_iter().filter(|f| f.repack && !told.contains(&f.dir)) {
                    match crate::download_size(&f.dir) {
                        Some(n) if last.insert(f.dir.clone(), n) == Some(n) => done.push(f.dir), // same size twice in a row
                        Some(_) => {}
                        None => _ = last.remove(&f.dir), // a client is still writing it
                    }
                }
                if !done.is_empty() {
                    told.extend(done.clone());
                    let _ = tx.send(Ev::Watched(done));
                    ctx.request_repaint();
                }
            }
        });
    }

    /// Delete a game's repack folder off the UI thread (a 100 GB delete takes a while).
    fn delete_repack_bg(&mut self, g: Game, note: Option<PathBuf>) {
        self.deleting_repack.insert(g.dir.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = crate::delete_repack(&g).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Ev::RepackGone(g.dir, note, r));
        });
    }

    fn select(&mut self, i: Option<usize>) {
        self.sel = i;
        self.side = None;
        self.confirm_delete = false;
        self.confirm_restore = None;
        self.confirm_repack = false;
        self.confirm_move = None;
        self.move_pick = false;
        self.pending = Ok(vec![]);
        self.exes = None;
        self.store_text = i.and_then(|i| self.lib.games.get(i)).and_then(|g| g.store_id).map(|n| n.to_string()).unwrap_or_default();
        self.art_query = i.and_then(|i| self.lib.games.get(i)).map(|g| steam::art_term(&g.name)).unwrap_or_default();
        self.sgdb_hits = None;
    }

    /// Leaving a game's settings: keep whatever field still had the cursor (its lost_focus never comes).
    fn leave_settings(&mut self, i: usize) {
        self.side = None; // the emulator or files may have changed in there
        self.save();
        self.write_script(i);
        self.easy_gear = false;
        self.confirm_delete = false;
        self.confirm_restore = None;
        self.confirm_move = None;
        self.move_pick = false;
    }

    fn poll(&mut self) {
        // the app stays open for days and Wine is chatty: keep the newest 20 000 lines
        if self.log.len() > 20_000 {
            self.log.drain(..self.log.len() - 20_000);
        }
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Log(s) => self.log_line(s, self.job_game.clone()), // from a job: its game's page
                Ev::Note(game, text, error) => self.notify(game, text, error),
                Ev::ArtReady(dir) => self.art_changed(&dir),
                Ev::Watched(dirs) => {
                    for d in dirs {
                        if self.watching.contains(&d) {
                            continue;
                        }
                        let name = d.file_name().unwrap_or_default().to_string_lossy();
                        desktop_notify(&format!("New repack found: {name}"));
                        self.watching.push(d);
                    }
                }
                Ev::Scanned(found) => {
                    self.scanning = false;
                    if found.is_empty() {
                        self.notify(None, "Nothing new found in that folder.", false);
                    } else {
                        self.scan = Some(found.into_iter().map(|f| (f, true)).collect());
                    }
                }
                Ev::RepackGone(dir, note, r) => {
                    self.deleting_repack.remove(&dir);
                    self.disk = None;
                    self.disk_rows.remove(&dir);
                    if self.disk_page && let Some(g) = self.lib.games.iter().find(|g| g.dir == dir).cloned() {
                        self.measure_disk(vec![g]);
                    }
                    match r {
                        Ok(0) => self.notify(note, "The repack folder was deleted.", false),
                        Ok(left) => self.notify(note, format!("The repack's files were deleted; {left} other file(s) in its folder were kept."), false),
                        Err(e) => self.notify(note, e, true),
                    }
                }
                Ev::SgdbHits(dir, r) => self.sgdb_hits = Some((dir, r)),
                Ev::UpdatesFound(dir, r) => _ = self.found.insert(dir, r),
                Ev::DiskUse(dir, sizes) => self.disk = Some((dir, sizes)),
                Ev::DiskRow(dir, sizes) => _ = self.disk_rows.insert(dir, sizes),
                Ev::SetUp(dir, kind, exe, emu, args) => {
                    if let Some(g) = self.lib.games.iter_mut().find(|g| g.dir == dir) {
                        (g.kind, g.exe, g.emulator, g.args) = (kind, exe, emu, args);
                    }
                }
                Ev::Removed(dir) => {
                    let Some(i) = self.lib.games.iter().position(|g| g.dir == dir) else { continue };
                    let g = self.lib.games.remove(i);
                    self.save();
                    self.art.retain(|(d, _), _| *d != g.dir);
                    self.queue.retain(|(d, _)| *d != g.dir);
                    self.notify(None, format!("{} was removed.", g.name), false);
                    if self.easy_open == Some(i) {
                        (self.easy_open, self.easy_gear) = (None, false); // that game's page is gone
                    } else if let Some(o) = self.easy_open.filter(|&o| o > i) {
                        self.easy_open = Some(o - 1); // the open page keeps its game
                    }
                    self.select(self.easy_open);
                    self.want_game = Some(i.saturating_sub(1));
                }
                Ev::Moved(old, new) => {
                    let Some(i) = self.lib.games.iter().position(|g| g.dir == old) else { continue };
                    let old_script = self.lib.games[i].launch_script();
                    self.lib.games[i].moved_to(&new);
                    self.art_changed(&old);
                    for (d, _) in self.queue.iter_mut().filter(|(d, _)| *d == old) {
                        *d = new.clone();
                    }
                    if self.job_game.as_ref() == Some(&old) {
                        self.job_game = Some(new.clone()); // Done looks the game up by it
                    }
                    (self.side, self.disk, self.exes) = (None, None, None);
                    self.found.remove(&old);
                    self.disk_rows.remove(&old);
                    self.save();
                    self.write_script(i); // also rewrites the app-menu entry (same file name)
                    let g = self.lib.games[i].clone();
                    if g.prefix().join("dosdevices").is_dir() && let Err(e) = g.map_game_drive(&|s: String| _ = self.tx.send(Ev::Log(s))) {
                        self.say(format!("error: drive R: {e:#}"));
                    }
                    let mut text = format!("{} was moved to {}.", g.name, new.display());
                    if let Some(vdf) = &g.steam {
                        // the entry is edited in place: its appid, artwork and play time stay
                        let r = if steam::running() {
                            Err(anyhow::anyhow!("Steam is running"))
                        } else {
                            steam::retarget(vdf, &old_script, &g.launch_script(), &g.dir)
                        };
                        match r {
                            Ok(true) => {}
                            Ok(false) => text += " Its Steam shortcut was not found: add the game to Steam again.",
                            Err(e) => text += &format!(" Its Steam shortcut was not updated ({e:#}): close Steam, remove the game from Steam in its settings and add it again."),
                        }
                    }
                    self.notify(Some(new), text, false);
                }
                Ev::Status(dir, st) => {
                    if let Some(g) = self.lib.games.iter_mut().find(|g| g.dir == dir) {
                        g.status = st;
                        if st == Status::Installed {
                            let _ = update::delete_backups(g); // a fresh install: earlier update backups would undo it wrongly
                            if !g.applied.is_empty() {
                                // reinstall over updates: the files are at the repack's version again (a typed-in version with no updates stays)
                                g.version = crate::repack_version(&g.repack_dir);
                                g.applied.clear();
                            }
                        }
                    }
                    self.save();
                    self.refresh_script(&dir);
                    // the open game's state is only reset when it is the one that finished
                    if st == Status::Installed && self.sel.and_then(|i| self.lib.games.get(i)).is_some_and(|g| g.dir == dir) {
                        self.select(self.sel);
                    }
                }
                Ev::Undone(dir, from, to) => {
                    if let Some(g) = self.lib.games.iter_mut().find(|g| g.dir == dir) {
                        // a failed update's undo leaves the history alone: it never got an entry
                        if update::vcmp(&g.version, &to) == std::cmp::Ordering::Equal {
                            g.applied.pop();
                        }
                        g.version = from;
                    }
                    self.save();
                    self.refresh_script(&dir); // a crack DLL the update added is gone again
                    self.notify(Some(dir), "The update was undone.", false);
                }
                Ev::Applied(dir, v, line) => {
                    if let Some(g) = self.lib.games.iter_mut().find(|g| g.dir == dir) {
                        g.version = v;
                        g.applied.push(line);
                    }
                    self.save();
                    self.refresh_script(&dir); // the update may have added a crack DLL that needs an override
                }
                Ev::Progress(done, total) => {
                    if self.rate.is_none_or(|(_, first)| done < first) {
                        self.rate = Some((Instant::now(), done)); // first sample of this step
                    }
                    self.progress = Some((done, total));
                }
                Ev::Step(s) => {
                    self.log.push(format!("-- {s}"));
                    self.job_step = s;
                    (self.progress, self.rate) = (None, None);
                }
                Ev::StoreId(dir, id) => {
                    if let Some(g) = self.lib.games.iter_mut().find(|g| g.dir == dir && g.store_id.is_none()) {
                        g.store_id = Some(id);
                        self.save();
                    }
                }
                Ev::ProtonDb(id, r) => _ = self.protondb.insert(id, Some(r)),
                Ev::ProtonList(r) => self.releases = Some(r),
                Ev::ProtonsChanged => self.protons = crate::protons(),
                Ev::Done(ok) => {
                    let game = self.job_game.as_ref().and_then(|d| self.lib.games.iter().find(|g| g.dir == *d)).cloned();
                    // an update applied (or a game installed): what is left to update changed
                    if let Some(g) = &game {
                        self.find_updates(vec![g.clone()]);
                    }
                    // a long job finished while the user may be elsewhere
                    if self.job_started.elapsed() > Duration::from_secs(30) && which_path("notify-send").is_some() {
                        let what = game.map(|g| format!("{}: {}", self.job_label, g.name)).unwrap_or(self.job_label.to_string());
                        let text = if ok { format!("{what}: done") } else { format!("{what}: failed, see repackd") };
                        desktop_notify(&text);
                    }
                    self.busy = false;
                    self.running = None;
                    self.progress = None;
                    self.job_game = None;
                    (self.side, self.disk) = (None, None); // the job may have changed backups, saves, files
                    // the next queued job, for a game that is still in the library
                    while let Some((dir, job)) = self.queue.pop_front() {
                        if self.deleting_repack.contains(&dir) && matches!(job, Job::Install { .. } | Job::Verify) {
                            self.log.push("skipped a queued job: the repack is being deleted".into());
                            continue;
                        }
                        if let (Some(g), Some(ctx)) = (self.lib.games.iter().find(|g| g.dir == dir).cloned(), self.ctx.clone()) {
                            self.spawn(&ctx, g, job);
                            break;
                        }
                    }
                }
                Ev::StartFailed(dir, text, fix) => {
                    self.notify(Some(dir.clone()), text, true);
                    self.fix = fix.map(|f| (dir, f));
                }
                Ev::NotPlaying(dir) => _ = self.playing.remove(&dir),
                Ev::Played(dir, at, secs) => {
                    self.playing.remove(&dir);
                    let Some(g) = self.lib.games.iter_mut().find(|g| g.dir == dir) else { continue };
                    g.playtime += secs;
                    g.last_played = Some(at);
                    self.side = None; // a save backup follows
                    let g = g.clone();
                    self.save();
                    if self.settings.backup_saves_after_play && secs >= 60 && g.prefix().join("drive_c").is_dir() {
                        std::thread::spawn(move || {
                            if crate::backup_saves(&g, true).is_ok() {
                                crate::prune_backups(&g, 5);
                            }
                        });
                    }
                }
            }
        }
    }

    /// Measure games for the Disk usage page on a background thread, one event per game.
    fn measure_disk(&self, games: Vec<Game>) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for g in games {
                let _ = tx.send(Ev::DiskRow(g.dir.clone(), crate::disk_sizes(&g)));
            }
        });
    }

    /// Run a job that belongs to no single game (a Proton download), with the same progress and log.
    fn spawn_global(&mut self, ctx: &egui::Context, label: &'static str, f: impl FnOnce(&UiReport) -> Result<()> + Send + 'static) {
        (self.busy, self.job_game, self.running, self.progress, self.rate) = (true, None, None, None, None);
        self.job_started = Instant::now();
        self.job_label = label;
        self.job_step = String::new();
        self.cancel = Default::default();
        let (tx, ctx, cancel) = (self.tx.clone(), ctx.clone(), self.cancel.clone());
        std::thread::spawn(move || {
            let report = UiReport { tx: tx.clone(), ctx: ctx.clone(), cancel };
            let r = f(&report);
            if let Err(e) = &r {
                let _ = tx.send(Ev::Note(None, format!("{e:#}"), true));
            }
            let _ = tx.send(Ev::Done(r.is_ok()));
            ctx.request_repaint();
        });
    }

    /// Start a job for this game, or queue it behind the running one.
    fn spawn(&mut self, ctx: &egui::Context, g: Game, job: Job) {
        if self.busy {
            self.notify(Some(g.dir.clone()), "Queued: it starts when the running job is done.", false);
            self.queue.push_back((g.dir, job));
            return;
        }
        self.job_started = Instant::now();
        self.busy = true;
        self.note = None;
        self.progress = None;
        self.rate = None;
        self.job_step = String::new();
        self.job_game = Some(g.dir.clone());
        self.job_label = match job {
            Job::Verify => "Checking files",
            Job::Install { .. } => "Installing",
            Job::Import => "Setting up",
            Job::BackupSaves => "Backing up saves",
            Job::RestoreSaves(_) => "Restoring saves",
            Job::Winetricks(_) => "Running winetricks",
            Job::RunExe(_) => "Running a program",
            Job::Updates { .. } => "Updating",
            Job::UndoUpdate => "Undoing the update",
            Job::Delete => "Deleting",
            Job::Move { .. } => "Moving",
        };
        // only Wine jobs have processes for Cancel to stop
        self.running = (!matches!(job, Job::Verify | Job::BackupSaves | Job::RestoreSaves(_) | Job::UndoUpdate | Job::Delete | Job::Move { .. })).then(|| g.prefix());
        self.cancel = Default::default();
        let (tx, ctx, cancel, root, settings) = (self.tx.clone(), ctx.clone(), self.cancel.clone(), self.lib.root.clone(), self.settings.clone());
        std::thread::spawn(move || {
            let report = UiReport { tx: tx.clone(), ctx: ctx.clone(), cancel };
            let log = |s: String| report.log(s);
            let r: Result<()> = match job {
                Job::Verify => match crate::checksum_file(&g) {
                    None => {
                        log("no checksum list for this game".into());
                        Ok(())
                    }
                    Some(f) => check(&f, &report),
                },
                // the checks change nothing: when one fails, an installed game stays installed (and playable)
                Job::Install { verify, unattended } => crate::install_checks(&g, verify, &report).and_then(|setup| {
                    let _ = tx.send(Ev::Status(g.dir.clone(), Status::Installing));
                    let r = install(&g, &setup, verify, unattended, &report);
                    if r.is_ok() {
                        // type, game file and emulator from what was installed: a walk of the whole game, here
                        // and not on the UI thread
                        let mut set = g.clone();
                        set.auto_setup(&settings);
                        let _ = tx.send(Ev::SetUp(g.dir.clone(), set.kind, set.exe, set.emulator, set.args));
                    }
                    let _ = tx.send(Ev::Status(g.dir.clone(), if r.is_ok() { Status::Installed } else { Status::Failed }));
                    r.map(|()| log(format!("installed into {}", g.install_dir().display())))
                }),
                Job::Delete => crate::delete_files(&root, &g).map(|()| _ = tx.send(Ev::Removed(g.dir.clone()))),
                Job::Move { to } => crate::move_game(&g, &to, &report).map(|new| {
                    log(format!("moved to {}", new.display()));
                    let _ = tx.send(Ev::Moved(g.dir.clone(), new));
                }),
                Job::BackupSaves => crate::backup_saves(&g, false).map(|b| {
                    let text = match b {
                        Some(p) => format!("Saves backed up to {}", p.display()),
                        None => "Nothing to back up yet: the game has not run.".into(),
                    };
                    let _ = tx.send(Ev::Note(Some(g.dir.clone()), text, false));
                }),
                Job::RestoreSaves(p) => crate::restore_saves(&g, &p).map(|()| _ = tx.send(Ev::Note(Some(g.dir.clone()), format!("Saves restored from {}", p.display()), false))),
                Job::Import => crate::prepare_import(&g, &report).map(|()| _ = tx.send(Ev::Note(Some(g.dir.clone()), "Ready to play.".into(), false))),
                Job::Updates { ups, verify, backup } => (|| {
                    let mut g = g.clone();
                    for u in &ups {
                        let name = u.dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
                        log(format!("applying {name}"));
                        update::apply(&g, u, verify, backup, &report)?;
                        g.version = u.to.clone(); // the next update's backup records this as its "before"
                        let _ = tx.send(Ev::Applied(g.dir.clone(), u.to.clone(), format!("v{} ({name})", u.to)));
                    }
                    log(format!("{} update(s) applied", ups.len()));
                    Ok(())
                })(),
                Job::UndoUpdate => update::undo(&g, &report).map(|(from, to)| _ = tx.send(Ev::Undone(g.dir.clone(), from, to))),
                Job::RunExe(exe) => {
                    let mut c = g.umu();
                    c.current_dir(exe.parent().unwrap_or(&exe)).arg(&exe);
                    run(c, &log).map(|st| log(format!("{} finished ({st})", exe.display())))
                }
                Job::Winetricks(verbs) => {
                    let mut c = g.umu();
                    c.arg("winetricks").arg("-q").args(verbs);
                    run(c, &log).map(|st| log(format!("winetricks finished ({st})")))
                }
            };
            let ok = r.is_ok();
            if let Err(e) = r {
                log(format!("error: {e:#}"));
            }
            let _ = tx.send(Ev::Done(ok));
            ctx.request_repaint();
        });
    }

    fn add_repack(&mut self, dir: PathBuf, import: bool) -> Option<usize> {
        match if import { self.lib.import(dir) } else { self.lib.add(dir) } {
            Ok(i) => {
                if easy::needs_cjk(&self.lib.games[i].name) {
                    self.styled = false; // loads the CJK font next frame
                }
                let st = self.settings.clone();
                self.lib.games[i].auto_setup(&st);
                self.save();
                self.select(Some(i));
                self.fetch_covers(i, false);
                Some(i)
            }
            Err(e) => {
                self.notify(None, format!("{e:#}"), true); // shown in the library, where Add was clicked
                None
            }
        }
    }

    /// Every option of one game, in sections, with the log at the end (the gear on the game page).
    fn detail(&mut self, ui: &mut Ui, i: usize) {
        let ctx = ui.ctx().clone();
        let idle = !self.busy;
        let g = self.lib.games[i].clone();
        let imported = !g.has_repack();
        let installed = g.status == Status::Installed;
        let windows = g.kind == Kind::Windows;
        let has_prefix = self.side(&g).has_prefix;
        let sub = ui.visuals().weak_text_color();
        let err = ui.visuals().error_fg_color;
        let section = |ui: &mut Ui, title: &str| {
            ui.add_space(22.0);
            ui.label(RichText::new(title).heading());
            ui.separator();
        };
        let grid = |id: &str| egui::Grid::new(id.to_string()).num_columns(2).spacing([18.0, 10.0]).min_col_width(170.0);

        section(ui, "About");
        grid("about").show(ui, |ui| {
            row_label(ui, "Name");
            let r = ui.add(egui::TextEdit::singleline(&mut self.lib.games[i].name).desired_width(420.0));
            if r.gained_focus() {
                self.name_was = Some(g.name.clone());
            }
            if r.lost_focus() {
                let was = self.name_was.take().unwrap_or_else(|| g.name.clone());
                if self.lib.games[i].name.trim().is_empty() {
                    self.lib.games[i].name = was; // a blank name would leave a blank tile and Steam entry
                } else if self.lib.games[i].name != was {
                    if easy::needs_cjk(&self.lib.games[i].name) {
                        self.styled = false; // loads the CJK font next frame
                    }
                    self.save();
                    self.fetch_covers(i, false); // missing artwork is looked up under the new name; the user's images stay
                    if g.steam.is_some() {
                        self.notify(Some(g.dir.clone()), "Steam keeps the old name: remove and re-add the game in Steam to rename it there.", false);
                    }
                }
            }
            ui.end_row();
            row_label(ui, "Version");
            if ui.add(egui::TextEdit::singleline(&mut self.lib.games[i].version).hint_text("unknown").desired_width(160.0)).lost_focus() {
                self.save();
                self.pending = Ok(vec![]); // the chain was built from the old version
            }
            ui.end_row();
            row_label(ui, if imported { "Game folder" } else { "Repack" });
            let src = g.game_dir.as_ref().unwrap_or(&g.repack_dir);
            ui.add(egui::Label::new(RichText::new(src.display().to_string()).color(sub)).wrap());
            ui.end_row();
            if !imported && installed {
                row_label(ui, "Game folder");
                ui.add(egui::Label::new(RichText::new(g.install_dir().display().to_string()).color(sub)).wrap());
                ui.end_row();
            }
        });

        section(ui, "Launch");
        grid("launch").show(ui, |ui| {
            row_label(ui, "Type");
            let mut kind = g.kind;
            egui::ComboBox::from_id_salt("kind").width(380.0).selected_text(kind.label()).show_ui(ui, |ui| {
                for k in Kind::ALL {
                    ui.selectable_value(&mut kind, k, k.label());
                }
            });
            if kind != g.kind {
                let gm = &mut self.lib.games[i];
                gm.kind = kind;
                gm.exe = None; // the picked file is of the old kind
                self.side = None; // its emulator checks are of the old kind too
                let _ = std::fs::remove_file(gm.launch_script()); // a Steam shortcut must not keep running the old kind
                gm.args = kind.default_args(self.settings.emulator(kind));
                let steam = gm.steam.is_some();
                self.save();
                if steam {
                    self.notify(Some(g.dir.clone()), "Type changed: launch options were reset, and the Steam shortcut works again once the game file is set.", false);
                }
                self.select(Some(i));
            }
            ui.end_row();
            row_label(ui, "Game file");
            ui.allocate_ui_with_layout(egui::vec2(560.0, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                if self.exes.is_none() {
                    self.exes = Some(targets(&g.install_dir(), g.kind));
                }
                let list = std::mem::take(&mut self.exes).unwrap_or_default(); // given back below: no copy per frame
                let rel = |e: &PathBuf| e.strip_prefix(g.install_dir()).unwrap_or(e).display().to_string();
                let shown = match &g.exe {
                    Some(e) if e.is_file() => rel(e),
                    Some(e) => format!("{} (missing)", rel(e)),
                    None if !installed => "Install the game first".into(),
                    None => "No game file found".into(),
                };
                egui::ComboBox::from_id_salt("exe").width(380.0).selected_text(shown).show_ui(ui, |ui| {
                    for e in list.iter() {
                        if ui.selectable_label(g.exe.as_ref() == Some(e), rel(e)).clicked() {
                            self.lib.games[i].exe = Some(e.clone());
                            self.save();
                            self.write_script(i);
                        }
                    }
                });
                self.exes = Some(list);
                if ui.button("Rescan").on_hover_text("look for game files again, e.g. after an update").clicked() {
                    self.select(Some(i));
                }
            });
            ui.end_row();
            if !windows {
                let k = g.kind;
                row_label(ui, match k {
                    Kind::Switch => "Switch emulator",
                    _ => "PS4 emulator",
                });
                ui.allocate_ui_with_layout(egui::vec2(620.0, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // the game's own (bundled with the repack, run through Proton) or the shared one from Settings
                    let own = g.emulator.clone().filter(|e| e.is_file());
                    let shared = self.settings.emulator(k).to_string();
                    let bundled = g.bundled_emulator();
                    let (text, color) = match (&own, shared.as_str()) {
                        (Some(e), _) => (format!("bundled: {} (runs with Proton)", e.strip_prefix(g.install_dir()).unwrap_or(e).display()), sub),
                        (None, "") => ("none set".to_string(), err),
                        (None, e) if !Path::new(e).is_file() => (format!("{e} (missing)"), err),
                        (None, e) => (e.to_string(), sub),
                    };
                    ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
                    if own.is_some() {
                        if Path::new(&shared).is_file() && ui.button("Use Settings' emulator").on_hover_text(&shared).clicked() {
                            let gm = &mut self.lib.games[i];
                            (gm.emulator, gm.args) = (None, k.default_args(&shared));
                            self.side = None;
                            self.save();
                            self.write_script(i);
                        }
                    } else if let Some((e, args)) = bundled
                        && ui.button("Use bundled").on_hover_text("the emulator FitGirl included with the repack, already set up; runs with Proton").clicked()
                    {
                        let gm = &mut self.lib.games[i];
                        (gm.emulator, gm.args) = (Some(e), args);
                        self.side = None;
                        self.save();
                        self.write_script(i);
                    }
                    if ui.button("Settings").on_hover_text("the emulator shared by all games of this type").clicked() {
                        self.global = true;
                    }
                });
                ui.end_row();
            }
            // switches for the tools that are installed (FSR, the FPS counter and the GPU need none)
            ui.label("Extras"); // top-aligned like the multi-line fields, not centred on the tall stack
            ui.vertical(|ui| {
                // which tools are installed: looked up once, not every frame
                static AVAILABLE: std::sync::OnceLock<Vec<bool>> = std::sync::OnceLock::new();
                let available = AVAILABLE.get_or_init(|| crate::TWEAKS.iter().map(|t| t.needs.is_none_or(|b| which_path(b).is_some())).collect());
                for (t, _) in crate::TWEAKS.iter().zip(available).filter(|(_, a)| **a) {
                    if !windows && matches!(t.key, "fsr" | "dxvk_hud") {
                        continue; // Proton-only
                    }
                    let mut on = self.lib.games[i].tweaks.iter().any(|k| k == t.key);
                    if ui.checkbox(&mut on, t.label).changed() {
                        let tw = &mut self.lib.games[i].tweaks;
                        tw.retain(|k| k != t.key);
                        if on {
                            tw.push(t.key.to_string());
                        }
                        self.save();
                        self.write_script(i);
                    }
                }
            });
            ui.end_row();
            row_label(ui, "App menu");
            let mut on = g.desktop;
            if ui.checkbox(&mut on, "Show in the app menu").on_hover_text("a .desktop entry that runs this game's launch script").changed() {
                self.lib.games[i].desktop = on;
                if on {
                    self.write_script(i);
                } else {
                    crate::remove_desktop(&g, &crate::apps_dir());
                }
                self.save();
            }
            ui.end_row();
            row_label(ui, "Last run");
            let out = g.dir.join("last-run.log");
            if ui.add_enabled(self.side(&g).last_run, egui::Button::new("Why won't it start?")).on_disabled_hover_text("the game has not been started from repackd yet").clicked() {
                let text = std::fs::read_to_string(&out).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                self.log.push(format!("-- last run of {} ({} lines, the last 300 shown)", g.name, lines.len()));
                self.log.extend(lines[lines.len().saturating_sub(300)..].iter().map(|l| l.to_string()));
            }
            ui.end_row();
        });

        ui.add_space(22.0);
        egui::CollapsingHeader::new(RichText::new("Advanced").heading()).default_open(false).show(ui, |ui| {
            grid("advanced").show(ui, |ui| {
                    if windows {
                        row_label(ui, "Compatibility version");
                        self.proton_combo(ui, i);
                        ui.end_row();
                    }
                    if !windows && !imported {
                        row_label(ui, "Compatibility version (setup)");
                        self.proton_combo(ui, i);
                        ui.end_row();
                    }
                    row_label(ui, "Launch options");
                    let hint = if windows { "e.g. -dx11 -skipintro" } else { "{file} is the game file" };
                    if ui.add(egui::TextEdit::singleline(&mut self.lib.games[i].args).hint_text(hint).desired_width(380.0)).lost_focus() {
                        self.save();
                        self.write_script(i);
                    }
                    ui.end_row();
                    ui.label("Extra settings (advanced)");
                    if ui.add(egui::TextEdit::multiline(&mut self.lib.games[i].env).hint_text("one per line, e.g.\nDXVK_ASYNC=1").desired_rows(2).desired_width(380.0)).lost_focus() {
                        self.save();
                        self.write_script(i);
                    }
                    ui.end_row();
            });
        });

        section(ui, "Steam");
        if self.steam_users.is_empty() {
            ui.label(RichText::new("No Steam account found on this PC.").color(sub));
        } else {
            ui.horizontal(|ui| {
                if self.steam_users.len() > 1 {
                    let name = |p: &PathBuf| p.ancestors().nth(2).and_then(|d| d.file_name()).unwrap_or_default().to_string_lossy().into_owned();
                    egui::ComboBox::from_id_salt("steamuser").selected_text(name(&self.steam_users[self.steam_pick])).show_ui(ui, |ui| {
                        for (n, u) in self.steam_users.iter().enumerate() {
                            ui.selectable_value(&mut self.steam_pick, n, name(u));
                        }
                    });
                }
                if g.steam.is_none() {
                    if ui.add_enabled(installed, egui::Button::new("Add to Steam")).on_disabled_hover_text("install the game first").clicked()
                        && (g.exe.is_some() || self.pick_exe(i))
                    {
                        let vdf = self.steam_users[self.steam_pick].clone();
                        self.steam_edit(i, Some(vdf));
                    }
                    ui.label(RichText::new("Then Steam starts the game: playtime, overlay, controller settings.").color(sub).small());
                } else if ui.button("Remove from Steam").clicked() {
                    self.steam_edit(i, None);
                }
            });
        }

        section(ui, "Updates");
        if g.applied.is_empty() {
            ui.label(RichText::new("No updates applied.").color(sub));
        }
        for a in &g.applied {
            ui.label(format!("Applied {a}"));
        }
        if let Some((from, _)) = self.side(&g).undo.clone().filter(|(f, t)| [f, t].iter().any(|v| update::vcmp(v, &g.version) == std::cmp::Ordering::Equal)) {
            ui.horizontal(|ui| {
                if ui.add_enabled(idle, egui::Button::new(format!("Undo update (back to v{from})"))).on_hover_text(format!("puts the game files back as they were at v{from}")).clicked() {
                    self.spawn(&ctx, g.clone(), Job::UndoUpdate);
                }
                if ui.add_enabled(idle, egui::Button::new(match &self.disk { Some((d, [.., b])) if *d == g.dir && *b > 0 => format!("Delete undo copies (frees {})", gb(*b)), _ => "Delete undo copies".to_string() })).on_hover_text("frees their space; updates can't be undone after this").clicked() {
                    (self.side, self.disk) = (None, None);
                    match update::delete_backups(&g) {
                        Ok(()) => self.notify(Some(g.dir.clone()), "Update backups deleted.", false),
                        Err(e) => self.say(format!("error: {e:#}")),
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            let folders = ui.add_enabled(idle, egui::Button::new("Add update folders…")).on_hover_text("pick one or more FitGirl update folders; they are applied oldest first").clicked();
            let archives = ui.add_enabled(idle, egui::Button::new("Add update archives…")).on_hover_text("scene updates as .rar/.7z/.zip (for a multi-part RAR pick its .part1.rar); unpacked, installed, then their crack copied in").clicked();
            let picked = if folders {
                rfd::FileDialog::new().set_title("Update folders").pick_folders()
            } else if archives {
                rfd::FileDialog::new().set_title("Update archives").add_filter("Archives", &["rar", "7z", "zip"]).pick_files()
            } else {
                None
            };
            if let Some(paths) = picked {
                self.pending = paths.iter().map(|d| update::parse(d)).collect::<Result<Vec<_>, _>>().and_then(|ups| update::ladder(&g.version, ups)).map_err(|e| format!("{e:#}"));
                if matches!(&self.pending, Ok(u) if u.is_empty()) {
                    // all skipped as not newer: say so instead of showing nothing
                    let v = if g.version.is_empty() { "the game's version".to_string() } else { format!("v{}", g.version) };
                    self.pending = Err(format!("None of them is newer than {v}. If the game's version is wrong, correct it above."));
                }
            }
            let has_plan = matches!(&self.pending, Ok(u) if !u.is_empty()) || self.pending.is_err();
            if let Ok(ups) = &self.pending
                && !ups.is_empty()
            {
                let why = if !installed { "install the game first" } else if self.no_umu { "umu-launcher is missing" } else { "this game's job is running" };
                let label = format!("{} {} update(s)", if self.busy { "Queue" } else { "Apply" }, ups.len());
                if ui.add_enabled(!self.busy_with(&g) && !self.queued(&g) && !self.no_umu && installed, egui::Button::new(label)).on_hover_text("A setup window will open: click Next / Install / Finish in it.").on_disabled_hover_text(why).clicked() {
                    let ups = std::mem::replace(&mut self.pending, Ok(vec![])).unwrap_or_default();
                    self.spawn(&ctx, g.clone(), Job::Updates { ups, verify: self.settings.verify, backup: self.settings.backup_updates });
                }
            }
            if has_plan && ui.button("Clear").clicked() {
                self.pending = Ok(vec![]);
            }
            if ui.add_enabled(idle && installed, egui::Button::new("Check for updates"))
                .on_hover_text("looks inside the repack folder, next to it, and in Downloads for this game's update folders and archives")
                .clicked()
            {
                self.pending = update::find_for(&g);
                if matches!(&self.pending, Ok(u) if u.is_empty()) {
                    self.notify(Some(g.dir.clone()), "No newer update found next to the game or in Downloads.", false);
                }
            }
        });
        match &self.pending {
            Ok(ups) => {
                let mut v = if g.version.is_empty() { "?".to_string() } else { format!("v{}", g.version) };
                for u in ups {
                    ui.label(format!("{v} -> v{}   {}", u.to, u.dir.file_name().unwrap_or_default().to_string_lossy()));
                    v = format!("v{}", u.to);
                }
            }
            Err(e) => _ = ui.colored_label(err, e),
        }

        if windows {
            section(ui, "Saves");
            ui.horizontal(|ui| {
                if ui.add_enabled(idle && has_prefix, egui::Button::new("Back up saves")).on_hover_text("copies the game's AppData/Documents saves to .repackd-saves in the games folder; Delete does this too").clicked() {
                    self.spawn(&ctx, g.clone(), Job::BackupSaves);
                }
                let list = self.side(&g).saves.clone();
                if list.is_empty() {
                    ui.label(RichText::new("No backups yet.").color(sub));
                } else {
                    egui::ComboBox::from_id_salt("restore").selected_text("Restore a backup…").show_ui(ui, |ui| {
                        for b in list {
                            let label = b.file_name().unwrap_or_default().to_string_lossy().trim_end_matches(".tar.gz").replace("-auto", " (after playing)").replace('_', " ");
                            if ui.selectable_label(false, label).clicked() {
                                self.confirm_restore = Some(b);
                            }
                        }
                    });
                }
            });
            if let Some(b) = self.confirm_restore.clone() {
                let when = b.file_name().unwrap_or_default().to_string_lossy().trim_end_matches(".tar.gz").replace("-auto", " (after playing)").replace('_', " ");
                ui.label(format!("Replace the current saves with the backup from {when}?"));
                ui.horizontal(|ui| {
                    if ui.add_enabled(idle, egui::Button::new(RichText::new("Yes, restore").color(err))).clicked() {
                        self.spawn(&ctx, g.clone(), Job::RestoreSaves(b));
                        self.confirm_restore = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_restore = None;
                    }
                });
            }
        }

        section(ui, "Artwork");
        ui.horizontal(|ui| {
            row_label(ui, "Steam store id");
            let r = ui.add(egui::TextEdit::singleline(&mut self.store_text).hint_text("optional").desired_width(150.0))
                .on_hover_text("the number in the game's store.steampowered.com/app/<id> link; pasting the link works too");
            if r.changed() {
                // a pasted store link: the digits after /app/; else the first number in the text
                let t = self.store_text.split("/app/").nth(1).unwrap_or(&self.store_text);
                self.lib.games[i].store_id = t.split(|c: char| !c.is_ascii_digit()).find(|d| !d.is_empty()).and_then(|d| d.parse().ok());
            }
            if r.lost_focus() {
                self.save();
            }
            let source = !self.settings.sgdb_key.is_empty() || self.lib.games[i].store_id.is_some();
            if ui.add_enabled(source, egui::Button::new("Fetch artwork")).on_disabled_hover_text("add a SteamGridDB key (top bar) or a store id first").clicked() {
                self.fetch_covers(i, true);
                self.fetch_art(i); // and the Steam shortcut's, when there is one
            }
        });
        if self.settings.sgdb_key.is_empty() && self.lib.games[i].store_id.is_none() {
            ui.label(RichText::new("Covers come from SteamGridDB (add a key in Settings) or, with a store id, from Steam.").color(sub).small());
        }
        // the right game by hand: Switch/PlayStation titles and abbreviated names ("TLoZ") are often not found by name
        ui.horizontal(|ui| {
            row_label(ui, "Search SteamGridDB");
            let r = ui.add(egui::TextEdit::singleline(&mut self.art_query).desired_width(300.0));
            let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let can = !self.settings.sgdb_key.is_empty() && !self.art_query.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new("Search")).on_disabled_hover_text("add a SteamGridDB key in Settings first").clicked() || (go && can) {
                let (q, key, tx, dir) = (self.art_query.trim().to_string(), self.settings.sgdb_key.clone(), self.tx.clone(), g.dir.clone());
                self.sgdb_hits = Some((dir.clone(), Ok(vec![])));
                std::thread::spawn(move || _ = tx.send(Ev::SgdbHits(dir, steam::sgdb_search(&q, &key).map_err(|e| format!("{e:#}")))));
            }
        });
        if let Some((dir, hits)) = self.sgdb_hits.clone()
            && dir == g.dir
        {
            match hits {
                Err(e) => _ = ui.colored_label(err, e),
                Ok(list) if list.is_empty() => _ = ui.label(RichText::new("No results (yet).").color(sub)),
                Ok(list) => {
                    egui::Grid::new("hits").num_columns(2).spacing([18.0, 6.0]).show(ui, |ui| {
                        for (id, name, year) in list.into_iter().take(8) {
                            ui.label(match year {
                                Some(y) => format!("{name}  ({y})"),
                                None => name.clone(),
                            });
                            if ui.button("Use").clicked() {
                                self.lib.games[i].sgdb_id = Some(id);
                                self.save();
                                self.sgdb_hits = None;
                                self.fetch_covers(i, true);
                                self.fetch_art(i);
                                self.notify(Some(g.dir.clone()), format!("Artwork from \"{name}\" on SteamGridDB: fetching…"), false);
                            }
                            ui.end_row();
                        }
                    });
                }
            }
        }
        if g.sgdb_id.is_some() {
            ui.horizontal(|ui| {
                ui.label(RichText::new("A SteamGridDB game picked by hand is used for this game's artwork.").color(sub).small());
                if ui.small_button("Reset").on_hover_text("look the game up by its name again").clicked() {
                    self.lib.games[i].sgdb_id = None;
                    self.save();
                }
            });
        }
        ui.horizontal(|ui| {
            row_label(ui, "Your own images");
            for (label, which) in [("Cover…", "cover"), ("Banner…", "hero"), ("Logo…", "logo")] {
                if ui.button(label).on_hover_text(match which {
                    "cover" => "a portrait image for the library tile (2:3, e.g. 600x900)",
                    "hero" => "a wide banner for the game page (about 3:1, e.g. 1920x620)",
                    _ => "a logo with a transparent background, shown on the banner",
                }).clicked()
                    && let Some(f) = rfd::FileDialog::new().add_filter("Images", &crate::IMAGE_EXTS).pick_file()
                {
                    match g.set_own_art(which, &f) {
                        Ok(()) => self.art_changed(&g.dir),
                        Err(e) => self.say(format!("error: {e:#}")),
                    }
                }
            }
        });

        section(ui, "Files");
        if let Some(to) = self.confirm_move.clone() {
            let outside = g.game_dir.as_ref().filter(|d| !d.starts_with(&g.dir));
            let dest = to.join(g.dir.file_name().unwrap_or_default());
            let kept = outside.map(|d| format!(" The game folder {} stays where it is: only repackd's folder (prefix, launch script, artwork) moves.", d.display())).unwrap_or_default();
            ui.label(format!("Move {} to {}?{kept} Don't close repackd until it is done.", g.dir.display(), dest.display()));
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Yes, move it").color(err)).clicked() {
                    self.confirm_move = None;
                    self.spawn(&ctx, g.clone(), Job::Move { to });
                }
                if ui.button("Keep it here").clicked() {
                    self.confirm_move = None;
                }
            });
        }
        ui.horizontal(|ui| {
            // the desktop's own file manager: copy mods into the game folder there
            let mut dirs = vec![("Browse game files", g.install_dir())];
            if windows {
                dirs.push(("Open game data folder", g.prefix()));
            }
            for (label, dir) in dirs {
                if ui.add_enabled(dir.is_dir(), egui::Button::new(label)).clicked() {
                    // D-Bus may have to start the file manager first: not on the UI thread
                    let tx = self.tx.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = crate::open_folder(&dir) {
                            let _ = tx.send(Ev::Log(format!("error: open folder: {e:#}")));
                        }
                    });
                }
            }
            if ui.add_enabled(self.confirm_move.is_none() && !self.busy_with(&g) && !self.queued(&g), egui::Button::new("Move…")).on_hover_text("moves repackd's folder for this game (prefix, launch script, artwork, shader cache) to another disk").clicked() {
                self.move_pick = !self.move_pick;
                if self.move_pick {
                    self.move_drives = crate::drives(&self.settings.root());
                }
            }
        });
        if self.move_pick && self.confirm_move.is_none() {
            // only a size already measured: never computed here, on the UI thread
            let size = self.disk_rows.get(&g.dir).map(|s| s[0] + s[1]).or_else(|| self.disk.as_ref().filter(|(d, _)| *d == g.dir).map(|(_, s)| s[0] + s[1]));
            let here = self.move_drives.iter().filter(|d| g.dir.starts_with(&d.mount)).max_by_key(|d| d.mount.as_os_str().len()).map(|d| d.dev);
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            let mut pick = None;
            for d in &self.move_drives {
                let why = if here == Some(d.dev) {
                    Some("Current drive")
                } else if !d.writable {
                    Some("No permission")
                } else if size.is_some_and(|s| d.free < s + (1 << 30)) {
                    Some("Not enough space")
                } else {
                    None
                };
                let name = if d.target.starts_with(&home) { "Home".to_string() } else { d.mount.display().to_string() };
                let label = format!("{name}  ·  {} free{}", gb(d.free), why.map(|w| format!("  ·  {w}")).unwrap_or_default());
                let b = ui.add_enabled(why.is_none(), egui::Button::new(label)).on_hover_text(why.map_or_else(|| d.target.display().to_string(), str::to_string));
                if b.clicked() {
                    pick = Some(d.target.clone());
                }
            }
            if ui.small_button("Other folder…").clicked() && let Some(to) = rfd::FileDialog::new().set_title("Move into folder").pick_folder() {
                pick = Some(to);
            }
            if let Some(to) = pick {
                match crate::move_refusal(&g, &to) {
                    Ok(()) => (self.confirm_move, self.move_pick) = (Some(to), false),
                    Err(e) => self.say(format!("error: cannot move {}: {e:#}", g.name)),
                }
            }
        }
        if installed {
            ui.horizontal(|ui| {
                match &self.disk {
                    Some((d, [game, pfx, repack, backups])) if *d == g.dir => {
                        let mut parts = vec![format!("Game {}", gb(*game)), format!("{} {}", if imported { "game data folder" } else { "game data folder (Wine prefix)" }, gb(*pfx))];
                        if *repack > 0 {
                            parts.push(format!("repack {}", gb(*repack)));
                        }
                        if *backups > 0 {
                            parts.push(format!("update backups {}", gb(*backups)));
                        }
                        ui.label(RichText::new(parts.join(" · ")).color(sub));
                    }
                    _ => {
                        if ui.button("Show disk use").clicked() {
                            let (g, tx) = (g.clone(), self.tx.clone());
                            std::thread::spawn(move || {
                                let [game, pfx, repack] = crate::disk_sizes(&g);
                                let _ = tx.send(Ev::DiskUse(g.dir.clone(), [game, pfx, repack, update::backups_size(&g)]));
                            });
                        }
                    }
                }
            });
        }

        ui.add_space(22.0);
        egui::CollapsingHeader::new(RichText::new("Advanced tools").heading()).default_open(false).show(ui, |ui| {
            if windows {
                ui.horizontal(|ui| {
                    if ui.add_enabled(idle && has_prefix && !self.no_umu, egui::Button::new("Run .exe in the game data folder…")).on_hover_text("mod installers, trainers, patchers: run with this game's Proton").on_disabled_hover_text("set up or install the game first").clicked()
                        && let Some(exe) = rfd::FileDialog::new().add_filter("Windows programs", &["exe"]).set_directory(g.install_dir()).pick_file()
                    {
                        self.spawn(&ctx, g.clone(), Job::RunExe(exe));
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.verbs).hint_text("Windows components to install, e.g. vcrun2022").desired_width(380.0));
                    let verbs: Vec<String> = self.verbs.split_whitespace().map(String::from).collect();
                    let why = if self.no_winetricks { format!("winetricks is not installed: {}", crate::install_hint("winetricks")) } else { "type the verbs first".into() };
                    if ui.add_enabled(idle && !verbs.is_empty() && !self.no_umu && !self.no_winetricks, egui::Button::new("Run winetricks")).on_disabled_hover_text(why).clicked() {
                        self.spawn(&ctx, g.clone(), Job::Winetricks(verbs));
                    }
                });
                ui.horizontal(|ui| {
                    let size = self.side(&g).shader_cache;
                    ui.label(format!("Shader cache: {:.0} MB", size as f64 / 1e6));
                    // Play sets no busy flag, so the running check (a /proc scan) is made on click, not every frame
                    if ui.add_enabled(size > 0 && idle, egui::Button::new("Clear shader cache")).on_hover_text("games rebuild it as they play").clicked() {
                        if !crate::game_pids(&g).is_empty() {
                            self.say(format!("{} is running: close it before clearing its shader cache", g.name));
                        } else {
                            match std::fs::remove_dir_all(g.dir.join("shadercache")) {
                                Ok(()) => self.say(format!("cleared the shader cache of {}", g.name)),
                                Err(e) => self.say(format!("error: clear shader cache: {e}")),
                            }
                            self.side = None;
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                if ui.button("Copy diagnostics").on_hover_text("versions, hardware, this game's settings and its last run, as text for a bug report").clicked() {
                    ctx.copy_text(crate::diagnostics(&g, &self.settings));
                    self.say(format!("copied diagnostics for {} to the clipboard", g.name));
                }
            });
        });

        section(ui, "Install");
        if imported {
            ui.label(RichText::new("These are your own files. repackd will not delete them.").color(sub));
            ui.horizontal(|ui| {
                if windows || has_prefix {
                    let label = if has_prefix { "Redo setup" } else { "Finish setup" };
                    if ui.add_enabled(idle && !self.no_umu, egui::Button::new(label)).on_hover_text("creates the game data folder (Wine prefix) with the Windows components the game ships").clicked() {
                        self.spawn(&ctx, g.clone(), Job::Import);
                    }
                }
                let list = self.side(&g).checksum;
                if ui.add_enabled(idle && list, egui::Button::new("Check files"))
                    .on_hover_text("against FitGirl's list; files changed by updates, mods or settings show as different")
                    .on_disabled_hover_text("no FitGirl checksum list in this folder")
                    .clicked()
                {
                    self.spawn(&ctx, g.clone(), Job::Verify);
                }
            });
        } else {
            if !installed && let Some(need) = g.size {
                let free = free_space(&g.dir); // not cached: the user may free space with the page open
                let short = free.is_some_and(|f| f < need);
                let text = format!("Needs {}, {} free{}", gb(need), free.map(gb).unwrap_or("?".into()), if short { ": not enough space" } else { "" });
                ui.label(RichText::new(text).color(if short { err } else { sub }));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy_with(&g) && !self.queued(&g), egui::Button::new("Check repack files")).clicked() {
                    self.spawn(&ctx, g.clone(), Job::Verify);
                }
                let label = if installed { "Reinstall" } else { "Install" };
                let why = if self.no_umu { "umu-launcher is missing" } else { "another job is running" };
                let label = if self.busy && !self.busy_with(&g) { format!("Queue {}", label.to_lowercase()) } else { label.to_string() };
                let why = if g.repack_dir.is_dir() { why } else { "the repack folder is gone" };
                if ui.add_enabled(!self.busy_with(&g) && !self.queued(&g) && !self.no_umu && g.repack_dir.is_dir(), egui::Button::new(label)).on_disabled_hover_text(why).clicked() {
                    self.spawn(&ctx, g.clone(), Job::Install { verify: self.settings.verify, unattended: self.settings.unattended });
                }
            });
        }

        if installed && g.has_repack() {
            if !g.repack_dir.is_dir() {
                ui.label(RichText::new("The repack folder was deleted: reinstalling or checking its files needs it again.").color(sub));
            } else if !self.confirm_repack {
                if ui.add_enabled(!self.busy_with(&g), egui::Button::new("Delete the repack folder…")).on_hover_text("frees its space; the installed game keeps working").clicked() {
                    self.confirm_repack = true;
                }
            } else {
                ui.label(format!("Delete {}? Reinstalling and checking the repack's files won't be possible anymore.", g.repack_dir.display()));
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Yes, delete the repack").color(err)).clicked() {
                        self.confirm_repack = false;
                        self.delete_repack_bg(g.clone(), Some(g.dir.clone()));
                    }
                    if ui.button("Keep it").clicked() {
                        self.confirm_repack = false;
                    }
                });
            }
        }

        section(ui, "Delete");
        self.delete_confirm(ui, i);

        ui.add_space(22.0);
        let log_head = ui.label(RichText::new("Log").heading());
        ui.separator();
        if std::mem::take(&mut self.scroll_to_log) {
            log_head.scroll_to_me(Some(egui::Align::Min));
        }
        egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(6).inner_margin(10).show(ui, |ui| {
            ui.set_height(260.0);
            ui.set_width(ui.available_width());
            self.log_panel(ui);
        });
        ui.add_space(12.0);
    }

    /// Delete with a confirmation step; the same on the game page and in settings.
    fn delete_confirm(&mut self, ui: &mut Ui, i: usize) {
        let g = self.lib.games[i].clone();
        let err = ui.visuals().error_fg_color;
        if !self.confirm_delete {
            let label = if g.game_dir.is_some() { "Remove from library" } else { "Delete game" };
            if ui.add_enabled(!self.busy_with(&g), egui::Button::new(RichText::new(label).color(err))).clicked() {
                self.confirm_delete = true;
            }
            return;
        }
        ui.vertical(|ui| {
            let what = match (&g.game_dir, g.status) {
                (Some(_), _) => "Remove it from repackd? The game's own files are kept; saves are backed up first.".to_string(),
                (None, Status::Installed) => {
                    // only a size already measured: never computed here, on the UI thread
                    let known = self.disk_rows.get(&g.dir).map(|s| s[0] + s[1]).or_else(|| self.disk.as_ref().filter(|(d, _)| *d == g.dir).map(|(_, s)| s[0] + s[1]));
                    let size = known.map(|b| format!(" and free {}", gb(b))).unwrap_or_default();
                    format!("Delete the installed game{size}? Your saves are copied to a backup first. The repack folder stays. This can't be stopped once started.")
                }
                (None, _) => "Remove it from the library? The repack folder is kept.".to_string(),
            };
            ui.label(what);
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy_with(&g) && !self.queued(&g), egui::Button::new(RichText::new("Yes, delete").color(err))).clicked() {
                    // off the UI thread: the saves backup and a 100 GB delete take a while
                    self.spawn(&ui.ctx().clone(), g.clone(), Job::Delete);
                    self.confirm_delete = false;
                }
                if ui.button("Keep it").clicked() {
                    self.confirm_delete = false;
                }
            });
        });
    }

    /// Proton for this game: the builds found on this PC, plus whatever custom value it already has.
    fn proton_combo(&mut self, ui: &mut Ui, i: usize) {
        let cur = self.lib.games[i].proton.clone();
        let mut list = self.protons.clone();
        if !list.iter().any(|(_, v)| *v == cur) && !cur.is_empty() {
            list.push((format!("{cur} (custom)"), cur.clone()));
        }
        let shown = list.iter().find(|(_, v)| *v == cur).map(|(l, _)| l.clone()).unwrap_or_default();
        egui::ComboBox::from_id_salt("proton").width(380.0).selected_text(shown).show_ui(ui, |ui| {
            for (label, value) in list {
                if ui.selectable_label(cur == value, label).clicked() {
                    self.lib.games[i].proton = value;
                    self.save();
                    self.write_script(i);
                }
            }
        });
    }

    /// Start the game: through Steam when it has a shortcut there (playtime, overlay), else its launch script.
    /// Picks the most likely game file first when none is chosen yet.
    fn launch(&mut self, i: usize) {
        // none chosen yet, or an update renamed it: take the best guess again
        if !self.lib.games[i].exe.as_ref().is_some_and(|e| e.is_file()) && !self.pick_exe(i) {
            return;
        }
        if !self.write_script(i) {
            return;
        }
        let g = &self.lib.games[i];
        // the 8 s "Starting…" guard is shorter than many games take to show a window
        if !crate::game_pids(g).is_empty() {
            let dir = g.dir.clone();
            self.notify(Some(dir.clone()), "Already running.", false);
            self.fix = Some((dir, crate::Fix::Stop));
            return;
        }
        let via_steam = g.steam_appid.filter(|_| g.steam.is_some());
        let mut c = match via_steam {
            Some(id) => {
                let mut c = Command::new(if which_path("steam").is_some() { "steam" } else { "xdg-open" });
                c.arg(format!("steam://rungameid/{}", steam::game_id(id)));
                c
            }
            None => Command::new(g.launch_script()),
        };
        // the script's output, for "Last run" and for saying why a game stopped right away
        let out = g.dir.join("last-run.log");
        if via_steam.is_none()
            && let Ok(f) = std::fs::File::create(&out)
            && let Ok(f2) = f.try_clone()
        {
            c.stdout(f).stderr(f2);
        }
        // another Proton to suggest when the game crashes under this one
        let other = (g.kind == Kind::Windows).then(|| self.protons.iter().map(|(_, v)| v.clone()).find(|v| *v != g.proton)).flatten();
        self.launching = Some((g.dir.clone(), Instant::now()));
        self.playing.insert(g.dir.clone());
        self.side = None; // last-run.log now exists
        let (tx, dir, game, ctx) = (self.tx.clone(), g.dir.clone(), g.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let t = Instant::now();
            let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            // Steam: started and left running (it may be Steam itself, open for hours); the script: run to its end
            let status = if via_steam.is_some() {
                // reaped on its own thread: a short `steam steam://…` call must not linger as a zombie
                c.spawn().map(|mut child| _ = std::thread::spawn(move || child.wait())).map(|()| None)
            } else {
                c.status().map(Some)
            };
            match status {
                Err(e) => {
                    let _ = tx.send(Ev::NotPlaying(dir.clone()));
                    let _ = tx.send(Ev::Note(Some(dir), format!("could not start: {e}"), true));
                    return;
                }
                // the script runs as long as the game; Steam hands the game off and returns at once
                Ok(Some(st)) if t.elapsed() < Duration::from_secs(20) => {
                    let log = std::fs::read_to_string(&out).unwrap_or_default();
                    match crate::diagnose(&log, other.as_deref()) {
                        Some((text, fix)) => _ = tx.send(Ev::StartFailed(dir.clone(), text, fix)),
                        None if !st.success() => _ = tx.send(Ev::StartFailed(dir.clone(), "The game closed right away. Open Game settings, then \"Why won't it start?\"".to_string(), None)),
                        None => {}
                    }
                }
                Ok(_) => {}
            }
            let (mut t, mut at) = (t, at);
            if via_steam.is_some() {
                // until the game is up (Steam may take a while); its playtime starts then
                while crate::game_pids(&game).is_empty() {
                    if t.elapsed() > Duration::from_secs(180) {
                        let _ = tx.send(Ev::NotPlaying(dir));
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                t = Instant::now();
                at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            }
            // shortcut: playtime counted while repackd stays open; a game still running when it closes is not counted
            // the processes found, checked by their /proc entry; a full scan of all processes only when they are
            // all gone (the game may have started others meanwhile)
            let mut pids = crate::game_pids(&game);
            while !pids.is_empty() {
                std::thread::sleep(Duration::from_secs(5));
                // a zombie keeps its /proc entry until reaped: count it as gone
                pids.retain(|p| std::fs::read_to_string(format!("/proc/{p}/stat")).is_ok_and(|st| st.rsplit_once(") ").is_some_and(|(_, r)| !r.starts_with('Z'))));
                if pids.is_empty() {
                    pids = crate::game_pids(&game);
                }
            }
            let _ = tx.send(Ev::Played(dir, at, t.elapsed().as_secs()));
            if let Some(ctx) = ctx {
                ctx.request_repaint();
            }
        });
    }

    /// Choose the first launch target (the picker's best guess); false when the folder has none.
    fn pick_exe(&mut self, i: usize) -> bool {
        let g = &self.lib.games[i];
        match targets(&g.install_dir(), g.kind).into_iter().next() {
            Some(e) => {
                self.lib.games[i].exe = Some(e);
                self.save();
                true
            }
            None => {
                let msg = format!("error: no game file found in {}", g.install_dir().display());
                self.say(msg);
                false
            }
        }
    }

    /// Download the game's cover/hero art for the library, off the UI thread (needs a SteamGridDB key or a store id).
    /// Artwork for every game without a cover (never fetched, or added before the key was set).
    fn fetch_missing_covers(&mut self) {
        for i in 0..self.lib.games.len() {
            if self.lib.games[i].art("cover").is_none() {
                self.fetch_covers(i, false);
            }
        }
    }

    /// `force`: fetch all four images again (the user asked); otherwise only the missing ones, so images the
    /// user picked are kept, and not again for a game whose lookup failed with the same name, ids and key.
    fn fetch_covers(&mut self, i: usize, force: bool) {
        let g = self.lib.games[i].clone();
        let key = self.settings.sgdb_key.clone();
        if key.trim().is_empty() && g.store_id.is_none() {
            return;
        }
        let tried = g.dir.join(".art-not-found");
        // the key hashed, so a corrected key tries again; and a day old at most, so an offline start is retried
        let stamp = format!("{}|{:?}|{:?}|{:x}", g.name, g.store_id, g.sgdb_id, {
            use std::hash::{Hash, Hasher};
            let mut h = std::hash::DefaultHasher::new();
            key.trim().hash(&mut h);
            h.finish()
        });
        let fresh = std::fs::metadata(&tried).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|e| e < Duration::from_secs(86400)));
        if !force && fresh && std::fs::read_to_string(&tried).is_ok_and(|s| s == stamp) {
            return;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let log = |s: String| _ = tx.send(Ev::Log(s));
            match g.fetch_art(&key, !force, &log) {
                Ok((n, sid)) => {
                    let _ = std::fs::remove_file(&tried);
                    log(format!("{}: {n} artwork image(s)", g.name));
                    if let Some(id) = sid {
                        let _ = tx.send(Ev::StoreId(g.dir.clone(), id));
                    }
                }
                Err(e) => {
                    let _ = std::fs::write(&tried, &stamp); // not again at every start
                    let _ = tx.send(Ev::Note(Some(g.dir.clone()), format!("artwork: {e:#}"), true));
                }
            }
            let _ = tx.send(Ev::ArtReady(g.dir.clone()));
        });
    }

    /// Rewrite a game's launch script quietly after its files changed (an update, an undo, an install).
    fn refresh_script(&mut self, dir: &Path) {
        if let Some(g) = self.lib.games.iter().find(|g| g.dir == dir && g.exe.is_some()) {
            let _ = write_launch_script(g, &self.settings);
        }
    }

    /// Rewrite the game's launch script (no-op until a game file is picked); false = it could not be written.
    fn write_script(&mut self, i: usize) -> bool {
        if self.lib.games[i].exe.is_none() {
            return true;
        }
        match write_launch_script(&self.lib.games[i], &self.settings) {
            Ok(()) => true,
            Err(e) => {
                self.say(format!("error: launch script: {e:#}"));
                false
            }
        }
    }

    /// The whole session's log, newest at the bottom.
    fn log_panel(&mut self, ui: &mut Ui) {
        let row = ui.text_style_height(&egui::TextStyle::Monospace);
        // only the visible rows are laid out: Proton/winetricks output grows to thousands of lines
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink(false).show_rows(ui, row, self.log.len(), |ui, range| {
            for l in &self.log[range] {
                let bad = ["error", "mismatch", "missing", "unreadable"].iter().any(|p| l.starts_with(p));
                let c = if bad { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
                ui.add(egui::Label::new(RichText::new(l).monospace().color(c)).truncate());
            }
        });
    }

    /// Fetch the ProtonDB rating for a Steam store id once per session, in the background.
    fn protondb_for(&mut self, id: u32) -> Option<Rating> {
        if let Some(r) = self.protondb.get(&id) {
            return r.clone();
        }
        self.protondb.insert(id, None);
        let tx = self.tx.clone();
        std::thread::spawn(move || _ = tx.send(Ev::ProtonDb(id, steam::protondb(id).map_err(|e| format!("{e:#}")))));
        None
    }

    /// What the running job is doing (heading), its bar with Cancel, then speed, time left and its latest log line.
    fn job_row(&mut self, ui: &mut Ui, width: f32, height: f32) {
        let mut step = if self.job_step.is_empty() { self.job_label.to_string() } else { self.job_step.clone() };
        if !self.queue.is_empty() {
            step += &format!(" ({} more queued)", self.queue.len());
        }
        ui.label(RichText::new(format!("{step}…")).strong());
        let (f, text) = match self.progress {
            Some((d, 0)) => (0.0, format!("{} written", gb(d))), // total unknown
            Some((d, t)) => (d as f32 / t.max(1) as f32, format!("{} of {}  ({}%)", gb(d), gb(t), d * 100 / t.max(1))),
            None => (0.0, String::new()),
        };
        let unknown = self.progress.is_none_or(|(_, t)| t == 0);
        ui.horizontal(|ui| {
            ui.add(egui::ProgressBar::new(f).text(text).desired_width(width).desired_height(height).animate(unknown && ui.input(|i| i.focused))); // animating redraws every frame
            // every job stops at its next file or step; Wine jobs also lose their processes
            if self.busy && self.job_label != "Deleting" && ui.add_enabled(!self.cancel.load(std::sync::atomic::Ordering::Relaxed), egui::Button::new("Cancel").min_size(egui::vec2(110.0, height))).clicked() {
                self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                let n = self.running.clone().map_or(0, |pfx| crate::kill_prefix(&pfx));
                self.say(format!("Cancelling… ({n} process(es) stopped)"));
            }
        });
        if let (Some((d, t)), Some((t0, first))) = (self.progress, self.rate) {
            let secs = t0.elapsed().as_secs_f64();
            let speed = d.saturating_sub(first) as f64 / secs.max(0.001);
            if secs > 3.0 && speed > 0.0 {
                let left = t.saturating_sub(d) as f64 / speed;
                let left = if t == 0 { String::new() } else if left < 90.0 { format!("  ·  {left:.0} s left") } else { format!("  ·  about {:.0} min left", left / 60.0) };
                ui.label(RichText::new(format!("{:.0} MB/s{left}", speed / 1e6)).weak());
            }
        }
        // the latest line from the job itself (setup, umu, winetricks), so "working" is never a black box
        if let Some(last) = self.log.iter().rev().find(|l| !l.starts_with("-- ") && !l.trim().is_empty()) {
            ui.add(egui::Label::new(RichText::new(last.as_str()).monospace().weak().small()).truncate());
        }
        if matches!(self.job_label, "Installing" | "Updating") && self.job_step.starts_with("Unpacking") {
            ui.label(RichText::new("If a window with an error appears, press Cancel, then INSTALL AGAIN.").weak().small());
        }
    }

    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&settings_file()) {
            self.say(format!("error: save settings: {e:#}"));
        }
    }

    /// Store the SteamGridDB key, then fetch artwork for every game that has none yet.
    fn set_sgdb_key(&mut self, key: String) {
        self.settings.sgdb_key = key.trim().to_string();
        self.key_text = self.settings.sgdb_key.clone();
        if let Err(e) = self.settings.save(&settings_file()) {
            return self.say(format!("error: save settings: {e:#}"));
        }
        self.fetch_missing_covers();
    }

    /// Save the emulator path and rewrite the scripts of every game that uses it.
    fn emulator_changed(&mut self, k: Kind) {
        self.side = None;
        if let Err(e) = self.settings.save(&settings_file()) {
            self.say(format!("error: save settings: {e:#}"));
        }
        for i in 0..self.lib.games.len() {
            if self.lib.games[i].kind == k {
                self.write_script(i);
            }
        }
    }

    /// Add (`Some(vdf)`) or remove (`None`) the game's Steam shortcut.
    fn steam_edit(&mut self, i: usize, add: Option<PathBuf>) {
        if steam::running() {
            return self.say("error: close Steam first (it overwrites its shortcut list when it exits), then try again");
        }
        // Steam runs launch.sh: make sure it exists and matches the profile before pointing a shortcut at it
        if add.is_some() && !(self.write_script(i) && self.lib.games[i].launch_script().is_file()) {
            return self.say("error: no launch script yet: pick the game file (and the emulator) first");
        }
        let g = &self.lib.games[i];
        let r = match &add {
            Some(vdf) => steam::add(vdf, &g.name, &g.launch_script(), &g.dir).map(|(added, id)| (added, Some(id))),
            None => steam::remove(g.steam.as_ref().unwrap(), &g.launch_script()).map(|removed| {
                if let (Some(vdf), Some(id)) = (&g.steam, g.steam_appid) {
                    steam::remove_art(vdf, id);
                }
                (removed, None)
            }),
        };
        match r {
            Ok((changed, appid)) => {
                let msg = match (&add, changed) {
                    (Some(_), true) => "Added to Steam. Restart Steam to see it.",
                    (Some(_), false) => "Already in Steam.",
                    (None, _) => "Removed from Steam.",
                };
                let added = add.is_some();
                self.lib.games[i].steam = add;
                self.lib.games[i].steam_appid = appid;
                self.save();
                let dir = self.lib.games[i].dir.clone();
                self.notify(Some(dir), msg, false);
                if added {
                    self.fetch_art(i);
                }
            }
            Err(e) => self.say(format!("error: {e:#}")),
        }
    }

    /// Download Steam library artwork for the game's shortcut, off the UI thread.
    fn fetch_art(&mut self, i: usize) {
        let g = &self.lib.games[i];
        let (Some(vdf), Some(appid)) = (g.steam.clone(), g.steam_appid) else { return };
        if self.settings.sgdb_key.trim().is_empty() && g.store_id.is_none() {
            return; // nothing to fetch from; the page says how to add a source
        }
        let (name, key, sid, sgdb, tx, dir) = (g.name.clone(), self.settings.sgdb_key.clone(), g.store_id, g.sgdb_id, self.tx.clone(), g.dir.clone());
        std::thread::spawn(move || {
            let log = |s: String| _ = tx.send(Ev::Log(s));
            match steam::fetch_art(&vdf, appid, &name, &key, sid, sgdb, &log) {
                Ok(n) => log(format!("Steam artwork: {n} image(s) saved; Steam shows them after a restart")),
                Err(e) => _ = tx.send(Ev::Note(Some(dir), format!("Steam artwork: {e:#}"), true)),
            }
        });
    }
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let Some(p) = &self.pad else { return };
        for _ in p.feed(raw, self.global || self.easy_gear) {
            // Menu: the game's settings on its page, the global ones in the library
            match self.easy_open {
                Some(_) if !self.global => self.easy_gear = true,
                _ => self.global = true,
            }
        }
        if p.scrolling() {
            ctx.request_repaint(); // keep scrolling while the stick is held
        }
    }

    /// Wine processes outlive the window; an installer left running would race a later reinstall
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(pfx) = &self.running {
            crate::kill_prefix(pfx);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        if self.ctx.is_none() {
            self.ctx = Some(ui.ctx().clone());
            self.pad = pad::start(ui.ctx().clone());
            self.start_watch(ui.ctx().clone());
        }
        self.poll();
        for uri in std::mem::take(&mut self.forget) {
            ui.ctx().forget_image(&uri); // only the game whose artwork changed reloads
        }
        if !self.styled {
            easy::set_theme(&self.settings.theme);
            easy::style(ui.ctx());
            if !self.cjk && self.lib.games.iter().any(|g| easy::needs_cjk(&g.name)) {
                self.cjk = easy::load_cjk(ui.ctx());
            }
            self.styled = true;
        }
        easy::show(self, ui);
        // decoded images and their file bytes stay cached next to their GPU textures: drop them once loading
        // is done (the textures stay; 40 covers would hold ~120 MB otherwise)
        let pending = ui.ctx().has_pending_images();
        if self.images_pending && !pending {
            let l = ui.ctx().loaders();
            l.bytes.lock().iter().for_each(|b| b.forget_all());
            l.image.lock().iter().for_each(|i| i.forget_all());
        }
        self.images_pending = pending;
    }
}
