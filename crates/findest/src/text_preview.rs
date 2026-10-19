//! Findest's own preview for text files Quick Look shows nothing for (YAML,
//! TOML...): a syntax-highlighted panel over the file list. Space opens and
//! closes it, like Quick Look; ↑/↓ follow the selection.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use findest_core::highlight::{self, Highlighted};
use gpui::{
    Context, FontWeight, HighlightStyle, InteractiveElement, IntoElement, ObjectFit, ParentElement, Rgba,
    StatefulInteractiveElement, Styled, StyledImage, StyledText, Task, UniformListScrollHandle, Window, div, img,
    ListHorizontalSizingBehavior, prelude::FluentBuilder, px, uniform_list,
};

use crate::browser::Browser;

pub struct TextPreview {
    pub path: PathBuf,
    /// `None` while reading; an error message if the file can't be read.
    content: Option<Result<Arc<Highlighted>, String>>,
    scroll: UniformListScrollHandle,
    _task: Task<()>,
}

impl Browser {
    /// The file Space would preview: the single selection.
    pub(crate) fn preview_target(&self) -> Option<PathBuf> {
        let selection = self.selection();
        match selection.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// Space: opens the text preview for YAML and the like, else Quick Look.
    /// Returns false when Quick Look should handle it.
    pub(crate) fn toggle_text_preview(&mut self, cx: &mut Context<Self>) -> bool {
        if self.text_preview.take().is_some() {
            cx.notify();
            return true;
        }
        match self.preview_target() {
            Some(path) if handles(&path) => {
                self.open_text_preview(path, cx);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn open_text_preview(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let dark = self.theme.background.l < 0.5;
        let file = path.clone();
        let read = cx.background_executor().spawn(async move { highlight::highlight_file(&file, dark) });
        let opened = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let path = opened;
            let result = read.await;
            this.update(cx, |this, cx| {
                if let Some(preview) = this.text_preview.as_mut().filter(|p| p.path == path) {
                    preview.content = Some(result.map(Arc::new).map_err(|e| format!("Can't read {}: {e}", findest_core::paths::display_name(&path))));
                    cx.notify();
                }
            })
            .ok();
        });
        let scroll = UniformListScrollHandle::new();
        self.text_preview = Some(TextPreview { path: path.clone(), content: None, scroll, _task: task });
        cx.notify();
    }

    /// Keeps the open preview on the selection (called each frame). Moving to
    /// a file it doesn't handle hands over to Quick Look.
    pub(crate) fn follow_text_preview(&mut self, fetched: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.text_preview.as_ref().map(|p| p.path.clone()) else { return };
        let target = self.preview_target();
        if target.as_deref() == Some(open.as_path()) || !fetched {
            return;
        }
        match target {
            Some(path) if handles(&path) => self.open_text_preview(path, cx),
            Some(path) if !path.is_dir() => {
                self.text_preview = None;
                crate::quick_look::toggle(window, vec![path], cx);
            }
            _ => self.text_preview = None,
        }
    }

    pub(crate) fn render_text_preview(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let preview = self.text_preview.as_ref()?;
        let theme = &self.theme;
        let font_size = theme.font_size;
        let line_height = (font_size * 1.45).round();
        let name = findest_core::paths::display_name(&preview.path);

        let mut subtitle = String::new();
        let mut notes = Vec::new();
        if let Some(Ok(text)) = &preview.content {
            subtitle = format!("{} · {} lines", text.syntax, text.lines.len());
            notes = text.notes.clone();
        }

        let close_icon = self.icons.peek(&self.symbol_key_colored("xmark", theme.text_muted));
        let close = div()
            .id("text-preview-close")
            .size(px(24.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .cursor_pointer()
            .text_color(theme.text_muted)
            .hover(|s| s.bg(theme.hover))
            .tooltip(crate::render::tooltip("Close (Space or Esc)"))
            .on_click(cx.listener(|this, _, _, cx| {
                this.text_preview = None;
                cx.notify();
            }))
            .child(match close_icon {
                Some(image) => img(image).size(px(11.)).object_fit(ObjectFit::Contain).into_any_element(),
                None => div().child("✕").into_any_element(),
            });
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .pl_1()
            .pr_3()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(close)
            .child(div().font_weight(FontWeight::SEMIBOLD).truncate().child(name))
            .child(div().flex_1().text_color(theme.text_muted).text_size(px(font_size - 1.)).child(subtitle));

        let gutter = theme.text_muted;
        let body = match &preview.content {
            None => div().p_4().text_color(theme.text_muted).child("Reading…").into_any_element(),
            Some(Err(message)) => div().p_4().text_color(theme.danger).child(message.clone()).into_any_element(),
            Some(Ok(text)) => {
                let text = text.clone();
                let digits = text.lines.len().max(1).to_string().len();
                let gutter_width = digits as f32 * font_size * 0.62 + 16.;
                // Long lines scroll sideways; the longest one sets the width.
                let widest = text.lines.iter().enumerate().max_by_key(|(_, l)| l.text.chars().count()).map(|(ix, _)| ix);
                uniform_list(
                    "text-preview-lines",
                    text.lines.len(),
                    cx.processor(move |_this, range: std::ops::Range<usize>, _window, _cx| {
                        range
                            .map(|ix| {
                                let line = &text.lines[ix];
                                let highlights = line.spans.iter().map(|span| {
                                    let [r, g, b] = span.rgb;
                                    let color = Rgba { r: r as f32 / 255., g: g as f32 / 255., b: b as f32 / 255., a: 1. };
                                    let style = HighlightStyle {
                                        color: Some(color.into()),
                                        font_weight: span.bold.then_some(FontWeight::BOLD),
                                        ..Default::default()
                                    };
                                    (span.range.clone(), style)
                                });
                                div()
                                    .h(px(line_height))
                                    .flex()
                                    .flex_row()
                                    .whitespace_nowrap()
                                    .child(
                                        div()
                                            .w(px(gutter_width))
                                            .flex_none()
                                            .pr_3()
                                            .flex()
                                            .justify_end()
                                            .text_color(gutter)
                                            .child((ix + 1).to_string()),
                                    )
                                    .child(StyledText::new(line.text.clone()).with_highlights(highlights))
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(preview.scroll.clone())
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .with_width_from_item(widest)
                .size_full()
                .py_2()
                .into_any_element()
            }
        };

        Some(
            div()
                .id("text-preview")
                .occlude()
                .absolute()
                .top(px(48.))
                .bottom(px(48.))
                .left(px(48.))
                .right(px(48.))
                .flex()
                .flex_col()
                .rounded_lg()
                .bg(theme.surface)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .overflow_hidden()
                .child(header)
                .when(!notes.is_empty(), |d| {
                    d.child(
                        div()
                            .px_3()
                            .py_1()
                            .border_b_1()
                            .border_color(theme.border)
                            .text_size(px(font_size - 1.))
                            .text_color(theme.text_muted)
                            .child(notes.join(" · ")),
                    )
                })
                .child(div().flex_1().min_h_0().font_family("Menlo").text_size(px(font_size - 1.)).child(body)),
        )
    }
}

/// True if `path` gets Findest's text preview instead of Quick Look.
pub fn handles(path: &Path) -> bool {
    highlight::has_own_preview(path) && !path.is_dir()
}
