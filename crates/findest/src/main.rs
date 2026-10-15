mod browser;
mod command;
mod icons;
mod install;
mod native_drag;
mod render;
mod system;
mod text_input;
mod theme;
mod watcher;

use std::path::PathBuf;

use futures::StreamExt;
use gpui::{
    App, Application, Bounds, KeyBinding, Menu, MenuItem, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowHandle, WindowOptions, prelude::*, px, size,
};

use crate::browser::{Browser, BrowserInit, Quitting};
use findest_core::config::Config;
use findest_core::session::Session;
use crate::command::{Quit, Run};

/// Keys that work regardless of config (re-applied whenever the keymap reloads).
pub fn bind_global_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
}

fn menus() -> Vec<Menu> {
    let item = |label: &str, command: &str| MenuItem::action(label.to_string(), Run::new(command));
    vec![
        Menu {
            name: "Findest".into(),
            items: vec![
                item("Settings…", "open_config"),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit Findest", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                item("New Window", "new_window"),
                item("New Folder", "new_folder"),
                item("New File", "new_file"),
                MenuItem::separator(),
                item("Open", "open"),
                item("Rename", "rename"),
                item("Duplicate", "duplicate"),
                MenuItem::separator(),
                item("Add to Favorites", "add_favorite"),
                item("Reveal in Finder", "reveal_in_finder"),
                MenuItem::separator(),
                item("Move to Trash", "trash"),
                item("Delete Permanently…", "delete_permanently"),
                item("Empty Trash…", "empty_trash"),
                MenuItem::separator(),
                item("Close Window", "close_window"),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                item("Copy", "copy"),
                item("Paste", "paste"),
                item("Move Here", "move_here"),
                item("Copy Path", "copy_path"),
                MenuItem::separator(),
                item("Select All", "select_all"),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                item("Show/Hide Info Panel", "toggle_info"),
                item("Show/Hide Hidden Files", "toggle_hidden"),
                item("Refresh", "refresh"),
            ],
        },
        Menu {
            name: "Go".into(),
            items: vec![
                item("Back", "go_back"),
                item("Forward", "go_forward"),
                item("Enclosing Folder", "go_up"),
                MenuItem::separator(),
                item("Recents", "show_recents"),
                item("Home", "go_home"),
                item("Go to Folder…", "go_to_folder"),
            ],
        },
    ]
}

/// Opens a browser window, cascaded from any already open.
pub fn open_browser_window(init: BrowserInit, cx: &mut App) -> Option<WindowHandle<Browser>> {
    let offset = px(28. * cx.windows().len() as f32);
    let mut bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
    bounds.origin.x += offset;
    bounds.origin.y += offset;
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Findest".into()), ..Default::default() }),
                window_min_size: Some(size(px(600.), px(360.))),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Browser::new(init, window, cx)),
        )
        .ok()?;
    window
        .update(cx, |browser, window, cx| {
            window.focus(&browser.focus);
            cx.activate(true);
        })
        .ok();
    Some(window)
}

/// Windows to open at launch: an explicit path, else the last session's windows.
fn initial_windows(start: Option<PathBuf>) -> Vec<BrowserInit> {
    let session = Session::load(&Session::path());
    if start.is_some() {
        return vec![BrowserInit { start, info_panel: session.info_panel, ..Default::default() }];
    }
    let restore = Config::load(&Config::path()).map(|c| c.general.restore_session).unwrap_or(true);
    let mut inits: Vec<BrowserInit> = if restore {
        session
            .windows
            .into_iter()
            .map(|w| BrowserInit { start: w.folder, recents: w.recents, expanded: w.expanded, info_panel: session.info_panel })
            .collect()
    } else {
        Vec::new()
    };
    if inits.is_empty() {
        inits.push(BrowserInit { info_panel: session.info_panel, ..Default::default() });
    }
    inits
}

fn main() {
    if install::handoff_if_needed() {
        return;
    }
    // `findest <path>` when running in place (debug builds / FINDEST_NO_BUNDLE).
    let start = std::env::args_os().skip(1).map(PathBuf::from).find(|p| !p.to_string_lossy().starts_with('-'));
    let start = start.and_then(|p| std::path::absolute(p).ok());

    // Folders opened via `findest <path>`, Finder's "Open With" or dropping on the Dock icon.
    let (url_tx, mut url_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    let app = Application::new();
    app.on_open_urls(move |urls| {
        let _ = url_tx.unbounded_send(urls);
    });
    app.run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| {
            cx.set_global(Quitting(true));
            cx.quit();
        });
        bind_global_keys(cx);
        text_input::bind_keys(cx);
        cx.set_menus(menus());

        for init in initial_windows(start) {
            open_browser_window(init, cx);
        }

        cx.spawn(async move |cx| {
            while let Some(urls) = url_rx.next().await {
                let Some(path) = urls.iter().rev().find_map(|u| findest_core::paths::path_from_file_url(u)) else {
                    continue;
                };
                // Show it in the frontmost window, or a new one if none is open.
                cx.update(|cx| {
                    let front = cx
                        .active_window()
                        .and_then(|w| w.downcast::<Browser>())
                        .or_else(|| cx.windows().iter().rev().find_map(|w| w.downcast::<Browser>()));
                    match front {
                        Some(window) => {
                            window
                                .update(cx, |browser, window, cx| {
                                    browser.navigate_to_path(path, cx);
                                    window.activate_window();
                                })
                                .ok();
                        }
                        None => {
                            open_browser_window(BrowserInit::at(Some(path)), cx);
                        }
                    }
                })
                .ok();
            }
        })
        .detach();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
