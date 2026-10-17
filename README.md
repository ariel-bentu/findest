# Findest

A fast, keyboard-friendly file browser for macOS, written in Rust on [GPUI](https://gpui.rs) (Zed's GPU-accelerated UI framework).

## Layout

```
crates/
  findest-core/   UI-free library: listing, sorting, file ops, config, recents, launchers (unit tested)
  findest/        The GPUI app: window, sidebar, file list, menus, keybindings
```

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

## Develop

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

- `[general]`: start folder, hidden files, sort order, Trash confirmation, recents size
- `[appearance]`: `dark` / `light` / `system` theme, font, row height, per-color overrides
- `[[favorites]]`: sidebar folders (you can also add them from the app with ⌃⌘T)
- `[[openers]]`: "Open in …" targets, e.g. VS Code and Terminal, with `{path}` / `{dir}` placeholders
- `[keybindings]`: any shortcut → command, layered on the defaults (`"none"` disables one)

## Default shortcuts

| Keys | Command | Keys | Command |
|---|---|---|---|
| ↑ ↓ / ⇧↑ ⇧↓ | move / extend selection | ⌘O, ⌘↓, double-click | open |
| ↩ | rename | ⌘↑ | enclosing folder |
| ⌘[ / ⌘] | back / forward | ⇧⌘G | go to folder |
| ⇧⌘N / ⌥⌘N | new folder / new file | ⌘D | duplicate |
| ⌫ | move to Trash (asks) | ⇧⌫ | move to Trash (no prompt) |
| ⌘⌫ | move to Trash | ⌥⌘⌫ | delete permanently |
| ⌘C / ⌘V / ⌥⌘V | copy / paste / move here | ⌥⌘C | copy path |
| ⌘E | open in VS Code | ⌥⌘T | open in Terminal |
| ⇧⌘. | toggle hidden files | ⌘R | refresh |
| ⌃⌘T | add to Favorites | ⇧⌘R | Recents |
| ⌥⌘R | reveal in Finder | ⌘, | open config |

Typing letters jumps to the first matching name.

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
| `hidden:` | include hidden files |

Search **This folder** or **Everywhere**. Press ↓ or Enter to move to the results and Esc to close search. Run `cargo run --release -p findest-core --example search_bench` to benchmark indexing and search on your machine.

## Mouse

- **▾** next to back/forward: recently visited folders.
- Drag items onto a folder, a sidebar entry, or a path segment to move them. Hold ⌥ to copy. Drops across volumes copy, like Finder. You can also drop files in from Finder.
- Right-click items, empty space, or sidebar entries for context menus.
