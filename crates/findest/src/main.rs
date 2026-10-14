mod browser;
mod command;
mod icons;
mod render;
mod text_input;
mod theme;
mod watcher;

use gpui::{
    App, Application, Bounds, KeyBinding, Menu, MenuItem, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowOptions, prelude::*, px, size,
};

use crate::browser::Browser;
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
            items: vec![item("Show/Hide Hidden Files", "toggle_hidden"), item("Refresh", "refresh")],
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

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        bind_global_keys(cx);
        text_input::bind_keys(cx);
        cx.set_menus(menus());

        let bounds = Bounds::centered(None, size(px(1100.), px(700.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions { title: Some("Findest".into()), ..Default::default() }),
                    window_min_size: Some(size(px(600.), px(360.))),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Browser::new(window, cx)),
            )
            .expect("failed to open window");
        window
            .update(cx, |browser, window, cx| {
                window.focus(&browser.focus);
                cx.activate(true);
            })
            .ok();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}
