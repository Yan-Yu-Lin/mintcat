# MINTCAT
![Logo](https://raw.githubusercontent.com/iriscats/mintcat/refs/heads/main/public/icon.ico)

# Introduction
MintCat is a Deep Rock Galactic mod loader and integration tool. It is built with Rust and Tauri, and the frontend is built with React and Typescript.


# Features
## 1. Mod Management:
- Add mods from mod.io
- Add mods from local files
- Update mods
- Mod rename

2. mod.io Search, Download, Update


# Architecture
1. Frontend: React + Typescript + Vite + AntDesign
2. Backend: Rust + Tauri 


## Tauri 2.0
https://v2.tauri.app/start/


## AntDesign
https://ant-design.antgroup.com/index-cn


## Modio RESTful API
https://docs.mod.io/restapiref/#getting-started


# Build & Dev

```
pnpm install 
pnpm tauri dev
```

# Publish

```shell
./release.sh
```


# App Path

## Config Path
```
Windows: 
C:\Users\Alice\AppData\Roaming\com.mint.cat

macOS:
~/Library/Application Support/com.mint.cat
```

## Log Path
```
Windows: 
C:\Users\Alice\AppData\Local\com.mint.cat\logs

macOS:
~/Library/Logs/com.mint.cat/mintcat.log
```

## Cache Path
```
Windows: 
C:\Users\Alice\AppData\Local\com.mint.cat\

macOS:
~/Library/Caches/com.mint.cat
```


# Headless CLI (`mintcat-cli`, Linux)

`src-tauri/crates/mintcat-cli` is a command-line front end for scripts and AI agents. It uses
MintCat's own SQLite DB (`~/.config/com.mint.cat/mintcat.sqlite`) and dlopens the **same
integrator runtime** the GUI uses (`~/.local/share/com.mint.cat/plugins/versions/<ver>/libmintcat_integrator.so`),
feeding it the same inputs as `src/tasks/ModInstallTask.ts`. Profiles, enabled states and
install hashes stay shared with the GUI.

```bash
cd src-tauri
cargo build --release -p mintcat-cli          # -> target/release/mintcat-cli
# Oodle: liboo2corelinux64.so.9 must sit beside the binary (same rule as the GUI; auto-downloads)
```

| Command | What it does |
|---|---|
| `games` | games, pak paths, active game |
| `set-game-path <drg\|rc> <pak\|auto>` | set `FSD-WindowsNoEditor.pak` / `RogueCore-Windows.pak` path (`auto` = Steam lookup) |
| `use-game <drg\|rc>` | switch the active game (GUI game selector) |
| `profiles`, `use-profile <name>` | list / activate profiles of a game |
| `list [--enabled]` | mods of the active profile: id, enabled, folder, source, missing files |
| `enable <sel>...` / `disable <sel>...` | selector = profile-mod id, `m<mod_id>`, exact name, or unique substring |
| `add <file.pak\|file.zip\|folder> [--folder NAME [--create-folder]] [--disabled]` | add a local mod. A folder with `js/main.js`, `pak/`, `dll/` or `Content/` counts as a mod |
| `install [--dry-run] [--force] [--offline] [--target PAK]` | the GUI's Save: refresh/download online mods, ensure UE4SSL/DRG/RC zips, uninstall, integrate |
| `update [--dry-run] [--all]` | refresh mod.io metadata and download new versions (uses the mod.io login stored by the GUI) |
| `doctor` | runtime / Oodle / GUI + game processes / mod.io auth check |

Global flags: `--json` (one JSON object on stdout; progress on stderr), `-g/--game drg|rc`
(operate on a game without switching the active one), `--db PATH`, `-q`.

Exit codes: `0` ok, `1` failed, `3` refused by a safety check, `4` not found / bad selector.

Safety behaviour:
* Every command that writes the DB first makes a `mintcat.sqlite.cli-<action>-<time>.bak` beside it.
* DB writes are refused while the MintCat GUI is running (it caches state and would overwrite changes).
  Close the GUI, or pass `--allow-gui-running`.
* `install` refuses when the target game is running, or when foreign `.pak` files are present
  (`--allow-foreign-paks`), or when an old `mods_P.pak` exists (`--allow-old-mint`). These are the GUI's confirm dialogs.
* The GUI's install always deletes `Binaries/Win64/ue4ss/` (`uninstall_ue4ss`), and with it any
  hand-placed UE4SS mods. The CLI snapshots `ue4ss/` to `~/.local/share/com.mint.cat/cli-backups/`
  (keeps the 5 newest per game) and afterwards restores:
  * folders under `ue4ss/mods/` that no MintCat mod owns, in full;
  * config and log files of managed mods;
  * extra files such as `*.bak` and `UE4SS-settings.ini` customisations (with MintCat's required keys re-applied).

  `--no-preserve` gives the exact GUI behaviour.
* `install --target <copy>/Content/Paks/<game pak>` installs into a copy of the game and never writes the DB.
* The manifest hash is written with a port of the GUI's non-standard `md5()`, so GUI and CLI agree
  on when nothing changed and no reinstall is needed.

Not supported: ModCat (modcat.top) sources, adding mods from mod.io URLs, and mod.io login. If the stored
token expires, `update` fails with `mod.io Unauthorized`; log in once in the GUI to fix it.

```bash
mintcat-cli --json list --enabled
mintcat-cli -g rc set-game-path rc "/mnt/data/SteamLibrary/steamapps/common/Deep Rock Galactic RogueCore/RogueCore/Content/Paks/RogueCore-Windows.pak"
mintcat-cli -g rc add ~/mods/RCEssentials --folder local --create-folder
mintcat-cli -g rc install --dry-run --json
mintcat-cli -g rc install
mintcat-cli update && mintcat-cli install
```
