//! Rendering for [`Browser`]: toolbar, sidebar, file list, status bar and menus.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use findest_core::listing::{Entry, SortKey, format_size};
use findest_core::locations;
use findest_core::paths::contract_tilde;
use findest_core::config::ViewMode;
use gpui::{
    AnyElement, Bounds, Context, DispatchPhase, ExternalPaths, FontWeight, Hsla, Image, MouseButton, MouseDownEvent,
    CursorStyle, ListHorizontalSizingBehavior, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Point, SharedString, Window, anchored, canvas, deferred, div, img,
    prelude::*, px, uniform_list,
};

use crate::browser::{
    Browser, Column, EditKind, GoToTarget, Location, MAX_TILE, MIN_TILE, MenuAction, MenuEntry, count_label, go_to_target,
};
use crate::command::BROWSER_CONTEXT;

const SIDEBAR_WIDTH: f32 = 200.0;
const INFO_PANEL_WIDTH: f32 = 270.0;
/// Images up to this size are previewed in the info panel.
const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;
const TOP_BAR_HEIGHT: f32 = 44.0;
/// Where the Name column starts in a row: list padding, row padding,
/// disclosure triangle and icon (the header starts its Name label there too).
const NAME_START: f32 = 4.0 + 8.0 + TREE_GUTTER + 22.0;
/// Grab width of a column edge in the header.
const COLUMN_HANDLE: f32 = 8.0;
/// Width of the disclosure-triangle column, and the indent per tree level.
const TREE_GUTTER: f32 = 14.0;
const TREE_INDENT: f32 = 16.0;

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_sidebar_icons(cx);
        self.sync_info(cx);
        if std::mem::take(&mut self.refocus_list) {
            window.focus(&self.focus);
        }
        if std::mem::take(&mut self.pending_name_new) {
            self.start_name_new(window, cx);
        }
        if self.view == ViewMode::Tiles {
            // The list's width is what the sidebar and info panel leave (both fixed).
            let mut width = window.viewport_size().width - px(SIDEBAR_WIDTH + 1. + 8.);
            if self.config.general.info_panel {
                width -= px(INFO_PANEL_WIDTH + 1.);
            }
            self.tile_columns = ((width / px(self.tile_cell_width())).floor() as usize).max(1);
        }
        let title = format!("Findest — {}", self.location_label());
        if title != self.last_title {
            window.set_window_title(&title);
            self.last_title = title;
        }

        let theme = &self.theme;
        let mut root = div()
            .id("browser")
            .key_context(BROWSER_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_run))
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .flex_row()
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(theme.font_size))
            // The sidebar runs full height; toolbar and status bar belong to the file pane.
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(self.render_toolbar(cx))
                    .children(self.render_go_to_folder(cx))
                    .children(self.render_search_bar(cx))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_1()
                            .min_h_0()
                            .child(self.render_file_list(cx))
                            .children(self.render_info_panel(cx)),
                    )
                    .child(self.render_path_bar(cx))
                    .child(self.render_status_bar(cx)),
            )
            .children(self.render_menu(cx))
            .children(self.render_syntax_help(cx));
        if let Some(family) = &theme.font_family {
            root = root.font_family(SharedString::from(family.clone()));
        }
        root
    }
}

impl Browser {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let hidden_label = if self.config.general.show_hidden { "Hidden: shown" } else { "Hidden: off" };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(TOP_BAR_HEIGHT))
            .px_2()
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .child(self.nav_button(true, cx))
            .child(self.nav_button(false, cx))
            .child(self.toolbar_button("up", "chevron.up", "↑", self.current_dir().is_some_and(|d| d.parent().is_some()), "go_up", cx))
            .child(div().flex_1().min_w_0().when(self.loading, |d| d.text_color(theme.text_muted).child("Loading…")))
            .child(self.render_search_field(cx))
            .child(self.syntax_help_button(cx))
            .child(self.toolbar_text_button("goto", "Go to…", "go_to_folder", cx))
            .child(self.toolbar_text_button("hidden", hidden_label, "toggle_hidden", cx))
            .child(self.view_button(ViewMode::List, cx))
            .child(self.view_button(ViewMode::Tiles, cx))
            .child(self.folder_menu_button(cx))
            .child(self.info_toggle_button(cx))
    }

    /// The current folder's path, above the status bar. Click a part to go
    /// there, right-click it for a menu; when it doesn't fit, the start is cut
    /// so the current folder stays visible.
    fn render_path_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let mut crumbs = div().flex().flex_row().flex_none().items_center().gap_1();
        match &self.location {
            Location::Dir(dir) => {
                let parts = locations::breadcrumbs(&self.home, dir);
                let count = parts.len();
                for (i, place) in parts.into_iter().enumerate() {
                    let last = i + 1 == count;
                    let label = if place.path == self.home { "~".to_string() } else { place.label };
                    if i > 0 {
                        crumbs = crumbs.child(div().text_color(theme.text_muted).child("›"));
                    }
                    let target = place.path.clone();
                    let menu_target = place.path.clone();
                    crumbs = crumbs.child(
                        div()
                            .id(SharedString::from(format!("crumb-{i}")))
                            .map(|d| self.drop_target(d, target.clone(), cx))
                            .px_1()
                            .rounded_sm()
                            .whitespace_nowrap()
                            .cursor_pointer()
                            .when(last, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .when(!last, |d| d.text_color(theme.text_muted))
                            .hover(|s| s.bg(theme.hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.navigate(Location::Dir(target.clone()), true, cx)
                            }))
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.open_path_menu(event.position, menu_target.clone(), cx);
                                }),
                            )
                            .child(label),
                    );
                }
            }
            Location::Recents => {
                crumbs = crumbs.child(div().px_1().font_weight(FontWeight::SEMIBOLD).child("Recents"));
            }
        }
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_end()
            .h(px(26.))
            .px_2()
            .overflow_hidden()
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(theme.font_size - 1.))
            // At least full width, so it's left-aligned while it fits; when it
            // doesn't, justify_end makes the overflow (and the cut) on the left.
            .child(crumbs.min_w_full())
    }

    /// The toolbar search box: a button until opened (⌘F), then a text field.
    fn render_search_field(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let icon = self.icons.peek(&self.symbol_key_colored("magnifyingglass", theme.text_muted));
        div()
            .id("search-field")
            .w(px(240.))
            .flex_none()
            .h(px(28.))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded_lg()
            .bg(theme.background)
            .border_1()
            .border_color(if self.search.is_some() { theme.accent } else { theme.border })
            .cursor_text()
            // Once open, clicks go to the text field itself (cursor placement).
            .when(self.search.is_none(), |d| {
                d.on_mouse_down(MouseButton::Left, cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_search(window, cx);
                }))
            })
            .child(match icon {
                Some(image) => img(image).size(px(13.)).object_fit(ObjectFit::Contain).into_any_element(),
                None => div().text_color(theme.text_muted).child("⌕").into_any_element(),
            })
            .child(match &self.search {
                Some(search) => div().flex_1().min_w_0().child(search.input.clone()).into_any_element(),
                None => div().flex_1().text_color(theme.text_muted).child("Search  ⌘F").into_any_element(),
            })
    }

    /// "?" next to the search box: opens the syntax popover.
    fn syntax_help_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.syntax_help.is_some();
        self.symbol_button("syntax-help", "questionmark.circle", "?", true)
            .w(px(28.))
            .when(open, |d| d.bg(self.theme.hover))
            .tooltip(tooltip("Search syntax and examples"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.menu = None;
                    this.syntax_help = match this.syntax_help {
                        Some(_) => None,
                        None => Some(Point { x: event.position.x + px(14.), y: event.position.y + px(18.) }),
                    };
                    cx.notify();
                }),
            )
    }

    /// Popover listing query examples; clicking one runs it.
    fn render_syntax_help(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let position = self.syntax_help?;
        let theme = &self.theme;
        let mut list = div()
            .id("syntax-help")
            .occlude()
            .w(px(520.))
            .p_2()
            .rounded_lg()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.syntax_help = None;
                cx.notify();
            }))
            .child(div().px_2().pb_1().font_weight(FontWeight::SEMIBOLD).child("Search syntax"))
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .text_size(px(theme.font_size - 1.))
                    .text_color(theme.text_muted)
                    .child("Click an example to try it. Combine any of these freely."),
            );
        for (i, (query, meaning)) in crate::browser_search::SYNTAX_EXAMPLES.iter().enumerate() {
            if query.is_empty() {
                list = list.child(div().my_1().h(px(1.)).bg(theme.border));
                continue;
            }
            list = list.child(
                div()
                    .id(("syntax-example", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py(px(3.))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.use_example_query(query, window, cx)))
                    .child(
                        div()
                            .w(px(170.))
                            .flex_none()
                            .font_family("Menlo")
                            .text_size(px(theme.font_size - 1.))
                            .text_color(theme.accent)
                            .child(*query),
                    )
                    .child(div().flex_1().min_w_0().text_size(px(theme.font_size - 1.)).text_color(theme.text_muted).child(*meaning)),
            );
        }
        Some(
            deferred(anchored().anchor(gpui::Corner::TopRight).position(position).snap_to_window_with_margin(px(8.)).child(list))
                .with_priority(1),
        )
    }

    /// Scope buttons, result count / progress, and index status (while searching).
    fn render_search_bar(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let search = self.search.as_ref()?;
        let theme = &self.theme;
        let scope_button = |id: &'static str, label: String, active: bool, everywhere: bool| {
            div()
                .id(id)
                .px_2()
                .py(px(2.))
                .rounded_md()
                .whitespace_nowrap()
                .cursor_pointer()
                .when(active, |d| d.bg(theme.accent).text_color(theme.selection_text))
                .when(!active, |d| d.text_color(theme.text_muted).hover(|s| s.bg(theme.hover)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_search_scope(everywhere, cx)))
                .child(label)
        };
        let (summary, is_error) = self.search_summary().unwrap_or_default();
        let index_status = cx.try_global::<crate::search_service::IndexService>().map(crate::search_service::status_text);
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .text_size(px(theme.font_size - 1.))
                .child(div().text_color(theme.text_muted).child("Search:"))
                .when(self.current_dir().is_some(), |d| {
                    d.child(scope_button("scope-here", format!("“{}”", self.scope_label()), !search.everywhere, false))
                })
                .child(scope_button("scope-all", "Everywhere".into(), search.everywhere, true))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(if is_error { theme.danger } else { theme.text_muted })
                        .child(summary),
                )
                .children(index_status.map(|status| {
                    div()
                        .id("index-status")
                        .flex_none()
                        .whitespace_nowrap()
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .tooltip(tooltip("Search index options"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, e: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.open_index_menu(Point { x: e.position.x - px(120.), y: e.position.y + px(14.) }, cx);
                            }),
                        )
                        .child(status)
                })),
        )
    }

    /// "⋯": actions on the current folder (New Folder, Paste, …), the same
    /// menu as right-clicking empty space in the list.
    fn folder_menu_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        self.symbol_button("folder-menu", "ellipsis.circle", "⋯", true)
            .tooltip(tooltip("Folder actions"))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.syntax_help = None;
                    this.commit_edit(window, cx);
                    this.clear_selection(cx);
                    window.focus(&this.focus);
                    let position = Point { x: event.position.x - px(14.), y: event.position.y + px(18.) };
                    this.open_list_menu(position, false, cx);
                }),
            )
    }

    /// List / tiles switch; the current view is highlighted.
    fn view_button(&self, view: ViewMode, cx: &mut Context<Self>) -> impl IntoElement {
        let (id, symbol, fallback, hint) = match view {
            ViewMode::List => ("view-list", "list.bullet.indent", "☰", "View as list (⌘1)"),
            ViewMode::Tiles => ("view-tiles", "square.grid.2x2", "▦", "View as tiles (⌘2)"),
        };
        self.symbol_button(id, symbol, fallback, true)
            .w(px(30.))
            .when(self.view == view, |d| d.bg(self.theme.hover))
            .tooltip(tooltip(hint))
            .on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx)))
    }

    /// Opens/closes the info panel; highlighted while it's open.
    fn info_toggle_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let open = self.config.general.info_panel;
        self.symbol_button("info-toggle", "sidebar.right", "▥", true)
            .when(open, |d| d.bg(self.theme.hover))
            .tooltip(tooltip(if open { "Hide info panel (⌘I)" } else { "Show info panel (⌘I)" }))
            .on_click(cx.listener(|this, _, window, cx| this.run(crate::command::Command::ToggleInfo, window, cx)))
    }

    /// Back/forward: click to navigate; long-press or right-click for history.
    /// Large toolbar button showing an SF Symbol (text fallback until it loads).
    fn symbol_button(&self, id: &'static str, symbol: &'static str, fallback: &'static str, enabled: bool) -> gpui::Stateful<gpui::Div> {
        let theme = &self.theme;
        let color = if enabled { theme.text } else { theme.text_muted };
        let icon = self.icons.peek(&self.symbol_key_colored(symbol, color));
        div()
            .id(id)
            .w(px(36.))
            .h(px(30.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_lg()
            .text_size(px(theme.font_size + 8.))
            .text_color(color)
            .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(theme.hover)))
            .child(match icon {
                Some(image) => img(image).w(px(18.)).h(px(18.)).object_fit(ObjectFit::Contain).into_any_element(),
                None => div().child(fallback).into_any_element(),
            })
    }

    /// Back/forward: click to navigate; long-press or right-click for history.
    fn nav_button(&self, back: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let enabled = if back { self.can_go_back() } else { self.can_go_forward() };
        let below = |p: Point<Pixels>| Point { x: p.x - px(14.), y: p.y + px(18.) };
        let (id, symbol, fallback) =
            if back { ("back", "chevron.left", "‹") } else { ("forward", "chevron.right", "›") };
        let hint = if back { "Back (⌘[) · hold for history" } else { "Forward (⌘]) · hold for history" };
        self.symbol_button(id, symbol, fallback, enabled)
            .tooltip(tooltip(hint))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.menu = None;
                    this.press_nav(back, below(event.position), cx);
                }),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.click_nav(back, cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.cancel_nav_press()))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_history_menu(back, below(event.position), cx);
                }),
            )
    }

    fn toolbar_button(&self, id: &'static str, symbol: &'static str, fallback: &'static str, enabled: bool, command: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        self.symbol_button(id, symbol, fallback, enabled).when(enabled, |d| {
            d.on_click(cx.listener(move |this, _, window, cx| {
                this.run(command.parse().expect("built-in command"), window, cx)
            }))
        })
    }

    fn toolbar_text_button(&self, id: &'static str, label: &'static str, command: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .id(id)
            .px_2()
            .h(px(24.))
            .flex()
            .items_center()
            .rounded_md()
            .whitespace_nowrap()
            .text_color(theme.text_muted)
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            .on_click(cx.listener(move |this, _, window, cx| this.run(command.parse().expect("built-in command"), window, cx)))
            .child(label)
    }

    fn render_go_to_folder(&self, cx: &gpui::App) -> Option<impl IntoElement> {
        let edit = self.edit.as_ref().filter(|e| matches!(e.kind, EditKind::GoToFolder))?;
        let theme = &self.theme;
        // Checked every frame while the bar is open: one stat() call, so typing gets live feedback.
        let (hint, hint_color, border) = match go_to_target(edit.input.read(cx).text()) {
            GoToTarget::Missing => ("⚠ This path doesn't exist", theme.danger, theme.danger),
            GoToTarget::File => ("↩ shows the file in its folder · ⎋ cancel", theme.text_muted, theme.accent),
            GoToTarget::Folder | GoToTarget::Empty => ("↩ go · ⎋ cancel", theme.text_muted, theme.accent),
        };
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .bg(theme.surface)
                .border_b_1()
                .border_color(theme.border)
                .child(div().text_color(theme.text_muted).whitespace_nowrap().child("Go to"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py(px(3.))
                        .rounded_md()
                        .bg(theme.background)
                        .border_1()
                        .border_color(border)
                        .child(edit.input.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(hint_color)
                        .text_size(px(theme.font_size - 1.))
                        .whitespace_nowrap()
                        .child(hint),
                ),
        )
    }

    /// Everything listed in the sidebar below Recents, grouped by section.
    fn sidebar_places(&self) -> Vec<SidebarPlace> {
        let mut places = Vec::new();
        for (ix, fav) in self.config.favorites.iter().enumerate() {
            let path = fav.resolved_path();
            places.push(SidebarPlace {
                section: "Favorites",
                id: format!("fav-{ix}"),
                symbol: place_symbol(&self.home, &path),
                label: fav.label(),
                path,
                favorite: Some(ix),
            });
        }
        for (ix, place) in self.cloud.iter().enumerate() {
            let icloud = place.path == locations::icloud_drive(&self.home);
            places.push(SidebarPlace {
                section: "Cloud",
                id: format!("cloud-{ix}"),
                symbol: if icloud { "icloud" } else { "cloud" },
                label: place.label.clone(),
                path: place.path.clone(),
                favorite: None,
            });
        }
        let mut push_location = |id: String, symbol, label: String, path: PathBuf| {
            places.push(SidebarPlace { section: "Locations", id, symbol, label, path, favorite: None });
        };
        push_location("loc-root".into(), "internaldrive", "Macintosh HD".into(), PathBuf::from("/"));
        for (ix, v) in self.volumes.iter().enumerate() {
            push_location(format!("loc-{ix}"), "externaldrive", v.label.clone(), v.path.clone());
        }
        push_location("loc-trash".into(), "trash", "Trash".into(), self.trash_dir());
        places
    }

    /// Requests any sidebar symbols not yet rendered (call before `render_sidebar`).
    pub(crate) fn load_sidebar_icons(&mut self, cx: &mut Context<Self>) {
        let names: Vec<&'static str> =
            ["clock"].into_iter().chain(self.sidebar_places().iter().map(|p| p.symbol)).collect();
        for name in names {
            let key = self.symbol_key(name);
            self.request_icon(key, cx);
        }
        let (enabled, disabled) = (self.theme.text, self.theme.text_muted);
        for name in [
            "chevron.left",
            "chevron.right",
            "chevron.up",
            "sidebar.right",
            "xmark",
            "magnifyingglass",
            "questionmark.circle",
            "ellipsis.circle",
            "scissors",
            "doc.on.doc",
            "list.bullet.indent",
            "square.grid.2x2",
        ] {
            for color in [enabled, disabled] {
                let key = self.symbol_key_colored(name, color);
                self.request_icon(key, cx);
            }
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(self.render_places(cx))
    }

    fn render_places(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().id("sidebar").flex().flex_col().flex_1().min_h_0().overflow_y_scroll().py_2();

        list = list.child(self.sidebar_item(
            "recents".into(),
            "clock",
            "Recents".into(),
            self.location == Location::Recents,
            cx.listener(|this, _: &MouseDownEvent, _, cx| this.navigate(Location::Recents, true, cx)),
            None,
            cx,
        ));

        let mut section = "";
        for place in self.sidebar_places() {
            if place.section != section {
                section = place.section;
                list = list.child(self.sidebar_header(section));
            }
            let path = place.path.clone();
            list = list.child(self.sidebar_item(
                place.id.into(),
                place.symbol,
                place.label.into(),
                self.location == Location::Dir(path.clone()),
                cx.listener({
                    let path = path.clone();
                    move |this, _: &MouseDownEvent, _, cx| this.navigate_to_path(path.clone(), cx)
                }),
                Some((path, place.favorite)),
                cx,
            ));
        }
        list
    }

    fn sidebar_header(&self, label: &'static str) -> impl IntoElement {
        div()
            .px_3()
            .pt_3()
            .pb_1()
            .text_size(px(self.theme.font_size - 2.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(self.theme.text_muted)
            .child(label)
    }

    #[allow(clippy::too_many_arguments)]
    fn sidebar_item(
        &self,
        id: SharedString,
        symbol: &'static str,
        label: SharedString,
        active: bool,
        on_click: impl Fn(&MouseDownEvent, &mut Window, &mut gpui::App) + 'static,
        menu_target: Option<(PathBuf, Option<usize>)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .id(id)
            .mx_2()
            .px_2()
            .h(px(theme.row_height))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .when(active, |d| d.bg(theme.hover).font_weight(FontWeight::MEDIUM))
            .when(!active, |d| d.hover(|s| s.bg(theme.hover)))
            .on_mouse_down(MouseButton::Left, on_click)
            .when_some(menu_target, |d, (path, favorite)| {
                self.drop_target(d, path.clone(), cx).on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.open_sidebar_menu(event.position, path.clone(), favorite, cx);
                        cx.stop_propagation();
                    }),
                )
            })
            .child(
                div().w(px(18.)).h(px(16.)).flex_none().flex().items_center().justify_center().child(
                    match self.icons.peek(&self.symbol_key(symbol)) {
                        Some(image) => img(image).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                        None => div().text_color(theme.accent).child("•").into_any_element(),
                    },
                ),
            )
            .child(div().flex_1().min_w_0().truncate().child(label))
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let searching = self.search_active(cx);
        // Recents and search results show a "Where" column and aren't sortable.
        let recents = self.location == Location::Recents || searching;
        let search_idle = self.search.as_ref().is_some_and(|s| !s.running && s.error.is_none() && s.note.is_none());
        let empty_message = if searching {
            (self.entries.is_empty() && search_idle).then_some("No results.")
        } else if self.loading || self.load_error.is_some() {
            None
        } else if self.entries.is_empty() {
            Some(if recents { "Files you open from Findest show up here." } else { "This folder is empty." })
        } else {
            None
        };

        div()
            .id("file-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .children((self.view == ViewMode::List).then(|| self.render_header(recents, cx)))
            .child(
                div()
                    .id("list-area")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when_some(self.current_dir(), |d, dir| self.drop_handlers(d, dir, cx))
                    .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, window, cx| {
                        this.maybe_start_drag(event, window, cx);
                        if cx.has_active_drag() {
                            this.drag_autoscroll(window, cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.menu = None;
                            this.commit_edit(window, cx);
                            this.clear_selection(cx);
                            window.focus(&this.focus);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.commit_edit(window, cx);
                            this.clear_selection(cx);
                            this.open_list_menu(event.position, false, cx);
                            window.focus(&this.focus);
                        }),
                    )
                    .child(match self.view {
                        ViewMode::List => uniform_list(
                            "entries",
                            self.entries.len(),
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .map(|ix| {
                                        let icon = this.icon(ix, cx);
                                        this.render_row(ix, icon, cx)
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                        .size_full()
                        .into_any_element(),
                        ViewMode::Tiles => self.render_tiles(cx).into_any_element(),
                    })
                    .when_some(empty_message, |d, msg| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(40.))
                                .w_full()
                                .flex()
                                .justify_center()
                                .text_color(theme.text_muted)
                                .child(msg),
                        )
                    })
                    .children(self.render_load_error(cx)),
            )
    }

    fn render_load_error(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let error = self.load_error.as_ref()?;
        let theme = &self.theme;
        let mut panel = div()
            .absolute()
            .top(px(40.))
            .left_0()
            .right_0()
            .px_8()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .text_center();
        if error.not_permitted {
            panel = panel
                .child(div().font_weight(FontWeight::SEMIBOLD).child("macOS is blocking access to this folder"))
                .child(div().text_color(theme.text_muted).max_w(px(480.)).child(permission_hint()))
                .child(
                    div()
                        .id("open-privacy")
                        .mt_2()
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(theme.accent)
                        .text_color(theme.selection_text)
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|this, _, _, cx| {
                            let result = findest_core::launch::open_url(
                                "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles",
                            );
                            if let Err(e) = result {
                                this.set_error(format!("{e:#}"), cx);
                            }
                        }))
                        .child("Open Privacy Settings"),
                );
        } else {
            panel = panel.child(div().text_color(theme.danger).child(error.message.clone()));
        }
        Some(panel)
    }

    fn render_header(&self, recents: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let general = &self.config.general;
        let column = |id: &'static str, label: &'static str, key: Option<SortKey>, width: Option<f32>, right: bool| {
            let arrow = match key {
                Some(k) if !recents && general.sort_by == k => if general.sort_ascending { " ▲" } else { " ▼" },
                _ => "",
            };
            div()
                .id(id)
                .px_2()
                .whitespace_nowrap()
                .overflow_hidden()
                .map(|d| match width {
                    Some(w) => d.w(px(w)).flex_none(),
                    None => d.flex_1().min_w_0(),
                })
                .when(right, |d| d.flex().justify_end())
                .when_some(key.filter(|_| !recents), |d, key| {
                    d.cursor_pointer().hover(|s| s.text_color(theme.text)).on_click(cx.listener(move |this, _, _, cx| this.set_sort(key, cx)))
                })
                .child(format!("{label}{arrow}"))
        };
        // Follow the list when it's scrolled sideways.
        let scroll_x = self.scroll.0.borrow().base_handle.offset().x;
        div()
            .h(px(theme.row_height))
            .flex_none()
            .overflow_hidden()
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(theme.font_size - 1.))
            .text_color(theme.text_muted)
            .child(
                div()
                    .relative()
                    .left(scroll_x)
                    .h_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .pl(px(NAME_START))
                    .child(column("h-name", "Name", Some(SortKey::Name), Some(self.columns.name), false))
                    .child(column("h-modified", "Date Modified", Some(SortKey::Modified), Some(self.columns.modified), false))
                    .child(column("h-size", "Size", Some(SortKey::Size), Some(self.columns.size), true))
                    .child(if recents {
                        column("h-where", "Where", None, Some(self.columns.kind), false)
                    } else {
                        column("h-kind", "Kind", Some(SortKey::Kind), Some(self.columns.kind), false)
                    })
                    .children(self.column_handles(cx)),
            )
    }

    /// Draggable right edges of the columns (double-click resets).
    fn column_handles(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = &self.theme;
        let mut edge = NAME_START;
        let mut handles = Vec::new();
        let columns =
            [("edge-name", Column::Name), ("edge-modified", Column::Modified), ("edge-size", Column::Size), ("edge-kind", Column::Kind)];
        for (id, column) in columns {
            edge += self.column_width(column);
            let dragging = self.column_drag.is_some();
            handles.push(
                div()
                    .id(id)
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(edge - COLUMN_HANDLE / 2.))
                    .w(px(COLUMN_HANDLE))
                    .flex()
                    .justify_center()
                    .cursor(CursorStyle::ResizeLeftRight)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            if e.click_count == 2 {
                                this.reset_column(column, cx);
                            } else {
                                this.start_column_drag(column, e.position.x);
                            }
                        }),
                    )
                    .child(div().w(px(1.)).my_1().bg(theme.border).when(dragging, |d| d.bg(theme.text_muted)))
                    .into_any_element(),
            );
        }
        // Track the drag anywhere in the window, not just over the header.
        let this = cx.entity().downgrade();
        handles.push(
            canvas(
                |_, _, _| (),
                move |_, (), window, _| {
                    let moved = this.clone();
                    window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble && e.pressed_button == Some(MouseButton::Left) {
                            moved.update(cx, |b, cx| b.drag_column(e.position.x, cx)).ok();
                        }
                    });
                    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Bubble {
                            this.update(cx, |b, cx| b.end_column_drag(cx)).ok();
                        }
                    });
                },
            )
            .absolute()
            .size_full()
            .into_any_element(),
        );
        handles
    }

    fn tile_cell_width(&self) -> f32 {
        (self.tile_size + 28.).max(84.)
    }

    /// Tile rows: the picture plus up to two lines of name.
    fn render_tiles(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let columns = self.tile_columns.max(1);
        let line = (self.theme.font_size - 1.) * 1.3;
        let row_height = self.tile_size + 8. + 2. * line + 18.;
        uniform_list(
            "tiles",
            self.entries.len().div_ceil(columns),
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range
                    .map(|row| {
                        let start = row * columns;
                        let end = (start + columns).min(this.entries.len());
                        let tiles: Vec<AnyElement> = (start..end)
                            .map(|ix| {
                                let image = this.tile_image(ix, cx);
                                this.render_tile(ix, image, line, cx)
                            })
                            .collect();
                        div().w_full().h(px(row_height)).flex().flex_row().children(tiles).into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(self.scroll.clone())
        .size_full()
        .px_1()
        .pt_1()
    }

    fn render_tile(&self, ix: usize, image: Option<Arc<Image>>, line: f32, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let entry = &self.entries[ix];
        let selected = self.selected.contains(&entry.path);
        let renaming = self.renaming().filter(|(p, _)| *p == entry.path).map(|(_, input)| input.clone());
        let width = self.tile_cell_width() - 8.;
        let size = self.tile_size;

        let picture = div()
            .p(px(4.))
            .rounded_lg()
            .when(selected, |d| d.bg(theme.selection.opacity(0.3)))
            .when(!selected, |d| d.group_hover("tile", |s| s.bg(theme.hover)))
            .child(div().size(px(size)).flex().items_center().justify_center().child(match image {
                Some(image) => img(image).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                None => div().text_size(px(size * 0.5)).child(icon_for(entry)).into_any_element(),
            }));
        let label = match renaming {
            Some(input) => div()
                .w(px(width))
                .px_1()
                .rounded_sm()
                .bg(theme.background)
                .text_color(theme.text)
                .border_1()
                .border_color(theme.accent)
                .child(input)
                .into_any_element(),
            None => div()
                .max_w(px(width))
                .max_h(px(2. * line))
                .overflow_hidden()
                .px_1()
                .rounded_md()
                .text_center()
                .line_height(px(line))
                .when(selected, |d| d.bg(theme.selection).text_color(theme.selection_text))
                .child(entry.display_name().to_string())
                .into_any_element(),
        };
        div()
            .id(ix)
            .group("tile")
            .w(px(self.tile_cell_width()))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(6.))
            .gap_1()
            .text_size(px(theme.font_size - 1.))
            .when((entry.is_hidden || self.is_cut(&entry.path)) && !selected, |d| d.opacity(0.5))
            .when(entry.is_dir, |d| self.drop_target(d, entry.path.clone(), cx))
            .map(|d| self.item_mouse_handlers(d, ix, cx))
            .child(picture)
            .child(label)
            .into_any_element()
    }

    /// Click, drag, and context-menu handling shared by list rows and tiles.
    fn item_mouse_handlers(&self, el: gpui::Stateful<gpui::Div>, ix: usize, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        el.on_mouse_up(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseUpEvent, _, cx| {
                // A plain click on one item of a multi-selection (that didn't
                // turn into a drag) narrows the selection to it, like Finder.
                let m = event.modifiers;
                if this.drag_origin.take().is_some() && !m.platform && !m.shift && this.selected.len() > 1 {
                    this.select_only(ix, cx);
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.menu = None;
                let path = this.entries.get(ix).map(|e| e.path.clone());
                if this.renaming().is_some_and(|(p, _)| Some(p) == path.as_deref()) {
                    return; // Let the text field handle the click.
                }
                this.commit_edit(window, cx);
                window.focus(&this.focus);
                let m = event.modifiers;
                if m.platform {
                    this.toggle_selected(ix, cx);
                } else if m.shift {
                    this.select_range_to(ix, cx);
                } else if event.click_count >= 2 {
                    this.open_entry(ix, cx);
                } else if !path.as_ref().is_some_and(|p| this.selected.contains(p)) {
                    this.select_only(ix, cx);
                }
                // Pressing on a selected item may become a drag of the whole selection.
                if path.as_ref().is_some_and(|p| this.selected.contains(p)) && event.click_count < 2 {
                    this.drag_origin = Some(event.position);
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                this.commit_edit(window, cx);
                window.focus(&this.focus);
                let path = this.entries.get(ix).map(|e| e.path.clone());
                if path.is_some_and(|p| !this.selected.contains(&p)) {
                    this.select_only(ix, cx);
                }
                this.open_list_menu(event.position, true, cx);
            }),
        )
    }

    fn render_row(&self, ix: usize, icon: Option<Arc<Image>>, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let entry = &self.entries[ix];
        let selected = self.selected.contains(&entry.path);
        let renaming = self.renaming().filter(|(p, _)| *p == entry.path).map(|(_, input)| input.clone());
        let recents = self.location == Location::Recents || self.search_active(cx);
        let (fg, muted) = if selected { (theme.selection_text, theme.selection_text.opacity(0.8)) } else { (theme.text, theme.text_muted) };
        let dim = (entry.is_hidden || self.is_cut(&entry.path)) && !selected;

        // Indentation comes out of the Name column, so the others line up.
        let name_width = px((self.columns.name - self.depth(ix) as f32 * TREE_INDENT).max(0.));
        let name: AnyElement = match renaming {
            Some(input) => div()
                .w(name_width)
                .flex_none()
                .px_1()
                .rounded_sm()
                .bg(theme.background)
                .text_color(theme.text)
                .border_1()
                .border_color(theme.accent)
                .child(input)
                .into_any_element(),
            None => div()
                .w(name_width)
                .flex_none()
                .px_2()
                .truncate()
                .child(entry.display_name().to_string())
                .into_any_element(),
        };

        let last_column = if recents {
            entry.path.parent().map(contract_tilde).unwrap_or_default()
        } else {
            entry.kind()
        };

        let row = div()
            .id(ix)
            .flex_1()
            .h(px(theme.row_height))
            .pl_2()
            .pr_1()
            .flex()
            .flex_row()
            .items_center()
            .rounded_md()
            .text_color(fg)
            .when(dim, |d| d.opacity(0.6))
            .when(selected, |d| d.bg(theme.selection))
            .when(!selected, |d| d.hover(|s| s.bg(theme.hover)))
            .when(entry.is_dir, |d| self.drop_target(d, entry.path.clone(), cx))
            .map(|d| self.item_mouse_handlers(d, ix, cx))
            .child(self.render_disclosure(ix, cx))
            .child(
                div().w(px(22.)).flex_none().flex().items_center().child(match icon {
                    Some(icon) => img(icon).size(px(18.)).into_any_element(),
                    None => div().child(icon_for(entry)).into_any_element(),
                }),
            )
            .child(name)
            .child(
                div()
                    .w(px(self.columns.modified))
                    .flex_none()
                    .px_2()
                    .truncate()
                    .text_color(muted)
                    .child(entry.modified.map(format_time).unwrap_or_else(|| "--".into())),
            )
            .child(
                div()
                    .w(px(self.columns.size))
                    .flex_none()
                    .px_2()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .text_color(muted)
                    .child(if entry.is_dir || entry.is_package { "--".into() } else { format_size(entry.size) }),
            )
            .child(div().w(px(self.columns.kind)).flex_none().px_2().truncate().text_color(muted).child(last_column));
        // uniform_list lays rows out as roots: claim at least the visible width so
        // the hover and selection backgrounds span it; wider columns scroll sideways.
        // The inset is here rather than list padding, which sideways scrolling doubles.
        div().min_w_full().px_1().flex().child(row).into_any_element()
    }

    fn render_info_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.config.general.info_panel {
            return None;
        }
        Some(
            div()
                .relative()
                .flex_none()
                .h_full()
                .child(self.render_info_contents()?)
                .child(self.info_close_button(cx)),
        )
    }

    fn render_info_contents(&self) -> Option<impl IntoElement> {
        let theme = &self.theme;
        let panel = div()
            .id("info-panel")
            .w(px(INFO_PANEL_WIDTH))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .p_4()
            .flex()
            .flex_col()
            .gap_3();

        if self.selected.len() > 1 {
            let selected: Vec<&Entry> = self.entries.iter().filter(|e| self.selected.contains(&e.path)).collect();
            let folders = selected.iter().filter(|e| e.is_dir).count();
            let files = selected.len() - folders;
            let bytes: u64 = selected.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
            let mut rows = vec![];
            if files > 0 {
                rows.push(("Files", format!("{files} · {}", format_size(bytes))));
            }
            if folders > 0 {
                rows.push(("Folders", folders.to_string()));
            }
            return Some(
                panel
                    .child(self.info_title(format!("{} selected", count_label(selected.len())), "Multiple items".into()))
                    .child(self.info_rows(rows)),
            );
        }

        let Some(info) = &self.info else {
            return Some(panel.child(div().pt_8().w_full().text_center().text_color(theme.text_muted).child("No selection")));
        };
        let entry = self.entries.iter().find(|e| e.path == info.path).cloned().or_else(|| Entry::from_path(&info.path).ok())?;

        // Preview: the image itself, an AppKit rendering, or a large icon. Text
        // files additionally get their first lines below the details.
        let preview: AnyElement = if crate::browser::gpui_decodes(&entry.path) && entry.size <= MAX_PREVIEW_BYTES {
            img(entry.path.clone()).w_full().max_h(px(220.)).object_fit(ObjectFit::Contain).rounded_md().into_any_element()
        } else if let Some(image) = info.preview.clone() {
            img(image).w_full().max_h(px(260.)).object_fit(ObjectFit::Contain).rounded_md().into_any_element()
        } else {
            match info.icon_key.as_ref().and_then(|k| self.icons.peek(k)) {
                Some(icon) => img(icon).size(px(96.)).into_any_element(),
                None => div().text_size(px(64.)).child(icon_for(&entry)).into_any_element(),
            }
        };
        let text_preview = info.text.clone().filter(|t| !t.is_empty()).map(|text| {
            div()
                .id("text-preview")
                .w_full()
                .max_h(px(360.))
                .overflow_y_scroll()
                .p_2()
                .rounded_md()
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .font_family("Menlo")
                .text_size(px(theme.font_size - 2.))
                .line_height(px(theme.font_size + 3.))
                .child(text)
        });

        let mut rows: Vec<(&'static str, String)> = Vec::new();
        let size = if entry.is_dir || entry.is_package {
            match (info.folder_size, info.computing) {
                (Some((bytes, items)), _) => format!("{} · {}", format_size(bytes), count_label(items as usize)),
                (None, true) => "Calculating…".into(),
                (None, false) => "--".into(),
            }
        } else {
            format!("{} ({} bytes)", format_size(entry.size), findest_core::info::group_digits(entry.size))
        };
        rows.push(("Size", size));
        if let Some((w, h)) = info.dimensions {
            rows.push(("Dimensions", format!("{w} × {h}")));
        }
        if let Some(file) = &info.file {
            rows.extend(file.created.map(|t| ("Created", format_full_time(t))));
            rows.extend(file.modified.map(|t| ("Modified", format_full_time(t))));
            rows.extend(file.accessed.map(|t| ("Last opened", format_full_time(t))));
        }
        if let Some(parent) = entry.path.parent() {
            rows.push(("Where", locations::breadcrumbs(&self.home, parent).iter().map(|p| p.label.as_str()).collect::<Vec<_>>().join(" › ")));
        }
        if let Some(file) = &info.file {
            if let Some(target) = &file.link_target {
                rows.push(("Links to", contract_tilde(target)));
            }
            let mode = file.mode & 0o777;
            rows.push(("Permissions", format!("{} ({mode:o})", findest_core::info::permissions_string(mode))));
        }
        if entry.is_hidden {
            rows.push(("Hidden", "Yes".into()));
        }

        Some(
            panel
                .child(div().w_full().flex().justify_center().py_2().child(preview))
                .child(self.info_title(entry.display_name().to_string(), entry.kind()))
                .child(self.info_rows(rows))
                .children(text_preview),
        )
    }

    /// Small ✕ pinned to the panel's top-right corner.
    fn info_close_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let icon = self.icons.peek(&self.symbol_key_colored("xmark", theme.text_muted));
        div()
            .id("info-close")
            .absolute()
            .top(px(8.))
            .right(px(8.))
            .size(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_color(theme.text_muted)
            .hover(|s| s.bg(theme.hover))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .tooltip(tooltip("Close info panel (⌘I)"))
            .on_click(cx.listener(|this, _, window, cx| this.run(crate::command::Command::ToggleInfo, window, cx)))
            .child(match icon {
                Some(image) => img(image).size(px(11.)).object_fit(ObjectFit::Contain).into_any_element(),
                None => div().child("✕").into_any_element(),
            })
    }

    fn info_title(&self, title: String, subtitle: String) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(self.theme.font_size + 2.)).child(title))
            .child(div().text_color(self.theme.text_muted).child(subtitle))
    }

    fn info_rows(&self, rows: Vec<(&'static str, String)>) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .children(rows.into_iter().map(|(label, value)| {
                div()
                    .flex()
                    .flex_row()
                    .gap_3()
                    .child(
                        div()
                            .w(px(78.))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .text_size(px(theme.font_size - 1.))
                            .text_color(theme.text_muted)
                            .child(label),
                    )
                    .child(div().flex_1().min_w_0().overflow_hidden().text_size(px(theme.font_size - 1.)).child(value))
            }))
    }

    /// Indentation plus the ▸/▾ toggle for folder rows.
    fn render_disclosure(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let entry = &self.entries[ix];
        let indent = self.depth(ix) as f32 * TREE_INDENT;
        let toggle = self.is_expandable(ix);
        let expanded = self.is_expanded(&entry.path);
        let path = entry.path.clone();
        let muted = if self.selected.contains(&entry.path) { self.theme.selection_text } else { self.theme.text_muted };
        div()
            .id(("disclosure", ix))
            .flex_none()
            .ml(px(indent))
            .w(px(TREE_GUTTER))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(self.theme.font_size - 3.))
            .text_color(muted)
            .when(toggle, |d| {
                d.cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.menu = None;
                            this.toggle_expanded(path.clone(), cx);
                        }),
                    )
                    .child(if expanded { "▼" } else { "▶" })
            })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let mut summary = count_label(self.entries.len());
        if !self.selected.is_empty() {
            summary = format!("{} of {summary} selected", self.selected.len());
            let bytes = self.selection_size();
            if bytes > 0 {
                summary = format!("{summary} · {}", format_size(bytes));
            }
        }
        let capacity = self.volume.map(|(available, total)| {
            format!("{} available of {}", format_size(available), format_size(total))
        });
        let (message, color): (String, Hsla) = match &self.status {
            Some(status) => (status.message.clone(), if status.is_error { theme.danger } else { theme.text_muted }),
            None => (String::new(), theme.text_muted),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_4()
            .h(px(24.))
            .px_3()
            .bg(theme.surface)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(theme.font_size - 1.))
            .text_color(theme.text_muted)
            .child(div().flex_none().whitespace_nowrap().child(summary))
            .child(div().flex_1().min_w_0().flex().justify_end().text_color(color).truncate().child(message))
            .children(self.render_type_ahead())
            .children(self.render_index_activity(cx))
            .children(self.render_transfers(cx))
            .children(self.render_clipboard(cx))
            .children(capacity.map(|c| div().flex_none().whitespace_nowrap().child(c)))
            .when(self.view == ViewMode::Tiles, |d| d.child(self.render_tile_slider(cx)))
    }

    /// What the search index is doing: "Indexing… 340,000 items", paused,
    /// off, or "Index up to date". Click for the index menu.
    fn render_index_activity(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let service = cx.try_global::<crate::search_service::IndexService>()?;
        let status = crate::search_service::status_text(service);
        let idle = service.enabled() && !service.building && !service.is_paused() && service.index.is_some();
        let (label, hint) = if idle {
            ("Index up to date".to_string(), format!("{status}. Click to pause, rebuild or turn off indexing"))
        } else {
            (status, "Click to pause, resume or turn off search indexing".to_string())
        };
        let theme = &self.theme;
        Some(
            div()
                .id("index-activity")
                .flex_none()
                .whitespace_nowrap()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                .tooltip(tooltip(hint))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, e: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.open_index_menu(Point { x: e.position.x - px(120.), y: e.position.y - px(150.) }, cx);
                    }),
                )
                .child(label),
        )
    }

    /// Progress of running copies/moves, with a cancel button. Quick ones
    /// finish before it would appear, so they don't flash.
    fn render_transfers(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        use std::sync::atomic::Ordering::Relaxed;
        let shown: Vec<_> = self.transfers.iter().filter(|t| t.started.elapsed().as_millis() > 400).collect();
        if shown.is_empty() {
            return None;
        }
        let theme = &self.theme;
        let sum = |f: fn(&findest_core::ops::Progress) -> u64| shown.iter().map(|t| f(&t.progress)).sum::<u64>();
        let (total_files, done_files) = (sum(|p| p.total_files.load(Relaxed)), sum(|p| p.done_files.load(Relaxed)));
        let (total_bytes, done_bytes) = (sum(|p| p.total_bytes.load(Relaxed)), sum(|p| p.done_bytes.load(Relaxed)));
        let counted = shown.iter().all(|t| t.progress.counted.load(Relaxed));
        let verb = if shown.iter().all(|t| t.verb == shown[0].verb) { shown[0].verb } else { "Transferring" };
        let label = if !counted {
            format!("{verb}… counting items")
        } else if total_bytes > 0 {
            format!("{verb} {done_files} of {} · {} of {}", count_label(total_files as usize), format_size(done_bytes), format_size(total_bytes))
        } else {
            format!("{verb} {done_files} of {}", count_label(total_files as usize))
        };
        let fraction = if !counted {
            0.0
        } else if total_bytes > 0 {
            done_bytes as f32 / total_bytes as f32
        } else if total_files > 0 {
            done_files as f32 / total_files as f32
        } else {
            1.0
        };
        Some(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap_2()
                .child(div().whitespace_nowrap().text_color(theme.text).child(label))
                .child(
                    div()
                        .w(px(120.))
                        .h(px(5.))
                        .rounded_full()
                        .bg(theme.border)
                        .child(div().h_full().w(px(120. * fraction.clamp(0., 1.))).rounded_full().bg(theme.accent)),
                )
                .child(
                    div()
                        .id("cancel-transfer")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.hover).text_color(theme.text))
                        .tooltip(tooltip("Cancel"))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_transfers(cx)))
                        .child("✕"),
                ),
        )
    }

    /// What's waiting to be pasted (⌘C / ⌘X), with a button to forget it.
    /// What's been typed to jump to a name; red when nothing matches.
    fn render_type_ahead(&self) -> Option<impl IntoElement> {
        let ta = &self.type_ahead;
        if !ta.is_active() {
            return None;
        }
        let theme = &self.theme;
        Some(
            div()
                .flex_none()
                .max_w(px(240.))
                .px_2()
                .rounded_md()
                .bg(theme.hover)
                .text_color(if ta.missed { theme.danger } else { theme.text })
                .truncate()
                .child(format!("Go to “{}”", ta.query)),
        )
    }

    fn render_clipboard(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let first = self.file_clipboard.first()?;
        let theme = &self.theme;
        let n = self.file_clipboard.len();
        let what = if n == 1 { format!("“{}”", findest_core::paths::display_name(first)) } else { count_label(n) };
        let (symbol, verb) = if self.clipboard_cut { ("scissors", "cut") } else { ("doc.on.doc", "copied") };
        let icon = self.icons.peek(&self.symbol_key_colored(symbol, theme.text_muted));
        Some(
            div()
                .id("clipboard")
                .flex()
                .flex_row()
                .flex_none()
                .max_w(px(320.))
                .items_center()
                .gap_1()
                .pl_2()
                .rounded_md()
                .bg(theme.hover)
                .tooltip(tooltip(if self.clipboard_cut { "⌘V moves these here" } else { "⌘V copies these here" }))
                .children(icon.map(|i| img(i).size(px(12.)).flex_none()))
                .child(div().min_w_0().truncate().child(format!("{what} {verb}")))
                .child(
                    div()
                        .id("clear-clipboard")
                        .flex_none()
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, window, cx| this.run(crate::command::Command::ClearClipboard, window, cx)))
                        .child("✕"),
                ),
        )
    }

    /// Tile size slider. Dragging is tracked window-wide so it keeps working
    /// when the pointer leaves the track.
    fn render_tile_slider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const WIDTH: f32 = 110.;
        const KNOB: f32 = 12.;
        fn size_at(bounds: Bounds<Pixels>, x: Pixels) -> f32 {
            let t = (x - bounds.left() - px(KNOB / 2.)) / (bounds.size.width - px(KNOB));
            MIN_TILE + t.clamp(0., 1.) * (MAX_TILE - MIN_TILE)
        }
        let theme = &self.theme;
        let t = (self.tile_size - MIN_TILE) / (MAX_TILE - MIN_TILE);
        let this = cx.entity().downgrade();
        div()
            .id("tile-size")
            .flex_none()
            .w(px(WIDTH))
            .h(px(16.))
            .relative()
            .cursor_pointer()
            .tooltip(tooltip("Tile size (⌘= / ⌘-)"))
            .child(div().absolute().left(px(KNOB / 2.)).right(px(KNOB / 2.)).top(px(6.5)).h(px(3.)).rounded_full().bg(theme.border))
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(t * (WIDTH - KNOB)))
                    .size(px(KNOB))
                    .rounded_full()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.text_muted),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        let down = this.clone();
                        window.on_mouse_event(move |e: &MouseDownEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && bounds.contains(&e.position) {
                                down.update(cx, |b, cx| {
                                    b.tile_slider_drag = true;
                                    b.set_tile_size(size_at(bounds, e.position.x), cx);
                                })
                                .ok();
                            }
                        });
                        let moved = this.clone();
                        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble && e.pressed_button == Some(MouseButton::Left) {
                                moved
                                    .update(cx, |b, cx| {
                                        if b.tile_slider_drag {
                                            b.set_tile_size(size_at(bounds, e.position.x), cx);
                                        }
                                    })
                                    .ok();
                            }
                        });
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                this.update(cx, |b, cx| {
                                    if std::mem::take(&mut b.tile_slider_drag) {
                                        b.tile_size_committed(cx);
                                    }
                                })
                                .ok();
                            }
                        });
                    },
                )
                .absolute()
                .size_full(),
            )
    }

    fn render_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let menu = self.menu.as_ref()?;
        let theme = &self.theme;
        let mut list = div()
            .id("context-menu")
            .occlude()
            .min_w(px(220.))
            .py_1()
            .rounded_lg()
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.menu = None;
                cx.notify();
            }));
        for (i, entry) in menu.entries.iter().enumerate() {
            list = match entry {
                MenuEntry::Separator => list.child(div().my_1().h(px(1.)).bg(theme.border)),
                MenuEntry::Header(text) => list.child(
                    div()
                        .px_3()
                        .pt_1()
                        .pb(px(2.))
                        .text_size(px(theme.font_size - 2.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_muted)
                        .child(text.clone()),
                ),
                MenuEntry::Item { label, detail, action, danger } => {
                    let action: MenuAction = action.clone();
                    list.child(
                        div()
                            .id(i)
                            .mx_1()
                            .px_2()
                            .h(px(theme.row_height - 2.))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_4()
                            .rounded_md()
                            .cursor_pointer()
                            .when(*danger, |d| d.text_color(theme.danger))
                            .hover(|s| s.bg(theme.selection).text_color(theme.selection_text))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // Refocus the list first: the action may move focus
                                // elsewhere (e.g. Rename focuses the text field).
                                window.focus(&this.focus);
                                this.run_menu_action(action.clone(), window, cx);
                            }))
                            .child(div().flex_1().whitespace_nowrap().child(label.clone()))
                            .children(detail.clone().map(|s| div().text_color(theme.text_muted).whitespace_nowrap().child(s))),
                    )
                }
            };
        }
        Some(deferred(anchored().position(menu.position).snap_to_window().child(list)).with_priority(1))
    }
}

impl Browser {
    /// Accepts drops (from Findest or other apps) into `dest`, highlighting on hover.
    fn drop_target<E: InteractiveElement + StatefulInteractiveElement + Styled>(&self, el: E, dest: PathBuf, cx: &mut Context<Self>) -> E {
        let highlight = self.theme.accent.opacity(0.25);
        self.drop_handlers(el, dest, cx).drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(highlight))
    }

    /// All drops, including drags started in Findest itself, arrive as native
    /// file drags (see `native_drag`).
    fn drop_handlers<E: InteractiveElement>(&self, el: E, dest: PathBuf, cx: &mut Context<Self>) -> E {
        el.on_drop(cx.listener(move |this, files: &ExternalPaths, window, cx| {
            cx.stop_propagation();
            let copy = crate::native_drag::option_key_down();
            this.drop_paths(files.paths().to_vec(), dest.clone(), copy, window, cx);
        }))
    }
}

/// Hover hint for toolbar buttons.
struct Tooltip(SharedString);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(gpui::rgb(0x2b2d31))
            .border_1()
            .border_color(gpui::rgb(0x45474d))
            .text_color(gpui::rgb(0xf0f0f0))
            .text_sm()
            .shadow_md()
            .child(self.0.clone())
    }
}

fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| Tooltip(text.clone())).into()
}

/// macOS grants privacy permissions to the app that *launched* a process.
/// LaunchServices records it in `__CFBundleIdentifier`.
fn permission_hint() -> String {
    const OURS: &str = "dev.findest.Findest";
    match std::env::var("__CFBundleIdentifier") {
        Ok(id) if id == OURS => "Cloud drives and other protected folders need permission. Turn on Findest under \
            System Settings › Privacy & Security › Full Disk Access, then quit and reopen Findest."
            .into(),
        launcher => format!(
            "Findest was started from another app ({}), so macOS applies that app's permissions. \
             Open Findest.app from Finder, the Dock or Spotlight instead (scripts/bundle.sh --install), \
             and grant it Full Disk Access.",
            launcher.unwrap_or_else(|_| "a terminal".into())
        ),
    }
}

struct SidebarPlace {
    section: &'static str,
    id: String,
    symbol: &'static str,
    label: String,
    path: PathBuf,
    favorite: Option<usize>,
}

/// Finder-style SF Symbol for well-known folders.
fn place_symbol(home: &std::path::Path, path: &std::path::Path) -> &'static str {
    if path == home {
        return "house";
    }
    if path == std::path::Path::new("/Applications") || path == home.join("Applications") {
        return "square.grid.2x2";
    }
    match path.strip_prefix(home).ok().and_then(|p| p.to_str()) {
        Some("Desktop") => "menubar.dock.rectangle",
        Some("Documents") => "doc",
        Some("Downloads") => "arrow.down.circle",
        Some("Movies") => "film",
        Some("Music") => "music.note",
        Some("Pictures") => "photo",
        Some("Public") => "person.2",
        Some("Library") => "building.columns",
        _ => "folder",
    }
}

fn icon_for(entry: &Entry) -> &'static str {
    if entry.is_dir {
        return "📁";
    }
    if entry.is_package {
        return "📦";
    }
    match entry.extension().map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "heic" | "webp" | "svg" | "tiff") => "🖼",
        Some("mp4" | "mov" | "mkv" | "avi") => "🎞",
        Some("mp3" | "wav" | "flac" | "m4a" | "aac") => "🎵",
        Some("pdf") => "📕",
        Some("zip" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "dmg") => "🗜",
        Some(
            "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "h" | "cpp" | "swift" | "rb" | "sh"
            | "json" | "toml" | "yaml" | "yml" | "html" | "css",
        ) => "📝",
        _ => "📄",
    }
}

/// "15 October 2026 at 14:03".
fn format_full_time(time: SystemTime) -> String {
    let time: DateTime<Local> = time.into();
    time.format("%-d %B %Y at %H:%M").to_string()
}

fn format_time(time: SystemTime) -> String {
    let time: DateTime<Local> = time.into();
    let today = Local::now().date_naive();
    let date = time.date_naive();
    if date == today {
        format!("Today, {}", time.format("%H:%M"))
    } else if today.pred_opt() == Some(date) {
        format!("Yesterday, {}", time.format("%H:%M"))
    } else {
        time.format("%-d %b %Y, %H:%M").to_string()
    }
}
