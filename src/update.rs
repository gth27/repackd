//! Update ladder: FitGirl update folders ("Update v1.2 to v1.3", "... Build 123") and scene update archives
//! ("Game.Update.v1.5.2-RUNE.part1.rar": a patcher in `Update/`, a crack beside it) applied in version order.
use crate::{check, find_md5, find_setup, gb, install_redists, need_space, run, run_inno, which_path, Game, Report};
use serde::{Deserialize, Serialize};
use anyhow::{anyhow, bail, Context, Result};
use std::{cmp::Ordering, path::{Path, PathBuf}, process::{Command, Stdio}, time::SystemTime};

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub dir: PathBuf,
    pub from: Option<String>,
    pub to: String,
}

/// Version tokens in a name: after a standalone `v` or after `build` ("Cyberpunk 2077 v2.12", "Build 12345").
/// Bare numbers ("Cyberpunk 2077") are not versions.
pub fn versions(name: &str) -> Vec<String> {
    tokens(name, false)
}

/// `versions`, plus the forms update releases use without a `v` (seen on FitGirl's update digests):
/// "Update.4-RUNE", "Update_1774570-ElAmigos", "Update_21.08.2026", "from_01.05.2026_to_14.05.2026",
/// "to_93241.H2". Only for update names: in a game's own name "Update 3" or "Road to 1999" is no version.
fn tokens(name: &str, update_words: bool) -> Vec<String> {
    let lower = name.to_ascii_lowercase();
    let b = lower.as_bytes();
    let starts_word = |i: usize| i == 0 || !b[i - 1].is_ascii_alphanumeric();
    let words: &[&str] = if update_words { &["build", "update", "from", "to"] } else { &["build"] };
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let start = if b[i] == b'v' && starts_word(i) && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            Some(i + 1)
        } else if starts_word(i)
            && let Some(w) = words.iter().find(|w| b[i..].starts_with(w.as_bytes()))
        {
            let j = i + w.len() + b[i + w.len()..].iter().take_while(|c| matches!(c, b' ' | b'_' | b'-' | b'.')).count();
            b.get(j).is_some_and(u8::is_ascii_digit).then_some(j)
        } else {
            None
        };
        match start {
            Some(s) => {
                let e = token_end(b, s);
                out.push(name[s..e].trim_end_matches('.').to_string()); // as written: "93241.H1", "2026.1.Patch.20"
                i = e;
            }
            None => i += 1,
        }
    }
    out
}

/// Where a version starting at `s` ends: digits and dots, short letter tags glued on ("1.2.1d", "1.2.4e3",
/// "93241.H1", "1.00.a"; not ".incl" or "-RUNE"), and a patch number ("2026.1.Patch.20", "4.Hotfix.2").
fn token_end(b: &[u8], s: usize) -> usize {
    let sep = |c: &u8| matches!(c, b' ' | b'_' | b'-' | b'.');
    let mut e = s;
    loop {
        e += b[e..].iter().take_while(|c| c.is_ascii_digit() || **c == b'.').count();
        // a tag ends the version or carries one digit run ("e3", "H1"); "md5", ".blake3", ".incl" are no tags
        let letters = b[e..].iter().take_while(|c| c.is_ascii_alphabetic()).count();
        let after = b.get(e + letters);
        if (1..=2).contains(&letters) && (after.is_none_or(|c| !c.is_ascii_alphanumeric()) || (letters == 1 && after.is_some_and(u8::is_ascii_digit))) {
            e += letters;
            continue;
        }
        let j = e + b[e..].iter().take_while(|c| sep(c)).count();
        if let Some(w) = ["patch", "hotfix"].iter().find(|w| b[j..].starts_with(w.as_bytes())) {
            let k = j + w.len() + b[j + w.len()..].iter().take_while(|c| sep(c)).count();
            if b.get(k).is_some_and(u8::is_ascii_digit) {
                e = k;
                continue;
            }
        }
        return e;
    }
}

/// Numeric compare: "1.10" > "1.9", "1.2" == "1.2.0", "93241.H2" > "93241.H1", and dd.mm.yyyy dates by
/// year, month, day ("07.10.2026" > "29.07.2026").
pub fn vcmp(a: &str, b: &str) -> Ordering {
    let p = |s: &str| {
        let s = s.trim_start_matches(['v', 'V']); // a version typed in as "v1.2"
        let digits: Vec<u64> = s.split(|c: char| !c.is_ascii_digit()).filter(|x| !x.is_empty()).map(|x| x.parse::<u64>().unwrap_or(u64::MAX)).collect();
        if let [d, m, y] = digits[..]
            && (1..=31).contains(&d)
            && (1..=12).contains(&m)
            && y >= 1990
            && s.matches('.').count() == 2
        {
            return vec![y, m, d];
        }
        // digit runs, and short letter tags by their place in the alphabet ("1.30b" > "1.30a" > "1.30");
        // words like "Patch" or "Hotfix" only separate
        let mut v = vec![];
        let mut rest = s.to_ascii_lowercase();
        while !rest.is_empty() {
            let digit = rest.starts_with(|c: char| c.is_ascii_digit());
            let alpha = rest.starts_with(|c: char| c.is_ascii_alphabetic());
            let n = rest.find(|c: char| if digit { !c.is_ascii_digit() } else if alpha { !c.is_ascii_alphabetic() } else { c.is_ascii_alphanumeric() }).unwrap_or(rest.len());
            let run = &rest[..n];
            if digit {
                v.push(run.parse::<u64>().unwrap_or(u64::MAX));
            } else if alpha && run.len() <= 2 {
                v.push(run.bytes().fold(0, |acc, c| acc * 27 + u64::from(c - b'a' + 1)));
            }
            rest = rest[n..].to_string();
        }
        v
    };
    let (mut x, mut y) = (p(a), p(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x.cmp(&y)
}

/// The part number of a multi-part RAR ("x.part2.rar" -> 2); None for any other name ("Mario.Party.v1.rar").
fn rar_part(name: &str) -> Option<u32> {
    let lower = name.to_ascii_lowercase();
    let (_, n) = lower.strip_suffix(".rar")?.rsplit_once(".part")?;
    n.parse().ok()
}

/// Update archives repackd unpacks itself (multi-part RAR from its `.part1.rar`).
const ARCHIVES: [&str; 3] = ["rar", "7z", "zip"];

pub fn parse(dir: &Path) -> Result<Update> {
    let name = dir.file_name().unwrap_or_default().to_string_lossy();
    if dir.is_file() {
        let lower = name.to_ascii_lowercase();
        if !ARCHIVES.iter().any(|x| lower.ends_with(&format!(".{x}"))) {
            bail!("\"{name}\" is not an update folder or archive (.rar, .7z, .zip)");
        }
        if rar_part(&lower).is_some_and(|n| n != 1) {
            bail!("\"{name}\" is a later part: pick the first one (.part1.rar)");
        }
    }
    // the archive's extension and part number are no version ("v1.2.7z", ".part1")
    let lower = name.to_ascii_lowercase();
    let mut stem = &name[..];
    for x in ARCHIVES {
        if lower.ends_with(&format!(".{x}")) {
            stem = &name[..name.len() - x.len() - 1];
        }
    }
    if rar_part(&lower).is_some() {
        stem = &stem[..stem.to_ascii_lowercase().rfind(".part").unwrap_or(stem.len())];
    }
    let v = tokens(stem, true);
    let lower = stem.to_ascii_lowercase();
    match v.as_slice() {
        [] => bail!("no version in \"{name}\": rename the folder like \"Update v1.2 to v1.3\""),
        [to] => Ok(Update { dir: dir.into(), from: None, to: to.clone() }),
        [from, .., to] if lower.contains(" to ") || lower.contains("-to-") || lower.contains("_to_") => {
            Ok(Update { dir: dir.into(), from: Some(from.clone()), to: to.clone() })
        }
        [.., to] => Ok(Update { dir: dir.into(), from: None, to: to.clone() }),
    }
}

/// Order updates newer than `current` into a chain; a `from` that does not match where the chain is
/// would skip or repeat game files, so that is an error naming the missing step.
pub fn ladder(current: &str, mut ups: Vec<Update>) -> Result<Vec<Update>> {
    ups.sort_by(|a, b| vcmp(&a.to, &b.to));
    ups.dedup_by(|a, b| vcmp(&a.to, &b.to) == Ordering::Equal);
    let mut cur = current.to_string();
    let mut out = vec![];
    for u in ups {
        if !cur.is_empty() && vcmp(&u.to, &cur) != Ordering::Greater {
            continue; // already at or past this one
        }
        if let Some(f) = &u.from
            && !cur.is_empty()
            && vcmp(f, &cur) != Ordering::Equal
        {
            bail!("missing update: the game would be at v{cur}, but {} starts from v{f}", u.dir.file_name().unwrap_or_default().to_string_lossy());
        }
        cur = u.to.clone();
        out.push(u);
    }
    Ok(out)
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

/// One name is the other plus more words ("Dave the Diver" and "DAVE.THE.DIVER.In.the.Jungle.Content.Pack"),
/// unless the next word makes it a sequel: a number or a roman numeral ("Hades" is not "Hades II").
fn same_game(a: &[String], b: &[String]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let sequel = |w: &String| w.chars().all(|c| c.is_ascii_digit()) || ["ii", "iii", "iv", "v", "vi", "vii", "viii", "ix", "x"].contains(&w.as_str());
    // a one-word name must be the whole other name: "DOOM" is not "DOOM The Dark Ages"
    short.iter().map(String::len).sum::<usize>() >= 4 && (short.len() >= 2 || short.len() == long.len()) && long.starts_with(short) && long.get(short.len()).is_none_or(|w| !sequel(w))
}

/// Updates for this game lying where its files came from: inside its repack folder, next to the repack or
/// the imported game folder, and in ~/Downloads. An update belongs to the game when its name before
/// "Update" matches the game's ("DAVE.THE.DIVER.In.the.Jungle…Update.v1.0.6.2087" for "Dave The Diver
/// [FitGirl Repack]"). Returns the chain newer than the game's version, or why it does not chain.
pub fn find_for(g: &Game) -> Result<Vec<Update>, String> {
    let game = words(&crate::steam::art_term(&g.name));
    let mut dirs: Vec<PathBuf> = vec![];
    if g.has_repack() {
        dirs.push(g.repack_dir.clone());
        dirs.extend(g.repack_dir.parent().map(Path::to_path_buf));
    }
    dirs.extend(g.game_dir.as_deref().and_then(Path::parent).map(Path::to_path_buf));
    dirs.push(crate::home().join("Downloads"));
    dirs.dedup();
    let mut found = vec![];
    for d in &dirs {
        let inside_repack = *d == g.repack_dir;
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            // where the game's own name ends: the first "update"/"patch"/"hotfix" word
            // a word of its own: "Dispatch.Update" ends the game's name at "Update", not inside "Dispatch"
            let word_at = |w: &str| name.match_indices(w).map(|(i, _)| i).find(|&i| i == 0 || !name.as_bytes()[i - 1].is_ascii_alphanumeric());
            let Some(at) = ["update", "patch", "hotfix"].iter().filter_map(|w| word_at(w)).min() else { continue };
            let title = words(&name[..at]);
            let same = same_game(&game, &title);
            // FitGirl's own update folders inside the repack are named "Update v1.2 to v1.3", without the game
            if (same || (inside_repack && title.is_empty())) && Some(&p) != g.game_dir.as_ref() && p != g.repack_dir
                && let Ok(u) = parse(&p)
            {
                found.push(u);
            }
        }
    }
    let chain = ladder(&g.version, found).map_err(|e| format!("{e:#}"))?;
    if g.version.is_empty() && !chain.is_empty() {
        // with no version every found update looks newer, older ones too: applying could go backwards
        return Err("update files lie next to the game, but its version is unknown: set it in its settings first".into());
    }
    Ok(chain)
}

fn copy_tree(from: &Path, to: &Path, n: &mut usize, log: &impl Report) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let p = e.path();
        let name = e.file_name();
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            copy_tree(&p, &to.join(&name), n, log)?;
        } else if e.file_type().is_ok_and(|t| t.is_symlink()) {
            log.log(format!("skipped link {}", p.display())); // a link could point at any file on the PC
        } else if p.is_file() {
            std::fs::copy(&p, to.join(&name)).with_context(|| format!("copy {}", p.display()))?;
            *n += 1;
        }
    }
    Ok(())
}

/// Install one update into the game. An archive is unpacked next to the game first (and removed after).
/// Every update's installer shows its window and waits for the user's click: never silent. With `backup`,
/// the files it changes are copied first (`undo`).
pub fn apply(g: &Game, u: &Update, verify_first: bool, backup: bool, log: &impl Report) -> Result<()> {
    crate::ensure_stopped(g).map_err(|e| anyhow!("{e} An update over a running game can leave half-written files."))?;
    let backup = backup.then_some(u.to.as_str());
    // no size-based progress: the game folder is already full size, so the bar would sit at 100%
    let g = &Game { size: None, ..g.clone() };
    if !u.dir.is_file() {
        return apply_dir(g, &u.dir, false, verify_first, backup, log);
    }
    let tmp = g.dir.join("update-tmp");
    let r = (|| {
        extract(&u.dir, &tmp, &g.prefix(), log)?;
        apply_dir(g, &content_root(&tmp), true, verify_first, backup, log)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

fn extract(archive: &Path, to: &Path, prefix: &Path, log: &impl Report) -> Result<()> {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to)?;
    // a multi-part RAR is all files named like it up to ".partN"; anything else is the one file
    let name = archive.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
    let stem = name.rsplit_once(".part").map(|(s, _)| format!("{s}.part")).filter(|_| rar_part(&name).is_some());
    let packed: u64 = std::fs::read_dir(archive.parent().unwrap_or(Path::new("."))).into_iter().flatten().flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_ascii_lowercase();
            match &stem {
                Some(s) => n.starts_with(s.as_str()) && rar_part(&n).is_some(),
                None => n == name,
            }
        })
        .filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    need_space(to, packed, 1 << 30).map_err(|e| anyhow!("{e} (to unpack the update)"))?;
    log.step("Extracting the update archive");
    // 7-Zip reads RAR5 and multi-part archives and is in Arch's extra; unrar as a fallback
    let mut c = if let Some(z) = which_path("7z") {
        let mut c = Command::new(z);
        c.args(["x", "-y", "-bd"]).arg(format!("-o{}", to.display())).arg(archive);
        c
    } else if let Some(u) = which_path("unrar") {
        let mut c = Command::new(u);
        c.args(["x", "-o+", "-idq"]).arg(archive).arg(format!("{}/", to.display()));
        c
    } else {
        bail!("unpacking update archives needs 7-Zip: {}", crate::install_hint("7z"));
    };
    c.stdin(Stdio::null()); // a password prompt must fail, not wait
    // Cancel and closing repackd stop a job's processes by their WINEPREFIX: tag 7-Zip so they stop it too
    c.env("WINEPREFIX", prefix);
    let st = run(c, None, log)?;
    if !st.success() {
        bail!("could not unpack {} ({st})", archive.display());
    }
    Ok(())
}

/// A scene release folder: it holds the installer's `Update` folder, a `Crack`, or an installer itself.
fn release_inside(d: &Path) -> bool {
    find_setup(d).is_ok() || patch_exe(d).is_some() || ["update", "crack"].iter().any(|n| std::fs::read_dir(d).into_iter().flatten().flatten().any(|e| e.path().is_dir() && e.file_name().eq_ignore_ascii_case(n)))
}

/// Scene archives hold one folder named after the release ("Game.Update.v1.2-GROUP"): the update is inside it.
/// A lone game folder ("Data") is part of the update itself and stays.
fn content_root(d: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).collect();
    match entries.as_slice() {
        [one] if one.is_dir() && (!tokens(&one.file_name().unwrap_or_default().to_string_lossy(), true).is_empty() || release_inside(one)) => one.clone(),
        _ => d.to_path_buf(),
    }
}

/// One unpacked update:
/// - FitGirl: `setup.exe` (Inno, same dir and prefix as the game), or plain files copied over the install dir;
/// - scene: the installer in `Update/` (Inno `setup.exe`, or a `PATCH.exe` with a window), then every other
///   folder (`Crack`, or the group's name like `RUNE`) copied over the install dir, as their nfo says.
fn apply_dir(g: &Game, dir: &Path, archive: bool, verify_first: bool, backup: Option<&str>, log: &impl Report) -> Result<()> {
    let md5 = find_md5(dir);
    if verify_first && let Some(f) = &md5 {
        check(f, log).map_err(|e| anyhow!("{e}. Get the update files again."))?;
    }
    let sub = |name: &str| std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).find(|p| p.is_dir() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)));
    // a scene archive with its installer at the top (no Update/ folder); FitGirl's always ship a checksum list
    // (an unpacked TENOKE folder with PATCH.exe counts too: copying its patcher into the game would do nothing)
    let top_installer = md5.is_none() && (patch_exe(dir).is_some() || (archive && find_setup(dir).is_ok()));
    let scene = sub("update").or_else(|| top_installer.then(|| dir.to_path_buf()));
    if let Some(to) = backup {
        // what will change: the installer's own file list, the files to copy, or (a patcher that names
        // none) the whole game; plus the crack's files
        let (mut rels, mut whole) = (vec![], false);
        match find_setup(scene.as_deref().unwrap_or(dir)) {
            Ok(setup) => {
                rels = inno_targets(&setup);
                whole = rels.is_empty();
            }
            Err(_) if scene.is_some() => whole = true,
            Err(_) => rels = rel_files(dir, true),
        }
        if let Some(sc) = &scene {
            rels.extend(crack_dirs(dir, sc).iter().flat_map(|c| rel_files(c, false)));
        }
        let root = snapshot(g, to, rels, whole, log)?;
        let start = SystemTime::now();
        let r = run_update(g, dir, scene, log);
        if let Some(root) = root {
            if r.is_err() && !changed_since(&g.install_dir(), start) {
                // the update failed before touching the game: its undo copy (maybe the whole game) is dead weight
                let _ = std::fs::remove_dir_all(&root);
                if let Some(p) = root.parent() {
                    let _ = std::fs::remove_dir(p); // the backups folder, if that was its only one
                }
            } else {
                keep_only_created(&root, &g.install_dir());
            }
        }
        return r;
    }
    run_update(g, dir, scene, log)
}

/// Run the update's installer (or copy its files), then its crack.
fn run_update(g: &Game, dir: &Path, scene: Option<PathBuf>, log: &impl Report) -> Result<()> {
    crate::cancel_point(log)?;
    let Some(scene) = scene else {
        if let Ok(setup) = find_setup(dir) {
            run_inno(g, &setup, false, log)?;
            return install_redists(g, log); // blocked during its setup like the base game's
        }
        return copy_files(g, dir, log);
    };
    if let Ok(setup) = find_setup(&scene) {
        // the Inno log still says whether it finished (RUNE's own page aborts when run silently anyway)
        log.log(format!("In the update's window check the folder is {} (drive G:) and press Install; repackd copies the crack afterwards.", g.install_dir_win()));
        run_inno(g, &setup, false, log)?;
    } else if let Some(patch) = patch_exe(&scene) {
        match hdiff_patch(&scene).map(|bin| crate::hpatchz(log).map(|hz| (hz, bin))) {
            Some(Ok((hz, bin))) => native_patch(g, &hz, &bin, log)?,
            other => {
                if let Some(Err(e)) = other {
                    log.log(format!("native HDiffPatch unavailable ({e}); using the patcher's window"));
                }
                gui_patch(g, &patch, log)?;
            }
        }
    } else {
        bail!("{} has neither setup.exe nor PATCH.exe", scene.display());
    }
    copy_cracks(g, dir, &scene, log)
}

fn patch_exe(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("patch.exe")))
}

/// `Patch.bin` beside PATCH.exe when it is an HDiffPatch diff (starts with `HDIFF`).
fn hdiff_patch(dir: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let bin = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find(|p| p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("patch.bin")))?;
    let mut magic = [0u8; 5];
    std::fs::File::open(&bin).ok()?.read_exact(&mut magic).ok()?;
    (&magic == b"HDIFF").then_some(bin)
}

/// hpatchz patches the folder over itself: the result replaces the old files only when it is whole.
fn hpatchz_args(inst: &Path, bin: &Path) -> Vec<std::ffi::OsString> {
    vec!["-C-all".into(), "-f".into(), inst.into(), bin.into(), inst.into()]
}

/// An HDiffPatch `Patch.bin` applied with hpatchz: no window, no drive picking.
fn native_patch(g: &Game, hpatchz: &Path, bin: &Path, log: &impl Report) -> Result<()> {
    log.step(&format!("Patching {} (HDiffPatch, no window needed)", g.name));
    let inst = g.install_dir();
    // shortcut: the new data size is not known up front, so the diff's size plus 1 GB stands in; read it from the diff's header to be exact
    need_space(&inst, std::fs::metadata(bin)?.len(), 1 << 30)?;
    let start = SystemTime::now();
    let mut c = Command::new(hpatchz);
    c.args(hpatchz_args(&inst, bin));
    let st = run(c, None, log)?;
    if !st.success() {
        bail!("hpatchz failed ({st}): see its output above; the game files are unchanged unless it said otherwise");
    }
    if !changed_since(&inst, start) {
        bail!("the patch changed no game files");
    }
    Ok(())
}

/// The folders of a scene release besides the installer's: its crack (`Crack`, `RUNE`, ...).
fn crack_dirs(dir: &Path, scene: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.is_dir() && p != scene && !p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("md5")))
        .collect()
}

/// Every crack folder over the install dir.
fn copy_cracks(g: &Game, dir: &Path, scene: &Path, log: &impl Report) -> Result<()> {
    for c in crack_dirs(dir, scene) {
        let mut n = 0;
        copy_tree(&c, &g.install_dir(), &mut n, log)?;
        log.log(format!("copied {n} file(s) from {} into the game", c.file_name().unwrap_or_default().to_string_lossy()));
    }
    Ok(())
}

/// Files under `dir` as paths relative to it ("Bin/x.dll"); `plain_update` leaves out what `copy_files` skips.
fn rel_files(dir: &Path, plain_update: bool) -> Vec<String> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if plain_update && d == dir && (name == "md5" || name.ends_with(".md5") || name.ends_with(".txt")) {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) => out.extend(p.strip_prefix(dir).ok().map(|r| r.to_string_lossy().into_owned())),
                Err(_) => {}
            }
        }
    }
    out
}

/// The paths an Inno setup writes in the game folder: every "{app}\\…" in its header (the [Files] entries
/// and the script's own paths), as ANSI or UTF-16. Wildcards and folders are left out.
fn inno_targets(setup: &Path) -> Vec<String> {
    let Some(h) = crate::read_capped(setup, 64 << 20).ok().and_then(|b| crate::setup_header(&b)).filter(|h| !h.is_empty()) else { return vec![] };
    let ok = |c: u16| c >= 0x20 && char::from_u32(c.into()).is_some_and(|c| !"\"*?<>|".contains(c));
    let mut out = vec![];
    let pat: Vec<u16> = "{app}\\".encode_utf16().collect();
    let ansi: Vec<u16> = h.iter().map(|&b| b.into()).collect();
    let utf16 = |off: usize| h[off..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect::<Vec<u16>>();
    for text in [ansi, utf16(0), utf16(1)] {
        for i in 0..text.len().saturating_sub(pat.len()) {
            if text[i..i + pat.len()] == pat[..] {
                let rest = &text[i + pat.len()..];
                let n = rest.iter().take_while(|&&c| ok(c)).count();
                out.push(String::from_utf16_lossy(&rest[..n]).replace('\\', "/"));
            }
        }
    }
    // only plain paths inside the game folder: none that climb out of it ("..") or name a drive
    out.retain(|p| !p.is_empty() && !p.ends_with('/') && !p.starts_with('/') && !p.contains(':') && !p.split('/').any(|c| c == ".."));
    out.sort();
    out.dedup();
    out
}

/// One update's backup: the version before it, and the game files it changed (copied) or added.
#[derive(Serialize, Deserialize)]
pub struct Backup {
    pub from: String,
    pub to: String,
    files: Vec<String>,
    added: Vec<String>,
    /// The whole game was copied (a patcher that names no files).
    whole: bool,
}

fn backups_dir(g: &Game) -> PathBuf {
    g.dir.join("update-backup")
}

/// Copies on btrfs/XFS within one filesystem share the data (reflinks): instant, no space used.
fn shared_copies(a: &Path, b: &Path) -> bool {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let same = std::fs::metadata(a).and_then(|x| std::fs::metadata(b).map(|y| x.dev() == y.dev())).unwrap_or(false);
    let Ok(c) = std::ffi::CString::new(b.as_os_str().as_bytes()) else { return false };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    same && unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0 && [0x9123_683E, 0x5846_5342].contains(&(st.f_type as i64))
}

/// The backup's "added" list was every path the update might write; keep the ones it really created, so
/// Undo never deletes a file the game makes later at one of those paths.
fn keep_only_created(root: &Path, inst: &Path) {
    let file = root.join("manifest.json");
    if let Some(mut m) = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice::<Backup>(&b).ok()) {
        m.added.retain(|f| inst.join(f).is_file());
        if let Ok(b) = serde_json::to_vec(&m) {
            let _ = std::fs::write(&file, b);
        }
    }
}

/// Copy what the update will change into a new backup folder; its path, or None when no backup was made.
fn snapshot(g: &Game, to: &str, rels: Vec<String>, whole: bool, log: &impl Report) -> Result<Option<PathBuf>> {
    let inst = g.install_dir();
    let mut rels = if whole { rel_files(&inst, false) } else { rels };
    rels.sort();
    rels.dedup();
    let (files, added): (Vec<String>, Vec<String>) = rels.into_iter().partition(|r| inst.join(r).is_file());
    let need: u64 = files.iter().filter_map(|r| std::fs::metadata(inst.join(r)).ok()).map(|m| m.len()).sum();
    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    std::fs::create_dir_all(backups_dir(g))?;
    // two updates of a chain in one second each get their own folder
    let mut root = backups_dir(g).join(secs.to_string());
    let mut n = 2;
    while std::fs::create_dir(&root).is_err() {
        root = backups_dir(g).join(format!("{secs}-{n}"));
        n += 1;
        if n > 100 {
            bail!("could not make a backup folder in {}", backups_dir(g).display());
        }
    }
    if !shared_copies(&inst, &root) {
        if whole {
            let _ = std::fs::remove_dir_all(&root);
            log.log(format!("no backup: this patcher does not say which files it changes, and copying the whole game would take {}", gb(need)));
            return Ok(None);
        }
        need_space(&root, need, 1 << 30).map_err(|e| {
            let _ = std::fs::remove_dir_all(&root);
            anyhow!("no room to back up the files this update changes: {e}. Free some space, or turn off \"Back up before updating\" in Settings")
        })?;
    }
    log.step("Backing up the files the update changes");
    let r = (|| {
        let mut done = 0;
        for f in &files {
            crate::cancel_point(log)?;
            let dst = root.join("files").join(f);
            std::fs::create_dir_all(dst.parent().unwrap_or(&root))?;
            done += std::fs::copy(inst.join(f), &dst).with_context(|| format!("back up {f}"))?;
            log.progress(done, need.max(1));
        }
        let m = Backup { from: g.version.clone(), to: to.to_string(), files, added, whole };
        std::fs::write(root.join("manifest.json"), serde_json::to_vec(&m)?)?;
        log.log(format!("backed up {} file(s), {}, for Undo", m.files.len(), gb(need)));
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&root);
    }
    r.map(|()| Some(root))
}

/// The newest update backup of this game and its folder.
pub fn last_backup(g: &Game) -> Option<(Backup, PathBuf)> {
    // complete ones only (a copy cut off by closing the window has no manifest), newest by "<secs>[-n]"
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(backups_dir(g)).ok()?.flatten().map(|e| e.path()).filter(|d| d.join("manifest.json").is_file()).collect();
    dirs.sort_by_key(|d| {
        let n = d.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let (secs, k) = n.split_once('-').unwrap_or((&n, "1"));
        (secs.parse::<u64>().unwrap_or(0), k.parse::<u64>().unwrap_or(0))
    });
    let d = dirs.pop()?;
    Some((serde_json::from_slice(&std::fs::read(d.join("manifest.json")).ok()?).ok()?, d))
}

/// Space the update backups take (shared copies on btrfs count as their full size).
pub fn backups_size(g: &Game) -> u64 {
    crate::dir_size(&backups_dir(g))
}

pub fn delete_backups(g: &Game) -> Result<()> {
    match std::fs::remove_dir_all(backups_dir(g)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Whether the newest backup belongs to the game as it is: made by the update it is at, or by an update that
/// failed half-way (the game still at its "from"). Any other one is stale (backups were off for a later update).
pub fn can_undo(g: &Game, b: &Backup) -> bool {
    [&b.to, &b.from].iter().any(|v| vcmp(v, &g.version) == Ordering::Equal)
}

/// Put the game files back as they were before the newest backed-up update. Returns (from, to) of it.
pub fn undo(g: &Game, log: &impl Report) -> Result<(String, String)> {
    let (m, root) = last_backup(g).context("no update to undo")?;
    if !can_undo(g, &m) {
        bail!("the newest backup is from v{} to v{}, but the game is at v{}: it can't be undone safely", m.from, m.to, g.version);
    }
    crate::ensure_stopped(g)?;
    log.step(&format!("Undoing the update to v{}", m.to));
    let inst = g.install_dir();
    for f in &m.files {
        let dst = inst.join(f);
        std::fs::create_dir_all(dst.parent().unwrap_or(&inst))?;
        std::fs::copy(root.join("files").join(f), &dst).with_context(|| format!("restore {f}"))?;
    }
    for f in &m.added {
        let _ = std::fs::remove_file(inst.join(f));
    }
    // dirs the update created are empty now: deepest first; remove_dir refuses a non-empty one
    let mut dirs: Vec<PathBuf> = m.added.iter().flat_map(|f| inst.join(f).ancestors().skip(1).take_while(|p| *p != inst).map(Path::to_path_buf).collect::<Vec<_>>()).collect();
    dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(d);
    }
    // a whole-game backup knows no "added" list: files made since (saves, configs, mods) are left alone
    std::fs::remove_dir_all(&root)?;
    let at = if m.from.is_empty() { "back at its earlier version".to_string() } else { format!("at v{} again", m.from) };
    log.log(format!("restored {} file(s); the game is {at}", m.files.len()));
    Ok((m.from, m.to))
}

/// A scene patcher with its own window and no silent mode (TENOKE's HDiffPatch `PATCH.exe`): the user picks
/// the game folder in it. Closing it without patching changes no game file, and that is an error, not an update.
fn gui_patch(g: &Game, exe: &Path, log: &impl Report) -> Result<()> {
    log.step("Patching: in the patcher's window choose drive G: (the game's folder), then start it");
    log.log(format!("In the patcher, choose drive G: (that is {}) and start it; close the patcher when it is done.", g.install_dir_win()));
    g.map_game_drive(log)?;
    let start = SystemTime::now();
    let mut c = g.umu();
    c.current_dir(exe.parent().unwrap_or(exe)).arg(exe);
    let st = run(c, g.exe.as_deref(), log)?;
    if !changed_since(&g.install_dir(), start) {
        bail!("the patcher changed no game files ({st}): apply the update again and pick drive G: ({})", g.install_dir_win());
    }
    Ok(())
}

fn changed_since(dir: &Path, t: SystemTime) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(e.path()),
                Ok(_) if e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m >= t) => return true,
                _ => {}
            }
        }
    }
    false
}

/// A FitGirl patch folder without an installer: its files over the install dir.
fn copy_files(g: &Game, dir: &Path, log: &impl Report) -> Result<()> {
    let mut n = 0;
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if name == "md5" || name.ends_with(".md5") || name.ends_with(".txt") {
            continue; // FitGirl's checksum folder and readmes are not game files
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            copy_tree(&p, &g.install_dir().join(e.file_name()), &mut n, log)?;
        } else if e.file_type().is_ok_and(|t| t.is_symlink()) {
            log.log(format!("skipped link {}", p.display()));
        } else if p.is_file() {
            std::fs::copy(&p, g.install_dir().join(e.file_name()))?;
            n += 1;
        }
    }
    if n == 0 {
        bail!("{} has no setup.exe and no files to copy", dir.display());
    }
    log.log(format!("copied {n} file(s) into {}", g.install_dir().display()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(name: &str) -> Update {
        parse(Path::new(name)).unwrap()
    }

    #[test]
    fn version_tokens() {
        assert_eq!(versions("Cyberpunk 2077 v2.12 + DLC"), ["2.12"]);
        assert_eq!(versions("Update v1.04 to v1.05.1"), ["1.04", "1.05.1"]);
        assert_eq!(versions("Game Build 12345."), ["12345"]);
        assert!(versions("Survivor 2").is_empty());
        assert_eq!(versions("Pokémon Écarlate v1.3"), ["1.3"]); // multibyte chars before the token
        assert_eq!(vcmp("1.10", "1.9"), Ordering::Greater);
        assert_eq!(vcmp("1.2", "1.2.0"), Ordering::Equal);
    }

    #[test]
    fn ladder_orders_and_checks_gaps() {
        let ups = vec![up("Update v1.2 to v1.3"), up("Update v1.0 to v1.1"), up("Update v1.1 to v1.2"), up("Update v0.9 to v1.0")];
        let l = ladder("1.0", ups).unwrap();
        assert_eq!(l.iter().map(|u| u.to.as_str()).collect::<Vec<_>>(), ["1.1", "1.2", "1.3"]);
        let gap = ladder("1.0", vec![up("Update v1.0 to v1.1"), up("Update v1.2 to v1.3")]);
        assert!(gap.unwrap_err().to_string().contains("v1.1"));
        // no from version: order only
        assert_eq!(ladder("", vec![up("Hotfix v2"), up("Update v1.5")]).unwrap().len(), 2);
        assert!(parse(Path::new("Update")).is_err());
    }

    #[test]
    fn scene_archives() {
        let d = std::env::temp_dir().join(format!("repackd-scene-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let file = |n: &str| {
            std::fs::write(d.join(n), b"x").unwrap();
            d.join(n)
        };
        assert_eq!(parse(&file("Kingdom.Come.Deliverance.II.Mysteria.Ecclesiae.MULTi15.Update.v1.5.2-RUNE.part1.rar")).unwrap().to, "1.5.2");
        assert!(parse(&file("Kingdom.Come.Deliverance.II.Mysteria.Ecclesiae.MULTi15.Update.v1.5.2-RUNE.part2.rar")).unwrap_err().to_string().contains("part1"));
        assert_eq!(parse(&file("DAVE.THE.DIVER.In.the.Jungle.Content.Pack.Update.v1.0.6.2087-TENOKE.rar")).unwrap().to, "1.0.6.2087");
        assert!(parse(&file("notes.txt")).is_err());
        assert_eq!(parse(&file("Mario.Party.Superstars.Update.v1.1.rar")).unwrap().to, "1.1"); // ".Party" is no part number
        assert_eq!((rar_part("a.part02.rar"), rar_part("a.party.rar"), rar_part("a.part1.zip")), (Some(2), None, None));
        // names from FitGirl's update digests (October 2026): (from, to)
        for (n, from, to) in [
            ("ANTONBLAST.Update.v1.2.4e3-TENOKE.rar", None, "1.2.4e3"),
            ("Strike.Force.Heroes.Ninja.Class.Update.v1.30n-TENOKE.rar", None, "1.30n"),
            ("Agefield.High.Rock.the.School.Update.4-RUNE.rar", None, "4"),
            ("DOOM.The.Dark.Ages.Update.4.Hotfix.2-RUNE.rar", None, "4.Hotfix.2"),
            ("UBOAT.Type.IX.Distant.Coasts.Update.v2026.1.Patch.21-RUNE.rar", None, "2026.1.Patch.21"),
            ("DEAD.OR.ALIVE.6.Last.Round.Update.v1.10.incl.DLC-RUNE.part1.rar", None, "1.10"),
            ("Dawn.Break.Update.v20261005-TENOKE.rar", None, "20261005"),
            ("Sudden.Strike.5.France.Road.to.Liberation.Update.v1.09.29558-RUNE.rar", None, "1.09.29558"),
            ("Grim_Dawn_Update_from_v1.3.0.3_to_v1.3.0.8-ElAmigos.rar", Some("1.3.0.3"), "1.3.0.8"),
            ("Agefield_High_Rock_the_School_Update_21.08.2026-ElAmigos.rar", None, "21.08.2026"),
            ("UBOAT_Update_from_01.05.2026_to_14.05.2026-ElAmigos.rar", Some("01.05.2026"), "14.05.2026"),
            ("RIDE_6_Update_1774570-ElAmigos.rar", None, "1774570"),
            ("Mortal_Shell_II_Update_from_v93241.H1_to_93241.H2-ElAmigos.rar", Some("93241.H1"), "93241.H2"),
            ("Dune_Spice_Wars_Four_Updates_to_v2.1.0.32200-ElAmigos.rar", None, "2.1.0.32200"),
            ("Game.Update.v1.2.7z", None, "1.2"),
        ] {
            let u = parse(&file(n)).unwrap();
            assert_eq!((u.from.as_deref(), u.to.as_str()), (from, to), "{n}");
        }
        assert_eq!(vcmp("07.10.2026", "29.07.2026"), Ordering::Greater, "dates by year, month, day");
        assert_eq!(vcmp("93241.H2", "93241.H1"), Ordering::Greater);
        assert_eq!(vcmp("1.30b", "1.30a"), Ordering::Greater, "letter tags count");
        assert_eq!(vcmp("1.30a", "1.30"), Ordering::Greater);
        assert_eq!(vcmp("v1.2", "1.2"), Ordering::Equal, "a typed-in leading v");
        assert_eq!(vcmp("2026.1.Patch.21", "2026.1.Patch.20"), Ordering::Greater);
        assert_eq!(versions("v1.5.1.md5"), ["1.5.1"], "an extension is no tag");
        let w = |s: &str| words(s);
        assert!(same_game(&w("Dave The Diver"), &w("DAVE.THE.DIVER.In.the.Jungle.Content.Pack.")));
        assert!(!same_game(&w("Hades"), &w("Hades.II.")) && !same_game(&w("Hades II"), &w("Hades.")), "a sequel is another game");
        assert!(!same_game(&w("DOOM"), &w("DOOM.The.Dark.Ages.")) && same_game(&w("DOOM"), &w("DOOM.")), "one word: the whole name");
        assert_eq!(versions("Cyberpunk 2077 v2.31 + Update 3"), ["2.31"], "a repack's name: \"Update 3\" is no version");
        // the release folder inside the archive; its crack folders land at the game's root, keeping their paths
        let rel = d.join("x/Game.Update.v2-RUNE");
        for f in ["Update/Setup.exe", "RUNE/Bin/Win64Shared/steam_api64.dll", "Crack/version.dll", "rune.nfo"] {
            std::fs::create_dir_all(rel.join(f).parent().unwrap()).unwrap();
            std::fs::write(rel.join(f), b"").unwrap();
        }
        assert_eq!(content_root(&d.join("x")), rel);
        std::fs::create_dir_all(d.join("hotfix/Data")).unwrap(); // a zip of just the game's Data folder
        assert_eq!(content_root(&d.join("hotfix")), d.join("hotfix"));
        std::fs::create_dir_all(d.join("rune/Agefield.High.Rock.the.School.Update.4-RUNE/Update")).unwrap(); // no "v" in the name
        assert_eq!(content_root(&d.join("rune")), d.join("rune/Agefield.High.Rock.the.School.Update.4-RUNE"));
        let g = Game { dir: d.join("g"), ..Default::default() };
        copy_cracks(&g, &rel, &rel.join("Update"), &|_| {}).unwrap();
        let i = g.install_dir();
        assert!(i.join("Bin/Win64Shared/steam_api64.dll").is_file() && i.join("version.dll").is_file());
        assert!(!i.join("Setup.exe").exists() && !i.join("rune.nfo").exists());
        if which_path("7z").is_some() {
            let zip = d.join("Game.Update.v3.zip");
            assert!(Command::new("7z").args(["a", "-bd", "-bso0"]).arg(&zip).arg(&rel).status().unwrap().success());
            let out = d.join("unpacked");
            extract(&zip, &out, &d.join("pfx"), &|_| {}).unwrap();
            assert!(content_root(&out).join("Update/Setup.exe").is_file());
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn updates_are_found_next_to_the_game() {
        let d = std::env::temp_dir().join(format!("repackd-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let repack = d.join("Dave The Diver [FitGirl Repack]");
        std::fs::create_dir_all(repack.join("Update v1.0.6.2018 to v1.0.6.2050")).unwrap();
        for f in ["DAVE.THE.DIVER.In.the.Jungle.Content.Pack.Update.v1.0.6.2087-TENOKE.rar", "DAVE.THE.DIVER.Update.v1.0.5.1-TENOKE.rar", "Dredge.Update.v1.5-RUNE.rar", "Dave.Something.v2.rar"] {
            std::fs::write(d.join(f), b"").unwrap();
        }
        let g = Game { name: "Dave The Diver [FitGirl Repack]".into(), repack_dir: repack.clone(), dir: d.join("g"), version: "1.0.6.2018".into(), ..Default::default() };
        let tos: Vec<String> = find_for(&g).unwrap().into_iter().map(|u| u.to).collect();
        assert_eq!(tos, ["1.0.6.2050", "1.0.6.2087"], "newer ones of this game, in order; older, other games' and non-updates left out");
        // "patch" inside the game's own name is no end of it
        std::fs::write(d.join("Dispatch.Update.v1.0.5-RUNE.rar"), b"").unwrap();
        std::fs::create_dir_all(d.join("Dispatch")).unwrap();
        let dispatch = Game { name: "Dispatch".into(), repack_dir: d.join("Dispatch"), dir: d.join("g2"), version: "1.0".into(), ..Default::default() };
        assert_eq!(find_for(&dispatch).unwrap().len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn hdiff_patch_needs_the_magic() {
        let d = std::env::temp_dir().join(format!("repackd-hdiff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        assert!(hdiff_patch(&d).is_none(), "no Patch.bin");
        std::fs::write(d.join("Patch.bin"), "PK\x03\x04 not a diff").unwrap();
        assert!(hdiff_patch(&d).is_none());
        std::fs::write(d.join("Patch.bin"), "HDIFF19&&fadler64").unwrap();
        assert!(hdiff_patch(&d).is_some());
        std::fs::remove_dir_all(&d).unwrap();
        let a = hpatchz_args(Path::new("/g"), Path::new("/u/Patch.bin"));
        assert_eq!(a, ["-C-all", "-f", "/g", "/u/Patch.bin", "/g"]);
    }

    #[test]
    fn patcher_counts_only_if_it_changed_a_file() {
        let d = std::env::temp_dir().join(format!("repackd-changed-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let f = std::fs::File::create(d.join("sub/game.pak")).unwrap();
        let t = SystemTime::now();
        f.set_modified(t - std::time::Duration::from_secs(60)).unwrap();
        assert!(!changed_since(&d, t), "closed without patching");
        f.set_modified(t + std::time::Duration::from_secs(1)).unwrap();
        assert!(changed_since(&d, t), "a patched file deep in the folder");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn update_backup_and_undo() {
        let d = std::env::temp_dir().join(format!("repackd-undo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let g = Game { dir: d.join("g"), version: "1.0".into(), ..Default::default() };
        let inst = g.install_dir();
        std::fs::create_dir_all(inst.join("Data")).unwrap();
        std::fs::write(inst.join("Data/a.pak"), b"old").unwrap();
        // a plain update folder changing a.pak and adding b.pak
        let u = d.join("Update v1.0 to v1.1");
        std::fs::create_dir_all(u.join("Data")).unwrap();
        std::fs::write(u.join("Data/a.pak"), b"new").unwrap();
        std::fs::write(u.join("Data/b.pak"), b"added").unwrap();
        std::fs::create_dir_all(u.join("data")).unwrap();
        std::fs::write(u.join("data/new.txt"), b"x").unwrap();
        apply(&g, &parse(&u).unwrap(), false, true, &|_| {}).unwrap();
        assert_eq!(std::fs::read(inst.join("Data/a.pak")).unwrap(), b"new");
        let (b, _) = last_backup(&g).unwrap();
        assert_eq!((b.from.as_str(), b.to.as_str()), ("1.0", "1.1"));
        let stale = Game { version: "1.2".into(), ..g.clone() };
        assert!(undo(&stale, &|_| {}).is_err(), "a backup older than the game's version is refused");
        let mut g = g;
        g.version = "1.1".into();
        assert_eq!(undo(&g, &|_| {}).unwrap(), ("1.0".to_string(), "1.1".to_string()));
        assert_eq!(std::fs::read(inst.join("Data/a.pak")).unwrap(), b"old");
        assert!(!inst.join("Data/b.pak").exists() && last_backup(&g).is_none());
        assert!(!inst.join("data").exists() && inst.join("Data").is_dir(), "dirs the update added go, the others stay");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn plain_folder_update_copies_files() {
        let d = std::env::temp_dir().join(format!("repackd-up-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let u = d.join("Update v1.0 to v1.1");
        std::fs::create_dir_all(u.join("MD5")).unwrap();
        std::fs::create_dir_all(u.join("data")).unwrap();
        std::fs::write(u.join("data/patch.pak"), b"new").unwrap();
        std::fs::write(u.join("readme.txt"), b"x").unwrap();
        let g = Game { name: "g".into(), repack_dir: d.clone(), dir: d.join("games/g"), status: crate::Status::Installed, exe: None, steam: None, ..Default::default() };
        std::fs::create_dir_all(g.install_dir().join("data")).unwrap();
        std::fs::write(g.install_dir().join("data/patch.pak"), b"old").unwrap();
        std::os::unix::fs::symlink(&u, u.join("data/loop")).unwrap(); // a link back up must not be walked forever
        apply(&g, &parse(&u).unwrap(), true, false, &|_| {}).unwrap();
        assert_eq!(std::fs::read(g.install_dir().join("data/patch.pak")).unwrap(), b"new");
        assert!(!g.install_dir().join("readme.txt").exists() && !g.install_dir().join("MD5").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
