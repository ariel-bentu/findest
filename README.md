# Findest

A fast, keyboard-friendly file browser for macOS, written in Rust on [GPUI](https://gpui.rs) (Zed's GPU-accelerated UI framework).

## Layout

```
crates/
  findest-core/   UI-free library: listing, sorting, file ops, config, recents, launchers (unit tested)
  findest/        The GPUI app: window, sidebar, file list, menus, keybindings
```

## Install

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
./scripts/make_icon.sh             # regenerate assets/AppIcon.icns
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

## Mouse

- **▾** next to back/forward: recently visited folders.
- Drag items onto a folder, a sidebar entry, or a path segment to move them. Hold ⌥ to copy. Drops across volumes copy, like Finder. You can also drop files in from Finder.
- Right-click items, empty space, or sidebar entries for context menus.
