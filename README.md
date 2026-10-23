# repackd

repackd is a desktop app for Arch-based Linux, written in Rust with egui. It installs game repacks you already have (setup.exe-style installers such as the FitGirl format) into a separate Wine prefix per game, using umu-launcher and GE-Proton, and then launches them. Console games run through emulators you set up yourself.

repackd does not download games, torrents or updates. It only works with files you already have on your disk. Use it only with games you are entitled to use.

## Features

- Verifies files against the repack's MD5 list before and after installing.
- Proton picker with in-app downloads (GE-Proton, Proton-CachyOS, Proton-EM) and removal of unused versions.
- Per-game tweaks: MangoHud, GameMode, Gamescope, launch options and winetricks.
- Adds games to Steam as non-Steam shortcuts, with artwork from SteamGridDB (free API key).
- Finds updates placed next to a game (FitGirl update folders and scene update archives) and applies them in version order. You press Apply. An optional backup lets you undo an update.
- Save backups after playing (the last 5 are kept).
- Playtime tracking.
- Gamepad navigation.
- Several themes.
- Disk usage page, per game: game, prefix and repack sizes, with the option to delete a repack you no longer need.
- Bulk import of a whole folder of repacks and unpacked games.
- Watch folder: a finished repack in your download folder is offered for adding.
- Move a game's repackd folder to another disk.
- Optional app-menu entry per game.
- Per-game shader cache, with its size shown.
- Switch and PS4 emulator support through emulators you provide.
- "Copy diagnostics" for bug reports.

## Requirements

Required: `umu-launcher`, `curl`, `gcc-libs`, `glibc`, `libglvnd`, `libxkbcommon`, `systemd-libs`, `xdg-utils`. Building needs `cargo`.

Optional: `winetricks` (prefix tools), `7zip` (scene update archives), `libnotify` (notification when a long job ends), `mangohud`, `gamemode`, `gamescope`, `xdg-desktop-portal` (file pickers), `fontconfig` (CJK game names).

```
sudo pacman -S umu-launcher curl gcc-libs glibc libglvnd libxkbcommon systemd-libs xdg-utils cargo
sudo pacman -S --asdeps winetricks 7zip libnotify mangohud gamemode gamescope xdg-desktop-portal fontconfig
```

## Install

From a clone, build and install the package:

```
git clone https://github.com/gth27/repackd
cd repackd
makepkg -si
```

Or build the binary only. It ends up at `target/release/repackd`:

```
cargo build --release
```

## Quick start

1. Open repackd and press "Add game". Pick a repack folder (or a game that is already unpacked).
2. Let it verify the files against the MD5 list (on by default in Settings).
3. Press Install. The game gets its own Wine prefix with GE-Proton.
4. Press Play. Per-game options are under "Game settings".

Optional: in Settings, set a watch folder, a SteamGridDB key and emulator paths.

## Where data lives

- Library: `$XDG_DATA_HOME/repackd/library.json`
- Settings: `$XDG_DATA_HOME/repackd/settings.json`
- Games (prefixes, launch scripts, artwork, shader cache, logs): `~/Games/repackd`
- App-menu entries: `$XDG_DATA_HOME/applications`

`XDG_DATA_HOME` defaults to `~/.local/share`.

## Safety

- Installers and games run as your user under Wine/Proton. Wine is not a sandbox: the `Z:` drive maps to `/`.
- Only install repacks from sources you trust.
- The MD5 check catches corrupt downloads, not malicious ones.

## Troubleshooting

- Open the game's settings and press "Copy diagnostics". It copies versions, hardware, the game's settings and its last run as text for a bug report.
- Under Launch in the game's settings, "Last run" shows the output of the last start. If the install failed, the log is also in Settings.

## License

MIT. See [LICENSE](LICENSE).
