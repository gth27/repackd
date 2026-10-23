//! Non-Steam shortcuts in `userdata/<id>/config/shortcuts.vdf` (binary VDF). The parser keeps every key and
//! byte it does not know, so a write never drops fields newer Steam versions add (`FlatpakAppID`, `sortas`, ...).
use anyhow::{bail, Context, Result};
use std::{hash::{BuildHasher, Hasher}, path::{Path, PathBuf}};

#[derive(Debug, Clone, PartialEq)]
pub enum Val {
    Map(Vec<(Vec<u8>, Val)>),
    Str(Vec<u8>),
    Int(u32),
    U64(u64),
}

fn cstr(b: &[u8], i: &mut usize) -> Result<Vec<u8>> {
    let n = b[*i..].iter().position(|&c| c == 0).context("unterminated VDF string")?;
    let s = b[*i..*i + n].to_vec();
    *i += n + 1;
    Ok(s)
}

fn take<const N: usize>(b: &[u8], i: &mut usize) -> Result<[u8; N]> {
    let v = b.get(*i..*i + N).context("truncated VDF")?.try_into()?;
    *i += N;
    Ok(v)
}

fn map(b: &[u8], i: &mut usize) -> Result<Vec<(Vec<u8>, Val)>> {
    let mut m = vec![];
    loop {
        let t = *b.get(*i).context("truncated VDF")?;
        *i += 1;
        if t == 8 {
            return Ok(m);
        }
        let k = cstr(b, i)?;
        let v = match t {
            0 => Val::Map(map(b, i)?),
            1 => Val::Str(cstr(b, i)?),
            2 => Val::Int(u32::from_le_bytes(take(b, i)?)),
            7 => Val::U64(u64::from_le_bytes(take(b, i)?)),
            _ => bail!("unknown VDF type {t:#x}: not touching this shortcuts.vdf"),
        };
        m.push((k, v));
    }
}

pub fn parse(b: &[u8]) -> Result<Vec<(Vec<u8>, Val)>> {
    let mut i = 0;
    let m = map(b, &mut i)?;
    if i != b.len() {
        bail!("trailing bytes in shortcuts.vdf");
    }
    Ok(m)
}

fn write_map(m: &[(Vec<u8>, Val)], out: &mut Vec<u8>) {
    for (k, v) in m {
        out.push(match v { Val::Map(_) => 0, Val::Str(_) => 1, Val::Int(_) => 2, Val::U64(_) => 7 });
        out.extend_from_slice(k);
        out.push(0);
        match v {
            Val::Map(m) => write_map(m, out),
            Val::Str(s) => {
                out.extend_from_slice(s);
                out.push(0);
            }
            Val::Int(n) => out.extend_from_slice(&n.to_le_bytes()),
            Val::U64(n) => out.extend_from_slice(&n.to_le_bytes()),
        }
    }
    out.push(8);
}

pub fn write(m: &[(Vec<u8>, Val)]) -> Vec<u8> {
    let mut out = vec![];
    write_map(m, &mut out);
    out
}

/// The `shortcuts.vdf` path of every Steam account on this machine (native and Flatpak Steam).
pub fn users() -> Vec<PathBuf> {
    let home = crate::home();
    let mut out = vec![];
    for root in [".local/share/Steam", ".var/app/com.valvesoftware.Steam/.local/share/Steam"] {
        let Ok(rd) = std::fs::read_dir(home.join(root).join("userdata")) else { continue };
        for e in rd.flatten() {
            let cfg = e.path().join("config");
            if cfg.is_dir() && e.file_name() != "0" {
                out.push(cfg.join("shortcuts.vdf"));
            }
        }
    }
    out.sort();
    out
}

pub fn running() -> bool {
    std::process::Command::new("pgrep").args(["-x", "steam"]).output().is_ok_and(|o| o.status.success())
}

fn quoted(p: &Path) -> Vec<u8> {
    format!("\"{}\"", p.display()).into_bytes()
}

fn get<'a>(m: &'a [(Vec<u8>, Val)], key: &str) -> Option<&'a Val> {
    m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key.as_bytes())).map(|(_, v)| v)
}

/// Load the file (missing = empty), let `f` change the shortcut list, and write it back with a backup when
/// `f` returns true. Steam rewrites the file on exit, so the caller must check `running()` first.
fn edit(vdf: &Path, f: impl FnOnce(&mut Vec<(Vec<u8>, Val)>) -> bool) -> Result<bool> {
    let mut root = match std::fs::read(vdf) {
        Ok(b) => parse(&b).with_context(|| format!("parse {}", vdf.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e).with_context(|| format!("read {}", vdf.display())),
    };
    if get(&root, "shortcuts").is_none() {
        root.push((b"shortcuts".to_vec(), Val::Map(vec![])));
    }
    let Some((_, Val::Map(list))) = root.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(b"shortcuts")) else {
        bail!("shortcuts.vdf: \"shortcuts\" is not a map");
    };
    if !f(list) {
        return Ok(false);
    }
    for (n, (k, _)) in list.iter_mut().enumerate() {
        *k = n.to_string().into_bytes(); // Steam expects the keys 0..n in order
    }
    if vdf.exists() {
        std::fs::copy(vdf, vdf.with_extension("vdf.repackd.bak")).context("back up shortcuts.vdf")?;
    }
    let tmp = vdf.with_extension("vdf.repackd.tmp");
    std::fs::write(&tmp, write(&root))?;
    std::fs::rename(&tmp, vdf)?;
    Ok(true)
}

fn has_exe(e: &Val, exe: &[u8]) -> bool {
    matches!(e, Val::Map(m) if matches!(get(m, "Exe"), Some(Val::Str(s)) if s == exe))
}

fn appid_of(e: &Val) -> Option<u32> {
    match e {
        Val::Map(m) => match get(m, "appid") {
            Some(Val::Int(n)) => Some(*n),
            _ => None,
        },
        _ => None,
    }
}

/// Add a shortcut that runs `exe` (our launch script). Returns whether it was added (false: one with that exe
/// already exists) and the shortcut's appid, which names its artwork and its `steam://rungameid/` link.
pub fn add(vdf: &Path, name: &str, exe: &Path, start_dir: &Path) -> Result<(bool, u32)> {
    let q = quoted(exe);
    let mut id = 0;
    let added = edit(vdf, |list| {
        if let Some((_, e)) = list.iter().find(|(_, e)| has_exe(e, &q)) {
            id = appid_of(e).unwrap_or(0);
            return false;
        }
        let appid = std::hash::RandomState::new().build_hasher().finish() as u32 | 0x8000_0000;
        id = appid;
        let s = |k: &str, v: Vec<u8>| (k.as_bytes().to_vec(), Val::Str(v));
        let i = |k: &str, v: u32| (k.as_bytes().to_vec(), Val::Int(v));
        let entry = vec![
            i("appid", appid),
            s("AppName", name.replace('\0', "").into()), // a NUL would end the string early and corrupt the file
            s("Exe", q.clone()),
            s("StartDir", quoted(start_dir)),
            s("icon", vec![]),
            s("ShortcutPath", vec![]),
            s("LaunchOptions", vec![]),
            i("IsHidden", 0),
            i("AllowDesktopConfig", 1),
            i("AllowOverlay", 1),
            i("OpenVR", 0),
            i("Devkit", 0),
            s("DevkitGameID", vec![]),
            i("DevkitOverrideAppID", 0),
            i("LastPlayTime", 0),
            s("FlatpakAppID", vec![]),
            s("sortas", vec![]),
            (b"tags".to_vec(), Val::Map(vec![])),
        ];
        list.push((vec![], Val::Map(entry)));
        true
    })?;
    Ok((added, id))
}

/// The 64-bit game id Steam uses for a non-Steam shortcut in `steam://rungameid/<id>`.
pub fn game_id(appid: u32) -> u64 {
    (appid as u64) << 32 | 0x0200_0000
}

/// Steam's custom artwork folder for this account.
pub fn grid_dir(vdf: &Path) -> PathBuf {
    vdf.with_file_name("grid")
}

/// Remove artwork repackd stored for this shortcut.
pub fn remove_art(vdf: &Path, appid: u32) {
    for e in std::fs::read_dir(grid_dir(vdf)).into_iter().flatten().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let stem = n.split('.').next().unwrap_or("");
        if [format!("{appid}"), format!("{appid}p"), format!("{appid}_hero"), format!("{appid}_logo")].contains(&stem.to_string()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Search words for an artwork lookup: the repack name without `[FitGirl Repack]`, `(…)`, versions,
/// `+ DLCs` and the like.
pub fn art_term(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in name.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            '+' if depth == 0 => break, // "+ All DLCs", "+ Bonus Content"
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // " - " stays: folder names use it where the title has a colon ("Kingdom Come - Deliverance II")
    let words: Vec<&str> = out.split_whitespace().filter(|w| *w != "-" && *w != "–").collect();
    let cut = words.iter().position(|w| {
        let l = w.to_ascii_lowercase();
        (l.starts_with('v') && l[1..].starts_with(|c: char| c.is_ascii_digit())) || l == "build"
    });
    words[..cut.unwrap_or(words.len())].join(" ").trim_end_matches([' ', '-', '–', ':', ',']).to_string()
}

fn curl(url: &str, key: Option<&str>, out: Option<&Path>) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut c = std::process::Command::new("curl");
    // URLs partly come from SteamGridDB's answers: https only, and "--" so none is read as an option
    c.args(["-sSfL", "--proto", "=https", "-m", "60", "-K", "-"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    if let Some(o) = out {
        c.arg("-o").arg(o); // before "--": after it, curl would take "-o" for a URL
    }
    c.args(["--", url]);
    let mut child = c.spawn().context("start curl")?;
    // the API key goes in on stdin as a curl config line, so it never shows up in the process list
    let cfg = key.map(|k| format!("header = \"Authorization: Bearer {k}\"\n")).unwrap_or_default();
    child.stdin.take().unwrap().write_all(cfg.as_bytes())?;
    let o = child.wait_with_output()?;
    if !o.status.success() {
        bail!("{url}: {}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(o.stdout)
}

const SGDB: &str = "https://www.steamgriddb.com/api/v2";

/// Letters and digits only, lower case: "Kingdom Come: Deliverance II" and "kingdom come deliverance ii" match.
pub(crate) fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// One SteamGridDB search hit: id, name, release year.
pub type SgdbHit = (u64, String, Option<i32>);

/// SteamGridDB's search for a term (any platform: PC, Switch, PlayStation, ...).
pub fn sgdb_search(term: &str, key: &str) -> Result<Vec<SgdbHit>> {
    let enc: String = term.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    let found = curl(&format!("{SGDB}/search/autocomplete/{enc}"), Some(key), None).map_err(|e| {
        if e.to_string().contains("401") { anyhow::anyhow!("SteamGridDB rejected the API key: check it in Settings") } else { e }
    })?;
    let found: serde_json::Value = serde_json::from_slice(&found)?;
    let year = |t: i64| 1970 + (t / 31_556_952) as i32; // seconds to a year is all a list needs
    Ok(found["data"].as_array().cloned().unwrap_or_default().iter().filter_map(|g| Some((g["id"].as_u64()?, g["name"].as_str()?.to_string(), g["release_date"].as_i64().map(year)))).collect())
}

/// Shorter forms of a name to try when the full one finds nothing: without abbreviation words
/// ("TLoZ - Echoes of Wisdom" -> "Echoes of Wisdom"), then dropping words from the front and the back.
pub fn search_terms(name: &str) -> Vec<String> {
    let term = art_term(name);
    let words: Vec<&str> = term.split_whitespace().collect();
    let abbrev = |w: &&str| w.len() <= 5 && w.chars().filter(|c| c.is_ascii_uppercase()).count() >= 2 && !w.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    let mut out = vec![term.clone()];
    let plain: Vec<&str> = words.iter().filter(|w| !abbrev(w)).copied().collect();
    out.push(plain.join(" "));
    for k in 1..words.len().saturating_sub(1) {
        out.push(words[k..].join(" "));
        out.push(words[..words.len() - k].join(" "));
    }
    out.retain(|t| t.split_whitespace().count() >= 2 || t == &term);
    let mut seen = std::collections::HashSet::new();
    out.retain(|t| seen.insert(t.clone()));
    out
}

/// The SteamGridDB game for a name: the result whose name matches best, not just the first one
/// ("Kingdom Come" alone is a different game than "Kingdom Come: Deliverance II"). Shorter forms of the
/// name are tried when the full one finds nothing.
pub fn sgdb_game(name: &str, key: &str) -> Result<(u64, String)> {
    for term in search_terms(name) {
        let list = sgdb_search(&term, key)?;
        let want = norm(&term);
        // an exact name, else a close one: at least half of the search words in it (never just "the first hit")
        let words = |s: &str| s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 2).map(str::to_lowercase).collect::<Vec<_>>();
        let tw = words(&term);
        let close = |n: &str| {
            let nw = words(n);
            tw.iter().filter(|w| nw.contains(w)).count() * 2 >= tw.len().max(2)
        };
        if let Some((id, n, _)) = list.iter().find(|g| norm(&g.1) == want).or_else(|| list.iter().find(|g| close(&g.1))) {
            return Ok((*id, n.clone()));
        }
    }
    bail!("SteamGridDB found nothing for \"{}\": search for it in the game's settings, or pick your own images there", art_term(name))
}

/// The Steam store id SteamGridDB knows for one of its games, if it is a Steam game at all.
pub fn sgdb_steam_id(sgdb_id: u64, key: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_slice(&curl(&format!("{SGDB}/games/id/{sgdb_id}?platformdata=steam"), Some(key), None).ok()?).ok()?;
    v["data"]["external_platform_data"]["steam"][0]["id"].as_str()?.parse().ok()
}

/// ProtonDB's rating for a Steam game: tier ("platinum", "gold", ...) and how many reports it rests on.
pub fn protondb(store_id: u32) -> Result<(String, u64)> {
    let v: serde_json::Value = serde_json::from_slice(&curl(&format!("https://www.protondb.com/api/v1/reports/summaries/{store_id}.json"), None, None)?)?;
    Ok((v["tier"].as_str().context("no ProtonDB rating yet")?.to_string(), v["total"].as_u64().unwrap_or(0)))
}

fn ext(url: &str) -> &str {
    url.rsplit('.').next().filter(|e| crate::IMAGE_EXTS.contains(e)).unwrap_or("png")
}

/// Fetch library artwork for a shortcut into Steam's grid folder (Steam shows it after a restart).
pub fn fetch_art(vdf: &Path, appid: u32, name: &str, sgdb_key: &str, store_id: Option<u32>, sgdb_id: Option<u64>, log: &impl crate::Report) -> Result<usize> {
    let stems = [format!("{appid}p"), format!("{appid}"), format!("{appid}_hero"), format!("{appid}_logo")];
    fetch_images(&grid_dir(vdf), &stems, name, sgdb_key, store_id, sgdb_id, false, log)
}

/// Portrait cover, wide banner, hero and logo for a game into `dir` as `<stem>.<ext>` (stems in that order):
/// from SteamGridDB by name when there is an API key, otherwise from Steam's image CDN when the game's Steam
/// store id is known. Returns how many images were saved.
/// `sgdb_id` is a SteamGridDB game the user picked; without it the game is searched by name.
/// `only_missing` keeps images already there (the user's own picks): only the missing kinds are fetched.
#[allow(clippy::too_many_arguments)]
pub fn fetch_images(grid: &Path, stems: &[String; 4], name: &str, sgdb_key: &str, store_id: Option<u32>, sgdb_id: Option<u64>, only_missing: bool, log: &impl crate::Report) -> Result<usize> {
    std::fs::create_dir_all(grid)?;
    let have = |stem: &str| crate::IMAGE_EXTS.iter().any(|x| grid.join(format!("{stem}.{x}")).is_file());
    if only_missing && stems.iter().all(|s| have(s)) {
        return Ok(0);
    }
    let [portrait, wide, hero, logo] = stems.clone();
    let mut saved = 0;
    let mut save = |url: &str, stem: String, key: Option<&str>| {
        if only_missing && have(&stem) {
            return;
        }
        // downloaded beside it first: a timeout must not leave the old image cut off
        let dst = grid.join(format!("{stem}.{}", ext(url)));
        let part = dst.with_extension(format!("{}.part", ext(url)));
        match curl(url, key, Some(&part)).and_then(|_| std::fs::rename(&part, &dst).map_err(Into::into)) {
            Ok(_) => {
                saved += 1;
                // an older copy in another format would be picked over the new one
                for x in crate::IMAGE_EXTS.iter().filter(|x| **x != ext(url)) {
                    let _ = std::fs::remove_file(grid.join(format!("{stem}.{x}")));
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                log.log(format!("artwork: {e:#}"));
            }
        }
    };
    if !sgdb_key.trim().is_empty() {
        let key = sgdb_key.trim();
        let api = SGDB;
        let (id, found) = match sgdb_id {
            Some(id) => (id, format!("game #{id}")),
            None => sgdb_game(name, key)?,
        };
        log.log(format!("artwork from SteamGridDB: {found}"));
        for (kind, query, stem) in [
            ("grids", "?dimensions=600x900", portrait),
            ("grids", "?dimensions=920x430,460x215", wide),
            ("heroes", "", hero),
            ("logos", "", logo),
        ] {
            let list: serde_json::Value = serde_json::from_slice(&curl(&format!("{api}/{kind}/game/{id}{query}"), Some(key), None)?)?;
            if let Some(url) = list["data"][0]["url"].as_str() {
                save(url, stem, None);
            }
        }
    } else if let Some(sid) = store_id {
        let cdn = format!("https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/{sid}");
        save(&format!("{cdn}/library_600x900_2x.jpg"), portrait, None);
        save(&format!("{cdn}/header.jpg"), wide, None);
        save(&format!("{cdn}/library_hero.jpg"), hero, None);
        save(&format!("{cdn}/logo.png"), logo, None);
    } else {
        bail!("no artwork source: add a SteamGridDB API key in settings, or the game's Steam store id");
    }
    if saved == 0 && !only_missing {
        bail!("no artwork found for \"{name}\"");
    }
    Ok(saved)
}

/// Point the shortcut that runs `old_exe` at `exe` and `start_dir`, keeping its appid, artwork and play time.
/// Returns false when there was none.
pub fn retarget(vdf: &Path, old_exe: &Path, exe: &Path, start_dir: &Path) -> Result<bool> {
    let q = quoted(old_exe);
    edit(vdf, |list| {
        let mut changed = false;
        for (_, e) in list.iter_mut().filter(|(_, e)| has_exe(e, &q)) {
            let Val::Map(m) = e else { continue };
            for (k, v) in m.iter_mut() {
                if k.eq_ignore_ascii_case(b"Exe") {
                    *v = Val::Str(quoted(exe));
                } else if k.eq_ignore_ascii_case(b"StartDir") {
                    *v = Val::Str(quoted(start_dir));
                }
            }
            changed = true;
        }
        changed
    })
}

/// Remove every shortcut that runs `exe`. Returns false when there was none.
pub fn remove(vdf: &Path, exe: &Path) -> Result<bool> {
    let q = quoted(exe);
    edit(vdf, |list| {
        let n = list.len();
        list.retain(|(_, e)| !has_exe(e, &q));
        list.len() != n
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape of a real file: one shortcut, ends with tags/entry/shortcuts/root terminators.
    fn sample() -> Vec<u8> {
        let mut b = b"\0shortcuts\0\x000\0\x02appid\0\xfa3H\xc0\x01AppName\0Game\0\x01Exe\0\"/g/game.exe\"\0".to_vec();
        b.extend_from_slice(b"\x02LastPlayTime\0\xaf\xc8j\x00\x01FlatpakAppID\0\0\x01sortas\0\0\0tags\0\x08\x08\x08\x08");
        b
    }

    #[test]
    fn retarget_keeps_appid() {
        let d = std::env::temp_dir().join(format!("repackd-retarget-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let vdf = d.join("shortcuts.vdf");
        std::fs::write(&vdf, sample()).unwrap();
        assert!(!retarget(&vdf, Path::new("/nope"), Path::new("/n/l.sh"), Path::new("/n")).unwrap());
        assert!(retarget(&vdf, Path::new("/g/game.exe"), Path::new("/n/l.sh"), Path::new("/n")).unwrap());
        let root = parse(&std::fs::read(&vdf).unwrap()).unwrap();
        let Some((_, Val::Map(list))) = root.iter().find(|(k, _)| k == b"shortcuts") else { panic!() };
        let Val::Map(e) = &list[0].1 else { panic!() };
        assert!(has_exe(&list[0].1, b"\"/n/l.sh\"") && matches!(get(e, "appid"), Some(Val::Int(n)) if *n == 0xc04833fa));
        assert!(vdf.with_extension("vdf.repackd.bak").is_file());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shorter_search_terms() {
        let t = search_terms("TLoZ - Echoes of Wisdom [FitGirl Repack]");
        assert_eq!(t[0], "TLoZ Echoes of Wisdom");
        assert!(t.contains(&"Echoes of Wisdom".to_string()), "{t:?}");
        assert_eq!(search_terms("Hades")[0], "Hades"); // one word: just itself
        assert!(!search_terms("GTA V").contains(&"V".to_string()));
    }

    #[test]
    fn art_terms() {
        assert_eq!(art_term("BRAVELY DEFAULT FLYING FAIRY HD Remaster [FitGirl Repack]"), "BRAVELY DEFAULT FLYING FAIRY HD Remaster");
        assert_eq!(art_term("Cyberpunk 2077 v2.12 + All DLCs [FitGirl Repack]"), "Cyberpunk 2077");
        assert_eq!(art_term("Elden Ring - Deluxe Edition (Build 1234)"), "Elden Ring Deluxe Edition");
        assert_eq!(art_term("Kingdom Come - Deliverance II"), "Kingdom Come Deliverance II");
        assert_eq!(norm("Kingdom Come: Deliverance II"), norm(&art_term("Kingdom Come - Deliverance II")));
        assert_eq!(art_term("Baldur's Gate 3 – v4.1.1 + Bonus"), "Baldur's Gate 3");
    }

    #[test]
    fn roundtrip_keeps_unknown_keys() {
        let b = sample();
        assert_eq!(write(&parse(&b).unwrap()), b);
    }

    #[test]
    fn add_dedupe_remove() {
        let dir = std::env::temp_dir().join(format!("repackd-steam-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let vdf = dir.join("shortcuts.vdf");
        std::fs::write(&vdf, sample()).unwrap();
        let (exe, sd) = (Path::new("/x/launch.sh"), Path::new("/x"));
        let (added, id) = add(&vdf, "New", exe, sd).unwrap();
        assert!(added && id & 0x8000_0000 != 0);
        assert_eq!(add(&vdf, "New", exe, sd).unwrap(), (false, id), "existing shortcut keeps its appid");
        assert_eq!(game_id(id) >> 32, id as u64);
        let root = parse(&std::fs::read(&vdf).unwrap()).unwrap();
        let Some(Val::Map(list)) = get(&root, "shortcuts") else { panic!() };
        assert_eq!(list.iter().map(|(k, _)| k.as_slice()).collect::<Vec<_>>(), [b"0", b"1"]);
        assert_eq!(std::fs::read(vdf.with_extension("vdf.repackd.bak")).unwrap(), sample());
        assert!(remove(&vdf, exe).unwrap());
        assert!(!remove(&vdf, exe).unwrap());
        assert_eq!(std::fs::read(&vdf).unwrap(), sample());
        // a NUL in the name must not cut the file short
        assert!(add(&vdf, "bad\0name", Path::new("/y/launch.sh"), sd).unwrap().0);
        let root = parse(&std::fs::read(&vdf).unwrap()).unwrap();
        assert_eq!(write(&root), std::fs::read(&vdf).unwrap());
        assert!(remove(&vdf, Path::new("/y/launch.sh")).unwrap());
        // missing file: created from scratch
        let fresh = dir.join("new.vdf");
        assert!(add(&fresh, "A", exe, sd).unwrap().0);
        assert_eq!(parse(&std::fs::read(&fresh).unwrap()).unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
