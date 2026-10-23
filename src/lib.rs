//! repackd: install local FitGirl-style (Inno Setup) repacks into per-game umu/GE-Proton prefixes.
pub mod steam;
pub mod ui;
pub mod update;

use anyhow::{anyhow, bail, Context, Result};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

/// Where long jobs send their output. Any `Fn(String)` is one (log only); a UI adds `progress` for a bar.
pub trait Report: Sync {
    fn log(&self, line: String);
    /// Bytes unpacked so far out of the bytes the installer said it needs.
    fn progress(&self, _done: u64, _total: u64) {}
    /// What the job is doing now, in a few words ("Unpacking", "Installing runtimes").
    fn step(&self, _what: &str) {}
    /// The user pressed Cancel: long steps without Wine processes (checks, copies, downloads) stop at their next file.
    fn cancelled(&self) -> bool {
        false
    }
}

/// Stop here when the user pressed Cancel.
pub(crate) fn cancel_point(log: &impl Report) -> Result<()> {
    if log.cancelled() {
        bail!("cancelled");
    }
    Ok(())
}

impl<F: Fn(String) + Sync> Report for F {
    fn log(&self, line: String) {
        self(line)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub enum Status {
    #[default]
    New,
    Installing,
    Installed,
    Failed,
}

/// How the game runs. Emulated repacks still unpack through `setup.exe` under Proton; the game file is
/// then started with a native Linux emulator.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Kind {
    #[default]
    Windows,
    Switch,
    Ps4,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Windows, Kind::Switch, Kind::Ps4];

    pub fn label(self) -> &'static str {
        match self {
            Kind::Windows => "Windows game",
            Kind::Switch => "Nintendo Switch (emulator)",
            Kind::Ps4 => "PS4 (shadPS4)",
        }
    }

    /// Files the launch picker offers.
    fn is_target(self, p: &Path) -> bool {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        match self {
            Kind::Windows => name.ends_with(".exe") && !name.starts_with("unins"),
            Kind::Switch => name.ends_with(".nsp") || name.ends_with(".xci"),
            Kind::Ps4 => name == "eboot.bin",
        }
    }

    /// Names of Linux emulators for this kind (programs, Flatpak ids and AppImage names contain them).
    fn candidates(self) -> &'static [&'static str] {
        match self {
            Kind::Windows => &[],
            Kind::Switch => &["ryujinx", "ryubing", "eden", "citron", "torzu", "sudachi", "yuzu"],
            Kind::Ps4 => &["shadps4"],
        }
    }

    /// A Linux emulator for this kind on this PC: on PATH, a Flatpak (its exported launcher), or an AppImage
    /// in the usual folders (not Downloads). First found wins.
    pub fn find_emulator(self) -> Option<PathBuf> {
        let names = self.candidates();
        if names.is_empty() {
            return None;
        }
        let path_hit = names.iter().flat_map(|n| [n.to_string(), format!("{}{}", n[..1].to_uppercase(), &n[1..])]).find_map(|n| which_path(&n));
        if path_hit.is_some() {
            return path_hit;
        }
        let h = home();
        let dirs = [
            h.join(".local/share/flatpak/exports/bin"),
            PathBuf::from("/var/lib/flatpak/exports/bin"),
            h.join("Applications"),
            h.join("AppImages"),
            h.join(".local/bin"), // not ~/Downloads or ~/Games: untrusted files there would be run as the emulator
        ];
        dirs.iter().enumerate().filter_map(|(k, d)| Some((k, std::fs::read_dir(d).ok()?))).flat_map(|(k, rd)| rd.flatten().map(move |e| (k, e.path()))).find_map(|(k, p)| {
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
            use std::os::unix::fs::PermissionsExt;
            // Flatpak launchers are app ids ("io.github.ryubing.Ryujinx"); elsewhere the name starts with the
            // emulator's ("Ryujinx-1.3.AppImage"), so "golden-tool" is no "eden"; the name first, then disk checks
            let named = names.iter().any(|c| if k < 2 { n.contains(c) } else { n.starts_with(c) }) && !n.ends_with(".exe");
            let runnable = || p.is_file() && (n.ends_with(".appimage") || std::fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0));
            (named && runnable()).then_some(p)
        })
    }

    /// Launch args for a new game of this kind; `{file}` is the picked game file.
    pub fn default_args(self, emulator: &str) -> String {
        let bin = Path::new(emulator).file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        match self {
            Kind::Windows => String::new(),
            Kind::Switch if bin.contains("ryujinx") || bin.contains("ryubing") => "{file}".into(),
            Kind::Switch => "-f -g {file}".into(), // yuzu family
            Kind::Ps4 => "-g {file}".into(),
        }
    }
}

/// Emulator paths, per machine (`settings.json` next to the library).
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Settings {
    #[serde(default)]
    pub switch: String,
    #[serde(default)]
    pub ps4: String,
    /// SteamGridDB API key for Steam artwork (free: steamgriddb.com, Preferences → API).
    #[serde(default)]
    pub sgdb_key: String,
    /// Check FitGirl's checksums before and after installing (all games).
    #[serde(default = "yes")]
    pub verify: bool,
    /// Install without the installer's own windows (all games).
    #[serde(default = "yes")]
    pub unattended: bool,
    /// Copy the files an update changes before it runs, so it can be undone.
    #[serde(default = "yes")]
    pub backup_updates: bool,
    /// Back up a game's saves each time it was played for a minute or more (the last 5 are kept).
    #[serde(default = "yes")]
    pub backup_saves_after_play: bool,
    /// Colour preset (a name from the UI's list; empty = the first).
    #[serde(default)]
    pub theme: String,
    /// Library order: "name", "played" (most recent first) or "" (as added).
    #[serde(default)]
    pub sort: String,
    /// A folder the user downloads repacks into: finished ones are offered for adding.
    #[serde(default)]
    pub watch_dir: Option<PathBuf>,
    /// Repack folders the user waved away from the watch folder's banners.
    #[serde(default)]
    pub watch_dismissed: Vec<PathBuf>,
    /// Where new games go (None = `games_root()`); existing games keep their dir.
    #[serde(default)]
    pub games_root: Option<PathBuf>,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    // a first start (no settings.json yet) gets the same as an old file without these keys: checks and silent installs on
    fn default() -> Self {
        serde_json::from_str("{}").expect("every field has a serde default")
    }
}

impl Settings {
    /// The games root in effect: the setting, else `~/Games/repackd`.
    pub fn root(&self) -> PathBuf {
        self.games_root.clone().unwrap_or_else(games_root)
    }

    pub fn load(path: &Path) -> Self {
        let mut s: Settings = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|_| {
                // kept aside, not saved over by the next change: it holds the API key and emulator paths
                let _ = std::fs::rename(path, path.with_extension("json.bad"));
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        for k in [Kind::Switch, Kind::Ps4] {
            if !Path::new(s.emulator(k)).is_file()
                && let Some(b) = k.find_emulator()
            {
                *s.emulator_mut(k) = b.display().to_string();
            }
        }
        s
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn emulator(&self, k: Kind) -> &str {
        match k {
            Kind::Windows => "",
            Kind::Switch => &self.switch,
            Kind::Ps4 => &self.ps4,
        }
    }

    pub fn emulator_mut(&mut self, k: Kind) -> &mut String {
        match k {
            Kind::Switch => &mut self.switch,
            _ => &mut self.ps4, // Windows has no emulator; callers only pass emulated kinds
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
pub struct Game {
    pub name: String,
    pub repack_dir: PathBuf,
    /// Everything we own for this game: `pfx/` and `launch.sh`. Delete removes exactly this.
    pub dir: PathBuf,
    pub status: Status,
    #[serde(default)]
    pub exe: Option<PathBuf>,
    /// The shortcuts.vdf we added this game to.
    #[serde(default)]
    pub steam: Option<PathBuf>,
    #[serde(default)]
    pub kind: Kind,
    /// Current game version ("" = unknown); set from the repack name and by each applied update.
    #[serde(default)]
    pub version: String,
    /// Update folders applied, oldest first, as "v<to> (<folder name>)".
    #[serde(default)]
    pub applied: Vec<String>,
    /// `KEY=value` per line, exported by the launch script.
    #[serde(default)]
    pub env: String,
    /// Extra launch args; for emulators `{file}` is the game file.
    #[serde(default)]
    pub args: String,
    /// umu `PROTONPATH`: "GE-Proton" (latest, downloaded by umu), a release like "GE-Proton10-34", or a path.
    #[serde(default = "default_proton")]
    pub proton: String,
    /// Bytes the installer says the game needs, read from setup.exe when the repack is added (None = unknown).
    #[serde(default)]
    pub size: Option<u64>,
    /// appid of our Steam shortcut (names its artwork; launches through `steam://rungameid/`).
    #[serde(default)]
    pub steam_appid: Option<u32>,
    /// The game's Steam store id, typed in by the user: artwork from Steam's CDN without a SteamGridDB key.
    #[serde(default)]
    pub store_id: Option<u32>,
    /// Imported game: its files stay here (unpacked elsewhere, e.g. on Windows) instead of inside the prefix.
    #[serde(default)]
    pub game_dir: Option<PathBuf>,
    /// The SteamGridDB game the user picked for artwork (searching by name found the wrong one or none).
    #[serde(default)]
    pub sgdb_id: Option<u64>,
    /// This game's own emulator (the Windows one FitGirl bundles in console repacks), used instead of the one
    /// in Settings.
    #[serde(default)]
    pub emulator: Option<PathBuf>,
    /// Seconds played through repackd, and when it was last started (Unix time).
    #[serde(default)]
    pub playtime: u64,
    #[serde(default)]
    pub last_played: Option<u64>,
    /// Launch switches that are on (`Tweak::key`s).
    #[serde(default)]
    pub tweaks: Vec<String>,
    /// setup.exe's header was read for size and version (whatever it found), so it is not read at every start.
    #[serde(default)]
    pub header_read: bool,
    /// A `.desktop` entry in the app menu that runs the launch script.
    #[serde(default)]
    pub desktop: bool,
}

/// A launch switch: environment variables and/or a wrapper program in front of the game.
pub struct Tweak {
    pub key: &'static str,
    pub label: &'static str,
    env: &'static [(&'static str, &'static str)],
    wrap: &'static [&'static str],
    /// The program it needs, if any (the switch is hidden without it).
    pub needs: Option<&'static str>,
}

pub const TWEAKS: [Tweak; 6] = [
    Tweak { key: "mangohud", label: "MangoHud overlay (FPS, temperatures)", env: &[("MANGOHUD", "1")], wrap: &[], needs: Some("mangohud") },
    Tweak { key: "gamemode", label: "GameMode (CPU governor, priority)", env: &[], wrap: &["gamemoderun"], needs: Some("gamemoderun") },
    Tweak { key: "gamescope", label: "Gamescope (fullscreen compositor)", env: &[], wrap: &["gamescope", "-f", "--"], needs: Some("gamescope") },
    Tweak { key: "fsr", label: "FSR upscaling (fullscreen; pick a lower resolution in the game)", env: &[("WINE_FULLSCREEN_FSR", "1")], wrap: &[], needs: None },
    Tweak { key: "dxvk_hud", label: "DXVK FPS counter", env: &[("DXVK_HUD", "fps")], wrap: &[], needs: None },
    Tweak { key: "dgpu", label: "Use the dedicated GPU (laptops)", env: &[("DRI_PRIME", "1")], wrap: &[], needs: None },
];

/// Artwork file types repackd reads and saves.
pub const IMAGE_EXTS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];
/// The largest each artwork is drawn (cover tile, wide banner, hero, logo on the hero), with room for HiDPI.
/// SteamGridDB has 4K heroes and 8000 px logos: kept as they are, one game page holds ~90 MB of pixels.
const ART_MAX: [(&str, u32, u32); 4] = [("cover", 600, 900), ("wide", 920, 430), ("hero", 1920, 620), ("logo", 800, 400)];

fn default_proton() -> String {
    "GE-Proton".into()
}

impl Game {
    pub fn prefix(&self) -> PathBuf {
        self.dir.join("pfx")
    }
    pub(crate) fn slug(&self) -> String {
        self.dir.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }
    pub fn install_dir(&self) -> PathBuf {
        match &self.game_dir {
            Some(d) => d.clone(),
            None => self.prefix().join("drive_c/Games").join(self.slug()),
        }
    }
    /// `install_dir` as setup.exe sees it: `C:\Games\<slug>`, or `R:\<folder>` for an imported game (`map_game_drive`).
    pub fn install_dir_win(&self) -> String {
        match &self.game_dir {
            Some(d) => format!(r"R:\{}", d.file_name().unwrap_or_default().to_string_lossy()),
            None => format!(r"C:\Games\{}", self.slug()),
        }
    }

    /// Give an imported game's folder its own drive: R: is the folder holding it (Inno takes no bare drive
    /// root as its dir). Under Z: (all of `/`), installers that check the free space of the target's drive root
    /// get the root filesystem's instead of the game disk's: RUNE's KCD2 update refused a disk with 291 GB free.
    pub fn map_game_drive(&self, log: &impl Report) -> Result<()> {
        let Some(dir) = &self.game_dir else { return Ok(()) };
        let dev = self.prefix().join("dosdevices");
        if !dev.is_dir() {
            create_prefix(self, log)?;
        }
        let link = dev.join("r:");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(dir.parent().unwrap_or(dir), &link).context("map drive R: to the game's folder")
    }
    /// The game's folder was moved from `self.dir` to `new`: point every stored path under it at the new place.
    pub fn moved_to(&mut self, new: &Path) {
        let old = std::mem::replace(&mut self.dir, new.to_path_buf());
        let re = |p: &mut PathBuf| {
            if let Ok(r) = p.strip_prefix(&old) {
                *p = new.join(r);
            }
        };
        for p in [&mut self.exe, &mut self.game_dir, &mut self.emulator].into_iter().flatten().chain([&mut self.repack_dir]) {
            re(p);
        }
    }
    /// repackd's own copy of the game's artwork (`cover`, `wide`, `hero`, `logo`), if fetched.
    pub fn art(&self, which: &str) -> Option<PathBuf> {
        IMAGE_EXTS.iter().map(|e| self.dir.join(format!("{which}.{e}"))).find(|p| p.is_file())
    }
    /// Download the game's artwork into its own dir for the library view. Also returns the Steam store id
    /// SteamGridDB knows for the game when it has none yet (for ProtonDB and Steam's own artwork).
    pub fn fetch_art(&self, sgdb_key: &str, only_missing: bool, log: &impl Report) -> Result<(usize, Option<u32>)> {
        let stems = ["cover", "wide", "hero", "logo"].map(String::from);
        let n = steam::fetch_images(&self.dir, &stems, &self.name, sgdb_key, self.store_id, self.sgdb_id, only_missing, log)?;
        self.shrink_art();
        let key = sgdb_key.trim();
        let sid = match self.store_id {
            None if !key.is_empty() => self.sgdb_id.or_else(|| steam::sgdb_game(&self.name, key).ok().map(|(id, _)| id)).and_then(|id| steam::sgdb_steam_id(id, key)),
            _ => None,
        };
        Ok((n, sid))
    }

    /// Use an image file from disk as the game's `which` artwork (cover, hero, logo), replacing any other.
    pub fn set_own_art(&self, which: &str, file: &Path) -> Result<()> {
        let ext = file.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).filter(|e| IMAGE_EXTS.contains(&e.as_str())).context("pick a PNG, JPG or WebP image")?;
        std::fs::create_dir_all(&self.dir)?;
        for old in IMAGE_EXTS {
            let _ = std::fs::remove_file(self.dir.join(format!("{which}.{old}")));
        }
        std::fs::copy(file, self.dir.join(format!("{which}.{ext}"))).with_context(|| format!("copy {}", file.display()))?;
        self.shrink_art();
        Ok(())
    }

    /// Scale artwork bigger than `ART_MAX` down in place (only the header is read when it already fits).
    /// Returns whether a file changed. A file that can't be read or written is left as it is.
    pub fn shrink_art(&self) -> bool {
        let mut changed = false;
        for (which, w, h) in ART_MAX {
            let Some(p) = self.art(which) else { continue };
            if image::image_dimensions(&p).is_ok_and(|(iw, ih)| iw <= w && ih <= h) {
                continue;
            }
            let Ok(img) = image::open(&p) else { continue };
            let ext = p.extension().unwrap_or_default().to_string_lossy().into_owned();
            let tmp = p.with_extension(format!("small.{ext}")); // same format, picked by the extension
            if img.resize(w, h, image::imageops::FilterType::Triangle).save(&tmp).is_ok() && std::fs::rename(&tmp, &p).is_ok() {
                changed = true;
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
        changed
    }
    /// The emulator FitGirl bundled with a console repack (Windows programs, run through Proton) and the
    /// launch options it needs; `{dir}` is the install folder, `{file}` the game file.
    pub fn bundled_emulator(&self) -> Option<(PathBuf, String)> {
        let d = self.install_dir();
        let found = match self.kind {
            Kind::Switch => [("emu/Ryujinx.exe", "-r {dir}\\data {file}"), ("Ryujinx.exe", "{file}"), ("eden.exe", "-g {file}"), ("citron.exe", "-g {file}"), ("suyu.exe", "-g {file}"), ("yuzu.exe", "-g {file}")]
                .iter()
                .find(|(f, _)| d.join(f).is_file())
                .copied(),
            Kind::Ps4 => [("shadPS4.exe", "-g {file}")].iter().find(|(f, _)| d.join(f).is_file()).copied(),
            Kind::Windows => None,
        };
        found.map(|(f, a)| (d.join(f), a.to_string()))
    }

    /// The emulator this game starts with: its own (bundled) one, else the one in Settings; None if neither exists.
    pub fn effective_emulator(&self, settings: &Settings) -> Option<PathBuf> {
        self.emulator.clone().filter(|e| e.is_file()).or_else(|| Some(PathBuf::from(settings.emulator(self.kind))).filter(|e| e.is_file()))
    }

    /// Fill in what an installed game needs and nobody set yet: its type (a folder with .nsp/.xci is a Switch
    /// game, with eboot.bin a PS4 one), its game file, and an emulator (the Linux one in Settings, else the
    /// one bundled with the repack). Returns whether anything changed. A game file the user picked is kept.
    pub fn auto_setup(&mut self, settings: &Settings) -> bool {
        let before = (self.kind, self.exe.clone(), self.emulator.clone(), self.args.clone());
        let unset = !self.exe.as_ref().is_some_and(|e| e.is_file());
        let files = if unset { files_under(&self.install_dir()) } else { vec![] };
        if unset
            && self.kind == Kind::Windows
            && let Some(k) = [Kind::Switch, Kind::Ps4].into_iter().find(|&k| files.iter().any(|f| k.is_target(f)))
        {
            self.kind = k;
            self.args = k.default_args(settings.emulator(k));
        }
        // a folder on a disk that is not mounted finds nothing: keep the user's pick for when it is back
        if unset && let Some(t) = rank(&files, self.kind).into_iter().next() {
            self.exe = Some(t);
        }
        if self.kind != Kind::Windows && self.effective_emulator(settings).is_none()
            && let Some((emu, args)) = self.bundled_emulator()
        {
            (self.emulator, self.args) = (Some(emu), args);
        }
        before != (self.kind, self.exe.clone(), self.emulator.clone(), self.args.clone())
    }

    /// Has a repack to (re)install from; imported games have none.
    pub fn has_repack(&self) -> bool {
        !self.repack_dir.as_os_str().is_empty()
    }
    pub(crate) fn proton_path(&self) -> &str {
        if self.proton.trim().is_empty() { "GE-Proton" } else { self.proton.trim() }
    }
    pub fn launch_script(&self) -> PathBuf {
        self.dir.join("launch.sh")
    }
    pub fn umu(&self) -> Command {
        let mut c = Command::new("umu-run");
        c.env("WINEPREFIX", self.prefix()).env("GAMEID", "umu-default").env("PROTONPATH", self.proton_path());
        // winebrowser hands URLs to the host browser (FitGirl's setup ends by opening her site), and
        // winemenubuilder copies every installer's Start-menu shortcuts and file types into the real desktop menu
        c.env("WINEDLLOVERRIDES", BASE_OVERRIDES);
        // Windows' "don't search the current dir" switch; some tools (Claude Code) export it and Wine honours it,
        // so FitGirl's unpacker can't start its helpers (rzw.exe, xtool.exe) and fails with Unarc error -2
        c.env_remove("NoDefaultCurrentDirectoryInExePath");
        c
    }
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share"))
}

pub fn data_file() -> PathBuf {
    data_home().join("repackd/library.json")
}

/// Where the desktop's app menu looks for `.desktop` entries.
pub fn apps_dir() -> PathBuf {
    data_home().join("applications")
}

pub fn settings_file() -> PathBuf {
    data_file().with_file_name("settings.json")
}

pub fn games_root() -> PathBuf {
    home().join("Games/repackd")
}

/// Write next to the target, then rename over it: a crash mid-write leaves the old file, never half a file.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().context("path has no parent")?)?;
    let tmp = path.with_extension("tmp");
    // 0600: settings.json holds the SteamGridDB key (mode applies only when the tmp file is created)
    let _ = std::fs::remove_file(&tmp);
    let w = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(data));
    w.with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))
}

/// Read at most `max` bytes of a regular file; links, devices and FIFOs are refused (untrusted folders).
pub fn read_capped(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    if !path.symlink_metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut v = vec![];
    std::fs::File::open(path)?.take(max).read_to_end(&mut v)?;
    Ok(v)
}

/// `p` resolved without touching the disk ("a/../b" becomes "b").
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => _ = out.pop(),
            c => out.push(c),
        }
    }
    out
}

fn is_regular(p: &Path) -> bool {
    p.symlink_metadata().is_ok_and(|m| m.file_type().is_file())
}

const SETUP_CAP: u64 = 64 << 20;
const MD5_CAP: u64 = 8 << 20;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

pub struct Library {
    pub path: PathBuf,
    pub root: PathBuf,
    pub games: Vec<Game>,
}

impl Library {
    pub fn load(path: PathBuf, root: PathBuf) -> Result<Self> {
        let games = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let mut games: Vec<Game> = games;
        for g in games.iter_mut().filter(|g| g.has_repack() && !g.header_read && (g.size.is_none() || g.version.is_empty())) {
            // libraries from before sizes and versions were read from the installer; once, found or not
            let Some(setup) = find_setup(&g.repack_dir).ok().and_then(|p| read_capped(&p, SETUP_CAP).ok()) else { continue };
            g.header_read = true;
            g.size = g.size.or_else(|| setup_size(&setup));
            if g.version.is_empty() {
                g.version = setup_version(&setup).unwrap_or_default();
            }
        }
        for g in games.iter_mut().filter(|g| g.version.is_empty()) {
            g.version = g.game_dir.as_deref().and_then(version_file).unwrap_or_default(); // imported before this was read
        }
        for g in games.iter_mut().filter(|g| g.status == Status::Installed && g.exe.is_none()) {
            g.exe = targets(&g.install_dir(), g.kind).into_iter().next(); // installed before game files were picked up front
        }
        for g in games.iter().filter(|g| g.dir.starts_with(&root) && g.dir.is_dir()) {
            mark_game(&g.dir); // one-time backfill: games stay deletable after the root changes
        }
        Ok(Self { path, root, games })
    }

    pub fn save(&self) -> Result<()> {
        write_atomic(&self.path, &serde_json::to_vec_pretty(&self.games)?)
    }

    /// A game dir under `root` named after the game, unused on disk and in the library.
    fn new_dir(&self, name: &str) -> PathBuf {
        let base = slug(name);
        let mut dir = self.root.join(&base);
        for n in 2.. {
            if !dir.exists() && !self.games.iter().any(|g| g.dir == dir) {
                break;
            }
            dir = self.root.join(format!("{base}-{n}"));
        }
        mark_game(&dir); // marked from birth, so a later root change cannot orphan it
        dir
    }

    /// Add a repack folder; the game dir gets a unique slug of the name.
    pub fn add(&mut self, repack_dir: PathBuf) -> Result<usize> {
        find_setup(&repack_dir)?;
        if let Some(g) = self.games.iter().find(|g| g.repack_dir == repack_dir) {
            bail!("this repack is already in the library as \"{}\"", g.name);
        }
        let folder = repack_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let name = clean_name(&folder);
        let dir = self.new_dir(&name);
        let setup = find_setup(&repack_dir).ok().and_then(|p| read_capped(&p, SETUP_CAP).ok());
        let size = setup.as_deref().and_then(setup_size);
        let version = update::versions(&folder).pop().or_else(|| setup.as_deref().and_then(setup_version)).unwrap_or_default();
        self.games.push(Game { name, repack_dir, dir, version, proton: default_proton(), size, header_read: true, ..Default::default() });
        Ok(self.games.len() - 1)
    }

    /// Add a game that is already unpacked (no setup.exe): it keeps its folder and gets a prefix of its own.
    pub fn import(&mut self, game_dir: PathBuf) -> Result<usize> {
        if !game_dir.is_dir() {
            bail!("{} is not a folder", game_dir.display());
        }
        if let Some(g) = self.games.iter().find(|g| g.game_dir.as_ref() == Some(&game_dir)) {
            bail!("this folder is already in the library as \"{}\"", g.name);
        }
        let files = files_under(&game_dir); // one walk for the kind and the game file (an NTFS disk is slow to walk)
        let kind = [Kind::Switch, Kind::Ps4, Kind::Windows].into_iter().find(|&k| files.iter().any(|f| k.is_target(f))).context("no game in this folder: no .exe, .nsp/.xci or eboot.bin")?;
        let folder = game_dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let name = clean_name(&folder);
        let dir = self.new_dir(&name);
        let version = update::versions(&folder).pop().or_else(|| version_file(&game_dir)).unwrap_or_default();
        let exe = rank(&files, kind).into_iter().next(); // the picker's best guess, so Play and Steam have a file
        self.games.push(Game { name, dir, status: Status::Installed, kind, version, proton: default_proton(), exe, game_dir: Some(game_dir), ..Default::default() });
        Ok(self.games.len() - 1)
    }

    /// Remove the game's own dir (prefix, installed files, launch script) and the game; see `delete_files`.
    pub fn delete(&mut self, i: usize) -> Result<()> {
        delete_files(&self.root, &self.games[i])?;
        self.games.remove(i);
        Ok(())
    }


}

/// A folder name as a game name: drops "[FitGirl Repack]" tags, a trailing "+ 5 DLCs + Bonus" part and a trailing
/// version (that one is read from the folder name separately, so it is kept as the game's version). Never empty.
fn clean_name(folder: &str) -> String {
    let mut s = folder.to_string();
    // bracketed tags that say "repack"; an unclosed bracket is left alone
    while let Some((a, b)) = s.char_indices().find_map(|(i, c)| {
        let close = match c { '[' => ']', '(' => ')', _ => return None };
        let end = i + s[i..].find(close)?;
        s[i..=end].to_lowercase().contains("repack").then_some((i, end + 1))
    }) {
        s.replace_range(a..b, "");
    }
    // " + " starts an extras list when it begins with a number or names extras ("Dungeons + Dragons" stays)
    let extra = |r: &str| {
        let l = r.to_lowercase();
        r.starts_with(|c: char| c.is_ascii_digit()) || ["dlc", "bonus", "ost", "soundtrack"].iter().any(|k| l.contains(k))
    };
    if let Some(i) = s.match_indices(" + ").map(|(i, _)| i).find(|&i| extra(&s[i + 3..])) {
        s.truncate(i);
    }
    let trim = |s: &str| s.trim_matches(|c: char| c.is_whitespace() || c == '-' || c == '–' || c == ',').to_string();
    let mut s = trim(&s);
    if let Some((head, last)) = s.rsplit_once(char::is_whitespace)
        && last.starts_with(['v', 'V'])
        && !update::versions(last).is_empty()
    {
        s = trim(head);
    }
    if s.is_empty() { folder.to_string() } else { s }
}

/// Empty file in every game dir: `delete_files` accepts games outside the games root that have it.
const MOVED_MARK: &str = ".repackd-game";

/// Write the marker into an existing game dir, or create the dir with it for a new game (best effort: a missing marker only matters after a root change).
fn mark_game(dir: &Path) {
    if dir.join(MOVED_MARK).is_file() {
        return;
    }
    if !dir.exists() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(dir.join(MOVED_MARK), b"");
}

/// Delete a game's own dir (prefix, installed files, launch script) and its Steam shortcut, after backing up
/// its saves; never the repack folder, and never an imported game's folder (that lives outside `dir`).
/// Runs off the UI thread: tar and a 100 GB delete take a while.
pub fn delete_files(root: &Path, g: &Game) -> Result<()> {
    // guard against a hand-edited library.json pointing somewhere else; checked before anything changes
    // a game outside the root (moved, or from an old root) says so with a marker
    if !(g.dir.starts_with(root) || g.dir.join(MOVED_MARK).is_file()) || g.dir == root || g.dir.components().any(|c| c == std::path::Component::ParentDir) {
        bail!("refusing to delete {}: not inside {}", g.dir.display(), root.display());
    }
    if !game_pids(g).is_empty() {
        bail!("{} is running (game, installer or tool): close it or press Cancel first", g.name);
    }
    if g.steam.is_some() && steam::running() {
        bail!("close Steam first: this game has a Steam shortcut to remove");
    }
    if g.desktop {
        remove_desktop(g, &apps_dir());
    }
    if g.prefix().is_dir() {
        // the prefix holds the saves: keep a copy, and delete nothing if that fails
        backup_saves(g, false).context("saves could not be backed up, so nothing was deleted")?;
    }
    if let Some(vdf) = &g.steam {
        steam::remove(vdf, &g.launch_script())?;
        if let Some(id) = g.steam_appid {
            steam::remove_art(vdf, id);
        }
    }
    if g.dir.exists() {
        // renamed first (instant): a delete stopped half-way leaves a ".trash-" folder, cleared at the next start
        // beside the game when it is outside the root: the rename stays on its filesystem
        let trash = if g.dir.starts_with(root) { root } else { g.dir.parent().unwrap_or(root) }.join(format!(".trash-{}", g.slug()));
        let _ = std::fs::remove_dir_all(&trash);
        std::fs::rename(&g.dir, &trash).with_context(|| format!("move {} aside", g.dir.display()))?;
        std::fs::remove_dir_all(&trash).with_context(|| format!("delete {}", trash.display()))?;
    }
    Ok(())
}

/// Remove what a delete stopped half-way left behind in the games root and beside every game (old roots).
pub fn clear_trash(root: &Path, games: &[Game]) {
    let mut dirs = vec![root];
    for p in games.iter().filter_map(|g| g.dir.parent()) {
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    for d in dirs {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().starts_with(".trash-") {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// Why `p` cannot be the games root: inside a game's dir or a `.trash-` folder, or not writable.
pub fn root_refusal(p: &Path, games: &[Game]) -> Result<()> {
    let p = p.canonicalize().unwrap_or_else(|_| lexical(p));
    let canon = |d: &Path| d.canonicalize().unwrap_or_else(|_| d.to_path_buf());
    if games.iter().any(|g| p.starts_with(canon(&g.dir))) || p.components().any(|c| c.as_os_str().to_string_lossy().starts_with(".trash-")) {
        bail!("{} is inside a game's folder or a trash folder: pick another", p.display());
    }
    std::fs::create_dir_all(&p).with_context(|| format!("cannot create {}", p.display()))?;
    let probe = p.join(format!(".repackd-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").with_context(|| format!("{} is not writable", p.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// The version FitGirl's setup leaves in the game folder as the name of its checksum list ("v1.5.1.blake3").
fn version_file(dir: &Path) -> Option<String> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        let list = [".blake3", ".md5", ".sfv"].iter().any(|x| n.ends_with(x));
        let stem = n.rsplit_once('.').map_or(n.as_str(), |(s, _)| s);
        (list && n.starts_with('v')).then(|| update::versions(stem).pop()).flatten()
    })
}

pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_matches('-');
    if s.is_empty() { "game".into() } else { s.into() }
}

pub fn find_setup(repack: &Path) -> Result<PathBuf> {
    std::fs::read_dir(repack)
        .with_context(|| format!("read {}", repack.display()))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("setup.exe")) && is_regular(p))
        .with_context(|| format!("no setup.exe in {}", repack.display()))
}

/// Proton builds to offer: umu's auto-downloaded latest GE-Proton first, then every build found in Steam's
/// compatibility tools folders (GE-Proton, proton-cachyos, ...) and Valve's own Proton in every Steam library.
/// (label, value for `Game::proton`).
pub fn protons() -> Vec<(String, String)> {
    let mut out = vec![("Automatic (recommended)".to_string(), default_proton())];
    let h = home();
    let steam = h.join(".local/share/Steam");
    let mut roots = vec![steam.join("compatibilitytools.d"), h.join(".steam/root/compatibilitytools.d"), PathBuf::from("/usr/share/steam/compatibilitytools.d")];
    // Proton Experimental/Hotfix live in whichever library Steam put them in; libraryfolders.vdf lists them all
    let folders = std::fs::read_to_string(steam.join("steamapps/libraryfolders.vdf")).unwrap_or_default();
    let mut libs: Vec<PathBuf> = folders.lines().filter_map(|l| l.trim().strip_prefix("\"path\"")).map(|v| PathBuf::from(v.trim().trim_matches('"'))).collect();
    libs.push(steam);
    roots.extend(libs.iter().map(|l| l.join("steamapps/common")));
    let mut found: Vec<PathBuf> = roots.iter().filter_map(|r| std::fs::read_dir(r).ok()).flat_map(|rd| rd.flatten().map(|e| e.path())).filter(|p| p.join("proton").is_file()).collect();
    found.sort_by_key(|p| p.file_name().map(|n| n.to_ascii_lowercase()));
    found.dedup_by_key(|p| std::fs::canonicalize(&p).unwrap_or(p.clone())); // ~/.steam/root links to the same Steam
    for p in found {
        out.push((p.file_name().unwrap_or_default().to_string_lossy().into_owned(), p.display().to_string()));
    }
    out
}

/// Proton builds repackd can download: (label, GitHub repo).
pub const PROTON_SOURCES: [(&str, &str); 3] = [
    ("GE-Proton", "GloriousEggroll/proton-ge-custom"),
    ("Proton-CachyOS", "CachyOS/proton-cachyos"),
    ("Proton-EM", "BananaWorks07/Proton"),
];

/// A Proton release on GitHub: its tag, the plain x86-64 tarball and its checksum file.
#[derive(Clone, Debug)]
pub struct ProtonRelease {
    pub source: &'static str,
    pub tag: String,
    pub url: String,
    pub sha_url: Option<String>,
    pub size: u64,
}

impl ProtonRelease {
    /// Whether an installed folder is this release: GE-Proton11-11-x86_64, proton-cachyos-11.0-…-slr-x86_64, proton-EM-10.0-37.
    pub fn installed_as(&self, folder: &str) -> bool {
        folder == self.tag || folder.ends_with(&self.tag) || folder.contains(&format!("{}-", self.tag))
    }
}

/// Where downloaded Proton builds go: Steam's compatibility tools folder, which umu and Steam both read.
pub fn compat_dir() -> PathBuf {
    home().join(".local/share/Steam/compatibilitytools.d")
}

/// Sort key of a Proton name by its numbers: GE-Proton10-5 > GE-Proton9-27.
fn proton_ver(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).collect()
}

/// How many games use this Proton (`value` as in `protons()`). Games on the default "GE-Proton" count for the
/// newest installed GE-Proton folder, which is what umu runs.
pub fn proton_users(games: &[Game], protons: &[(String, String)], value: &str) -> usize {
    let name = |v: &str| v.rsplit('/').next().unwrap_or(v).to_string();
    let newest_ge = protons.iter().filter(|(_, v)| v.contains('/') && name(v).starts_with("GE-Proton")).max_by_key(|(_, v)| proton_ver(&name(v)));
    let is_default = newest_ge.is_some_and(|(_, v)| v == value);
    games.iter().filter(|g| g.proton_path() == value || (is_default && g.proton_path() == "GE-Proton")).count()
}

/// A GE-Proton release newer than every installed GE-Proton (`releases` come newest first).
pub fn newer_ge<'a>(releases: &'a [ProtonRelease], protons: &[(String, String)]) -> Option<&'a ProtonRelease> {
    let newest = protons.iter().filter(|(l, v)| v.contains('/') && l.starts_with("GE-Proton")).map(|(l, _)| proton_ver(l)).max();
    releases.iter().find(|r| r.source == "GE-Proton").filter(|r| Some(proton_ver(&r.tag)) > newest)
}

/// Delete an installed Proton build, only from a user-writable compatibilitytools.d (never /usr).
pub fn delete_proton(dir: &Path) -> Result<()> {
    let ok = dir.parent().is_some_and(|p| p.file_name().is_some_and(|n| n == "compatibilitytools.d")) && !dir.starts_with("/usr");
    // a link could point anywhere: only a real folder goes
    if !ok || !std::fs::symlink_metadata(dir)?.is_dir() {
        bail!("{} is not a Proton in a compatibilitytools.d folder", dir.display());
    }
    std::fs::remove_dir_all(dir).with_context(|| format!("delete {}", dir.display()))
}

/// Disk use of a game: [game folder, Wine prefix without the game inside it, repack folder].
pub fn disk_sizes(g: &Game) -> [u64; 3] {
    let game = dir_size(&g.install_dir());
    let pfx = dir_size(&g.prefix()).saturating_sub(if g.install_dir().starts_with(g.prefix()) { game } else { 0 });
    [game, pfx, if g.has_repack() { dir_size(&g.repack_dir) } else { 0 }]
}

/// The newest releases of every source in `PROTON_SOURCES`, newest first per source.
pub fn proton_releases() -> Result<Vec<ProtonRelease>> {
    let (mut out, mut errs) = (vec![], vec![]);
    for (source, repo) in PROTON_SOURCES {
        // one source down (rate limit, moved repo) still lists the others
        let r = (|| {
            let o = Command::new("curl").args(["-sSfL", "--proto", "=https", "-m", "30"]).arg(format!("https://api.github.com/repos/{repo}/releases?per_page=15")).output().context("start curl")?;
            if !o.status.success() {
                bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
            }
            releases_from(source, &serde_json::from_slice(&o.stdout)?)
        })();
        match r {
            Ok(v) => out.extend(v),
            Err(e) => errs.push(format!("{source}: {e:#}")),
        }
    }
    if out.is_empty() && !errs.is_empty() {
        bail!("GitHub: {}", errs.join("; "));
    }
    Ok(out)
}

fn releases_from(source: &'static str, v: &serde_json::Value) -> Result<Vec<ProtonRelease>> {
    let mut out = vec![];
    for r in v.as_array().context("unexpected GitHub answer")? {
        let assets = r["assets"].as_array().cloned().unwrap_or_default();
        let name = |a: &serde_json::Value| a["name"].as_str().unwrap_or_default().to_string();
        // the build for any x86-64 CPU: not the ARM ones, nor CachyOS' x86_64_v3 (needs a newer CPU)
        let Some(tar) = assets.iter().find(|a| {
            let n = name(a);
            (n.ends_with(".tar.gz") || n.ends_with(".tar.xz")) && !["aarch64", "arm64", "x86_64_v"].iter().any(|x| n.contains(x))
        }) else { continue };
        let stem = name(tar).trim_end_matches(".tar.gz").trim_end_matches(".tar.xz").to_string();
        let sha = assets.iter().find(|a| [format!("{stem}.sha512sum"), format!("{stem}.sha256sum")].contains(&name(a)));
        out.push(ProtonRelease {
            source,
            tag: r["tag_name"].as_str().unwrap_or_default().to_string(),
            url: tar["browser_download_url"].as_str().unwrap_or_default().to_string(),
            sha_url: sha.and_then(|a| a["browser_download_url"].as_str()).map(String::from),
            size: tar["size"].as_u64().unwrap_or(0),
        });
    }
    Ok(out)
}

/// Protocols curl may use for downloads named by GitHub's answers (tests download from local files).
const PROTO: &str = if cfg!(test) { "=https,file" } else { "=https" };

/// Download a Proton release, check it against its published SHA-512/256 sum, and unpack it into `compat_dir`.
pub fn install_proton(rel: &ProtonRelease, dir: &Path, log: &impl Report) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let safe = rel.tag.replace('/', "_");
    let tmp = dir.join(format!(".{safe}.download"));
    let stage = dir.join(format!(".{safe}.unpack"));
    log.step(&format!("Downloading {}", rel.tag));
    // a dropped connection must not hang the job forever: give up below 1 KB/s for a minute
    // the URLs come from GitHub's answer: https only, and "--" so none is read as an option
    let mut child = Command::new("curl").args(["-sSfL", "--proto", PROTO, "--speed-limit", "1024", "--speed-time", "60", "-o"]).arg(&tmp).arg("--").arg(&rel.url).stderr(Stdio::piped()).spawn().context("start curl")?;
    let st = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if log.cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(&tmp);
            bail!("cancelled");
        }
        log.progress(std::fs::metadata(&tmp).map_or(0, |m| m.len()), rel.size.max(1));
        std::thread::sleep(Duration::from_millis(400));
    };
    let result = (|| {
        if !st.success() {
            bail!("download failed ({st})");
        }
        if let Some(sha_url) = &rel.sha_url {
            log.step("Checking the download");
            let o = Command::new("curl").args(["-sSfL", "--proto", "=https", "-m", "30", "--", sha_url]).output()?;
            let want = String::from_utf8_lossy(&o.stdout).split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
            let tool = if sha_url.ends_with(".sha256sum") { "sha256sum" } else { "sha512sum" };
            let got = Command::new(tool).arg(&tmp).output()?;
            let got = String::from_utf8_lossy(&got.stdout).split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
            if want.is_empty() || want != got {
                bail!("the download does not match its published checksum: try again");
            }
        }
        log.step(&format!("Unpacking {}", rel.tag));
        // into a staging folder first: a half-unpacked build (disk full) would look installed and break games
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(&stage)?;
        let o = Command::new("tar").arg("-xf").arg(&tmp).arg("-C").arg(&stage).output()?; // .tar.gz or .tar.xz
        if !o.status.success() {
            bail!("unpacking failed: {}", String::from_utf8_lossy(&o.stderr).trim());
        }
        for e in std::fs::read_dir(&stage)?.flatten() {
            let to = dir.join(e.file_name());
            let _ = std::fs::remove_dir_all(&to); // only a broken copy of this same build can be in the way
            std::fs::rename(e.path(), &to).with_context(|| format!("move {} into place", to.display()))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir_all(&stage);
    result.map(|()| log.log(format!("{} installed in {}", rel.tag, dir.display())))
}

#[derive(Debug, PartialEq)]
enum Family {
    Arch,
    Debian,
    Fedora,
    Suse,
    Unknown,
}

/// Distro family from os-release text: `ID` first, then each `ID_LIKE` word.
fn distro_family(os_release: &str) -> Family {
    let val = |key: &str| os_release.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).unwrap_or("").trim_matches('"').to_owned();
    let words = format!("{} {}", val("ID"), val("ID_LIKE"));
    words
        .split_whitespace()
        .find_map(|w| match w {
            "arch" => Some(Family::Arch),
            "debian" | "ubuntu" => Some(Family::Debian),
            "fedora" => Some(Family::Fedora),
            "opensuse" | "suse" | "opensuse-leap" | "opensuse-tumbleweed" => Some(Family::Suse),
            _ => None,
        })
        .unwrap_or(Family::Unknown)
}

/// The command that installs `tool` (a binary name) on this system, for error and hover texts.
pub fn install_hint(tool: &str) -> String {
    static FAMILY: std::sync::OnceLock<Family> = std::sync::OnceLock::new();
    let family = FAMILY.get_or_init(|| distro_family(&std::fs::read_to_string("/etc/os-release").unwrap_or_default()));
    install_hint_for(family, tool)
}

fn install_hint_for(family: &Family, tool: &str) -> String {
    let pkg = match (family, tool) {
        (Family::Debian, "umu-run") => return "get umu-launcher from github.com/Open-Wine-Components/umu-launcher/releases (.deb)".into(),
        (Family::Suse, "umu-run") => return "get umu-launcher from github.com/Open-Wine-Components/umu-launcher/releases".into(),
        (_, "umu-run") => "umu-launcher",
        (_, "7z") => "7zip",
        (Family::Debian, "notify-send") => "libnotify-bin",
        (Family::Suse, "notify-send") => "libnotify-tools",
        (_, "notify-send") => "libnotify",
        (_, "gamemoderun") => "gamemode",
        (_, t) => t, // winetricks, mangohud, gamescope
    };
    let cmd = match family {
        Family::Arch => "sudo pacman -S",
        Family::Debian => "sudo apt install",
        Family::Fedora => "sudo dnf install",
        Family::Suse => "sudo zypper install",
        Family::Unknown => "install",
    };
    format!("{cmd} {pkg}")
}

/// `PATH` lookup: the umu-run / winetricks check at start and emulator detection.
pub fn which_path(bin: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(bin)).find(|p| p.is_file())
}

// ---------- saves ----------

/// The Windows profile folders games save into, inside each game's own prefix.
const SAVE_DIRS: &[&str] = &["AppData/Roaming", "AppData/Local", "AppData/LocalLow", "Documents", "Saved Games"];
/// Caches and Windows/Wine's own data in there: large and not saves.
const SAVE_SKIP: &[&str] = &[
    "AppData/Local/Temp",
    "AppData/Local/Microsoft",
    "AppData/Local/D3DSCache",
    "AppData/Local/NVIDIA",
    "AppData/Local/NVIDIA Corporation",
    "AppData/Local/AMD",
    "AppData/Roaming/Microsoft",
];

fn profile(g: &Game) -> PathBuf {
    g.prefix().join("drive_c/users/steamuser")
}

/// `~/Games/repackd-saves/<game>`: next to the games root, so deleting a game never deletes its backups.
pub fn saves_dir(g: &Game) -> PathBuf {
    let root = g.dir.parent().unwrap_or(Path::new("."));
    let name = format!("{}-saves", root.file_name().unwrap_or_default().to_string_lossy());
    root.with_file_name(name).join(g.slug())
}

/// Backups of this game's saves, newest first.
pub fn backups(g: &Game) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(saves_dir(g)).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().ends_with(".tar.gz")).collect();
    // newest first by the time written (names sort "x-auto.tar.gz" after "x-auto-2.tar.gz")
    v.sort_by_key(|p| std::cmp::Reverse((std::fs::metadata(p).and_then(|m| m.modified()).ok(), p.clone())));
    v
}

/// Pack the profile folders into `saves_dir/<date>.tar.gz` (`<date>-auto.tar.gz` for the ones made after
/// playing, which alone are pruned). None when the prefix has no profile yet.
pub fn backup_saves(g: &Game, auto: bool) -> Result<Option<PathBuf>> {
    let prof = profile(g);
    let dirs: Vec<&str> = SAVE_DIRS.iter().copied().filter(|d| prof.join(d).is_dir()).collect();
    if dirs.is_empty() {
        return Ok(None);
    }
    let out_dir = saves_dir(g);
    std::fs::create_dir_all(&out_dir)?;
    let stamp = Command::new("date").arg("+%Y-%m-%d_%H-%M-%S").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    let stamp = if auto { format!("{stamp}-auto") } else { stamp };
    let mut out = out_dir.join(format!("{stamp}.tar.gz"));
    let mut n = 2;
    while out.exists() {
        out = out_dir.join(format!("{stamp}-{n}.tar.gz")); // two backups in one second
        n += 1;
    }
    let mut c = Command::new("tar");
    c.arg("-czf").arg(&out).arg("-C").arg(&prof);
    for s in SAVE_SKIP {
        c.arg(format!("--exclude={s}"));
    }
    let o = c.args(&dirs).output().context("start tar")?;
    if !o.status.success() {
        let _ = std::fs::remove_file(&out);
        bail!("tar: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(Some(out))
}

/// Keep only the newest `keep` automatic save backups of this game (manual ones and those made before a
/// delete stay).
pub fn prune_backups(g: &Game, keep: usize) {
    // by the file name: a game folder like "nier-automata" must not make every backup look automatic
    for old in backups(g).into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with("-auto.tar.gz") || n.to_string_lossy().contains("-auto-"))).skip(keep) {
        let _ = std::fs::remove_file(old);
    }
}

/// Delete the repack after the game is installed, to free its space: only FitGirl's own files (setup.exe,
/// the files her checksum list names, *.bin, her MD5 folder and .bat), never the folder with anything else in
/// it. A repack unpacked straight into ~/Downloads keeps the rest of Downloads. Returns how many other
/// files were kept (0: the folder is gone too).
pub fn delete_repack(g: &Game) -> Result<usize> {
    if g.status != Status::Installed || !g.has_repack() {
        bail!("only the repack of an installed game can be deleted");
    }
    let dir = &g.repack_dir;
    let setup = find_setup(dir).context("this does not look like the repack folder anymore")?;
    if g.install_dir().starts_with(dir) || g.dir.starts_with(dir) {
        bail!("the game is installed inside its repack folder");
    }
    // "..\\fg-01.bin" in MD5/fitgirl-bins.md5: resolved without touching the disk, and kept inside the folder
    let mut gone = vec![setup];
    // only FitGirl's own list: another .md5 lying there (the user's photos.md5) names the user's files
    let fitgirls = |m: &PathBuf| {
        let in_md5_dir = m.parent().and_then(|p| p.file_name()).is_some_and(|n| n.eq_ignore_ascii_case("md5"));
        in_md5_dir || m.file_name().is_some_and(|n| n.to_string_lossy().to_ascii_lowercase().starts_with("fitgirl"))
    };
    if let Some(md5) = find_md5(dir).filter(fitgirls) {
        let base = md5.parent().unwrap_or(dir);
        let text = decode_ansi(read_capped(&md5, MD5_CAP)?);
        gone.extend(parse_md5(&text).into_iter().map(|(_, rel)| lexical(&base.join(rel.replace('\\', "/")))));
        gone.push(md5);
    }
    for e in std::fs::read_dir(dir)?.flatten() {
        let n = e.file_name().to_string_lossy().to_ascii_lowercase();
        // her bins by their names; other .bin files (an emulator BIOS in Downloads) stay
        let bin = n.ends_with(".bin") && (n.starts_with("fg-") || n.starts_with("fitgirl-"));
        if e.path().is_file() && (bin || (n.starts_with("verify bin files") && n.ends_with(".bat"))) {
            gone.push(e.path());
        }
    }
    // and only what really lies inside the folder: a linked subfolder could point anywhere
    let real_dir = std::fs::canonicalize(dir)?;
    let inside = |p: &Path| p.parent().and_then(|d| std::fs::canonicalize(d).ok()).is_some_and(|d| d.starts_with(&real_dir));
    for p in gone.iter().filter(|p| p.starts_with(dir) && p.parent() != dir.parent() && inside(p)) {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("delete {}", p.display())),
            _ => {}
        }
    }
    // her MD5 folder (lists, QuickSFV) once its checksum list is gone
    if let Some(md5_dir) = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("md5"))) {
        std::fs::remove_dir_all(&md5_dir).with_context(|| format!("delete {}", md5_dir.display()))?;
    }
    let left = std::fs::read_dir(dir)?.count();
    if left == 0 {
        std::fs::remove_dir(dir)?;
    }
    Ok(left)
}

/// The game's version as its repack says it: from the folder name, else from setup.exe's info page.
pub fn repack_version(repack: &Path) -> String {
    let name = repack.file_name().unwrap_or_default().to_string_lossy();
    update::versions(&name).pop().or_else(|| find_setup(repack).ok().and_then(|p| read_capped(&p, SETUP_CAP).ok()).and_then(|b| setup_version(&b))).unwrap_or_default()
}

/// Processes of this game: its Wine prefix's, plus for a native emulator the ones started with its game file
/// (they have no prefix).
pub fn game_pids(g: &Game) -> Vec<u32> {
    use std::os::unix::ffi::OsStrExt;
    let mut pids = prefix_pids(&g.prefix());
    if g.kind != Kind::Windows
        && let Some(file) = &g.exe
    {
        let want = file.as_os_str().as_bytes();
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
            if std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c.split(|&b| b == 0).any(|a| a == want)) && !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Unpack a backup over the profile: files in it replace the current ones, others are left alone.
pub fn restore_saves(g: &Game, archive: &Path) -> Result<()> {
    if !prefix_pids(&g.prefix()).is_empty() {
        bail!("{} is running: close it first, or it writes its own saves over the restored ones when it exits", g.name);
    }
    let prof = profile(g);
    std::fs::create_dir_all(&prof)?;
    let o = Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(&prof).output().context("start tar")?;
    if !o.status.success() {
        bail!("tar: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

// ---------- installer size ----------

/// The space an Inno 5 setup asks for: its header's `ExtraDiskSpaceRequired` (FitGirl puts the unpacked game
/// size there). The header is LZMA in 4 KiB CRC-checked blocks after the last "Inno Setup Setup Data"; the
/// field sits after the (empty) 20-byte password hash and 8-byte salt and before `SlicesPerDisk` = 1.
/// None for any other layout (Inno 6, uncompressed, no match): the size is then simply unknown.
pub fn setup_size(exe: &[u8]) -> Option<u64> {
    size_from(&setup_header(exe)?)
}

/// The game version FitGirl states on the installer's info page: "Game version: Steam BuildID 20663174"
/// (kept as the number, like her update folders name it) or "Based on P2P v1.0.1 Switch release". Only those
/// two lines count: the same page also lists bundled emulator and firmware versions.
pub fn setup_version(exe: &[u8]) -> Option<String> {
    version_from(&setup_header(exe)?)
}

fn version_from(header: &[u8]) -> Option<String> {
    for line in rtf_lines(header) {
        let low = line.to_ascii_lowercase();
        if let Some(rest) = low.strip_prefix("game version:") {
            let rest = rest.split('(').next().unwrap_or(rest); // "(Denuvoless)"
            let number = rest.split(|c: char| !c.is_ascii_digit() && c != '.').filter(|t| t.chars().any(|c| c.is_ascii_digit())).max_by_key(|t| t.len());
            return update::versions(rest).pop().or(number.map(|n| n.trim_matches('.').to_string()));
        }
        if low.starts_with("based on")
            && let Some(v) = update::versions(&line).into_iter().next()
        {
            return Some(v);
        }
    }
    None
}

/// Plain-text lines of the RTF pages in an Inno header (control words and braces dropped).
fn rtf_lines(header: &[u8]) -> Vec<String> {
    let t: String = header.iter().map(|&b| b as char).collect(); // Latin-1: RTF keeps non-ASCII as \'xx escapes
    let mut out = String::new();
    let mut it = t.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' | '}' => {}
            '\\' => {
                // \word123 plus one optional space, or \'xx, or an escaped symbol
                match it.peek() {
                    Some('\'') => {
                        it.next();
                        it.next();
                        it.next();
                    }
                    Some(c) if c.is_ascii_alphabetic() => {
                        let mut word = String::new();
                        while let Some(c) = it.next_if(|c| c.is_ascii_alphabetic()) {
                            word.push(c);
                        }
                        while it.next_if(|c| c.is_ascii_digit() || *c == '-').is_some() {}
                        it.next_if_eq(&' ');
                        if word == "par" || word == "line" {
                            out.push('\n'); // paragraph and line breaks end a line even without a newline after them
                        }
                    }
                    _ => _ = it.next(),
                }
            }
            c => out.push(c),
        }
    }
    out.split(['\r', '\n']).map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// The decompressed Inno 5 setup header (see `setup_size`), or None for other layouts.
pub(crate) fn setup_header(exe: &[u8]) -> Option<Vec<u8>> {
    let sig = b"Inno Setup Setup Data";
    let mut p = exe.windows(sig.len()).rposition(|w| w == sig)? + 64; // the first hit can be a string in the loader
    let size = u32::from_le_bytes(exe.get(p + 4..p + 8)?.try_into().ok()?) as usize;
    if *exe.get(p + 8)? != 1 {
        return None;
    }
    p += 9;
    let end = p.checked_add(size)?;
    let mut raw = vec![];
    while p + 4 < end {
        let n = 4096.min(end - p - 4);
        let crc = u32::from_le_bytes(exe.get(p..p + 4)?.try_into().ok()?);
        let data = exe.get(p + 4..p + 4 + n)?;
        if crc32fast::hash(data) != crc {
            return None;
        }
        raw.extend_from_slice(data);
        p += 4 + n;
    }
    let opts = lzma_rs::decompress::Options { unpacked_size: lzma_rs::decompress::UnpackedSize::UseProvided(None), memlimit: Some(64 << 20), ..Default::default() };
    // a crafted header must not expand to gigabytes (it is read when a repack is added and at start)
    struct Capped(Vec<u8>);
    impl std::io::Write for Capped {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.0.len() + b.len() > 64 << 20 {
                return Err(std::io::Error::other("setup header too large"));
            }
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut h = Capped(vec![]);
    lzma_rs::lzma_decompress_with_options(&mut &raw[..], &mut h, &opts).ok()?;
    Some(h.0)
}

fn size_from(h: &[u8]) -> Option<u64> {
    let hits: Vec<u64> = (28..h.len().saturating_sub(12))
        .filter(|&i| h[i - 28..i].iter().all(|&b| b == 0) && h[i + 8..i + 12] == [1, 0, 0, 0])
        .map(|i| u64::from_le_bytes(h[i..i + 8].try_into().unwrap()))
        .filter(|&v| (1 << 20..1 << 42).contains(&v))
        .collect();
    (hits.len() == 1).then(|| hits[0]) // two candidates: not sure which, so unknown
}

/// Free bytes on the filesystem holding `p` (or its nearest existing parent).
pub fn free_space(p: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let dir = p.ancestors().find(|a| a.exists())?;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

pub fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / 1e9)
}

// ---------- FitGirl checksum file ----------

/// The `.md5` FitGirl's "Verify BIN files before installation.bat" feeds to QuickSFV: in the repack dir or `MD5/`.
pub fn find_md5(repack: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = [repack.to_path_buf(), repack.join("MD5")]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md5")) && is_regular(p))
        .collect();
    found.sort_by_key(|p| !p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("fitgirl-bins.md5")));
    found.into_iter().next()
}

/// `<hash> *<path>` lines; `;` comments and blank lines skipped. Paths keep their Windows backslashes.
pub fn parse_md5(text: &str) -> Vec<(String, String)> {
    text.trim_start_matches('\u{feff}')
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with(';') {
                return None;
            }
            let (h, p) = l.split_once(char::is_whitespace)?;
            let p = p.trim_start().trim_start_matches('*');
            (h.len() == 32 && h.bytes().all(|b| b.is_ascii_hexdigit()) && !p.is_empty()).then(|| (h.to_ascii_lowercase(), p.to_string()))
        })
        .collect()
}

/// QuickSFV writes .md5 files in the Windows ANSI code page (1252), so a name like "KCD II – OST" has a
/// 0x96 en dash that is not UTF-8. UTF-8 files stay as they are.
fn decode_ansi(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| {
        const HI: [char; 32] = [
            '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
            '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
        ];
        e.into_bytes().iter().map(|&b| if (0x80..0xa0).contains(&b) { HI[b as usize - 0x80] } else { b as char }).collect()
    })
}

fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn md5_file(p: &Path, buf: &mut [u8], log: &impl Report) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Md5::new();
    let mut done = 0u64;
    loop {
        let n = f.read(buf)?;
        if n == 0 {
            break;
        }
        done += n as u64;
        if done % (64 << 20) < n as u64 && log.cancelled() {
            bail!("cancelled");
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().fold(String::with_capacity(32), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// Check every file the `.md5` lists; returns the bad ones ("mismatch: x" / "missing: x").
/// Files are hashed on up to 8 threads: a repack is a handful of multi-GB bins, an installed game thousands of
/// small files, and one thread leaves a 4 GB check twice as slow as `md5sum`. Results keep the file's order.
pub fn verify(md5: &Path, log: &impl Report) -> Result<Vec<String>> {
    let text = decode_ansi(read_capped(md5, MD5_CAP)?);
    let list = parse_md5(&text);
    if list.is_empty() {
        bail!("{} lists no files", md5.display());
    }
    let base = md5.parent().unwrap_or(Path::new("."));
    log.log(format!("verifying {} file(s) from {}", list.len(), md5.file_name().unwrap_or_default().to_string_lossy()));
    let path = |rel: &str| base.join(rel.replace('\\', "/"));
    // FitGirl's list in MD5/ names "..\\fg-01.bin": the repack folder is the limit
    let root = if base.file_name().is_some_and(|n| n.eq_ignore_ascii_case("md5")) { base.parent().unwrap_or(base) } else { base };
    let total: u64 = list.iter().filter_map(|(_, rel)| std::fs::metadata(path(rel)).ok()).map(|m| m.len()).sum();
    let hashed = std::sync::atomic::AtomicU64::new(0);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let bad = std::sync::Mutex::new(vec![]);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                // low priority (per thread on Linux): a game may be running meanwhile
                unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
                let mut buf = vec![0; 1 << 20]; // per thread, not per file: installed games are thousands of small files
                while !log.cancelled() {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    let Some((want, rel)) = list.get(n) else { break };
                    let p = path(rel);
                    // an entry may not climb out of the list's folder, and only regular files are read (no /dev/zero)
                    let r = if rel.starts_with(['/', '\\']) || !lexical(&path(rel)).starts_with(root) {
                        Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "outside the folder").into())
                    } else if !is_regular(&p) && p.symlink_metadata().is_ok() {
                        Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file").into())
                    } else {
                        md5_file(&p, &mut buf, log)
                    };
                    let size = std::fs::metadata(&p).map_or(0, |m| m.len());
                    log.progress(hashed.fetch_add(size, Ordering::Relaxed) + size, total.max(1));
                    let msg = match r {
                        Ok(got) if &got == want => continue,
                        Ok(_) => format!("mismatch: {rel}"),
                        // FitGirl's .md5 says so itself: "If you miss selective download files - that's OK"
                        Err(e) if not_found(&e) && rel.to_ascii_lowercase().contains("selective") => {
                            log.log(format!("not downloaded (optional): {rel}"));
                            continue;
                        }
                        Err(e) if not_found(&e) => format!("missing: {rel}"),
                        Err(e) => format!("unreadable: {rel} ({e})"),
                    };
                    bad.lock().unwrap().push((n, msg));
                }
            });
        }
    });
    cancel_point(log)?;
    let mut bad = bad.into_inner().unwrap();
    bad.sort();
    Ok(bad.into_iter().map(|(_, m)| m).collect())
}

/// `verify` that fails when anything is wrong, after logging each bad file.
pub fn check(md5: &Path, log: &impl Report) -> Result<()> {
    let bad = verify(md5, log)?;
    bad.iter().for_each(|b| log.log(b.clone()));
    if !bad.is_empty() {
        bail!("{} file(s) are damaged", bad.len());
    }
    log.log("all files verified".into());
    Ok(())
}

// ---------- install ----------

const LOG_WIN: &str = r"C:\repackd-install.log";

const BASE_OVERRIDES: &str = "winebrowser.exe=d;winemenubuilder.exe=d";
/// Started by FitGirl's setup without silent flags, so each waits for a click even in a silent install:
/// her QuickSFV after-check (repackd runs it itself), the online DirectX installer Proton has no use for,
/// and the VC++ runtimes (repackd runs those itself, silently, after setup).
const SETUP_BLOCKED: &str = "quicksfv.exe=d;dxwebsetup.exe=d;vc_redist.x86.exe=d;vc_redist.x64.exe=d;vcredist_x86.exe=d;vcredist_x64.exe=d";

pub fn inno_args(dir_win: &str, unattended: bool) -> Vec<String> {
    let mut a = vec!["/SP-".to_string(), format!("/DIR={dir_win}"), format!("/LOG={LOG_WIN}")];
    if unattended {
        a.extend(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/NOCANCEL"].map(String::from));
    }
    a
}

/// Lines of an Inno log that show FitGirl's unpacker failed (it can still exit 0 after a rollback).
pub fn log_errors(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("isdone") || l.contains("unarc") || l.contains("rolling back")
        })
        .map(str::to_string)
        .collect()
}

/// Run an Inno `setup.exe` (the game or one of its updates) into the game's prefix and dir, then judge it
/// by exit code and by its log, because FitGirl's unpacker can fail, roll back and still exit 0.
pub fn run_inno(g: &Game, setup: &Path, unattended: bool, log: &impl Report) -> Result<()> {
    let logf = g.prefix().join("drive_c/repackd-install.log");
    let _ = std::fs::remove_file(&logf); // a log from an earlier attempt would hide this one's result
    g.map_game_drive(log)?;
    let mut c = g.umu();
    c.env("WINEDLLOVERRIDES", format!("{BASE_OVERRIDES};{SETUP_BLOCKED}"));
    c.arg(setup).args(inno_args(&g.install_dir_win(), unattended));
    let c = memory_scope(c);
    log.log(format!("running {}", setup.display()));
    let done = AtomicBool::new(false);
    // a walk of the whole game each time: often enough for the bar, rarely when there is no bar (updates)
    let every = Duration::from_secs(if g.size.is_some() { 15 } else { 60 });
    let st = std::thread::scope(|s| {
        s.spawn(|| stall_watch(&g.install_dir(), g.size, &done, every, Duration::from_secs(300), log));
        let r = run(c, log);
        done.store(true, Ordering::Relaxed);
        r
    })?;
    let text = String::from_utf8_lossy(&std::fs::read(&logf).unwrap_or_default()).into_owned();
    inno_result(st.success(), &text, log)
}

/// Run inside its own systemd scope with `MemoryHigh` at half the RAM. FitGirl's unpacker sizes itself at
/// 60-75% of RAM; uncapped, systemd-oomd kills the desktop (shell, music player) to make room for it.
/// Without systemd the command runs as it is.
fn memory_scope(c: Command) -> Command {
    let total_kb = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| m.lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()));
    let (Some(sr), Some(kb)) = (which_path("systemd-run"), total_kb) else { return c };
    let mut s = Command::new(sr);
    s.args(["--user", "--scope", "--quiet", "--collect", "-p"]).arg(format!("MemoryHigh={}K", kb / 2)).arg("--").arg(c.get_program()).args(c.get_args());
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => s.env(k, v),
            None => s.env_remove(k),
        };
    }
    s
}

pub fn dir_size(d: &Path) -> u64 {
    std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| match e.metadata() {
        Ok(m) if m.is_dir() => dir_size(&e.path()),
        Ok(m) => m.len(),
        Err(_) => 0,
    }).sum()
}

/// Why `g` can't be moved into `to_parent` right now (checked again when the job runs).
pub fn move_refusal(g: &Game, to_parent: &Path) -> Result<()> {
    let new = to_parent.join(g.dir.file_name().context("the game folder has no name")?);
    if !game_pids(g).is_empty() {
        bail!("{} is running (game, installer or tool): close it first", g.name);
    }
    if g.steam.is_some() && steam::running() {
        bail!("close Steam first: this game has a Steam shortcut that must be updated");
    }
    if new.symlink_metadata().is_ok() {
        bail!("{} already exists: pick another folder", new.display());
    }
    // canonical on both sides: a symlinked parent can lead back into the game folder
    let inside = to_parent.canonicalize().context("the destination folder is not reachable")?;
    if inside.starts_with(g.dir.canonicalize().unwrap_or(g.dir.clone())) {
        bail!("the destination is inside the game's own folder");
    }
    Ok(())
}

/// Files, symlinks and file bytes under `d`, links not followed (what `copy_move` must reproduce).
fn tree_count(d: &Path) -> Result<(u64, u64, u64)> {
    let mut n = (0, 0, 0);
    for e in std::fs::read_dir(d).with_context(|| format!("read {}", d.display()))? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            let c = tree_count(&e.path())?;
            (n.0, n.1, n.2) = (n.0 + c.0, n.1 + c.1, n.2 + c.2);
        } else if t.is_symlink() {
            n.1 += 1;
        } else if t.is_file() {
            (n.0, n.2) = (n.0 + 1, n.2 + e.metadata()?.len());
        } else {
            bail!("{} is not a file, folder or link: not moved", e.path().display());
        }
    }
    Ok(n)
}

/// Copy `from` to `to` without following symlinks (prefixes link `z:` to `/`); links into `old` follow it to `new`.
fn copy_links_kept(old: &Path, new: &Path, from: &Path, to: &Path, done: &mut u64, total: u64, log: &impl Report) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (p, q, t) = (e.path(), to.join(e.file_name()), e.file_type()?);
        cancel_point(log)?;
        if t.is_dir() {
            copy_links_kept(old, new, &p, &q, done, total, log)?;
        } else if t.is_symlink() {
            let target = std::fs::read_link(&p)?;
            let target = target.strip_prefix(old).map_or(target.clone(), |r| new.join(r)); // relative links never match
            std::os::unix::fs::symlink(target, &q).with_context(|| format!("link {}", q.display()))?;
        } else {
            *done += std::fs::copy(&p, &q).with_context(|| format!("copy {}", p.display()))?; // reflinks where the filesystem can
            log.progress(*done, total);
        }
    }
    std::fs::set_permissions(to, std::fs::metadata(from)?.permissions())?; // after the contents: a read-only folder
    Ok(())
}

/// Copy `old` to the new, not yet existing `new` and check the copy against the source. On any error or
/// cancel our partial copy is deleted; the source is never touched.
fn copy_move(old: &Path, new: &Path, log: &impl Report) -> Result<()> {
    let before = tree_count(old)?;
    std::fs::create_dir(new).with_context(|| format!("create {}", new.display()))?; // fails if it exists: not ours to clean up
    let r = (|| {
        copy_links_kept(old, new, old, new, &mut 0, before.2, log)?;
        let after = tree_count(new)?;
        if after != before {
            bail!("the copy does not match the original ({after:?} against {before:?} files, links, bytes)");
        }
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(new);
    }
    r
}

/// Rename `old` to `new`, or on another filesystem copy, verify and delete `old` (a failed delete only logs).
fn relocate(old: &Path, new: &Path, log: &impl Report) -> Result<()> {
    if std::fs::rename(old, new).is_ok() {
        return Ok(());
    }
    // any rename failure (normally EXDEV, another disk) falls back to copying, which fails by itself if it must
    // space is checked only here: a same-disk rename needs none
    let size = dir_size(old);
    if let Some(free) = new.parent().and_then(free_space)
        && free < size + (1 << 30)
    {
        bail!("not enough space at {}: needs {} plus 1 GB spare, {} free", new.parent().unwrap_or(new).display(), gb(size), gb(free));
    }
    log.step("Copying to the new folder");
    copy_move(old, new, log)?;
    log.step("Removing the old folder");
    if let Err(e) = std::fs::remove_dir_all(old) {
        log.log(format!("the copy is complete, but the old folder {} could not be fully removed: {e}", old.display()));
    }
    Ok(())
}

/// Move the game's own folder (and its save backups) into `to_parent`; returns the new folder. The library
/// entry is the caller's to update.
pub fn move_game(g: &Game, to_parent: &Path, log: &impl Report) -> Result<PathBuf> {
    move_refusal(g, to_parent)?;
    let new = to_parent.join(g.dir.file_name().context("the game folder has no name")?);
    relocate(&g.dir, &new, log)?;
    if let Err(e) = std::fs::write(new.join(MOVED_MARK), b"") {
        log.log(format!("could not mark the game as moved ({e}): Delete will refuse it until the folder is back in the games root"));
    }
    // saves sit beside the game folder's parent: follow it; a failure keeps the old backups where they are
    let (from, to) = (saves_dir(g), saves_dir(&Game { dir: new.clone(), ..g.clone() }));
    if from != to && from.is_dir() && to.symlink_metadata().is_err() {
        let r = std::fs::create_dir_all(to.parent().unwrap_or(&to)).map_err(Into::into).and_then(|()| relocate(&from, &to, log));
        if let Err(e) = r {
            log.log(format!("warning: save backups stay in {}: {e:#}", from.display()));
        }
    }
    Ok(new)
}

/// FitGirl's unpacker shows its own error dialog even in silent mode and then waits forever, so a setup
/// that stops writing for `stall` gets one hint in the log (it can't be killed blindly: redist installers
/// at the end also write nothing for a while).
fn stall_watch(dir: &Path, total: Option<u64>, done: &AtomicBool, every: Duration, stall: Duration, log: &impl Report) {
    let (mut size, mut since, mut warned) = (0, Instant::now(), false);
    let mut next = Instant::now() + every;
    while !done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200).min(every));
        if Instant::now() < next {
            continue;
        }
        next = Instant::now() + every;
        let now = dir_size(dir);
        if let Some(t) = total {
            log.progress(now.min(t), t);
        }
        if now != size {
            (size, since, warned) = (now, Instant::now(), false);
        } else if !warned && since.elapsed() >= stall {
            log.log(format!(
                "no progress for {} min. If a window with an error appears, press Cancel, then INSTALL AGAIN.",
                stall.as_secs() / 60
            ));
            warned = true;
        }
    }
}

fn inno_result(ok: bool, text: &str, log: &impl Report) -> Result<()> {
    let errs = log_errors(text);
    // a killed or crashed setup still exits 0 through umu and leaves a partial game dir; only a setup that
    // ran to its end writes "Log closed."
    let finished = text.lines().rev().find(|l| !l.trim().is_empty()).is_some_and(|l| l.trim_end().ends_with("Log closed."));
    if ok && errs.is_empty() && finished {
        return Ok(());
    }
    if errs.is_empty() && !finished {
        bail!("installer did not finish (it was closed, crashed or cancelled)");
    }
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].iter().for_each(|l| log.log(l.to_string()));
    bail!("installer failed{}", errs.first().map(|e| format!(": {}", e.trim())).unwrap_or_else(|| " (non-zero exit)".into()))
}

/// Kill every process of this prefix: umu, setup, FitGirl's unpacker, wineserver. They are found by
/// `WINEPREFIX` in `/proc/*/environ` (Proton runs them as `<pfx>/pfx/`), because a stuck ISDone error dialog
/// keeps a silent install alive forever and nothing else knows all their PIDs.
pub fn kill_prefix(pfx: &Path) -> usize {
    prefix_pids(pfx).into_iter().filter(|&pid| unsafe { libc::kill(pid as i32, libc::SIGKILL) } == 0).count()
}

/// PIDs of this user's processes running in the prefix.
pub fn prefix_pids(pfx: &Path) -> Vec<u32> {
    let mut out = vec![];
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
        // through the threads: Wine's main thread can exit first, and a zombie leader's own environ reads empty
        // while its other threads keep running (and keep the ISDone window open). Other users' processes: unreadable.
        // the process's own environ first (one read); its threads' only when that reads empty
        let env = std::fs::read(e.path().join("environ")).ok().filter(|b| !b.is_empty()).or_else(|| {
            std::fs::read_dir(e.path().join("task")).into_iter().flatten().flatten().find_map(|t| std::fs::read(t.path().join("environ")).ok().filter(|b| !b.is_empty()))
        });
        let Some(env) = env else { continue };
        let ours = env.split(|&b| b == 0).filter_map(|kv| kv.strip_prefix(b"WINEPREFIX=")).any(|v| Path::new(&*String::from_utf8_lossy(v)).starts_with(pfx));
        if ours && pid != std::process::id() {
            out.push(pid);
        }
    }
    out
}

/// Run with stdout and stderr streamed to `log` line by line.
pub fn run(mut c: Command, log: &impl Report) -> Result<ExitStatus> {
    let mut child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().with_context(|| format!("start {:?}", c.get_program()))?;
    let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    std::thread::scope(|s| {
        s.spawn(|| BufReader::new(out).lines().map_while(Result::ok).for_each(|l| log.log(l)));
        BufReader::new(err).lines().map_while(Result::ok).for_each(|l| log.log(l));
    });
    Ok(child.wait()?)
}

/// Everything an install checks before it changes anything: the repack's files, its setup.exe, disk space.
/// A failure here leaves an installed game as it was (still playable). Returns setup.exe.
pub fn install_checks(g: &Game, verify_first: bool, log: &impl Report) -> Result<PathBuf> {
    if !game_pids(g).is_empty() {
        bail!("{} is running: close it first (installing over a running game breaks both)", g.name);
    }
    if g.proton_path() == "GE-Proton" && !protons().iter().any(|(l, v)| v.contains('/') && l.starts_with("GE-Proton")) {
        log.log("The first start downloads Proton (about 500 MB).".into());
    }
    if verify_first {
        match find_md5(&g.repack_dir) {
            Some(f) => {
                log.step("Checking repack files");
                log.log("Checking the files first (this can take a while on big games).".into());
                // neutral message: updates and verify reuse `check`, so the advice is added per call site
                check(&f, log).map_err(|e| anyhow!("{e}, so nothing was installed. Download the repack again, then press INSTALL."))?
            }
            None => log.log("no checksum file, skipped verification".into()),
        }
    }
    let setup = find_setup(&g.repack_dir)?;
    if let (Some(need), Some(free)) = (g.size, free_space(&g.dir)) {
        let need = need.saturating_sub(dir_size(&g.install_dir())) + (512 << 20); // a reinstall overwrites what is there; margin for redists and logs
        if free < need {
            bail!("not enough disk space: the game needs {} more, {} free at {}", gb(need), gb(free), g.dir.display());
        }
    }
    cancel_point(log)?;
    Ok(setup)
}

/// Install from setup.exe (after `install_checks`), then the runtimes and FitGirl's after-install check.
pub fn install(g: &Game, setup: &Path, verify_first: bool, unattended: bool, log: &impl Report) -> Result<()> {
    std::fs::create_dir_all(&g.dir)?;
    log.step("Unpacking (FitGirl's setup.exe)");
    run_inno(g, setup, unattended, log)?;
    cancel_point(log)?;
    if !g.install_dir().is_dir() {
        bail!("installer finished but {} does not exist", g.install_dir().display());
    }
    install_redists(g, log)?;
    cancel_point(log)?;
    // FitGirl's after-install check: the installed files against the .md5 her setup ships
    let after = g.install_dir().join("_Redist/fitgirl.md5");
    if verify_first && after.is_file() {
        log.step("Checking installed files");
        check(&after, log).map_err(|e| anyhow!("{e} after installing. Reinstall the game."))?;
    }
    Ok(())
}

/// Set up an imported game: create its prefix with the VC++ runtimes it ships. No file check here: a game
/// that has been played and patched no longer matches FitGirl's list, and hashing 100+ GB takes many minutes.
pub fn prepare_import(g: &Game, log: &impl Report) -> Result<()> {
    std::fs::create_dir_all(&g.dir)?;
    install_redists(g, log)?;
    if !g.prefix().join("drive_c").is_dir() {
        create_prefix(g, log)?; // no runtimes to install (no _Redist), so nothing ran Wine yet
    }
    Ok(())
}

fn create_prefix(g: &Game, log: &impl Report) -> Result<()> {
    log.step("Creating the Wine prefix");
    let mut c = g.umu();
    c.args(["wineboot", "-u"]);
    let st = run(c, log)?;
    if !g.prefix().join("drive_c").is_dir() {
        bail!("could not create the Wine prefix ({st})");
    }
    Ok(())
}

/// The checksum list for this game's files: the repack's (before install) or, for an imported game, the
/// after-install list FitGirl's setup left in the game folder.
pub fn checksum_file(g: &Game) -> Option<PathBuf> {
    if g.has_repack() {
        find_md5(&g.repack_dir)
    } else {
        Some(g.install_dir().join("_Redist/fitgirl.md5")).filter(|p| p.is_file())
    }
}

/// The VC++ runtimes setup ships in `_Redist` (blocked during setup), installed silently. A failure only
/// warns: Wine's built-in runtime covers most games.
pub(crate) fn install_redists(g: &Game, log: &impl Report) -> Result<()> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(g.install_dir().join("_Redist")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    found.sort();
    for p in found {
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        let args: &[&str] = if name.starts_with("vc_redist") && name.ends_with(".exe") {
            &["/install", "/quiet", "/norestart"] // 2015-2022
        } else if name.starts_with("vcredist") && name.ends_with(".exe") {
            &["/q", "/norestart"] // 2005-2013
        } else {
            continue;
        };
        log.step("Installing runtimes (VC++)");
        log.log(format!("installing {name}"));
        let mut c = g.umu();
        c.arg(&p).args(args);
        match run(c, log)?.code() {
            Some(0 | 1638 | 3010) => {} // ok, newer one already there, reboot wanted
            code => log.log(format!("warning: {name} exited with {code:?}; the game may still run on Wine's built-in runtime")),
        }
    }
    Ok(())
}

/// A folder `scan_folder` found: a repack to install, or a game that is already unpacked.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub dir: PathBuf,
    pub repack: bool,
}

/// The paths a library entry already owns (canonicalized), for `scan_known`.
pub fn known_dirs(lib: &Library) -> Vec<PathBuf> {
    lib.games.iter().flat_map(|g| [g.repack_dir.clone(), g.install_dir()]).filter(|p| !p.as_os_str().is_empty()).map(|p| std::fs::canonicalize(&p).unwrap_or(p)).collect()
}

/// The repacks and unpacked games among the direct subfolders of `dir` that the library does not have yet.
pub fn scan_folder(dir: &Path, lib: &Library) -> Vec<Found> {
    scan_known(dir, &known_dirs(lib))
}

/// `scan_folder` against an already collected `known_dirs` (the watch thread has no library). Listings only.
pub fn scan_known(dir: &Path, known: &[PathBuf]) -> Vec<Found> {
    // (name, is a folder) of everything directly in `d`
    let list = |d: &Path| -> Vec<(String, bool)> { std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| (e.file_name().to_string_lossy().into_owned(), e.file_type().is_ok_and(|t| t.is_dir()))).collect() };
    let lower_has = |l: &[(String, bool)], f: &dyn Fn(&str) -> bool| l.iter().any(|(n, _)| f(&n.to_ascii_lowercase()));
    let mut subs: Vec<PathBuf> = list(dir).into_iter().filter(|(_, d)| *d).map(|(n, _)| dir.join(n)).collect();
    subs.sort();
    let mut out = vec![];
    for sub in subs {
        if known.contains(&std::fs::canonicalize(&sub).unwrap_or(sub.clone())) {
            continue;
        }
        let top = list(&sub);
        if top.iter().any(|(n, d)| !d && n.eq_ignore_ascii_case("setup.exe")) {
            let md5 = |l: &[(String, bool)]| lower_has(l, &|n| n.starts_with("fitgirl") && n.ends_with(".md5"));
            let md5_dir = top.iter().find(|(n, d)| *d && n.eq_ignore_ascii_case("md5")).map(|(n, _)| list(&sub.join(n)));
            if md5_dir.is_some_and(|l| md5(&l)) || lower_has(&top, &|n| (n.starts_with("fg-") || n.starts_with("fitgirl-")) && n.ends_with(".bin")) {
                out.push(Found { dir: sub, repack: true });
            }
        } else if top.iter().any(|(n, d)| !d && Kind::Windows.is_target(Path::new(n))) || top.iter().filter(|(_, d)| *d).any(|(n, _)| list(&sub.join(n)).iter().any(|(f, d)| !d && Kind::Windows.is_target(Path::new(f)))) {
            out.push(Found { dir: sub, repack: false });
        }
    }
    out
}

/// Total bytes in a download folder, or None while a client still has partial files in it. Metadata only.
pub fn download_size(dir: &Path) -> Option<u64> {
    let mut total = 0;
    for f in files_under(dir) {
        let n = f.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        if [".part", ".!qb", ".!ut", ".crdownload", ".aria2"].iter().any(|x| n.ends_with(x)) {
            return None;
        }
        total += std::fs::metadata(&f).map_or(0, |m| m.len());
    }
    Some(total)
}

/// Every launchable file of this kind under the install dir (uninstallers left out), for the picker.
pub fn targets(dir: &Path, kind: Kind) -> Vec<PathBuf> {
    rank(&files_under(dir), kind)
}

/// Every file under `dir`, at most 8 folders deep: a whole disk or home picked by mistake stays bounded.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((d, depth)) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.file_type() {
                // not following links: a link back to a parent folder would loop forever
                Ok(t) if t.is_dir() => {
                    if depth < 8 {
                        stack.push((e.path(), depth + 1));
                    }
                }
                Ok(_) => out.push(e.path()),
                Err(_) => {}
            }
        }
    }
    out
}

/// The files of this kind among `files`, the most likely game file first.
fn rank(files: &[PathBuf], kind: Kind) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = files.iter().filter(|p| kind.is_target(p)).cloned().collect();
    // the main game file first: not in a DLC/update folder (Switch repacks keep data/dlc next to data/games), then shallowest
    let extra = |p: &Path| p.components().any(|c| ["dlc", "update", "updates", "patch", "_redist"].contains(&c.as_os_str().to_string_lossy().to_ascii_lowercase().as_str()));
    // tools shipped next to the game: crash reporters, checksum tools, installers, anti-cheat services
    let helper = |p: &Path| {
        let n = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
        ["crash", "bugsplat", "sndrpt", "report", "rapidcrc", "quicksfv", "unins", "redist", "setup", "dxweb", "prereq", "easyanticheat", "battleye", "be_service"].iter().any(|h| n.contains(h))
    };
    out.sort_by_cached_key(|p| (extra(p), helper(p), p.components().count(), p.clone()));
    out
}

/// `file://` URI with everything but unreserved bytes and `/` percent-encoded (game names have spaces, brackets).
fn file_uri(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            s.push(b as char);
        } else {
            s += &format!("%{b:02X}");
        }
    }
    s
}

/// Show a folder in the desktop's file manager. `xdg-open` alone can pick a terminal or an editor for
/// directories (whatever claims inode/directory), so ask a real file manager over D-Bus first.
pub fn open_folder(dir: &Path) -> Result<()> {
    let arg = format!("['{}']", file_uri(dir).replace('\'', "%27"));
    let shown = Command::new("gdbus")
        .args(["call", "--session", "--dest", "org.freedesktop.FileManager1", "--object-path", "/org/freedesktop/FileManager1"])
        .args(["--method", "org.freedesktop.FileManager1.ShowFolders", &arg, ""])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !shown {
        Command::new("xdg-open").arg(dir).spawn().context("xdg-open")?;
    }
    Ok(())
}

fn sh_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

/// `KEY=value` lines; a bad line is an error so a typo does not silently drop a variable.
pub fn parse_env(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = vec![];
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (k, v) = l.split_once('=').with_context(|| format!("env line \"{l}\" needs KEY=value"))?;
        let ok = k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !ok {
            bail!("env name \"{k}\" is not valid");
        }
        out.push((k.to_string(), v.to_string()));
    }
    Ok(out)
}

/// Cracks and mod loaders (TENOKE, ASI loaders) ship a proxy DLL beside the exe under a system DLL's name.
/// Wine prefers its own builtin for these names, so the crack never loads and the game hangs or quits.
fn proxy_dlls(dir: &Path) -> String {
    const NAMES: [&str; 9] = ["version", "winmm", "winhttp", "wininet", "dinput8", "dsound", "xinput1_3", "xinput1_4", "dbghelp"];
    let present: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.file_name().to_str().map(str::to_ascii_lowercase)).collect();
    NAMES.iter().filter(|n| present.contains(&format!("{n}.dll"))).map(|n| format!("{n}=n,b")).collect::<Vec<_>>().join(";")
}

/// What a failed start's output points at, and the one-click fix for it if there is one.
#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    Winetricks(&'static str),
    /// Try this Proton (a `Game::proton` value).
    Proton(String),
}

/// Read a game's output (Wine/Proton log lines) after it stopped right away: the likely cause in plain words,
/// or None when the output names none.
pub fn diagnose(out: &str, other_proton: Option<&str>) -> Option<(String, Option<Fix>)> {
    // "err:module:import_dll Library MSVCP140.dll (which is needed by L"...") not found"
    let missing = out.lines().find_map(|l| l.split("import_dll Library ").nth(1).filter(|_| l.contains("not found"))?.split_whitespace().next().map(str::to_ascii_lowercase));
    if let Some(dll) = missing {
        let verb = [
            ("msvcp140", "vcrun2022"), ("vcruntime140", "vcrun2022"), ("ucrtbase", "vcrun2022"), ("concrt140", "vcrun2022"),
            ("msvcr120", "vcrun2013"), ("msvcp120", "vcrun2013"), ("msvcr110", "vcrun2012"), ("msvcp110", "vcrun2012"),
            ("msvcr100", "vcrun2010"), ("msvcp100", "vcrun2010"), ("d3dx9", "d3dx9"), ("d3dx10", "d3dx10"), ("d3dx11", "d3dx11_43"),
            ("d3dcompiler", "d3dcompiler_47"), ("xinput1_3", "xinput"), ("xaudio2_7", "xact"), ("mscoree", "dotnet48"), ("physxloader", "physx"),
        ]
        .iter()
        .find(|(p, _)| dll.starts_with(p))
        .map(|(_, v)| *v);
        return Some(match verb {
            Some(v) => (format!("The game is missing a Windows component ({dll})."), Some(Fix::Winetricks(v))),
            None => (format!("The game is missing a Windows component ({dll})."), None),
        });
    }
    let has = |p: &str| out.lines().any(|l| l.to_ascii_lowercase().contains(p));
    if has("easyanticheat") || has("battleye") {
        return Some(("This game uses anti-cheat that usually does not work on Linux.".into(), None));
    }
    if has("vk_error") || has("vkcreateinstance failed") || has("no vulkan") {
        return Some(("Your graphics driver is missing Vulkan support. Install the Vulkan driver package for your GPU from your distribution.".into(), None));
    }
    if has("unhandled exception") || has("unhandled page fault") || has("backtrace:") {
        return Some(match other_proton {
            Some(p) => ("The game crashed. Try a different compatibility version (Proton):".into(), Some(Fix::Proton(p.to_string()))),
            None => ("The game crashed. Download a different compatibility version (Proton) in Settings.".into(), None),
        });
    }
    None
}

/// Where a Switch emulator keeps its keys and firmware, and whether they are there. Some(problem, folder to
/// put them in) when something is missing; None when it looks ready (or it is not a Switch emulator we know).
/// repackd only checks: keys and firmware come from the user's own console.
pub fn emulator_setup_problem(g: &Game, settings: &Settings) -> Option<(String, PathBuf)> {
    if g.kind != Kind::Switch {
        return None;
    }
    let emu = g.effective_emulator(settings)?;
    let exe = emu.file_name()?.to_string_lossy().to_ascii_lowercase();
    let h = home();
    // (keys folder, firmware folder) for this emulator
    let (keys, firmware) = if exe.contains("ryujinx") || exe.contains("ryubing") {
        let root = if exe.ends_with(".exe") && g.args.contains("-r {dir}\\data") {
            g.install_dir().join("data") // FitGirl's bundled one runs with its own data folder
        } else if emu.parent().is_some_and(|d| d.join("portable").is_dir()) {
            emu.parent()?.join("portable")
        } else if emu.starts_with(h.join(".local/share/flatpak")) || emu.starts_with("/var/lib/flatpak") {
            h.join(".var/app/io.github.ryubing.Ryujinx/config/Ryujinx")
        } else {
            h.join(".config/Ryujinx")
        };
        (root.join("system"), root.join("bis/system/Contents/registered"))
    } else {
        let name = ["eden", "citron", "torzu", "sudachi", "suyu", "yuzu"].iter().find(|n| exe.contains(*n))?;
        if emu.starts_with(h.join(".local/share/flatpak")) || emu.starts_with("/var/lib/flatpak") {
            return None; // shortcut: Flatpak builds keep their data under ~/.var/app/<id>, not checked
        }
        let root = if exe.ends_with(".exe") { emu.parent()?.join("user") } else { h.join(".local/share").join(name) };
        (root.join("keys"), root.join("nand/system/Contents/registered"))
    };
    let emu_name = emu.file_stem()?.to_string_lossy().into_owned();
    if !keys.join("prod.keys").is_file() {
        return Some((format!("{emu_name} has no prod.keys yet: put the keys from your own Switch in this folder."), keys));
    }
    if std::fs::read_dir(&firmware).map_or(true, |mut d| d.next().is_none()) {
        return Some((format!("{emu_name} has no Switch firmware installed: install it from your own console in {emu_name} (File, Install Firmware)."), firmware));
    }
    None
}

/// The script Launch and the Steam shortcut run. Rewritten whenever the profile or emulator changes.
pub fn write_launch_script(g: &Game, settings: &Settings) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Some(file) = &g.exe else { bail!("pick the game file first") };
    let mut s = String::from("#!/bin/sh\nunset NoDefaultCurrentDirectoryInExePath\n");
    let proxies = if g.kind == Kind::Windows { proxy_dlls(file.parent().unwrap_or(file)) } else { String::new() };
    let mut env = parse_env(&g.env)?;
    // a switch whose tool was uninstalled since is left out (its wrapper would fail the launch)
    let on: Vec<&Tweak> = TWEAKS.iter().filter(|t| g.tweaks.iter().any(|k| k == t.key) && t.needs.is_none_or(|b| which_path(b).is_some())).collect();
    for (k, v) in on.iter().flat_map(|t| t.env) {
        if !env.iter().any(|(e, _)| e == k) {
            env.insert(0, (k.to_string(), v.to_string())); // the user's own line for the same variable wins
        }
    }
    let wrap: String = on.iter().flat_map(|t| t.wrap).map(|w| format!("{} ", sh_quote(Path::new(w)))).collect();
    match env.iter_mut().find(|(k, _)| k == "WINEDLLOVERRIDES") {
        Some((_, v)) if !proxies.is_empty() => *v = format!("{proxies};{v}"), // merged: the user's later entries win
        None if !proxies.is_empty() => env.insert(0, ("WINEDLLOVERRIDES".into(), proxies)),
        _ => {}
    }
    for (k, v) in &env {
        s += &format!("export {k}={}\n", sh_quote(Path::new(v)));
    }
    // a Windows emulator (bundled with the repack) runs through Proton and needs Windows paths: Z: is the whole disk
    let emu = if g.kind == Kind::Windows { None } else { g.effective_emulator(settings) };
    let windows_emu = emu.as_ref().is_some_and(|e| e.extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")));
    let place = |p: &Path| if windows_emu { format!("Z:{}", p.display()).replace('/', "\\") } else { p.display().to_string() };
    let workdir = match &emu {
        Some(e) if windows_emu => e.parent().unwrap_or(e).to_path_buf(),
        _ => file.parent().unwrap_or(file).to_path_buf(),
    };
    s += &format!("cd {} || exit 1\n", sh_quote(&workdir));
    // shortcut: args split on whitespace, no quoting inside; upgrade to shell-words if someone needs spaces in an arg
    let args = |s: &mut String| {
        for a in g.args.split_whitespace() {
            let a = a.replace("{file}", &place(file)).replace("{dir}", &place(&g.install_dir()));
            *s += " ";
            *s += &sh_quote(Path::new(&a));
        }
    };
    // what runs: the game itself, or the emulator with the game file in its args
    let program = match (g.kind, &emu) {
        (Kind::Windows, _) => file,
        (k, None) => bail!("no {} emulator: set one in Settings", k.label()),
        (_, Some(_)) if !g.args.contains("{file}") => bail!("launch options need {{file}} where the game file goes"),
        (_, Some(e)) => e,
    };
    if g.kind == Kind::Windows || windows_emu {
        // per-game shader caches that repackd owns (Clear shader cache); the user's own env line for a variable wins
        let cache = g.dir.join("shadercache");
        for (k, sub, v) in [("VKD3D_SHADER_CACHE_PATH", "vkd3d", None), ("MESA_SHADER_CACHE_DIR", "mesa", None), ("__GL_SHADER_DISK_CACHE_PATH", "nvidia", None), ("__GL_SHADER_DISK_CACHE_SKIP_CLEANUP", "", Some("1"))] {
            if env.iter().any(|(e, _)| e == k) {
                continue;
            }
            match v {
                Some(v) => s += &format!("export {k}={v}\n"),
                None => s += &format!("mkdir -p {d} && export {k}={d}\n", d = sh_quote(&cache.join(sub))),
            }
        }
        s += &format!("export WINEPREFIX={} GAMEID=umu-default PROTONPATH={}\nexec {wrap}umu-run {}", sh_quote(&g.prefix()), sh_quote(Path::new(g.proton_path())), sh_quote(program));
    } else {
        s += &format!("exec {wrap}{}", sh_quote(program));
    }
    args(&mut s);
    s += " \"$@\"\n";
    if g.desktop {
        write_desktop(g, &apps_dir())?;
    }
    let p = g.launch_script();
    let runnable = std::fs::metadata(&p).is_ok_and(|m| m.permissions().mode() & 0o111 == 0o111);
    if runnable && std::fs::read(&p).is_ok_and(|old| old == s.as_bytes()) {
        return Ok(()); // unchanged (every start checks every game's script)
    }
    std::fs::create_dir_all(&g.dir)?; // an imported console game has no folder of its own yet
    std::fs::write(&p, s)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Field value in a `.desktop` file's quoted `Exec`: the spec escapes `"`, `` ` ``, `$`, `\` for the command
/// line, then the file format doubles every `\`; `%` is a field code.
fn desktop_exec(p: &Path) -> String {
    let mut o = String::from("\"");
    for c in p.display().to_string().chars() {
        match c {
            '"' | '`' | '$' => o.push_str(&format!("\\\\{c}")),
            '\\' => o.push_str("\\\\\\\\"),
            '%' => o.push_str("%%"),
            _ => o.push(c),
        }
    }
    o + "\""
}

fn desktop_file(g: &Game, apps: &Path) -> PathBuf {
    apps.join(format!("repackd-{}.desktop", g.slug()))
}

/// The game's app-menu entry in `apps`, rewritten only when its content changed.
pub fn write_desktop(g: &Game, apps: &Path) -> Result<()> {
    let icon = g.art("icon").or_else(|| g.art("cover")).map(|i| format!("Icon={}\n", i.display())).unwrap_or_default();
    let s = format!("[Desktop Entry]\nType=Application\nName={}\nExec={}\n{icon}Categories=Game;\nTerminal=false\n", g.name.replace(['\n', '\r'], " "), desktop_exec(&g.launch_script()));
    let p = desktop_file(g, apps);
    if std::fs::read(&p).is_ok_and(|old| old == s.as_bytes()) {
        return Ok(());
    }
    write_atomic(&p, s.as_bytes())
}

pub fn remove_desktop(g: &Game, apps: &Path) {
    let _ = std::fs::remove_file(desktop_file(g, apps));
}

/// A plain-text report for bug reports: versions, hardware, the game's settings and its last run's tail.
pub fn diagnostics(g: &Game, settings: &Settings) -> String {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let os = read("/etc/os-release").lines().find_map(|l| l.strip_prefix("PRETTY_NAME=")).map(|v| v.trim_matches('"').to_string()).unwrap_or_default();
    let cpu = read("/proc/cpuinfo").lines().find(|l| l.starts_with("model name")).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string()).unwrap_or_default();
    let mut gpus = vec![];
    for e in std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if !n.starts_with("card") || n.contains('-') {
            continue; // connectors are "card0-DP-1"
        }
        let d = e.path().join("device");
        let id = |f: &str| std::fs::read_to_string(d.join(f)).unwrap_or_default().trim().to_string();
        let drv = std::fs::read_link(d.join("driver")).ok().and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_default();
        gpus.push(format!("{n} {}:{} driver {drv}", id("vendor"), id("device")));
    }
    let tweaks: Vec<&str> = TWEAKS.iter().filter(|t| g.tweaks.iter().any(|k| k == t.key)).map(|t| t.key).collect();
    let log = read(&g.dir.join("last-run.log").to_string_lossy());
    let lines: Vec<&str> = log.lines().collect();
    format!(
        "repackd {}\nOS: {os}\nKernel: {}\nCPU: {cpu}\nGPU: {}\n\nGame: {}\nType: {}\nProton: {}\nGame file: {}\nEmulator: {}\nLaunch args: {}\nEnv:\n{}\nTweaks: {}\nPrefix exists: {}\nVersion: {}\n\nLast run (last 60 lines):\n{}\n",
        env!("CARGO_PKG_VERSION"),
        read("/proc/sys/kernel/osrelease").trim(),
        gpus.join("; "),
        g.name,
        g.kind.label(),
        g.proton,
        g.exe.as_ref().map(|e| e.display().to_string()).unwrap_or_default(),
        g.effective_emulator(settings).map(|e| e.display().to_string()).unwrap_or_default(),
        g.args,
        g.env.trim_end(),
        tweaks.join(", "),
        g.prefix().is_dir(),
        g.version,
        lines[lines.len().saturating_sub(60)..].join("\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_hints() {
        let cachy = distro_family("NAME=\"CachyOS Linux\"\nID=cachyos\nID_LIKE=arch\n");
        assert_eq!(cachy, Family::Arch);
        assert_eq!(install_hint_for(&cachy, "7z"), "sudo pacman -S 7zip");
        let mint = distro_family("ID=linuxmint\nID_LIKE=\"ubuntu debian\"\n");
        assert_eq!(mint, Family::Debian);
        assert_eq!(install_hint_for(&mint, "notify-send"), "sudo apt install libnotify-bin");
        assert!(install_hint_for(&mint, "umu-run").starts_with("get umu-launcher"));
        let nobara = distro_family("ID=nobara\nID_LIKE=\"fedora\"\n");
        assert_eq!(install_hint_for(&nobara, "gamemoderun"), "sudo dnf install gamemode");
        let odd = distro_family("ID=nixos\n");
        assert_eq!(odd, Family::Unknown);
        assert_eq!(install_hint_for(&odd, "winetricks"), "install winetricks");
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("repackd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn move_copy_keeps_links() {
        use std::os::unix::fs::symlink;
        let d = tmp("move");
        let (old, new) = (d.join("g"), d.join("out/g"));
        std::fs::create_dir_all(old.join("pfx/drive_c")).unwrap();
        std::fs::create_dir_all(d.join("out")).unwrap();
        std::fs::write(old.join("launch.sh"), b"abc").unwrap();
        std::fs::write(old.join("pfx/drive_c/save.dat"), b"12345").unwrap();
        symlink(old.join("launch.sh"), old.join("pfx/abs")).unwrap();
        symlink("/", old.join("pfx/z:")).unwrap();
        symlink(".", old.join("pfx/pfx")).unwrap();
        copy_move(&old, &new, &|_: String| {}).unwrap();
        assert_eq!(std::fs::read_link(new.join("pfx/abs")).unwrap(), new.join("launch.sh"));
        assert_eq!(std::fs::read_link(new.join("pfx/z:")).unwrap(), Path::new("/"));
        assert_eq!(std::fs::read_link(new.join("pfx/pfx")).unwrap(), Path::new("."));
        assert_eq!(std::fs::read(new.join("pfx/drive_c/save.dat")).unwrap(), b"12345");
        assert_eq!(tree_count(&new).unwrap(), (2, 3, 8)); // `/` was not followed
        assert_eq!(tree_count(&old).unwrap(), (2, 3, 8));
        // the library paths follow the folder
        let mut g = Game { name: "g".into(), dir: old.clone(), exe: Some(old.join("pfx/drive_c/a.exe")), game_dir: Some(d.join("elsewhere")), ..Default::default() };
        g.moved_to(&new);
        assert_eq!((g.dir.clone(), g.exe.clone(), g.game_dir.clone()), (new.clone(), Some(new.join("pfx/drive_c/a.exe")), Some(d.join("elsewhere"))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn move_copy_failure_leaves_source() {
        struct Stop;
        impl Report for Stop {
            fn log(&self, _: String) {}
            fn cancelled(&self) -> bool {
                true
            }
        }
        let d = tmp("movefail");
        let old = d.join("g");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("f"), b"x").unwrap();
        let taken = d.join("taken");
        std::fs::write(&taken, b"mine").unwrap();
        assert!(copy_move(&old, &taken, &|_: String| {}).is_err());
        assert_eq!(std::fs::read(&taken).unwrap(), b"mine"); // not ours: untouched
        assert_eq!(std::fs::read(old.join("f")).unwrap(), b"x");
        let part = d.join("part");
        assert!(copy_move(&old, &part, &Stop).is_err()); // cancelled
        assert!(!part.exists() && old.join("f").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn custom_root_and_old_root_games() {
        let d = tmp("customroot");
        let (old, new) = (d.join("old"), d.join("new"));
        let mut lib = Library::load(d.join("lib.json"), old.clone()).unwrap();
        let (a, b) = (d.join("rp-a"), d.join("rp-b"));
        for r in [&a, &b] {
            std::fs::create_dir_all(r).unwrap();
            std::fs::write(r.join("setup.exe"), b"MZ").unwrap();
        }
        let i = lib.add(a).unwrap();
        let first = lib.games[i].dir.clone();
        assert!(first.starts_with(&old) && first.join(MOVED_MARK).is_file());
        lib.root = new.clone(); // root switched: the old game keeps its dir
        let j = lib.add(b).unwrap();
        assert!(lib.games[j].dir.starts_with(&new) && lib.games[i].dir == first);
        std::fs::create_dir_all(old.join(".trash-left")).unwrap();
        clear_trash(&lib.root, &lib.games);
        assert!(!old.join(".trash-left").exists());
        lib.delete(i).unwrap();
        assert!(!first.exists() && !old.join(".trash-rp-a").exists());
        // a refused root: inside a game, or under a trash folder
        assert!(root_refusal(&lib.games[0].dir.join("sub"), &lib.games).is_err());
        assert!(root_refusal(&d.join("x/.trash-y/z"), &[]).is_err());
        assert!(root_refusal(&d.join("fresh"), &lib.games).is_ok() && d.join("fresh").is_dir());
        // an unmarked folder outside the root is still refused
        let out = d.join("elsewhere/g");
        std::fs::create_dir_all(&out).unwrap();
        assert!(delete_files(&new, &Game { name: "g".into(), dir: out.clone(), ..Default::default() }).is_err() && out.is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn delete_outside_root_needs_marker() {
        let d = tmp("delmoved");
        let (root, dir) = (d.join("root"), d.join("elsewhere/g"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let g = Game { name: "g".into(), dir: dir.clone(), ..Default::default() };
        assert!(delete_files(&root, &g).is_err() && dir.is_dir()); // unmarked: refused
        std::fs::write(dir.join(MOVED_MARK), b"").unwrap();
        delete_files(&root, &g).unwrap();
        assert!(!dir.exists() && !d.join("elsewhere/.trash-g").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn md5_parser() {
        let t = "\u{feff}; generated by QuickSFV\r\nD41D8CD98F00B204E9800998ECF8427E *..\\fitgirl-01.bin\r\n\r\nbad line\r\n0123456789abcdef0123456789abcdef  sub\\a b.bin\r\n";
        assert_eq!(
            parse_md5(t),
            [("d41d8cd98f00b204e9800998ecf8427e".into(), r"..\fitgirl-01.bin".into()), ("0123456789abcdef0123456789abcdef".into(), r"sub\a b.bin".into())]
        );
    }

    #[test]
    fn installer_info_versions() {
        // the two FitGirl info-page lines, with RTF around them, and the emulator lines that must not count
        let rtf = br"{\rtf1\ansi{\fonttbl}\pard Ryujinx (v1.1.1397.0) bundled\par Game version: Steam BuildID 20663174 (Denuvoless)\par}";
        assert_eq!(version_from(rtf).as_deref(), Some("20663174"));
        let rtf = b"{\\rtf1 Switch Firmware v18.0.0 added\r\n\\b Based on P2P v1.0.1 Switch release: 5.6 GB\\b0\r\n}";
        assert_eq!(version_from(rtf).as_deref(), Some("1.0.1"));
        assert_eq!(version_from(br"{\rtf1 Game version: v1.2.3 + 4 DLCs\par}").as_deref(), Some("1.2.3"));
        assert_eq!(version_from(b"no info page"), None);
    }

    #[test]
    fn proton_release_assets() {
        let a = |n: &str| serde_json::json!({"name": n, "browser_download_url": format!("https://x/{n}"), "size": 7});
        let v = serde_json::json!([
            {"tag_name": "cachyos-11.0-20261015-slr", "assets": [a("proton-cachyos-11.0-20261015-slr-arm64.tar.xz"), a("proton-cachyos-11.0-20261015-slr-x86_64_v3.tar.xz"), a("proton-cachyos-11.0-20261015-slr-x86_64.tar.xz"), a("proton-cachyos-11.0-20261015-slr-x86_64.sha512sum")]},
            {"tag_name": "EM-10.0-37-HDR", "assets": [a("proton-EM-10.0-37-HDR.sha256sum"), a("proton-EM-10.0-37-HDR.tar.xz")]},
            {"tag_name": "source-only", "assets": [a("notes.txt")]},
        ]);
        let r = releases_from("x", &v).unwrap();
        assert_eq!(r.len(), 2);
        assert!(r[0].url.ends_with("slr-x86_64.tar.xz") && r[0].sha_url.as_deref().is_some_and(|s| s.ends_with("x86_64.sha512sum")), "{r:?}");
        assert!(r[1].sha_url.as_deref().is_some_and(|s| s.ends_with(".sha256sum")));
        assert!(r[0].installed_as("proton-cachyos-11.0-20261015-slr-x86_64") && r[1].installed_as("proton-EM-10.0-37-HDR"));
        let ge = ProtonRelease { tag: "GE-Proton11-1".into(), ..r[0].clone() };
        assert!(ge.installed_as("GE-Proton11-1-x86_64") && !ge.installed_as("GE-Proton11-11-x86_64"));
    }

    #[test]
    fn oversized_art_is_scaled_to_its_display_size() {
        let d = tmp("art");
        image::RgbaImage::new(4000, 1000).save(d.join("logo.png")).unwrap();
        image::RgbImage::new(3840, 1240).save(d.join("hero.jpg")).unwrap();
        image::RgbaImage::new(600, 900).save(d.join("cover.png")).unwrap();
        let g = Game { dir: d.clone(), ..Default::default() };
        assert!(g.shrink_art());
        assert_eq!(image::image_dimensions(d.join("logo.png")).unwrap(), (800, 200)); // proportions kept
        assert_eq!(image::image_dimensions(d.join("hero.jpg")).unwrap().0, 1920);
        assert_eq!(image::image_dimensions(d.join("cover.png")).unwrap(), (600, 900));
        assert!(!g.shrink_art(), "already small: nothing rewritten");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 3, "no temp files left");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn version_from_fitgirls_checksum_list_name() {
        let d = tmp("vfile");
        std::fs::write(d.join("v1.5.1.blake3"), b"").unwrap();
        std::fs::write(d.join("video.md5x"), b"").unwrap();
        assert_eq!(version_file(&d).as_deref(), Some("1.5.1"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn launch_switches_and_start_diagnosis() {
        let d = tmp("tweaks");
        let g = Game { name: "g".into(), dir: d.clone(), exe: Some(d.join("g.exe")), tweaks: vec!["gamemode".into(), "mangohud".into()], env: "MANGOHUD=0".into(), ..Default::default() };
        write_launch_script(&g, &Settings::default()).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        // a switch whose tool is not installed is left out, so the wrapper only shows where gamemode exists
        let wrap = if which_path("gamemoderun").is_some() { "exec 'gamemoderun' umu-run " } else { "exec umu-run " };
        assert!(s.contains(wrap) && s.contains("export MANGOHUD='0'") && !s.contains("MANGOHUD='1'"), "{s}");
        let (text, fix) = diagnose("0124:err:module:import_dll Library MSVCP140.dll (which is needed by L\"C:\\g.exe\") not found\n", None).unwrap();
        assert_eq!(fix, Some(Fix::Winetricks("vcrun2022")), "{text}");
        assert_eq!(diagnose("wine: Unhandled page fault on read access", Some("GE-Proton10-34")).unwrap().1, Some(Fix::Proton("GE-Proton10-34".into())));
        assert_eq!(diagnose("nothing useful", None), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn switch_emulator_setup_is_checked_not_fetched() {
        let d = tmp("keys");
        let mut g = Game { dir: d.clone(), kind: Kind::Switch, args: "-r {dir}\\data {file}".into(), ..Default::default() };
        let inst = g.install_dir();
        std::fs::create_dir_all(inst.join("emu")).unwrap();
        std::fs::write(inst.join("emu/Ryujinx.exe"), b"").unwrap();
        g.emulator = Some(inst.join("emu/Ryujinx.exe"));
        let st = Settings::default();
        let (msg, dir) = emulator_setup_problem(&g, &st).unwrap();
        assert!(msg.contains("prod.keys") && dir == inst.join("data/system"), "{msg}");
        std::fs::create_dir_all(inst.join("data/system")).unwrap();
        std::fs::write(inst.join("data/system/prod.keys"), b"").unwrap();
        assert!(emulator_setup_problem(&g, &st).unwrap().0.contains("firmware"));
        std::fs::create_dir_all(inst.join("data/bis/system/Contents/registered/x.nca")).unwrap();
        assert_eq!(emulator_setup_problem(&g, &st), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn proton_unpacks_whole_or_not_at_all() {
        let d = tmp("pinst");
        std::fs::create_dir_all(d.join("src/P-1")).unwrap();
        std::fs::write(d.join("src/P-1/proton"), b"").unwrap();
        assert!(Command::new("tar").arg("-czf").arg(d.join("p.tar.gz")).arg("-C").arg(d.join("src")).arg("P-1").status().unwrap().success());
        let rel = |f: &str| ProtonRelease { source: "x", tag: "P-1".into(), url: format!("file://{}", d.join(f).display()), sha_url: None, size: 1 };
        let out = d.join("compat");
        install_proton(&rel("p.tar.gz"), &out, &|_| {}).unwrap();
        assert!(out.join("P-1/proton").is_file());
        std::fs::write(d.join("bad.tar.gz"), b"not a tarball").unwrap();
        assert!(install_proton(&ProtonRelease { tag: "P-2".into(), ..rel("bad.tar.gz") }, &out, &|_| {}).is_err());
        let left: Vec<_> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left, ["P-1"], "no half build or temp files left");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn ansi_md5_names() {
        // "Kingdom Come Deliverance II – OST" as QuickSFV writes it (en dash 0x96, é 0xe9)
        let b = b"a *..\\KCD II \x96 OST\\caf\xe9.mp3".to_vec();
        assert_eq!(decode_ansi(b), "a *..\\KCD II – OST\\café.mp3");
        assert_eq!(decode_ansi("ü – ok".as_bytes().to_vec()), "ü – ok"); // already UTF-8: untouched
    }

    #[test]
    fn verify_good_bad_missing() {
        let d = tmp("verify");
        std::fs::create_dir_all(d.join("MD5")).unwrap();
        std::fs::write(d.join("good.bin"), b"").unwrap();
        std::fs::write(d.join("bad.bin"), b"x").unwrap();
        let md5 = "d41d8cd98f00b204e9800998ecf8427e *..\\good.bin\nd41d8cd98f00b204e9800998ecf8427e *..\\bad.bin\nd41d8cd98f00b204e9800998ecf8427e *..\\gone.bin\nd41d8cd98f00b204e9800998ecf8427e *..\\fg-selective-japanese-vo.bin\n";
        std::fs::write(d.join("MD5/fitgirl-bins.md5"), md5).unwrap();
        let f = find_md5(&d).unwrap();
        assert_eq!(verify(&f, &|_| {}).unwrap(), [r"mismatch: ..\bad.bin", r"missing: ..\gone.bin"]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn verify_rejects_escapes_and_devices() {
        let d = tmp("verify-evil");
        std::fs::create_dir_all(d.join("MD5")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", d.join("zero.bin")).unwrap();
        let h = "d41d8cd98f00b204e9800998ecf8427e";
        std::fs::write(d.join("MD5/fitgirl-bins.md5"), format!("{h} *..\\..\\etc\\passwd\n{h} */etc/passwd\n{h} *..\\zero.bin\n")).unwrap();
        let r = verify(&d.join("MD5/fitgirl-bins.md5"), &|_| {}).unwrap();
        assert!(r.len() == 3 && r.iter().all(|m| m.starts_with("unreadable: ")), "{r:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn inno_flags() {
        assert_eq!(inno_args(r"C:\Games\my-game", false), ["/SP-", r"/DIR=C:\Games\my-game", r"/LOG=C:\repackd-install.log"]);
        assert_eq!(inno_args(r"C:\Games\my-game", true).len(), 7);
        assert_eq!(slug("Cyberpunk 2077: Ultimate (v2.1)"), "cyberpunk-2077-ultimate-v2-1");
        assert_eq!(slug("???"), "game");
    }

    #[test]
    fn launch_scripts() {
        let d = tmp("launch");
        let mut g = Game { name: "g".into(), dir: d.clone(), exe: Some(d.join("pfx/drive_c/Games/g/it's.exe")), env: "DXVK_ASYNC=1\n# c\n".into(), args: "-dx11".into(), ..Default::default() };
        let ryujinx = d.join("Ryujinx");
        std::fs::write(&ryujinx, b"").unwrap(); // the emulator must exist to be used
        let st = Settings { switch: ryujinx.display().to_string(), ..Default::default() };
        write_launch_script(&g, &st).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        assert!(s.contains("export DXVK_ASYNC='1'\n") && s.contains(r"umu-run '") && s.contains(r#"it'\''s.exe' '-dx11' "$@""#) && !s.contains("WINEDLLOVERRIDES"), "{s}");
        let game = d.join("pfx/drive_c/Games/g");
        std::fs::create_dir_all(&game).unwrap();
        for f in ["VERSION.dll", "winmm.dll", "steam_api64.dll"] {
            std::fs::write(game.join(f), b"").unwrap();
        }
        write_launch_script(&g, &st).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        assert!(s.contains("export WINEDLLOVERRIDES='version=n,b;winmm=n,b'\n"), "{s}");
        g.env = "WINEDLLOVERRIDES=dxgi=n".into(); // a ReShade user keeps the crack working
        write_launch_script(&g, &st).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        assert!(s.contains("export WINEDLLOVERRIDES='version=n,b;winmm=n,b;dxgi=n'\n") && s.matches("WINEDLLOVERRIDES").count() == 1, "{s}");
        assert!(s.contains(&format!("mkdir -p '{0}/shadercache/vkd3d' && export VKD3D_SHADER_CACHE_PATH='{0}/shadercache/vkd3d'\n", d.display())) && s.contains("export __GL_SHADER_DISK_CACHE_SKIP_CLEANUP=1\n"), "{s}");
        g.env = "MESA_SHADER_CACHE_DIR=/mine".into(); // the user's own line wins
        write_launch_script(&g, &st).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        assert!(s.contains("export MESA_SHADER_CACHE_DIR='/mine'\n") && s.matches("MESA_SHADER_CACHE_DIR").count() == 1, "{s}");
        g.env = String::new();
        g.kind = Kind::Switch;
        g.exe = Some(d.join("Game [01].nsp"));
        g.args = Kind::Switch.default_args(&st.switch);
        write_launch_script(&g, &st).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        assert!(s.contains(&format!("exec '{}' '{}/Game [01].nsp' \"$@\"", ryujinx.display(), d.display())) && !s.contains("WINEPREFIX"), "{s}");
        g.kind = Kind::Ps4; // no emulator set
        assert!(write_launch_script(&g, &st).is_err());
        g.env = "1BAD=x".into();
        assert!(parse_env(&g.env).is_err());
        assert_eq!(Kind::Switch.default_args("/usr/bin/eden"), "-f -g {file}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn desktop_entry() {
        let d = tmp("desktop");
        let apps = d.join("apps");
        let mut g = Game { name: "My Game".into(), dir: d.join(r#"games/a"b$c`d\e"#), ..Default::default() };
        std::fs::create_dir_all(&g.dir).unwrap();
        write_desktop(&g, &apps).unwrap();
        let f = apps.join(r#"repackd-a"b$c`d\e.desktop"#);
        let s = std::fs::read_to_string(&f).unwrap();
        let want = format!("Exec=\"{}/games/a\\\\\"b\\\\$c\\\\`d\\\\\\\\e/launch.sh\"\n", d.display());
        assert!(s.contains("Name=My Game\n") && s.contains(&want) && s.contains("Categories=Game;\nTerminal=false\n") && !s.contains("Icon="), "{s}");
        std::fs::write(g.dir.join("cover.png"), b"").unwrap();
        g.name = "Renamed".into();
        write_desktop(&g, &apps).unwrap();
        let s = std::fs::read_to_string(&f).unwrap();
        assert!(s.contains("Name=Renamed\n") && s.contains(&format!("Icon={}/cover.png\n", g.dir.display())), "{s}");
        remove_desktop(&g, &apps);
        assert!(!f.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn diagnostics_has_game_and_log_tail() {
        let d = tmp("diag");
        let log: String = (0..100).map(|n| format!("line-{n:03}\n")).collect();
        std::fs::write(d.join("last-run.log"), log).unwrap();
        let g = Game { name: "Diag Game".into(), dir: d.clone(), env: "A=1".into(), ..Default::default() };
        let r = diagnostics(&g, &Settings::default());
        assert!(r.contains(env!("CARGO_PKG_VERSION")) && r.contains("Game: Diag Game") && r.contains("A=1") && r.contains("line-099") && r.contains("line-040") && !r.contains("line-039"), "{r}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn targets_put_the_game_before_dlc() {
        let d = tmp("targets");
        for f in ["data/dlc/a-dlc.nsp", "data/games/Zelda.nsp", "emu/Ryujinx.exe", "_Redist/vc_redist.x64.exe", "unins000.exe", "Game.exe"] {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), b"").unwrap();
        }
        let rel = |v: Vec<PathBuf>| v.iter().map(|p| p.strip_prefix(&d).unwrap().display().to_string()).collect::<Vec<_>>();
        assert_eq!(rel(targets(&d, Kind::Switch)), ["data/games/Zelda.nsp", "data/dlc/a-dlc.nsp"]);
        assert_eq!(rel(targets(&d, Kind::Windows)), ["Game.exe", "emu/Ryujinx.exe", "_Redist/vc_redist.x64.exe"]);
        for f in ["RapidCRC.exe", "Bin/Win64/BsSndRpt64.exe", "Bin/Win64/KingdomCome.exe", "UnityCrashHandler64.exe"] {
            std::fs::create_dir_all(d.join(f).parent().unwrap()).unwrap();
            std::fs::write(d.join(f), b"").unwrap();
        }
        let w = rel(targets(&d, Kind::Windows));
        assert_eq!(&w[..3], ["Game.exe", "emu/Ryujinx.exe", "Bin/Win64/KingdomCome.exe"], "{w:?}");
        assert!(w[3..].iter().all(|p| !p.contains("KingdomCome")), "helpers after the game: {w:?}");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn targets_do_not_follow_folder_links() {
        let d = tmp("links");
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin/Game.exe"), b"").unwrap();
        std::os::unix::fs::symlink(&d, d.join("bin/loop")).unwrap(); // a mod tool's link back to the game root
        assert_eq!(targets(&d, Kind::Windows), [d.join("bin/Game.exe")]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn clean_names() {
        assert_eq!(clean_name("Dave the Diver [FitGirl Repack]"), "Dave the Diver");
        assert_eq!(clean_name("Game X - v1.2.3 + 5 DLCs + Bonus Content [FitGirl Repack]"), "Game X");
        assert_eq!(clean_name("Game Y, v2.0 (DODI Repack)"), "Game Y");
        assert_eq!(clean_name("Dungeons + Dragons"), "Dungeons + Dragons");
        assert_eq!(clean_name("[FitGirl Repack]"), "[FitGirl Repack]");
    }

    #[test]
    fn check_fails_only_on_bad_files() {
        let d = tmp("check");
        std::fs::write(d.join("a.bin"), b"").unwrap();
        std::fs::write(d.join("ok.md5"), "d41d8cd98f00b204e9800998ecf8427e *a.bin\n").unwrap();
        std::fs::write(d.join("bad.md5"), "00000000000000000000000000000000 *a.bin\n").unwrap();
        assert!(check(&d.join("ok.md5"), &|_| {}).is_ok());
        assert!(check(&d.join("bad.md5"), &|_| {}).unwrap_err().to_string().contains("1 file(s) are damaged"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn running_game_is_not_deleted_until_killed() {
        let d = tmp("running");
        let repack = d.join("Run Game");
        std::fs::create_dir_all(&repack).unwrap();
        std::fs::write(repack.join("setup.exe"), b"").unwrap();
        let mut lib = Library::load(d.join("lib.json"), d.join("games")).unwrap();
        lib.add(repack).unwrap();
        let pfx = lib.games[0].prefix();
        std::fs::create_dir_all(&pfx).unwrap();
        // stands in for a Wine process: Proton runs them with WINEPREFIX=<pfx>/pfx/
        let mut child = Command::new("sleep").arg("30").env("WINEPREFIX", pfx.join("pfx/")).spawn().unwrap();
        assert_eq!(prefix_pids(&pfx), [child.id()]);
        assert!(lib.delete(0).unwrap_err().to_string().contains("is running"));
        assert_eq!(kill_prefix(&pfx), 1);
        child.wait().unwrap();
        assert!(prefix_pids(&pfx).is_empty());
        lib.delete(0).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn proton_use_and_delete() {
        let d = tmp("protonuse");
        let c = d.join("compatibilitytools.d");
        std::fs::create_dir_all(c.join("GE-Proton9-27")).unwrap();
        std::fs::create_dir_all(c.join("GE-Proton10-5")).unwrap();
        let ps = vec![("GE-Proton (latest)".to_string(), "GE-Proton".to_string()), ("GE-Proton9-27".into(), c.join("GE-Proton9-27").display().to_string()), ("GE-Proton10-5".into(), c.join("GE-Proton10-5").display().to_string())];
        let on = |p: &str| Game { proton: p.into(), ..Default::default() };
        let games = [on(""), on("GE-Proton"), on(&ps[1].1), on(&ps[1].1)];
        assert_eq!(proton_users(&games, &ps, &ps[1].1), 2);
        assert_eq!(proton_users(&games, &ps, &ps[2].1), 2, "default games run the newest GE-Proton");
        let rel = |tag: &str| ProtonRelease { source: "GE-Proton", tag: tag.into(), url: String::new(), sha_url: None, size: 0 };
        assert!(newer_ge(&[rel("GE-Proton10-5")], &ps).is_none());
        assert_eq!(newer_ge(&[rel("GE-Proton10-6")], &ps).unwrap().tag, "GE-Proton10-6");
        assert!(delete_proton(&d).is_err(), "not inside a compatibilitytools.d");
        assert!(delete_proton(Path::new("/usr/share/steam/compatibilitytools.d/x")).is_err());
        delete_proton(&c.join("GE-Proton9-27")).unwrap();
        assert!(!c.join("GE-Proton9-27").exists() && c.join("GE-Proton10-5").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deleting_a_repack_keeps_other_files() {
        // a repack unpacked straight into a folder with the user's own files (e.g. ~/Downloads)
        let d = tmp("delrepack");
        let dl = d.join("Downloads");
        std::fs::create_dir_all(dl.join("MD5")).unwrap();
        for f in ["setup.exe", "fg-01.bin", "fg-selective-english.bin", "notes.txt", "scph5501.bin", "Verify BIN files before installation.bat"] {
            std::fs::write(dl.join(f), b"x").unwrap();
        }
        std::fs::write(dl.join("MD5/fitgirl-bins.md5"), "0123456789abcdef0123456789abcdef *..\\fg-01.bin\r\n0123456789abcdef0123456789abcdef *..\\..\\outside.txt\r\n").unwrap();
        std::fs::write(d.join("outside.txt"), b"keep").unwrap();
        let g = Game { repack_dir: dl.clone(), dir: d.join("games/g"), status: Status::Installed, ..Default::default() };
        assert_eq!(delete_repack(&g).unwrap(), 2);
        let mut left: Vec<_> = std::fs::read_dir(&dl).unwrap().flatten().map(|e| e.file_name()).collect();
        left.sort();
        assert_eq!(left, ["notes.txt", "scph5501.bin"], "only the repack's own files go");
        assert!(d.join("outside.txt").exists(), "a checksum list can't reach outside the folder");
        std::fs::remove_file(dl.join("notes.txt")).unwrap();
        std::fs::remove_file(dl.join("scph5501.bin")).unwrap();
        std::fs::write(dl.join("setup.exe"), b"x").unwrap();
        assert_eq!(delete_repack(&g).unwrap(), 0);
        assert!(!dl.exists(), "an emptied repack folder is removed");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_automatic_save_backups_are_pruned() {
        let d = tmp("prune");
        let g = Game { dir: d.join("games/nier-automata"), ..Default::default() }; // "-auto" in the game's folder
        let saves = saves_dir(&g);
        std::fs::create_dir_all(&saves).unwrap();
        std::fs::write(saves.join("2026-01-01_10-00-00.tar.gz"), b"").unwrap(); // made by hand
        for k in 0..7 {
            std::fs::write(saves.join(format!("2026-01-0{}_10-00-00-auto.tar.gz", k + 2)), b"").unwrap();
        }
        prune_backups(&g, 5);
        let left = backups(&g);
        assert_eq!(left.len(), 6, "{left:?}");
        assert!(saves.join("2026-01-01_10-00-00.tar.gz").exists(), "the manual backup stays");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_refuses_without_disk_space() {
        let d = tmp("space");
        std::fs::write(d.join("setup.exe"), b"").unwrap();
        let g = Game { name: "g".into(), repack_dir: d.clone(), dir: d.join("games/g"), size: Some(1 << 60), ..Default::default() };
        let e = install_checks(&g, false, &|_| {}).unwrap_err().to_string();
        assert!(e.contains("not enough disk space"), "{e}");
        assert!(!g.dir.exists(), "nothing created before the check");
        assert!(free_space(&d.join("not/yet/there")).is_some_and(|f| f > 0));
        assert_eq!(setup_size(b"not an installer"), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn scan_finds_repacks_and_games_not_known_ones() {
        let d = tmp("scan");
        let w = |p: &str| {
            std::fs::create_dir_all(d.join(p).parent().unwrap()).unwrap();
            std::fs::write(d.join(p), b"x").unwrap();
        };
        for p in ["dl/Repack A/setup.exe", "dl/Repack A/MD5/fitgirl-bins.md5", "dl/Game B/Bin/b.exe", "dl/Known C/setup.exe", "dl/Known C/fg-01.bin", "dl/Junk/readme.txt", "dl/Plain D/setup.exe"] {
            w(p);
        }
        let mut lib = Library::load(d.join("lib.json"), d.join("games")).unwrap();
        lib.games.push(Game { name: "c".into(), repack_dir: d.join("dl/Known C"), dir: d.join("games/c"), ..Default::default() });
        let got = scan_folder(&d.join("dl"), &lib);
        assert_eq!(got, [Found { dir: d.join("dl/Game B"), repack: false }, Found { dir: d.join("dl/Repack A"), repack: true }]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn import_keeps_the_game_folder() {
        let d = tmp("import");
        let gd = d.join("Kingdom Come - Deliverance II");
        std::fs::create_dir_all(gd.join("Bin/Win64")).unwrap();
        std::fs::write(gd.join("Bin/Win64/KingdomCome.exe"), b"").unwrap();
        let mut lib = Library::load(d.join("lib.json"), d.join("games")).unwrap();
        let i = lib.import(gd.clone()).unwrap();
        let g = &lib.games[i];
        assert_eq!((g.kind, g.status, g.install_dir()), (Kind::Windows, Status::Installed, gd.clone()));
        assert_eq!(g.install_dir_win(), r"R:\Kingdom Come - Deliverance II");
        std::fs::create_dir_all(g.prefix().join("dosdevices")).unwrap();
        g.map_game_drive(&|_| {}).unwrap();
        assert_eq!(std::fs::read_link(g.prefix().join("dosdevices/r:")).unwrap(), d);
        assert!(!g.has_repack());
        assert!(lib.import(gd.clone()).unwrap_err().to_string().contains("already"));
        std::fs::create_dir_all(lib.games[i].prefix()).unwrap();
        lib.delete(i).unwrap();
        assert!(gd.join("Bin/Win64/KingdomCome.exe").exists(), "the game's own folder is never deleted");
        // a Switch dump is recognised by its .nsp
        let sw = d.join("Zelda");
        std::fs::create_dir_all(sw.join("data/games")).unwrap();
        std::fs::write(sw.join("data/games/z.nsp"), b"").unwrap();
        std::fs::write(sw.join("suyu.exe"), b"").unwrap();
        let i = lib.import(sw).unwrap();
        assert_eq!(lib.games[i].kind, Kind::Switch);
        assert!(lib.import(d.join("empty-nothing")).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn saves_are_backed_up_before_delete_and_restore() {
        let d = tmp("saves");
        std::fs::create_dir_all(d.join("Some Game")).unwrap();
        std::fs::write(d.join("Some Game/setup.exe"), b"").unwrap();
        let mut lib = Library::load(d.join("lib.json"), d.join("games")).unwrap();
        lib.add(d.join("Some Game")).unwrap();
        let g = lib.games[0].clone();
        let save = profile(&g).join("AppData/Roaming/Studio/Game/save1.sav");
        let cache = profile(&g).join("AppData/Local/D3DSCache/big.bin");
        for (f, body) in [(&save, &b"progress"[..]), (&cache, &b"cache"[..])] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        lib.delete(0).unwrap();
        assert!(!g.dir.exists());
        let b = backups(&g);
        assert_eq!(b.len(), 1, "delete kept a backup outside the game dir: {b:?}");
        assert!(b[0].starts_with(d.join("games-saves/some-game")));
        restore_saves(&g, &b[0]).unwrap();
        assert_eq!(std::fs::read(&save).unwrap(), b"progress");
        assert!(!cache.exists(), "caches are not backed up");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn settings_defaults_match_with_and_without_a_file() {
        assert!(Settings::default().verify && Settings::default().unattended);
        let old: Settings = serde_json::from_str(r#"{"switch":"","ps4":"","sgdb_key":"k"}"#).unwrap();
        assert!(old.verify && old.unattended);
    }

    #[test]
    fn switch_repack_sets_itself_up_with_the_bundled_emulator() {
        let d = tmp("bundled");
        let mut g = Game { name: "z".into(), dir: d.join("z"), status: Status::Installed, proton: default_proton(), ..Default::default() };
        let inst = g.install_dir();
        for f in ["data/games/Zelda.nsp", "emu/Ryujinx.exe", "suyu.exe", "Launcher.exe"] {
            std::fs::create_dir_all(inst.join(f).parent().unwrap()).unwrap();
            std::fs::write(inst.join(f), b"").unwrap();
        }
        let st = Settings::default(); // no Linux emulator
        assert!(g.auto_setup(&st));
        assert_eq!((g.kind, g.exe.as_deref()), (Kind::Switch, Some(inst.join("data/games/Zelda.nsp").as_path())));
        assert_eq!(g.emulator.as_deref(), Some(inst.join("emu/Ryujinx.exe").as_path()));
        assert!(!g.auto_setup(&st), "a second run changes nothing");
        write_launch_script(&g, &st).unwrap();
        let s = std::fs::read_to_string(g.launch_script()).unwrap();
        let z = |p: &Path| format!("Z:{}", p.display()).replace('/', "\\");
        assert!(s.contains("exec umu-run") && s.contains(&format!("'-r' '{}\\data' '{}'", z(&inst), z(&inst.join("data/games/Zelda.nsp")))), "{s}");
        assert!(s.contains(&format!("cd '{}'", inst.join("emu").display())), "{s}");
        // a Linux emulator in Settings wins over the bundled one
        let emu = d.join("Ryujinx.AppImage");
        std::fs::write(&emu, b"").unwrap();
        let st = Settings { switch: emu.display().to_string(), ..Default::default() };
        g.emulator = None;
        g.args = Kind::Switch.default_args(&st.switch);
        assert!(!g.auto_setup(&st));
        write_launch_script(&g, &st).unwrap();
        assert!(std::fs::read_to_string(g.launch_script()).unwrap().contains(&format!("exec '{}' '{}'", emu.display(), inst.join("data/games/Zelda.nsp").display())));
        // an imported game on a disk that is not mounted keeps the file the user picked
        let picked = d.join("usb/Game/Bin/Game.exe");
        let mut off = Game { dir: d.join("off"), game_dir: Some(d.join("usb/Game")), exe: Some(picked.clone()), ..Default::default() };
        assert!(!off.auto_setup(&st) && off.exe == Some(picked));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn library_reads_old_entries() {
        let old = r#"[{"name":"a","repack_dir":"/r","dir":"/g/a","status":"Installed","exe":null,"steam":null}]"#;
        let g: Vec<Game> = serde_json::from_str(old).unwrap();
        assert_eq!((g[0].kind, g[0].version.as_str(), g[0].proton.as_str()), (Kind::Windows, "", "GE-Proton"));
    }

    #[test]
    fn stall_watch_warns_once_when_nothing_is_written() {
        let d = tmp("stall");
        let (done, lines) = (AtomicBool::new(false), std::sync::Mutex::new(vec![]));
        std::thread::scope(|s| {
            s.spawn(|| stall_watch(&d, None, &done, Duration::from_millis(20), Duration::from_millis(100), &|l| lines.lock().unwrap().push(l)));
            std::thread::sleep(Duration::from_millis(400));
            done.store(true, Ordering::Relaxed);
        });
        let n = lines.lock().unwrap().len();
        assert_eq!(n, 1, "one hint, not one per tick");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn file_uris_are_encoded() {
        assert_eq!(file_uri(Path::new("/home/a/Games/Some Game [FitGirl]/it's")), "file:///home/a/Games/Some%20Game%20%5BFitGirl%5D/it%27s");
        assert_eq!(file_uri(Path::new("/tmp/é")), "file:///tmp/%C3%A9");
    }

    #[test]
    fn log_scan() {
        let ok = "Log opened.\nInstallation process succeeded.\n2026-10-22 19:13:13.643   Log closed.\r\n\r\n";
        let killed = "Log opened.\nInstallation process succeeded.\n";
        let bad = "Log opened.\nMessage box (OK):\nISDone.dll: An error occurred when unpacking! Unarc.dll returned an error code: -1\nRolling back changes.\n";
        assert!(log_errors(ok).is_empty());
        assert_eq!(log_errors(bad).len(), 2);
        assert!(inno_result(true, ok, &|_| {}).is_ok());
        assert!(inno_result(true, bad, &|_| {}).unwrap_err().to_string().contains("ISDone")); // exit 0 after rollback
        assert!(inno_result(false, ok, &|_| {}).is_err());
        assert!(inno_result(true, killed, &|_| {}).unwrap_err().to_string().contains("did not finish"));
        assert!(inno_result(true, "", &|_| {}).is_err()); // no log at all
    }

    #[test]
    fn library_roundtrip_and_delete_guard() {
        let d = tmp("lib");
        let repack = d.join("Some Game [FitGirl Repack]");
        std::fs::create_dir_all(&repack).unwrap();
        std::fs::write(repack.join("Setup.exe"), b"").unwrap();
        let mut lib = Library::load(d.join("lib.json"), d.join("games")).unwrap();
        assert_eq!(lib.add(repack.clone()).unwrap(), 0);
        assert!(lib.add(repack.clone()).unwrap_err().to_string().contains("already in the library"));
        // same folder name elsewhere: a second game with its own dir
        let twin = d.join("other/Some Game [FitGirl Repack]");
        std::fs::create_dir_all(&twin).unwrap();
        std::fs::write(twin.join("setup.exe"), b"").unwrap();
        lib.add(twin).unwrap();
        assert_eq!(lib.games[1].dir, d.join("games/some-game-2"));
        lib.save().unwrap();
        let back = Library::load(d.join("lib.json"), d.join("games")).unwrap();
        assert_eq!(back.games, lib.games);
        std::fs::create_dir_all(lib.games[0].prefix()).unwrap();
        lib.delete(0).unwrap();
        assert!(!d.join("games/some-game-fitgirl-repack").exists() && repack.join("Setup.exe").exists());
        // a Steam shortcut is removed even when the game file was cleared (type change)
        let vdf = d.join("shortcuts.vdf");
        let g = &mut lib.games[0];
        std::fs::create_dir_all(&g.dir).unwrap();
        steam::add(&vdf, "g", &g.launch_script(), &g.dir).unwrap();
        g.steam = Some(vdf.clone());
        if !steam::running() {
            lib.delete(0).unwrap();
            assert!(!steam::remove(&vdf, &d.join("games/some-game-fitgirl-repack-2/launch.sh")).unwrap(), "shortcut left behind");
            lib.add(repack.clone()).unwrap();
        }
        // a bad dir is refused before the Steam entry is touched
        lib.games[0].dir = d.join("games/../elsewhere");
        lib.games[0].steam = Some(d.join("no-such-dir/shortcuts.vdf"));
        assert!(lib.delete(0).unwrap_err().to_string().contains("refusing"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
