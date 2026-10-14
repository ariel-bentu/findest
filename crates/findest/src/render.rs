//! Rendering for [`Browser`]: toolbar, sidebar, file list, status bar and menus.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use findest_core::listing::{Entry, SortKey, format_size};
use findest_core::locations;
use findest_core::paths::{contract_tilde, home_dir};
use gpui::{
    AnyElement, Context, ExternalPaths, FontWeight, Hsla, Image, MouseButton, MouseDownEvent,
    Pixels, Point, SharedString, Window, anchored, deferred, div, img, prelude::*, px, uniform_list,
};

use crate::browser::{Browser, DraggedFiles, EditKind, Location, MenuAction, MenuEntry, count_label};
use crate::command::BROWSER_CONTEXT;

const SIDEBAR_WIDTH: f32 = 200.0;
const COL_MODIFIED: f32 = 170.0;
const COL_SIZE: f32 = 90.0;
const COL_KIND: f32 = 130.0;

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.pending_rename) {
            self.start_rename(window, cx);
        }
        let title = self.location_label();
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
            .flex_col()
            .bg(theme.background)
            .text_color(theme.text)
            .text_size(px(theme.font_size))
            .child(self.render_toolbar(cx))
            .children(self.render_go_to_folder())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(self.render_file_list(cx)),
            )
            .child(self.render_status_bar())
            .children(self.render_menu(cx));
        if let Some(family) = &theme.font_family {
            root = root.font_family(SharedString::from(family.clone()));
        }
        root
    }
}

impl Browser {
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let mut crumbs = div().flex().flex_row().items_center().gap_1().flex_1().min_w_0().overflow_hidden();
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
                            .child(label),
                    );
                }
            }
            Location::Recents => {
                crumbs = crumbs.child(div().font_weight(FontWeight::SEMIBOLD).child("Recents"));
            }
        }

        let hidden_label = if self.config.general.show_hidden { "Hidden: shown" } else { "Hidden: off" };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(38.))
            .px_2()
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .child(self.nav_button(true, cx))
            .child(self.nav_button(false, cx))
            .child(self.toolbar_button("up", "↑", self.current_dir().is_some_and(|d| d.parent().is_some()), "go_up", cx))
            .child(crumbs)
            .when(self.loading, |d| d.child(div().text_color(theme.text_muted).child("Loading…")))
            .child(self.toolbar_text_button("hidden", hidden_label, "toggle_hidden", cx))
    }

    /// Back/forward: click to navigate; long-press or right-click for history.
    fn nav_button(&self, back: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let enabled = if back { self.can_go_back() } else { self.can_go_forward() };
        let below = |p: Point<Pixels>| Point { x: p.x - px(10.), y: p.y + px(14.) };
        div()
            .id(if back { "back" } else { "forward" })
            .w(px(26.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(theme.font_size + 4.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            .when(!enabled, |d| d.text_color(theme.text_muted.opacity(0.4)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.menu = None;
                    this.press_nav(back, below(event.position), cx);
                }),
            )
            .on_mouse_up(MouseButton::Left, cx.listener(move |this, _, _, cx| this.release_nav(back, cx)))
            .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, _| this.cancel_nav_press()))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.open_history_menu(back, below(event.position), cx);
                }),
            )
            .child(if back { "‹" } else { "›" })
    }

    fn toolbar_button(&self, id: &'static str, label: &'static str, enabled: bool, command: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        div()
            .id(id)
            .w(px(26.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(theme.font_size + 4.))
            .when(!enabled, |d| d.text_color(theme.text_muted.opacity(0.4)))
            .when(enabled, |d| {
                d.cursor_pointer().hover(|s| s.bg(theme.hover)).on_click(cx.listener(move |this, _, window, cx| {
                    this.run(command.parse().expect("built-in command"), window, cx)
                }))
            })
            .child(label)
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

    fn render_go_to_folder(&self) -> Option<impl IntoElement> {
        let edit = self.edit.as_ref().filter(|e| matches!(e.kind, EditKind::GoToFolder))?;
        let theme = &self.theme;
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
                .child(div().text_color(theme.text_muted).whitespace_nowrap().child("Go to folder"))
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py(px(3.))
                        .rounded_md()
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.accent)
                        .child(edit.input.clone()),
                ),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let mut list = div()
            .id("sidebar")
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .py_2()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border);

        list = list.child(self.sidebar_item(
            "recents".into(),
            "🕘",
            "Recents".into(),
            self.location == Location::Recents,
            cx.listener(|this, _: &MouseDownEvent, _, cx| this.navigate(Location::Recents, true, cx)),
            None,
            cx,
        ));

        list = list.child(self.sidebar_header("Favorites"));
        for (ix, fav) in self.config.favorites.iter().enumerate() {
            let path = fav.resolved_path();
            let icon = if path == home_dir() { "🏠" } else { "📁" };
            list = list.child(self.sidebar_item(
                format!("fav-{ix}").into(),
                icon,
                fav.label().into(),
                self.location == Location::Dir(path.clone()),
                cx.listener({
                    let path = path.clone();
                    move |this, _: &MouseDownEvent, _, cx| this.navigate_to_path(path.clone(), cx)
                }),
                Some((path, Some(ix))),
                cx,
            ));
        }

        if !self.cloud.is_empty() {
            list = list.child(self.sidebar_header("Cloud"));
            for (ix, place) in self.cloud.iter().enumerate() {
                let path = place.path.clone();
                list = list.child(self.sidebar_item(
                    format!("cloud-{ix}").into(),
                    "☁️",
                    place.label.clone().into(),
                    self.location == Location::Dir(path.clone()),
                    cx.listener({
                        let path = path.clone();
                        move |this, _: &MouseDownEvent, _, cx| this.navigate_to_path(path.clone(), cx)
                    }),
                    Some((path, None)),
                    cx,
                ));
            }
        }

        list = list.child(self.sidebar_header("Locations"));
        let root = PathBuf::from("/");
        let mut locations = vec![(root, "💻", "Macintosh HD".to_string())];
        locations.extend(self.volumes.iter().map(|v| (v.path.clone(), "💽", v.label.clone())));
        for (ix, (path, icon, label)) in locations.into_iter().enumerate() {
            list = list.child(self.sidebar_item(
                format!("loc-{ix}").into(),
                icon,
                label.into(),
                self.location == Location::Dir(path.clone()),
                cx.listener({
                    let path = path.clone();
                    move |this, _: &MouseDownEvent, _, cx| this.navigate_to_path(path.clone(), cx)
                }),
                Some((path, None)),
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
        icon: &'static str,
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
            .child(div().flex_none().child(icon))
            .child(div().flex_1().min_w_0().truncate().child(label))
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = &self.theme;
        let recents = self.location == Location::Recents;
        let empty_message = if self.loading || self.load_error.is_some() {
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
            .child(self.render_header(recents, cx))
            .child(
                div()
                    .id("list-area")
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when_some(self.current_dir(), |d, dir| self.drop_handlers(d, dir, cx))
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
                    .child(
                        uniform_list(
                            "entries",
                            self.entries.len(),
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                let selection = Rc::new(if this.selected.is_empty() { Vec::new() } else { this.selection() });
                                range
                                    .map(|ix| {
                                        let icon = this.icon(ix, cx);
                                        this.render_row(ix, icon, &selection, cx)
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(self.scroll.clone())
                        .size_full()
                        .px_1(),
                    )
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
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(theme.row_height))
            .pl(px(30.))
            .pr_2()
            .border_b_1()
            .border_color(theme.border)
            .text_size(px(theme.font_size - 1.))
            .text_color(theme.text_muted)
            .child(column("h-name", "Name", Some(SortKey::Name), None, false))
            .child(column("h-modified", "Date Modified", Some(SortKey::Modified), Some(COL_MODIFIED), false))
            .child(column("h-size", "Size", Some(SortKey::Size), Some(COL_SIZE), true))
            .child(if recents {
                column("h-where", "Where", None, Some(COL_KIND), false)
            } else {
                column("h-kind", "Kind", Some(SortKey::Kind), Some(COL_KIND), false)
            })
    }

    fn render_row(&self, ix: usize, icon: Option<Arc<Image>>, selection: &Rc<Vec<PathBuf>>, cx: &mut Context<Self>) -> AnyElement {
        let theme = &self.theme;
        let entry = &self.entries[ix];
        let selected = self.selected.contains(&entry.path);
        let renaming = self.renaming().filter(|(p, _)| *p == entry.path).map(|(_, input)| input.clone());
        let is_renaming = renaming.is_some();
        let recents = self.location == Location::Recents;
        let (fg, muted) = if selected { (theme.selection_text, theme.selection_text.opacity(0.8)) } else { (theme.text, theme.text_muted) };
        let dim = entry.is_hidden && !selected;

        let name: AnyElement = match renaming {
            Some(input) => div()
                .flex_1()
                .min_w_0()
                .px_1()
                .rounded_sm()
                .bg(theme.background)
                .text_color(theme.text)
                .border_1()
                .border_color(theme.accent)
                .child(input)
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
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

        div()
            .id(ix)
            // uniform_list lays rows out as roots, so they must claim the full width
            // explicitly; otherwise they shrink to content and the name column collapses.
            .w_full()
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
            .when(!is_renaming, |d| {
                let paths = if selected { selection.clone() } else { Rc::new(vec![entry.path.clone()]) };
                let (bg, fg) = (theme.selection, theme.selection_text);
                d.on_drag(DraggedFiles { paths }, move |files: &DraggedFiles, offset, _, cx| {
                    let label = match files.paths.as_slice() {
                        [one] => findest_core::paths::display_name(one),
                        many => count_label(many.len()),
                    };
                    cx.new(|_| DragPreview { label, offset, bg, fg })
                })
            })
            .when(entry.is_dir, |d| self.drop_target(d, entry.path.clone(), cx))
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
                    } else {
                        this.select_only(ix, cx);
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
            .child(
                div().w(px(22.)).flex_none().flex().items_center().child(match icon {
                    Some(icon) => img(icon).size(px(18.)).into_any_element(),
                    None => div().child(icon_for(entry)).into_any_element(),
                }),
            )
            .child(name)
            .child(
                div()
                    .w(px(COL_MODIFIED))
                    .flex_none()
                    .px_2()
                    .truncate()
                    .text_color(muted)
                    .child(entry.modified.map(format_time).unwrap_or_else(|| "--".into())),
            )
            .child(
                div()
                    .w(px(COL_SIZE))
                    .flex_none()
                    .px_2()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .text_color(muted)
                    .child(if entry.is_dir || entry.is_package { "--".into() } else { format_size(entry.size) }),
            )
            .child(div().w(px(COL_KIND)).flex_none().px_2().truncate().text_color(muted).child(last_column))
            .into_any_element()
    }

    fn render_status_bar(&self) -> impl IntoElement {
        let theme = &self.theme;
        let mut summary = count_label(self.entries.len());
        if !self.selected.is_empty() {
            summary = format!("{} of {summary} selected", self.selected.len());
        }
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
        self.drop_handlers(el, dest, cx)
            .drag_over::<DraggedFiles>(move |s, _, _, _| s.bg(highlight))
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(highlight))
    }

    fn drop_handlers<E: InteractiveElement>(&self, el: E, dest: PathBuf, cx: &mut Context<Self>) -> E {
        let dest2 = dest.clone();
        el.on_drop(cx.listener(move |this, files: &DraggedFiles, window, cx| {
            cx.stop_propagation();
            let copy = window.modifiers().alt;
            this.drop_paths(files.paths.to_vec(), dest.clone(), copy, cx);
        }))
        .on_drop(cx.listener(move |this, files: &ExternalPaths, window, cx| {
            cx.stop_propagation();
            let copy = window.modifiers().alt;
            this.drop_paths(files.paths().to_vec(), dest2.clone(), copy, cx);
        }))
    }
}

/// The floating label that follows the cursor while dragging.
struct DragPreview {
    label: String,
    offset: Point<Pixels>,
    bg: Hsla,
    fg: Hsla,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .pl(self.offset.x + px(12.))
            .pt(self.offset.y + px(6.))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(self.bg.opacity(0.9))
                    .text_color(self.fg)
                    .text_sm()
                    .shadow_md()
                    .whitespace_nowrap()
                    .child(self.label.clone()),
            )
    }
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
