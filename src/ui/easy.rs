//! The window: a big-picture library in the chosen colour preset. A grid of covers, one page per game with one
//! big button (Install / Play), and a gear on that page for every option of the game. Keyboard: arrows,
//! Enter, Esc, F11.
use super::{App, Job, Note};
use crate::{find_setup, gb, Kind, Status};
use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Key, Rect, RichText, Sense, Stroke, StrokeKind, Ui};
use std::path::Path;
use std::time::Duration;

/// A colour preset: page, top bar, tiles/buttons, focus accent, Install and Play button fills, text on those
/// buttons, text, secondary text, errors.
pub(super) struct Theme {
    pub name: &'static str,
    dark: bool,
    bg: u32,
    bar: u32,
    tile: u32,
    accent: u32,
    install: u32,
    play: u32,
    on_button: u32,
    text: u32,
    sub: u32,
    err: u32,
}

pub(super) const THEMES: [Theme; 6] = [
    Theme { name: "Gruvbox", dark: true, bg: 0x282828, bar: 0x1d2021, tile: 0x3c3836, accent: 0xd79921, install: 0x458588, play: 0x98971a, on_button: 0xebdbb2, text: 0xebdbb2, sub: 0xa89984, err: 0xfb4934 },
    Theme { name: "Gruvbox Light", dark: false, bg: 0xfbf1c7, bar: 0xf2e5bc, tile: 0xebdbb2, accent: 0xb57614, install: 0x076678, play: 0x79740e, on_button: 0xfbf1c7, text: 0x3c3836, sub: 0x7c6f64, err: 0x9d0006 },
    Theme { name: "Catppuccin Mocha", dark: true, bg: 0x1e1e2e, bar: 0x181825, tile: 0x313244, accent: 0xf9e2af, install: 0x89b4fa, play: 0xa6e3a1, on_button: 0x1e1e2e, text: 0xcdd6f4, sub: 0xa6adc8, err: 0xf38ba8 },
    Theme { name: "Nord", dark: true, bg: 0x2e3440, bar: 0x242933, tile: 0x3b4252, accent: 0xebcb8b, install: 0x81a1c1, play: 0xa3be8c, on_button: 0x2e3440, text: 0xeceff4, sub: 0xaab4c6, err: 0xbf616a },
    Theme { name: "Dracula", dark: true, bg: 0x282a36, bar: 0x21222c, tile: 0x44475a, accent: 0xf1fa8c, install: 0xbd93f9, play: 0x50fa7b, on_button: 0x282a36, text: 0xf8f8f2, sub: 0xa9aecb, err: 0xff5555 },
    Theme { name: "Tokyo Night", dark: true, bg: 0x1a1b26, bar: 0x16161e, tile: 0x292e42, accent: 0xe0af68, install: 0x7aa2f7, play: 0x9ece6a, on_button: 0x1a1b26, text: 0xc0caf5, sub: 0x9aa5ce, err: 0xf7768e },
];

/// Text on the dark shades over artwork (badges, the banner's name), light in every preset.
const OVERLAY_TEXT: Color32 = Color32::from_gray(235);

static THEME: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Use the preset with this name (unknown: the first).
pub(super) fn set_theme(name: &str) {
    THEME.store(THEMES.iter().position(|t| t.name == name).unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
}

fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// The current preset's colours, by role.
struct C;
impl C {
    fn t() -> &'static Theme {
        &THEMES[THEME.load(std::sync::atomic::Ordering::Relaxed)]
    }
    fn bg() -> Color32 {
        rgb(Self::t().bg)
    }
    fn bar() -> Color32 {
        rgb(Self::t().bar)
    }
    fn tile() -> Color32 {
        rgb(Self::t().tile)
    }
    fn accent() -> Color32 {
        rgb(Self::t().accent)
    }
    fn install() -> Color32 {
        rgb(Self::t().install)
    }
    fn play() -> Color32 {
        rgb(Self::t().play)
    }
    fn on_button() -> Color32 {
        rgb(Self::t().on_button)
    }
    fn text() -> Color32 {
        rgb(Self::t().text)
    }
    fn sub() -> Color32 {
        rgb(Self::t().sub)
    }
    fn err() -> Color32 {
        rgb(Self::t().err)
    }
}
const TILE_W: f32 = 180.0;
const TILE_H: f32 = 270.0;
const GAP: f32 = 22.0;

/// The colour preset, large text, roomy spacing.
pub(super) fn style(ctx: &egui::Context) {
    let mut s = egui::Style { visuals: if C::t().dark { egui::Visuals::dark() } else { egui::Visuals::light() }, ..Default::default() };
    let v = &mut s.visuals;
    v.panel_fill = C::bg();
    v.window_fill = C::bg(); // dialogs: darker than the tile-coloured buttons on them
    v.extreme_bg_color = C::bar(); // text fields, scroll troughs
    v.faint_bg_color = C::tile();
    v.override_text_color = Some(C::text());
    v.weak_text_color = Some(C::sub());
    v.disabled_alpha = 0.7; // default 0.5 is too faint on dark themes
    v.error_fg_color = C::err();
    v.warn_fg_color = C::accent();
    v.hyperlink_color = C::install();
    v.selection.bg_fill = C::accent().lerp_to_gamma(C::bg(), 0.55);
    v.selection.stroke = Stroke::new(1.0, C::accent());
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, C::tile()); // separators
    for (w, fill) in [(&mut v.widgets.inactive, C::tile()), (&mut v.widgets.hovered, C::tile().lerp_to_gamma(C::accent(), 0.25)), (&mut v.widgets.active, C::tile().lerp_to_gamma(C::accent(), 0.4)), (&mut v.widgets.open, C::tile())] {
        (w.bg_fill, w.weak_bg_fill) = (fill, fill);
        w.corner_radius = CornerRadius::same(6);
        w.fg_stroke = Stroke::new(1.0, C::text());
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.5, C::accent());
    s.spacing.button_padding = vec2(16.0, 8.0);
    s.spacing.item_spacing = vec2(12.0, 12.0);
    use egui::TextStyle::*;
    for (t, size) in [(Heading, 26.0), (Body, 16.0), (Button, 17.0), (Small, 13.0), (Monospace, 13.0)] {
        if let Some(f) = s.text_styles.get_mut(&t) {
            f.size = size;
        }
    }
    ctx.set_global_style(s);
}

/// A name with Chinese, Japanese or Korean characters, which the built-in fonts lack.
pub(super) fn needs_cjk(name: &str) -> bool {
    name.chars().any(|c| c >= '\u{2E80}')
}

/// Add the system's CJK font as a fallback; whether it could be loaded.
pub(super) fn load_cjk(ctx: &egui::Context) -> bool {
    let Some(bytes) = std::process::Command::new("fc-match").args(["-f", "%{file}", "sans-serif:lang=ja"]).output().ok().and_then(|o| std::fs::read(String::from_utf8(o.stdout).ok()?).ok()) else { return false };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
    true
}

pub(super) fn show(app: &mut App, ui: &mut Ui) {
    if ui.input(|i| i.key_pressed(Key::F11)) {
        toggle_fullscreen(ui);
    }
    egui::Panel::top("easy-top").frame(egui::Frame::new().fill(C::bar()).inner_margin(egui::Margin::symmetric(24, 12))).show(ui, |ui| {
        if app.no_umu {
            let hint = crate::install_hint("umu-run");
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("repackd needs umu-launcher to run games. Install it, then restart repackd:").color(C::accent()).strong());
                ui.label(RichText::new(&hint).color(C::text()));
                if ui.small_button("Copy command").clicked() {
                    ui.ctx().copy_text(hint);
                }
            });
        }
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.set_min_height(36.0); // the title sits on the buttons' centre line
            let in_sub = app.easy_open.is_some() || app.global;
            if in_sub {
                let r = ui.add(egui::Label::new(RichText::new("<  LIBRARY").size(22.0).strong().color(C::text())).sense(Sense::click()));
                if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    if let (Some(i), true) = (app.easy_open, app.easy_gear) {
                        app.leave_settings(i);
                    }
                    app.global = false;
                    app.disk_page = false;
                    close_page(app);
                }
            } else {
                ui.label(RichText::new("LIBRARY").size(22.0).strong().color(C::text()));
                ui.add_space(16.0);
                if ui.add(egui::TextEdit::singleline(&mut app.search).hint_text("Search").desired_width(220.0).background_color(C::tile())).changed() {
                    app.easy_focus = 0; // Enter opens the first match
                }
                let sorts = [("", "As added"), ("name", "Name"), ("played", "Recently played")];
                let shown = sorts.iter().find(|(k, _)| *k == app.settings.sort).map_or("As added", |(_, l)| *l);
                let mut sort = app.settings.sort.clone();
                egui::ComboBox::from_id_salt("sort").selected_text(shown).show_ui(ui, |ui| {
                    for (k, l) in sorts {
                        ui.selectable_value(&mut sort, k.to_string(), l);
                    }
                });
                if sort != app.settings.sort {
                    app.settings.sort = sort;
                    app.save_settings();
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let full = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
                if ui.button(if full { "Window (F11)" } else { "Fullscreen (F11)" }).clicked() {
                    toggle_fullscreen(ui);
                }
                if !app.global {
                    let r = ui.button("      Settings").on_hover_text("settings for all games: artwork, installing, emulators, Proton versions");
                    gear(ui, pos2(r.rect.left() + 22.0, r.rect.center().y), 8.0, C::text());
                    if r.clicked() {
                        app.global = true;
                    }
                }
            });
        });
    });
    egui::CentralPanel::default().frame(egui::Frame::new().fill(C::bg()).inner_margin(egui::Margin::same(32))).show(ui, |ui| {
        if app.global {
            return if app.disk_page { disk_usage(app, ui) } else { global_settings(app, ui) };
        }
        match app.easy_open.filter(|&i| i < app.lib.games.len()) {
            Some(i) if app.easy_gear => settings(app, ui, i),
            Some(i) => page(app, ui, i),
            None => {
                app.easy_open = None;
                app.easy_gear = false;
                library(app, ui);
            }
        }
    });
}

/// Back to the library; a page's message stays with its page.
fn close_page(app: &mut App) {
    app.side = None; // files may have changed meanwhile (keys copied in, saves)
    app.easy_open = None;
    app.easy_gear = false;
    app.confirm_delete = false;
    if app.note.as_ref().is_some_and(|n| n.game.is_some()) {
        app.note = None;
    }
}

/// Settings for all games (top bar): artwork key, how to install, emulators, Proton versions.
fn global_settings(app: &mut App, ui: &mut Ui) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        app.global = false;
        return;
    }
    ui.horizontal(|ui| {
        if ui.button("<  Back").clicked() {
            app.global = false;
        }
        ui.vertical(|ui| {
            ui.label(RichText::new("Settings").heading());
            ui.label(RichText::new("For all games").color(C::sub()));
        });
    });
    if app.busy && app.job_game.is_none() {
        app.job_row(ui, 460.0, 28.0); // a Proton download
    }
    note_line(app, ui, None);
    let section = |ui: &mut Ui, title: &str| {
        ui.add_space(22.0);
        let r = ui.label(RichText::new(title).heading());
        ui.separator();
        r
    };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        section(ui, "Appearance");
        let cur = if app.settings.theme.is_empty() { THEMES[0].name } else { app.settings.theme.as_str() };
        let mut pick = cur.to_string();
        egui::ComboBox::from_id_salt("theme").selected_text(cur).width(260.0).show_ui(ui, |ui| {
            for t in &THEMES {
                ui.selectable_value(&mut pick, t.name.to_string(), t.name);
            }
        });
        if pick != cur {
            app.settings.theme = pick;
            app.save_settings();
            app.styled = false; // restyled next frame
        }

        section(ui, "Games folder");
        ui.label(RichText::new("New games go here. Existing games stay where they are; use Move in a game's settings to move one.").color(C::sub()));
        ui.add(egui::Label::new(RichText::new(app.lib.root.display().to_string()).color(C::sub())).wrap());
        ui.horizontal(|ui| {
            if ui.button("Choose…").clicked()
                && let Some(d) = rfd::FileDialog::new().set_title("Folder new games are installed into").pick_folder()
            {
                match crate::root_refusal(&d, &app.lib.games) {
                    Ok(()) => {
                        app.lib.root = d.clone();
                        app.settings.games_root = Some(d);
                        app.save_settings();
                    }
                    Err(e) => app.notify(None, format!("{e:#}"), true),
                }
            }
            if ui.add_enabled(app.settings.games_root.is_some(), egui::Button::new("Default")).clicked() {
                app.settings.games_root = None;
                app.lib.root = app.settings.root();
                app.save_settings();
            }
        });
        if ui.button("Disk usage").on_hover_text("what every game, its game data folder (Wine prefix) and its repack take").clicked() {
            app.disk_page = true;
            app.disk_rows.clear();
            app.measure_disk(app.lib.games.clone());
        }

        section(ui, "Watch folder");
        ui.label(RichText::new("Pick the folder where your downloads finish. New games there will appear in your library.").color(C::sub()));
        ui.add(egui::Label::new(RichText::new(app.settings.watch_dir.as_ref().map_or("Not set".into(), |d| d.display().to_string())).color(C::sub())).wrap());
        ui.horizontal(|ui| {
            if ui.button("Choose…").clicked()
                && let Some(d) = rfd::FileDialog::new().set_title("Folder your torrent client downloads repacks into").pick_folder()
            {
                app.settings.watch_dir = Some(d);
                app.save_settings();
                app.sync_watch();
            }
            if app.settings.watch_dir.is_some() && ui.button("Off").clicked() {
                app.settings.watch_dir = None;
                app.watching.clear();
                app.save_settings();
                app.sync_watch();
            }
        });

        section(ui, "Installing");
        let before = (app.settings.verify, app.settings.unattended);
        ui.checkbox(&mut app.settings.verify, "Check the files for damage when installing").on_hover_text("Check the files before and after installing (FitGirl's checksums)");
        ui.checkbox(&mut app.settings.unattended, "Hide the installer's windows when installing").on_hover_text("Install games without the installer's own windows (updates always show theirs)");
        let before = (before, app.settings.backup_updates, app.settings.backup_saves_after_play);
        ui.checkbox(&mut app.settings.backup_updates, "Keep an undo copy when updating").on_hover_text("Back up the files an update changes, so it can be undone (instant on btrfs, a full copy elsewhere)");
        ui.checkbox(&mut app.settings.backup_saves_after_play, "Back up saves after playing").on_hover_text("Back up saves after playing (the last 5 are kept)");
        if before != ((app.settings.verify, app.settings.unattended), app.settings.backup_updates, app.settings.backup_saves_after_play) {
            app.save_settings();
        }

        section(ui, "SteamGridDB key");
        ui.label(RichText::new("Covers, banners and logos come from SteamGridDB.").color(C::sub()));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut app.key_text).password(true).hint_text("paste the SteamGridDB API key").desired_width(380.0));
            if ui.add_enabled(app.key_text.trim() != app.settings.sgdb_key, egui::Button::new("Save key")).clicked() {
                let k = app.key_text.clone();
                app.set_sgdb_key(k);
                app.notify(None, if app.settings.sgdb_key.is_empty() { "Key removed." } else { "Key saved: fetching artwork for games that have none." }, false);
            }
            if !app.settings.sgdb_key.is_empty() {
                ui.label(RichText::new("key set").color(C::sub()));
            }
        });
        ui.label(RichText::new("Optional. Free account at steamgriddb.com, used only for cover art.").color(C::sub()).small());

        let r = section(ui, "Emulators");
        if std::mem::take(&mut app.scroll_to_emulators) {
            r.scroll_to_me(Some(egui::Align::TOP));
        }
        egui::Grid::new("emus").num_columns(2).spacing([18.0, 10.0]).min_col_width(170.0).show(ui, |ui| {
            for (k, label) in [(Kind::Switch, "Switch emulator"), (Kind::Ps4, "PS4 emulator")] {
                super::row_label(ui, label);
                ui.allocate_ui_with_layout(vec2(620.0, 34.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let r = ui.add(egui::TextEdit::singleline(app.settings.emulator_mut(k)).hint_text("the emulator program or AppImage").desired_width(380.0));
                    if ui.button("Browse…").clicked()
                        && let Some(f) = rfd::FileDialog::new().pick_file()
                    {
                        *app.settings.emulator_mut(k) = f.display().to_string();
                        app.emulator_changed(k);
                    } else if r.lost_focus() {
                        app.emulator_changed(k);
                    }
                    let emu = app.settings.emulator(k).to_string();
                    if !emu.is_empty() && !Path::new(&emu).is_file() {
                        ui.label(RichText::new("not found").color(C::err()));
                    }
                });
                ui.end_row();
            }
        });
        ui.label(RichText::new("Set each emulator up (keys, firmware) in the emulator itself.").color(C::sub()).small());

        section(ui, "Requirements");
        const TOOLS: [(&str, &str); 7] = [
            ("umu-run", "needed: runs every game and installer"),
            ("winetricks", "prefix tools and missing runtimes"),
            ("7z", "update archives (.rar, .7z, .zip)"),
            ("notify-send", "a notification when a long job ends"),
            ("mangohud", "the MangoHud switch"),
            ("gamemoderun", "the GameMode switch"),
            ("gamescope", "the Gamescope switch"),
        ];
        // looked up every 5 s, not every frame: a tool installed while the app is open shows up
        static FOUND: std::sync::Mutex<Option<(std::time::Instant, Vec<bool>)>> = std::sync::Mutex::new(None);
        let found = {
            let mut cache = FOUND.lock().unwrap_or_else(|e| e.into_inner());
            if cache.as_ref().is_none_or(|(t, _)| t.elapsed().as_secs() >= 5) {
                *cache = Some((std::time::Instant::now(), TOOLS.iter().map(|(b, _)| crate::which_path(b).is_some()).collect()));
            }
            cache.as_ref().map(|(_, f)| f.clone()).unwrap_or_default()
        };
        // only umu-run is needed to play; the rest wait in a collapsed header
        let tools = |ui: &mut Ui, id: &str, r: std::ops::Range<usize>| {
            egui::Grid::new(id).num_columns(3).spacing([18.0, 8.0]).show(ui, |ui| {
                for ((bin, what), found) in TOOLS[r.clone()].iter().zip(&found[r]) {
                    ui.label(*bin);
                    if *found {
                        ui.label(RichText::new("installed").color(C::play()));
                    } else {
                        ui.label(RichText::new(format!("missing: {}", crate::install_hint(bin))).color(if *bin == "umu-run" { C::err() } else { C::accent() }));
                    }
                    ui.label(RichText::new(*what).color(C::sub()).small());
                    ui.end_row();
                }
            });
        };
        tools(ui, "reqs", 0..1);
        egui::CollapsingHeader::new("Optional tools").default_open(false).show(ui, |ui| tools(ui, "reqs2", 1..TOOLS.len()));

        ui.add_space(22.0);
        egui::CollapsingHeader::new(RichText::new("Advanced: Proton versions").heading()).default_open(false).show(ui, |ui| {
            ui.label(RichText::new("Installed (each game picks one in its own settings):").color(C::sub()));
            for (label, value) in app.protons.clone() {
                let n = crate::proton_users(&app.lib.games, &app.protons, &value);
                let used = match n {
                    0 => "unused".to_string(),
                    1 => "used by 1 game".into(),
                    n => format!("used by {n} games"),
                };
                let path = Path::new(&value);
                // only the user's own folders: never /usr, never umu's automatic "GE-Proton" entry
                let own = path.parent().is_some_and(|p| p.ends_with("compatibilitytools.d")) && !path.starts_with("/usr");
                ui.horizontal(|ui| {
                    ui.label(if value == "GE-Proton" { label } else { format!("{label}    {value}") });
                    ui.label(RichText::new(used).color(C::sub()));
                    if !(own && n == 0) {
                        return;
                    }
                    if app.confirm_proton.as_ref() != Some(&value) {
                        if ui.add_enabled(!app.busy, egui::Button::new("Delete")).clicked() {
                            app.confirm_proton = Some(value.clone());
                        }
                        return;
                    }
                    ui.label(RichText::new("Delete it? Steam games may still use it.").color(C::err()));
                    if ui.button(RichText::new("Yes, delete").color(C::err())).clicked() {
                        app.confirm_proton = None;
                        if app.busy {
                            app.say("a job is running: wait for it before deleting a Proton".to_string());
                        } else if let Some(g) = app.lib.games.iter().find(|g| !crate::game_pids(g).is_empty()) {
                            app.say(format!("{} is running: close it before deleting a Proton", g.name));
                        } else {
                            let tx = app.tx.clone();
                            std::thread::spawn(move || {
                                let r = crate::delete_proton(Path::new(&value));
                                let _ = tx.send(super::Ev::ProtonsChanged);
                                let _ = tx.send(match r {
                                    Ok(()) => super::Ev::Note(None, "The Proton was deleted.".into(), false),
                                    Err(e) => super::Ev::Note(None, format!("{e:#}"), true),
                                });
                            });
                        }
                    }
                    if ui.button("Keep it").clicked() {
                        app.confirm_proton = None;
                    }
                });
            }
            if let Some(Ok(list)) = &app.releases
                && let Some(rel) = crate::newer_ge(list, &app.protons).cloned()
            {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("New: {} available", rel.tag)).color(C::accent()));
                    if ui.add_enabled(!app.busy, egui::Button::new("Download")).clicked() {
                        let tx = app.tx.clone();
                        app.spawn_global(ui.ctx(), "Getting Proton", move |r| {
                            crate::install_proton(&rel, &crate::compat_dir(), r)?;
                            let _ = tx.send(super::Ev::ProtonsChanged);
                            let _ = tx.send(super::Ev::Note(None, format!("{} is installed: pick it in a game's settings.", rel.tag), false));
                            Ok(())
                        });
                    }
                });
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!app.busy, egui::Button::new("Find Proton releases")).clicked() {
                    app.releases = None;
                    let tx = app.tx.clone();
                    std::thread::spawn(move || _ = tx.send(super::Ev::ProtonList(crate::proton_releases().map_err(|e| format!("{e:#}")))));
                }
            });
            ui.label(RichText::new("GE-Proton suits most games; Proton-CachyOS and Proton-EM carry extra fixes for some newer ones.").color(C::sub()).small());
            ui.add(egui::Label::new(RichText::new(format!("Downloads go to {}", crate::compat_dir().display())).color(C::sub()).small()).wrap());
            match app.releases.clone() {
                Some(Err(e)) => _ = ui.label(RichText::new(e).color(C::err())),
                Some(Ok(list)) => {
                    for (source, _) in crate::PROTON_SOURCES {
                        ui.add_space(8.0);
                        ui.label(RichText::new(source).color(C::accent()).strong());
                        if !list.iter().any(|r| r.source == source) {
                            ui.label(RichText::new("Could not be listed right now (GitHub's limit, or the project moved).").color(C::sub()).small());
                        }
                        egui::Grid::new(("releases", source)).num_columns(3).spacing([18.0, 8.0]).show(ui, |ui| {
                            for rel in list.iter().filter(|r| r.source == source).take(6) {
                                ui.label(&rel.tag);
                                ui.label(RichText::new(gb(rel.size)).color(C::sub()));
                                if app.protons.iter().any(|(folder, _)| rel.installed_as(folder)) {
                                    ui.label(RichText::new("installed").color(C::sub()));
                                } else if ui.add_enabled(!app.busy, egui::Button::new("Download")).clicked() {
                                    let (rel, tx) = (rel.clone(), app.tx.clone());
                                    app.spawn_global(ui.ctx(), "Getting Proton", move |r| {
                                        crate::install_proton(&rel, &crate::compat_dir(), r)?;
                                        let _ = tx.send(super::Ev::ProtonsChanged);
                                        let _ = tx.send(super::Ev::Note(None, format!("{} is installed: pick it in a game's settings.", rel.tag), false));
                                        Ok(())
                                    });
                                }
                                ui.end_row();
                            }
                        });
                    }
                }
                None => {}
            }
        });
        ui.add_space(12.0);
    });
}

/// Disk usage of every game (game folder, Wine prefix, repack), biggest first; sizes arrive from a thread.
fn disk_usage(app: &mut App, ui: &mut Ui) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        app.disk_page = false;
        return;
    }
    ui.horizontal(|ui| {
        if ui.button("<  Back").clicked() {
            app.disk_page = false;
        }
        ui.vertical(|ui| {
            ui.label(RichText::new("Disk usage").heading());
            let all = app.lib.games.iter().filter_map(|g| app.disk_rows.get(&g.dir)).fold([0u64; 3], |t, s| [t[0] + s[0], t[1] + s[1], t[2] + s[2]]);
            let done = app.lib.games.iter().all(|g| app.disk_rows.contains_key(&g.dir));
            let t = if done { "" } else { " …" };
            ui.label(RichText::new(format!("Total {}{t} · games {} · game data folders {} · repacks {}", gb(all.iter().sum()), gb(all[0]), gb(all[1]), gb(all[2]))).color(C::sub()));
        });
    });
    note_line(app, ui, None);
    ui.add_space(12.0);
    let mut rows: Vec<crate::Game> = app.lib.games.clone();
    rows.sort_by_key(|g| std::cmp::Reverse(app.disk_rows.get(&g.dir).map_or(0, |s| s.iter().sum::<u64>())));
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        for g in rows {
            let s = app.disk_rows.get(&g.dir).copied();
            ui.horizontal(|ui| {
                ui.label(RichText::new(&g.name).strong());
                let sz = |n: Option<u64>| n.map_or("…".to_string(), gb);
                ui.label(format!("{}  (game {} · prefix {} · repack {})", sz(s.map(|s| s.iter().sum())), sz(s.map(|s| s[0])), sz(s.map(|s| s[1])), sz(s.map(|s| s[2]))));
                if g.status != Status::Installed || !g.repack_dir.is_dir() {
                    return;
                }
                if app.confirm_disk.as_ref() != Some(&g.dir) {
                    if ui.add_enabled(!app.busy_with(&g), egui::Button::new("Delete repack")).clicked() {
                        app.confirm_disk = Some(g.dir.clone());
                    }
                    return;
                }
                ui.label(RichText::new(format!("Frees {}; reinstalling and checking its files won't be possible.", gb(s.map_or(0, |s| s[2])))).color(C::err()));
                if ui.button(RichText::new("Yes, delete the repack").color(C::err())).clicked() {
                    app.confirm_disk = None;
                    app.delete_repack_bg(g.clone(), None); // RepackGone re-measures this row
                }
                if ui.button("Keep it").clicked() {
                    app.confirm_disk = None;
                }
            });
        }
    });
}

/// Every option of one game (the gear on its page), its job's progress pinned on top.
fn settings(app: &mut App, ui: &mut Ui, i: usize) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        app.leave_settings(i);
        return;
    }
    ui.horizontal(|ui| {
        if ui.button("<  Back").clicked() {
            app.leave_settings(i);
        }
        ui.vertical(|ui| {
            ui.label(RichText::new(&app.lib.games[i].name).heading());
            let g = &app.lib.games[i];
            ui.label(RichText::new(if !g.has_repack() { "Imported (your own files)" } else if g.status == Status::Installed { "Installed" } else { "Not installed" }).color(C::sub()));
        });
    });
    let g = app.lib.games[i].clone();
    if app.busy_with(&g) {
        app.job_row(ui, 420.0, 26.0);
    }
    note_line(app, ui, Some(&g.dir));
    ui.add_space(4.0);
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        if app.sel != Some(i) {
            app.select(Some(i)); // per-game state of the settings view (exe list, pending updates, store id)
        }
        app.detail(ui, i);
    });
}

/// The message for this page (or for the library when `game` is None), if there is one.
fn note_line(app: &mut App, ui: &mut Ui, game: Option<&Path>) {
    let Some(Note { text, error, .. }) = app.note.clone().filter(|n| n.game.as_deref() == game) else { return };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(text).color(if error { C::err() } else { C::sub() })).wrap());
        if ui.small_button("Dismiss").clicked() {
            app.note = None;
        }
    });
}

/// "3 h 20 min", "12 min".
fn hours(secs: u64) -> String {
    match (secs / 3600, secs / 60 % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// "today", "yesterday", "5 days ago" for a Unix time.
fn ago(at: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    match now.saturating_sub(at) / 86400 {
        0 => "today".into(),
        1 => "yesterday".into(),
        d => format!("{d} days ago"),
    }
}

/// A gear: ring plus eight teeth.
fn gear(ui: &Ui, c: egui::Pos2, r: f32, color: Color32) {
    let p = ui.painter();
    p.circle_stroke(c, r * 0.55, Stroke::new(r * 0.32, color));
    for k in 0..8 {
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let d = vec2(a.cos(), a.sin());
        p.line_segment([c + d * r * 0.7, c + d * r * 1.05], Stroke::new(r * 0.36, color));
    }
}

fn toggle_fullscreen(ui: &Ui) {
    let full = ui.input(|i| i.viewport().fullscreen.unwrap_or(false));
    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
}

fn image(path: &std::path::Path) -> egui::Image<'static> {
    egui::Image::new(format!("file://{}", path.display()))
}

// ---------- library grid ----------

fn library(app: &mut App, ui: &mut Ui) {
    // the games shown, in order: tile positions map to games through this; the last tile is "Add game"
    let q = app.search.trim().to_lowercase();
    let games = &app.lib.games;
    let mut order: Vec<usize> = (0..games.len()).filter(|&i| q.is_empty() || games[i].name.to_lowercase().contains(&q)).collect();
    match app.settings.sort.as_str() {
        "name" => order.sort_by_cached_key(|&i| games[i].name.to_lowercase()),
        "played" => order.sort_by_key(|&i| std::cmp::Reverse(games[i].last_played.unwrap_or(0))),
        _ => {}
    }
    let n = order.len();
    if let Some(w) = app.want_game.take() {
        app.easy_focus = order.iter().position(|&x| x == w).unwrap_or(0);
    }
    app.easy_focus = app.easy_focus.min(n);
    note_line(app, ui, None);
    watch_banners(app, ui);
    import_dialog(app, ui);
    // nothing to scan for yet: a first-time user only needs Add game
    if !app.lib.games.is_empty()
        && ui.add_enabled(!app.scanning, egui::Button::new(if app.scanning { "Scanning…" } else { "Add many games…" })).on_hover_text("add every repack and unpacked game inside a folder").clicked()
        && let Some(dir) = rfd::FileDialog::new().set_title("A folder that holds repacks and games").pick_folder()
    {
        app.scanning = true;
        let (known, tx, ctx) = (crate::known_dirs(&app.lib), app.tx.clone(), ui.ctx().clone());
        std::thread::spawn(move || {
            let _ = tx.send(super::Ev::Scanned(crate::scan_known(&dir, &known)));
            ctx.request_repaint();
        });
    }
    if n == 0 && !app.lib.games.is_empty() {
        ui.label(RichText::new("No game matches the search.").color(C::sub()));
    } else if n == 0 {
        ui.label(RichText::new("No games yet. Press Add game and choose the game's folder.").color(C::sub()));
        ui.add_space(8.0);
    }
    let mut open = None;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        // tiles in explicit rows: the same column count drives the layout and Up/Down
        egui::Frame::new().inner_margin(egui::Margin::same(6)).show(ui, |ui| {
            let cols = (((ui.available_width() + GAP) / (TILE_W + GAP)).floor() as usize).max(1);
            let mv = |d: isize, f: usize| (f as isize + d).clamp(0, n as isize) as usize;
            let typing = ui.ctx().egui_wants_keyboard_input(); // arrows move the cursor in the search box, not the tiles
            ui.input(|i| {
                for (k, d) in [(Key::ArrowRight, 1), (Key::ArrowLeft, -1), (Key::ArrowDown, cols as isize), (Key::ArrowUp, -(cols as isize))] {
                    if i.key_pressed(k) && !typing {
                        app.easy_focus = mv(d, app.easy_focus);
                        app.scroll_focus = true;
                    }
                }
                // Enter opens the focused tile (the first match after a search), but no folder picker when nothing matched
                if i.key_pressed(Key::Enter) && (n > 0 || q.is_empty()) {
                    open = Some(app.easy_focus);
                }
            });
            let moving = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
            for row in 0..=n / cols {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = GAP;
                    for t in (row * cols..=n).take(cols) {
                        let (rect, r) = ui.allocate_exact_size(vec2(TILE_W, TILE_H + 34.0), Sense::click());
                        if r.hovered() && moving {
                            app.easy_focus = t; // the mouse moves the one focus, so only one tile is lit
                        }
                        let lit = t == app.easy_focus;
                        if lit && std::mem::take(&mut app.scroll_focus) {
                            r.scroll_to_me(None);
                        }
                        let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                        if t == n {
                            add_tile(ui, rect, lit);
                        } else {
                            let cover = app.art_of(order[t], "cover");
                            game_tile(app, ui, rect, order[t], cover.as_deref(), lit);
                            r.clone().on_hover_text(&app.lib.games[order[t]].name);
                        }
                        if r.clicked() {
                            app.easy_focus = t;
                            open = Some(t);
                        }
                    }
                });
                ui.add_space(GAP - ui.spacing().item_spacing.y);
            }
        });
    });
    match open {
        Some(t) if t == n => add_game(app, ui),
        Some(t) => {
            app.easy_open = Some(order[t]);
            app.side = None;
            app.confirm_delete = false;
            if app.note.as_ref().is_some_and(|n| n.game.is_none()) {
                app.note = None; // the library's message was read
            }
        }
        None => {}
    }
}

fn frame_rect(ui: &Ui, cover: Rect, lit: bool) {
    if lit {
        ui.painter().rect_stroke(cover.expand(2.0), CornerRadius::same(8), Stroke::new(3.0, C::accent()), StrokeKind::Outside);
    }
}

/// Text laid out centred within `width`, at most `rows` lines (the last ending in "…").
fn text_block(ui: &Ui, text: &str, size: f32, color: Color32, width: f32, rows: usize, center: bool) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text.to_string(), FontId::proportional(size), color, width);
    job.wrap.max_rows = rows;
    if center {
        job.halign = egui::Align::Center;
    }
    ui.painter().layout_job(job)
}

fn game_tile(app: &App, ui: &Ui, rect: Rect, i: usize, cover_art: Option<&Path>, lit: bool) {
    let g = &app.lib.games[i];
    let cover = Rect::from_min_size(rect.min, vec2(TILE_W, TILE_H));
    match cover_art {
        Some(p) => image(p).corner_radius(6).paint_at(ui, cover),
        None => {
            // no artwork: the name on a card, centred
            ui.painter().rect_filled(cover, CornerRadius::same(6), C::tile().lerp_to_gamma(C::bar(), 0.3));
            let galley = text_block(ui, &g.name, 20.0, C::text(), TILE_W - 24.0, 6, true);
            ui.painter().galley(pos2(cover.center().x, cover.center().y - galley.size().y / 2.0), galley, C::text());
        }
    }
    if !lit {
        ui.painter().rect_filled(cover, CornerRadius::same(6), Color32::from_black_alpha(40)); // unfocused tiles a touch darker
    }
    frame_rect(ui, cover, lit);
    // badge for anything but "ready"
    let busy = app.busy_with(g);
    let badge = if busy {
        Some(match app.progress {
            Some((d, t)) => format!("{} {}%", app.job_label, d * 100 / t.max(1)),
            None => format!("{}…", app.job_label),
        })
    } else {
        match g.status {
            Status::New => Some("Not installed".into()),
            Status::Failed => Some("Install failed".into()),
            _ if matches!(app.found.get(&g.dir), Some(Ok(u)) if !u.is_empty()) => Some("Update available".into()),
            _ => None,
        }
    };
    if let Some(b) = badge {
        let r = Rect::from_min_size(cover.left_bottom() - vec2(0.0, 30.0), vec2(TILE_W, 30.0));
        ui.painter().rect_filled(r, CornerRadius { nw: 0, ne: 0, sw: 6, se: 6 }, Color32::from_black_alpha(190));
        let c = if g.status == Status::Failed && !busy { Color32::from_rgb(0xfb, 0x49, 0x34) } else { OVERLAY_TEXT }; // light red: on the dark shade in every preset
        ui.painter().text(r.center(), Align2::CENTER_CENTER, b, FontId::proportional(14.0), c);
    }
    let name = text_block(ui, &g.name, 15.0, if lit { C::text() } else { C::sub() }, TILE_W, 1, false);
    ui.painter().galley(pos2(rect.left(), cover.bottom() + 8.0), name, C::text());
}

/// One banner per finished download in the watch folder.
fn watch_banners(app: &mut App, ui: &mut Ui) {
    let shown: Vec<_> = app.watching.iter().filter(|d| !app.lib.games.iter().any(|g| g.repack_dir == **d)).cloned().collect();
    for d in shown {
        let name = d.file_name().unwrap_or_default().to_string_lossy().into_owned();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("New repack found: {name}")).color(C::accent()));
            let install = ui.add_enabled(!app.no_umu, egui::Button::new("Add & install")).on_disabled_hover_text("umu-launcher is missing").clicked();
            if install || ui.small_button("Add").clicked() {
                app.watching.retain(|w| *w != d);
                if let Some(i) = app.add_repack(d.clone(), false)
                    && install
                {
                    let g = app.lib.games[i].clone();
                    app.spawn(ui.ctx(), g, Job::Install { verify: app.settings.verify, unattended: app.settings.unattended });
                }
            }
            if ui.small_button("Dismiss").clicked() {
                app.watching.retain(|w| *w != d);
                app.settings.watch_dismissed.push(d.clone());
                app.save_settings();
                app.sync_watch();
            }
        });
    }
}

/// The checklist of what "Add many games" found. Nothing is installed: repacks are added, games imported.
fn import_dialog(app: &mut App, ui: &Ui) {
    let Some(mut items) = app.scan.take() else { return };
    let (mut add, mut cancel) = (false, false);
    let n = items.iter().filter(|(_, on)| *on).count();
    egui::Window::new("Add many games").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0)).show(ui.ctx(), |ui| {
        for (f, on) in &mut items {
            let name = f.dir.file_name().unwrap_or_default().to_string_lossy();
            ui.checkbox(on, format!("{}: {name}", if f.repack { "Repack" } else { "Installed game" }));
        }
        ui.horizontal(|ui| {
            add = ui.add_enabled(n > 0, egui::Button::new(format!("Add {n}"))).clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    if add {
        let added = items.iter().filter(|(f, on)| *on && app.add_repack(f.dir.clone(), !f.repack).is_some()).count();
        app.notify(None, format!("Added {added} game(s). Press FINISH SETUP once on each imported game, then PLAY; nothing was installed."), false);
        if let Some(i) = app.lib.games.len().checked_sub(1).filter(|_| added > 0) {
            app.want_game = Some(i);
        }
    } else if !cancel {
        app.scan = Some(items);
    }
}

fn add_tile(ui: &Ui, rect: Rect, lit: bool) {
    let cover = Rect::from_min_size(rect.min, vec2(TILE_W, TILE_H));
    ui.painter().rect_filled(cover, CornerRadius::same(6), if lit { C::tile() } else { C::tile().lerp_to_gamma(C::bg(), 0.5) });
    frame_rect(ui, cover, lit);
    ui.painter().text(cover.center() - vec2(0.0, 18.0), Align2::CENTER_CENTER, "+", FontId::proportional(64.0), C::text());
    ui.painter().text(cover.center() + vec2(0.0, 36.0), Align2::CENTER_CENTER, "Add game", FontId::proportional(18.0), C::text());
}

/// One folder picker for both cases: a repack (has setup.exe) is added for installing, anything else is
/// imported as an already-unpacked game and set up right away.
fn add_game(app: &mut App, ui: &Ui) {
    let Some(dir) = rfd::FileDialog::new().set_title("Choose the game's folder (a repack or an installed game)").pick_folder() else { return };
    let repack = find_setup(&dir).is_ok();
    if let Some(i) = app.add_repack(dir, !repack) {
        app.easy_open = Some(i);
        app.want_game = Some(i);
        let g = app.lib.games[i].clone();
        if !repack && g.kind == Kind::Windows {
            if app.busy || app.no_umu {
                app.notify(Some(g.dir.clone()), "Added. Press FINISH SETUP once, then PLAY.", false);
            } else {
                app.spawn(ui.ctx(), g, Job::Import);
            }
        }
    }
}

// ---------- game page ----------

/// Paint an image inside `rect` keeping its proportions, aligned left (logos) or centred.
fn paint_fit(ui: &Ui, img: egui::Image<'_>, rect: Rect, left: bool) {
    let size = match img.load_for_size(ui.ctx(), rect.size()) {
        Ok(egui::load::TexturePoll::Ready { texture }) => texture.size,
        _ => return, // still loading: next frame
    };
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    let s = size * scale;
    let min = if left { pos2(rect.left(), rect.center().y - s.y / 2.0) } else { rect.center() - s / 2.0 };
    img.paint_at(ui, Rect::from_min_size(min, s));
}

/// The one big action for this game right now: what it says, its colour, and whether it can be pressed.
enum Primary {
    Play,
    Install { space: bool },
    SetUpPrefix,
    SetUpEmulator,
}

fn page(app: &mut App, ui: &mut Ui, i: usize) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        close_page(app);
        return;
    }
    let g = app.lib.games[i].clone();
    // hero banner: Steam/SteamGridDB heroes are about 3.1:1, so the box keeps that shape at every width
    let w = ui.available_width().min(1400.0);
    let hero_art = app.art_of(i, "hero");
    let h = if hero_art.is_some() { w / 3.1 } else { 150.0 }; // no art: a short band for the name, not an empty box
    let (hero, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    match hero_art {
        Some(p) => image(&p).corner_radius(8).paint_at(ui, hero),
        None => _ = ui.painter().rect_filled(hero, CornerRadius::same(8), C::tile().lerp_to_gamma(C::bar(), 0.5)),
    }
    let band = (hero.height() * 0.38).min(120.0);
    let shade = Rect::from_min_max(pos2(hero.left(), hero.bottom() - band), hero.max);
    ui.painter().rect_filled(shade, CornerRadius { nw: 0, ne: 0, sw: 8, se: 8 }, Color32::from_black_alpha(120));
    let inner = shade.shrink2(vec2(28.0, band * 0.12));
    match app.art_of(i, "logo") {
        Some(p) => paint_fit(ui, image(&p), Rect::from_min_size(inner.min, vec2(inner.width().min(360.0), inner.height())), true),
        None => {
            let galley = text_block(ui, &g.name, (band * 0.32).clamp(18.0, 38.0), OVERLAY_TEXT, inner.width(), 2, false);
            ui.painter().galley(pos2(inner.left(), inner.bottom() - galley.size().y), galley, OVERLAY_TEXT);
        }
    }
    ui.add_space(24.0);

    let busy_here = app.busy_with(&g);
    let installed = g.status == Status::Installed;
    // Enter plays only when no other control has the keyboard (pad A on a focused button presses that button)
    let enter = ui.input(|i| i.key_pressed(Key::Enter)) && ui.memory(|m| m.focused().is_none());
    // native Linux emulators run without umu; Windows games and the Windows emulators FitGirl bundles need it
    let needs_umu = !installed || g.kind == Kind::Windows || g.emulator.is_some(); // installing always runs setup.exe
    let queued = app.queued(&g);
    let starting = app.launching.as_ref().is_some_and(|(d, t)| *d == g.dir && t.elapsed() < Duration::from_secs(8));
    if starting {
        ui.ctx().request_repaint_after(Duration::from_millis(500)); // to drop "Starting…" on time
    }
    let side_free = if installed { None } else { crate::free_space(&g.dir) }; // not cached: space may be freed meanwhile
    let (has_prefix, has_emulator) = (app.side(&g).has_prefix, app.side(&g).has_emulator);
    let no_emu = g.kind != Kind::Windows && !has_emulator;
    let primary = if installed {
        if no_emu {
            Primary::SetUpEmulator
        } else if g.game_dir.is_some() && g.kind == Kind::Windows && !has_prefix {
            Primary::SetUpPrefix
        } else {
            Primary::Play
        }
    } else {
        Primary::Install { space: g.size.zip(side_free).is_none_or(|(need, free)| free >= need) }
    };
    if busy_here {
        app.job_row(ui, 460.0, 40.0);
    } else {
        ui.horizontal(|ui| {
            let (label, color) = match primary {
                Primary::Play if starting => ("STARTING…", C::play()),
                Primary::Play => ("PLAY", C::play()),
                Primary::Install { .. } | Primary::SetUpPrefix if queued => ("QUEUED", C::install()),
                Primary::Install { .. } | Primary::SetUpPrefix if app.busy => ("QUEUE", C::install()),
                Primary::Install { .. } if g.status == Status::Failed => ("INSTALL AGAIN", C::install()),
                Primary::Install { .. } => ("INSTALL", C::install()),
                Primary::SetUpPrefix => ("FINISH SETUP", C::install()),
                Primary::SetUpEmulator => ("SET UP EMULATOR", C::install()),
            };
            let why = match primary {
                _ if app.no_umu && needs_umu && !matches!(primary, Primary::SetUpEmulator) => Some(format!("umu-launcher is not installed: {}", crate::install_hint("umu-run"))),
                Primary::Install { .. } | Primary::SetUpPrefix if queued => Some("already queued: it starts when the running job is done".into()),
                Primary::Install { space: false } => Some("not enough disk space".into()),
                Primary::Install { .. } if !g.repack_dir.is_dir() => Some("no repack to install from (its folder is gone)".into()),
                Primary::Play if starting => Some("starting…".into()),
                _ => None,
            };
            let b = egui::Button::new(RichText::new(label).size(26.0).strong().color(C::on_button())).fill(color).min_size(vec2(260.0, 64.0)).corner_radius(6);
            let r = ui.add_enabled(why.is_none(), b);
            let r = match &why {
                Some(w) => r.on_disabled_hover_text(w),
                None => r,
            };
            if (r.clicked() || (enter && why.is_none())) && !app.confirm_delete {
                match primary {
                    Primary::Play => app.launch(i),
                    Primary::Install { .. } => app.spawn(ui.ctx(), g.clone(), Job::Install { verify: app.settings.verify, unattended: app.settings.unattended }),
                    Primary::SetUpPrefix => app.spawn(ui.ctx(), g.clone(), Job::Import),
                    Primary::SetUpEmulator => (app.global, app.scroll_to_emulators) = (true, true), // the emulator is set once for all games
                }
            }
            ui.add_space(12.0);
            ui.vertical(|ui| {
                match primary {
                    Primary::Install { space } => {
                        if let Some(need) = g.size {
                            let free = side_free.map(gb).unwrap_or("?".into());
                            let text = match side_free.filter(|f| !space && *f < need) {
                                Some(f) => format!("Not enough space: needs {}, {} free. Free {} or change the games folder in Settings.", gb(need), gb(f), gb(need - f)),
                                None => format!("Needs {}, {} free", gb(need), free),
                            };
                            ui.label(RichText::new(text).color(if space { C::sub() } else { C::err() }));
                        }
                    }
                    Primary::SetUpEmulator => _ = ui.label(RichText::new("This game needs an emulator. Pick one in Settings, Emulators.").color(C::sub())),
                    Primary::SetUpPrefix => _ = ui.label(RichText::new("One-time step before the first PLAY.").color(C::sub())),
                    Primary::Play if g.steam.is_some() => _ = ui.label(RichText::new("Starts through Steam").color(C::sub())),
                    Primary::Play => {}
                }
                if !g.version.is_empty() {
                    ui.label(RichText::new(format!("Version {}", g.version)).color(C::sub()));
                }
                // keys and firmware the emulator needs: checked here, never fetched
                if installed && let Some((msg, dir)) = app.side(&g).emulator.clone() {
                    ui.label(RichText::new(msg).color(C::accent()));
                    if ui.small_button("Open that folder").clicked() {
                        let _ = std::fs::create_dir_all(&dir);
                        std::thread::spawn(move || _ = crate::open_folder(&dir));
                    }
                }
                if let Some(at) = g.last_played {
                    ui.label(RichText::new(format!("Played {} · last played {}", hours(g.playtime), ago(at))).color(C::sub()));
                }
                // updates lying next to the game: shown, never applied without the user
                match app.found.get(&g.dir).cloned() {
                    Some(Ok(ups)) if !ups.is_empty() => {
                        ui.horizontal(|ui| {
                            let to = &ups[ups.len() - 1].to;
                            ui.label(RichText::new(format!("Update available: v{to}")).color(C::accent()).strong())
                                .on_hover_text(ups.iter().map(|u| u.dir.display().to_string()).collect::<Vec<_>>().join("\n"));
                            if ui.small_button("Update").on_hover_text("See what changes and apply it").clicked() {
                                app.select(Some(i));
                                app.pending = Ok(ups.clone());
                                app.easy_gear = true;
                            }
                        });
                    }
                    Some(Err(e)) => _ = ui.label(RichText::new(format!("Updates found, but: {e}")).color(C::err())),
                    _ => {}
                }
                // how well it runs on Linux, by Steam id (filled in from SteamGridDB, or typed in its settings)
                if g.kind == Kind::Windows
                    && let Some(id) = g.store_id
                {
                    match app.protondb_for(id) {
                        Some(Ok((tier, total))) => {
                            let c = match tier.as_str() {
                                "platinum" => C::text(),
                                "gold" => C::accent(),
                                "silver" => C::sub(),
                                "bronze" => Color32::from_rgb(0xd6, 0x5d, 0x0e), // gruvbox orange
                                _ => C::err(),
                            };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("ProtonDB:").color(C::sub()));
                                ui.label(RichText::new(tier.to_uppercase()).color(c).strong()).on_hover_text(format!("{total} reports on protondb.com"));
                            });
                        }
                        Some(Err(_)) => _ = ui.label(RichText::new("ProtonDB: no rating").color(C::sub())),
                        None => _ = ui.label(RichText::new("ProtonDB: checking…").color(C::sub())),
                    }
                }
            });
        });
    }
    if app.note.as_ref().is_some_and(|n| n.game.as_ref() == Some(&g.dir)) {
        note_line(app, ui, Some(&g.dir));
        // the one-click fix for why the game stopped right away
        if let Some((_, fix)) = app.fix.clone().filter(|(d, _)| *d == g.dir) {
            let label = match &fix {
                crate::Fix::Winetricks(_) => "Fix it (installs a Windows component)".to_string(),
                crate::Fix::Proton(p) => format!("Use {}", Path::new(p).file_name().map_or(p.clone(), |n| n.to_string_lossy().into_owned())),
            };
            let winetricks = matches!(fix, crate::Fix::Winetricks(_));
            if ui.add_enabled(!app.busy_with(&g) && !(winetricks && app.no_winetricks), egui::Button::new(label))
                .on_hover_text(match &fix {
                    crate::Fix::Winetricks(v) => format!("installs {v} with winetricks"),
                    crate::Fix::Proton(_) => "switches this game to that compatibility version".to_string(),
                })
                .on_disabled_hover_text(format!("winetricks is not installed: {}", crate::install_hint("winetricks")))
                .clicked()
            {
                app.fix = None;
                match fix {
                    crate::Fix::Winetricks(v) => app.spawn(ui.ctx(), g.clone(), Job::Winetricks(vec![v.to_string()])),
                    crate::Fix::Proton(p) => {
                        app.lib.games[i].proton = p;
                        app.save();
                        app.write_script(i);
                        app.notify(Some(g.dir.clone()), "Changed. Press PLAY to try again.", false);
                    }
                }
            }
        }
    } else if g.status == Status::Failed && !busy_here {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("The install failed. Press INSTALL AGAIN to retry.").color(C::err()));
            if ui.small_button("Show log").clicked() {
                (app.easy_gear, app.scroll_to_log) = (true, true);
            }
        });
    }
    ui.add_space(28.0);

    // the few extras, small and out of the way; the gear opens every option
    ui.horizontal_wrapped(|ui| {
        let r = ui.button("      Game settings").on_hover_text("Proton, emulator, launch options, updates, mods, saves, winetricks, log");
        gear(ui, pos2(r.rect.left() + 22.0, r.rect.center().y), 8.0, C::text()); // drawn: the default font has no ⚙
        if r.clicked() {
            app.easy_gear = true;
            app.confirm_delete = false;
        }
        if installed && !busy_here && !app.steam_users.is_empty() {
            if g.steam.is_none() {
                if ui.button("Add to Steam").on_hover_text("Steam then starts the game: playtime, overlay, controller settings").clicked() && (g.exe.is_some() || app.pick_exe(i)) {
                    let vdf = app.steam_users[app.steam_pick].clone();
                    app.steam_edit(i, Some(vdf));
                }
            } else if ui.button("Remove from Steam").clicked() {
                app.steam_edit(i, None);
            }
        }
        if !busy_here {
            app.delete_confirm(ui, i);
        }
    });
}
