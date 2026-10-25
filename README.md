# Findest

A fast, keyboard-friendly file browser for macOS, written in Rust on [GPUI](https://gpui.rs) (Zed's GPU-accelerated UI framework).

## Features

- **List or tiles:** a tree list that expands folders in place, with resizable columns, or a tiles grid with image thumbnails and adjustable size (⌘1 / ⌘2, ⌘= / ⌘-).
- **Quick Look:** Space previews the selection in the macOS Quick Look panel (images, PDFs, text, video and anything else macOS can preview). JSON, NDJSON, YAML, TOML and other config files open in Findest's own syntax-highlighted viewer, and Markdown shows formatted there (⌘⇧M or the Source button for the source). Select text with the mouse (double-click a word, triple-click a line, ⌘A) and copy it with ⌘C. ⌘F finds text in the file (⌘G / ⌘⇧G or Enter / ↑↓ for the next and previous match). Nested blocks and Markdown sections fold: hover the line numbers and click a chevron. ↑/↓ move through files while it's open.
- **Info panel (⌘I):** a preview plus size (folder totals computed in the background), dates, dimensions, location and permissions.
- **Fast search (⌘F):** an in-memory index of file names with a query language (wildcards, `ext:`, `size:`, `dm:`, `content:`, regex…), or **Live** search straight from the disk. See [Search](#search-f).
- **Type to jump:** start typing a name to select it; repeat a letter to cycle through matches.
- **Copy, cut and paste** with progress, speed and time left. Copies can be paused, cancelled, and retried where they stopped. Name clashes ask first (Replace, Keep Both, Skip). Paste goes into the selected folder, or next to the selected file.
- **Undo (⌘Z)** for the last paste, drop, duplicate, compress or uncompress.
- **Drag and drop** between Findest windows, Finder and other apps, with auto-scroll near the list's edges.
- **Compress and uncompress** zip files; opening a .zip unpacks it next to it, in Findest.
- **Folder colors:** right-click folders to pick a color. They're Finder's color tags, so Finder and Spotlight see them too.
- **Sidebar:** Recents, Favorites, iCloud Drive and other cloud drives, Macintosh HD, external volumes and the Trash.
- **Toolbar and path bar:** back/forward with history, the current folder's name (click for the folders above), a Go to bar that accepts pasted paths and completes them with Tab, a ⋯ menu of folder actions, and a clickable path bar at the bottom.
- **Multiple windows (⌘N)**, each restored at launch with its folder, expanded folders, view and column widths.
- **New folders and files start unnamed:** type a name, or press Esc to remove the item.
- **Open in…:** VS Code, Terminal or any app you configure, from the menu or a shortcut.
- **Default file viewer:** "Show in Finder" in other apps can open Findest instead (see [Install](#install)).
- **Sources:** browse things that aren't on disk (cloud resources, buckets, wikis) as folders, through a small program you plug in, e.g. a Kubernetes cluster. See [Sources](docs/sources.md).
- **Configurable:** theme, colors, fonts, favorites and every shortcut, in a commented TOML file that applies as soon as you save it.

## Install

With Homebrew. This builds from source on your Mac, so there are no Gatekeeper warnings and no Apple developer account is needed:

```sh
brew install ariel-bentu/findest/findest
findest            # first run installs Findest.app and opens it
```

Update with `brew upgrade findest`, then run `findest` once to refresh the app.

From a clone:

```sh
cargo install --path crates/findest
findest            # installs/updates Findest.app, then opens it
findest ~/code     # open a folder (like `code .`)
```

Run from a terminal, a release build of `findest` installs or updates `Findest.app`, in `/Applications`, or in `~/Applications` if `/Applications` isn't writable. It then hands off to the app. Findest needs to run as its own app: macOS gives privacy permissions (Full Disk Access, OneDrive and iCloud folders) to the app that launched it, so a binary run from a terminal gets the terminal's permissions. The bundle is signed with your "Apple Development" certificate if you have one, so permissions survive updates. To choose a different certificate, set `FINDEST_SIGN_IDENTITY`.

Findest also appears in Finder's "Open With" menu for folders.

To have "Show in Finder" / "Reveal in Finder" in other apps (browser downloads, editors) open Findest instead, choose **Findest › Use Findest as Default File Viewer**; **Use Finder as Default File Viewer** switches back. macOS doesn't let apps replace Finder for folders themselves, the Desktop, or the Dock. If you delete Findest without switching back, reveals fall back to Finder; `defaults delete -g NSFileViewer` removes the leftover setting.

## Command line and AI agents

`findest search` searches the same index from a terminal, so scripts and AI agents can find files anywhere in well under a second:

```sh
findest search 'ext:pdf invoice dm:2024' --in ~/Documents --limit 20
findest search 'Cargo.toml' --json        # one JSON object per line
findest search --help                     # options and the query language
```

It uses the index the app saves (once it has caught up after launch, then at most hourly). An index more than an hour old is first caught up with the changes since, from the FSEvents history; `--rescan` rebuilds it instead, and `--live` skips it. Folders outside the index are searched on disk.

`findest skill install` installs a [Claude Code skill](skills/findest-search/SKILL.md) that teaches agents to use it (into `~/.claude/skills`, or a folder you name). `findest skill show` prints it, e.g. for an `AGENTS.md`.

Search results include names from folders only Findest has access to (e.g. with Full Disk Access), so any terminal or agent running `findest search` can see them. Reading file contents (`content:`) still uses the terminal's own permissions.

## Develop

```
crates/
  findest-core/   UI-free library: listing, sorting, file ops, search, config, recents, launchers (unit tested)
  findest/        The GPUI app: window, sidebar, file list, menus, keybindings
```

```sh
cargo run -p findest [-- <path>]   # debug builds always run in place
cargo test --workspace
./scripts/make_icon.sh             # regenerate the app icon (needs Xcode 26+)
```

Set `FINDEST_NO_BUNDLE=1` to run a release build in place.

GPUI is built with `runtime_shaders`, so you don't need Xcode's separately downloaded Metal toolchain. To precompile shaders instead, run `xcodebuild -downloadComponent MetalToolchain` and remove that feature.

## Configuration

On first launch Findest writes a commented config to `~/.config/findest/config.toml` (override it with `$FINDEST_CONFIG`).
Changes apply as soon as you save the file. Open it with **⌘,**.

- `[general]`: start folder, session restore, info panel, default view and tile size, hidden files, sort order and folders first (also View ▸ Folders First On/Off or the right-click menu), Trash confirmation, recents size
- `[appearance]`: `dark` / `light` / `system` theme, font, row height, per-color overrides
- `[[favorites]]`: sidebar folders (you can also add them from the app with ⌃⌘T)
- `[[openers]]`: "Open in …" targets, e.g. VS Code and Terminal, with `{path}` / `{dir}` placeholders
- `[[sources]]`: hierarchies listed by a program you provide (an adapter), shown in the sidebar. See [docs/sources.md](docs/sources.md)
- `[search]`: indexing on/off, which folders are indexed and excluded, result limit, content-search size limit
- `[keybindings]`: any shortcut → command, layered on the defaults (`"none"` disables one)

## Default shortcuts

| Keys | Command | Keys | Command |
|---|---|---|---|
| ↑ ↓ | move selection | ⇧↑ ⇧↓ | extend selection |
| ⌥↑ / ⌥↓, Home / End | first / last item | ⌘A / Esc | select all / clear selection |
| → / ← | expand / collapse folder | Space, ⌘Y | Quick Look |
| ⌘O, ⌘↓, double-click | open | ↩ | rename |
| ⌘↑ | enclosing folder | ⌘[ / ⌘] | back / forward |
| ⇧⌘H | home | ⇧⌘G | go to folder |
| ⇧⌘N / ⌥⌘N | new folder / new file | ⌘D | duplicate |
| ⌘C / ⌘X / ⌘V | copy / cut / paste | ⌥⌘V | move here |
| ⌘Z | undo | ⌥⌘C | copy path |
| ⌘⌫ | move to Trash | ⌫ / ⇧⌫ | move to Trash (asks / no prompt) |
| ⌥⌘⌫ | delete permanently | ⇧⌘⌫ | empty Trash |
| ⌘F | search | ⌘I | info panel |
| ⌘1 / ⌘2 | list / tiles | ⌘= / ⌘- | bigger / smaller tiles |
| ⇧⌘. | toggle hidden files | ⌘R | refresh |
| ⌃⌘T | add to Favorites | ⇧⌘R | Recents |
| ⌘E | open in VS Code | ⌥⌘T | open in Terminal |
| ⌥⌘R | reveal in Finder | ⌘, | open config |
| ⌘N / ⌘W | new / close window | | |

In tiles, the arrow keys move through the grid in both directions. In Quick Look, Esc or clicking elsewhere closes the preview. Typing letters jumps to the first matching name.

## Search (⌘F)

Findest keeps an in-memory index of file names, a design inspired by FSearch and Everything: flat name-sorted arrays searched on all cores. It indexes about 1.2M files in ~7 s, and most queries finish in a few milliseconds. The index is saved to disk, so search works right away at launch, and macOS's file-change history (FSEvents) catches it up on whatever changed while Findest wasn't running, without a rescan. After that, file changes are applied as they happen by re-reading only the folders involved, and a full rescan runs once a day at background priority, so indexing stays out of the way of everything else. While a scan runs the status bar shows its progress; click it to pause indexing (for an hour, or until you resume) or turn it off for good (`indexing = false` under `[search]`). You choose what's indexed in `[search]` in the config.

| Query | Matches |
|---|---|
| `report` | name contains "report" (a capital letter makes it case-sensitive) |
| `*.pdf`, `img_??.jpg` | wildcards |
| `"two words"` | literal text |
| `a b`, `a \| b`, `!a` or `-a`, `( )` | AND, OR, NOT, grouping |
| `ext:jpg;png` | extension |
| `size:>10mb`, `size:1k..5mb`, `size:empty` | size |
| `dm:today`, `dm:week`, `dm:2024-05`, `dm:>2024-05-01` | date modified |
| `file:`, `folder:` | only files / folders |
| `path:src`, `src/main` | full path |
| `regex:^\d{4}-` | regular expression |
| `case:`, `nocase:` | force case sensitivity |
| `content:"TODO"` | text inside files (pair it with other terms to stay fast) |
| `status:Ready`, `type:TenantData` | in a source: an attribute it shows as a column |
| `hidden:` | include hidden files |

Search **This folder** or **Everywhere**. Press ↓ or Enter to move to the results. Esc or the ✕ in the box clears the query; Esc again closes search. Run `cargo run --release -p findest-core --example search_bench` to benchmark indexing and search on your machine.

**Live** searches the disk directly instead of the index, with the same query language: it sees every change at once and works anywhere (other drives, network shares, folders the index leaves out), at about the speed of [fd](https://github.com/sharkdp/fd) — fine for a project folder, seconds for a whole home folder. Findest switches to it by itself when "This folder" isn't indexed or there's no index (indexing is off or still building). Compare it with fd on your machine: `cargo run --release -p findest-core --example search_bench -- live ~/dev main.rs`.

## Mouse

- **▾** next to back/forward: recently visited folders.
- Click the folder name in the toolbar for the folders above it; right-click it, or any part of the path bar, for that folder's menu.
- Drag items onto a folder, a sidebar entry, or a path segment to move them. Hold ⌥ to copy. Drops across volumes copy, like Finder. You can also drop files in from Finder, or drag items out to other apps. Esc cancels a drag.
- Drag a column's edge in the list header to resize it; double-click the edge to reset it.
- Right-click items, empty space, or sidebar entries for context menus (Compress, folder colors, paste into a folder, Open in…, Add to Favorites and more).
- Click the indexing status in the status bar to pause, rebuild, or turn off indexing.
