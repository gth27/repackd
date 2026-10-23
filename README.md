# repackd

Install and play game repacks on Linux with Proton.

> **Note**
> repackd is in early development. Expect bugs and changing behaviour. Back up your saves. To report an issue, include the output of "Copy diagnostics" (in a game's settings).

repackd is a desktop app for Linux, written in Rust with egui. It installs game repacks you already have (setup.exe-style installers such as the FitGirl format) into a separate Wine prefix per game, using umu-launcher and GE-Proton, and then launches them. Console games run through emulators you set up yourself.

Tested on Arch-based systems; builds and passes its tests on Debian 13 and Fedora 42. Other distributions should work but are untested.

repackd does not download games, torrents or updates. It only works with files you already have on your disk. Use it only with games you are entitled to use.

## Requirements

- `umu-launcher` (required, runs the games)
- `curl`
- A Vulkan-capable GPU driver
- `xdg-desktop-portal` (the folder and file pickers) and `xdg-utils` (opening folders); desktop environments ship both
- Rust 1.85 or newer (edition 2024), only to build. Distro-packaged Rust on Debian/Ubuntu is usually too old, so use rustup.
- Optional: `winetricks`, `7zip`, `libnotify`, `mangohud`, `gamemode`, `gamescope`

When a tool is missing, repackd detects your distribution and shows the matching install command.

## Install

### Arch-based

```
sudo pacman -S base-devel rust umu-launcher
sudo pacman -S --asdeps winetricks 7zip libnotify mangohud gamemode gamescope
git clone https://github.com/gth27/repackd
cd repackd
makepkg -si
```

### Debian / Ubuntu

Also applies to Mint. Get `umu-launcher` from the .deb on the [releases page](https://github.com/Open-Wine-Components/umu-launcher/releases).

```
sudo apt install build-essential pkg-config libudev-dev curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# install the umu-launcher .deb from the releases page, then optionally:
sudo apt install winetricks 7zip libnotify-bin mangohud gamemode gamescope
git clone https://github.com/gth27/repackd
cd repackd
cargo build --release
mkdir -p ~/.local/bin ~/.local/share/applications
cp target/release/repackd ~/.local/bin/
cp repackd.desktop ~/.local/share/applications/
```

### Fedora

```
sudo dnf install gcc pkgconf-pkg-config systemd-devel curl umu-launcher
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
sudo dnf install winetricks 7zip libnotify mangohud gamemode gamescope
git clone https://github.com/gth27/repackd
cd repackd
cargo build --release
mkdir -p ~/.local/bin ~/.local/share/applications
cp target/release/repackd ~/.local/bin/
cp repackd.desktop ~/.local/share/applications/
```

### Other distributions

openSUSE build dependencies (untested): `gcc pkg-config systemd-devel`. Install `umu-launcher` from your package manager or the [releases page](https://github.com/Open-Wine-Components/umu-launcher/releases).

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
git clone https://github.com/gth27/repackd
cd repackd
cargo build --release
mkdir -p ~/.local/bin ~/.local/share/applications
cp target/release/repackd ~/.local/bin/
cp repackd.desktop ~/.local/share/applications/
```

The `repackd.desktop` copy is optional. Make sure `~/.local/bin` is in your `PATH`.

## Usage

### Add a game
1. Press "Add game". Pick a repack folder (it gets installed) or a game that is already unpacked (it gets imported).
2. To add many at once, press "Import a folder…". It finds every repack and unpacked game inside.

### Install
1. Press "INSTALL" on the game's page. It shows the disk space it needs.
2. With "Check the files before and after installing (FitGirl's checksums)" on (default), the files are verified first. Bad files are reported.
3. By default the installer runs without its own windows. Turn off "Install games without the installer's own windows" in Settings to click through them yourself. Update installers always show their windows.
4. If an install fails, press "INSTALL AGAIN". The log is in Settings.
5. Already-unpacked games show "SET UP", which only creates the Wine prefix.

### Play
Press "PLAY". The game page shows playtime and a ProtonDB rating. If a known problem is detected, a fix button may appear (for example a winetricks verb or another Proton).

### Settings per game
Press "Game settings" on the game page.
- Proton: pick which version this game uses. Download more under Settings, "Proton versions".
- Launch options, Environment and Switches (MangoHud, GameMode, Gamescope, FSR, DXVK FPS counter, dedicated GPU). Only tools you have installed are listed.
- "Run .exe in this prefix…" for mod installers and trainers, and "Run winetricks".
- "Show in the app menu" adds a menu entry.
- "Check files", "Clear shader cache", "Copy diagnostics".

### Updates
1. Put the update folders (FitGirl) or archives (scene .rar/.7z/.zip) next to the game or in Downloads.
2. A game with an update shows "Update available" and a "Review" button. You can also press "Check for updates", "Add update folders…" or "Add update archives…" in Game settings, Updates.
3. Check the order, then press "Apply". Updates run oldest first. The installer window appears and waits for your click.
HDiffPatch updates (PATCH.exe + Patch.bin) are applied natively; repackd downloads the official hpatchz once if it isn't installed.
4. With "Back up the files an update changes, so it can be undone" on (default), "Undo update to v…" puts the files back.

### Steam
1. Close Steam. It overwrites its shortcut list on exit, so repackd refuses while it runs.
2. Press "Add to Steam" on the game page. Steam then starts the game through its launch script.
3. For artwork, add a free SteamGridDB key in Settings and press "Save key". Without a key there is no art.
4. "Remove from Steam" undoes it.

### Saves
After you play for a minute or more, saves are backed up to a `.repackd-saves` folder inside your games folder (older installs: `~/Games/repackd-saves`). The last 5 are kept. Press "Back up saves" in Game settings for a manual copy. Restore asks "Yes, restore". Deleting a game keeps its backups.

### Emulators
Switch and PS4 games need an emulator you provide. Set its path in Settings ("Switch emulator", "PS4 emulator" with "Browse…"). Install the firmware and keys inside the emulator yourself. repackd only checks that the files exist. If the game says "SET UP EMULATOR", pick one in Game settings.

### Move or clean up
- Settings, "Disk usage" shows game, prefix and repack sizes per game.
- "Delete repack" frees the repack's space. The installed game keeps working, but you can no longer reinstall or check its files.
- "Move to another folder…" in Game settings moves a game's repackd folder to another disk.

### Watch folder
In Settings, "Watch folder", press "Choose…" and pick your download folder. A finished repack there is offered with "Add & install", "Add" or "Dismiss". "Off" turns it off.

### Controller
A gamepad works for navigation: D-pad and left stick move, the south button accepts, the east button goes back, the right stick scrolls. F11 toggles fullscreen.

## How it works

- Every game gets its own folder, `~/Games/repackd/<name>` (change the games folder in Settings, "Games folder"), with a Wine prefix in `pfx/`.
- Installers and games run through `umu-run` with GE-Proton, or the Proton you picked.
- A `launch.sh` script is written per game. Steam shortcuts and app-menu entries run that script.
- The MD5 check uses the repack's own checksum list.
- Updates: scene archives are unpacked, the installer runs, and crack folders are copied in. A backup snapshot is taken first so you can undo.
- Playtime is counted by watching the game's processes.

## Where data lives

- Library: `$XDG_DATA_HOME/repackd/library.json`
- Settings: `$XDG_DATA_HOME/repackd/settings.json`
- Games (prefixes, launch scripts, artwork, shader cache, logs): `~/Games/repackd` by default; set another in Settings, "Games folder". Existing games stay put; use "Move to another folder…" to move one.
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
