//! Findest's own preview for text files Quick Look shows nothing for (YAML,
//! TOML...): a syntax-highlighted panel over the file list. Space opens and
//! closes it, like Quick Look; ↑/↓ follow the selection. Markdown shows
//! formatted, with its source a toggle away. Text can be selected with the
//! mouse and copied (⌘A, ⌘C).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use findest_core::highlight::{self, Highlighted, Preview, Span};
use gpui::{
    ClipboardItem, Context, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveElement, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ObjectFit, ParentElement, Rgba, StatefulInteractiveElement, Styled, StyledImage,
    StyledText, Task, TextLayout, UniformListScrollHandle, Window, div, img, ListHorizontalSizingBehavior,
    prelude::FluentBuilder, px, uniform_list,
};

use crate::browser::Browser;

pub struct TextPreview {
    pub path: PathBuf,
    /// `None` while reading; an error message if the file can't be read.
    content: Option<Result<Arc<Preview>, String>>,
    /// Markdown: show the source instead of the formatted text.
    show_source: bool,
    selection: Option<Selection>,
    /// The mouse button went down on the text and is still held.
    selecting: bool,
    scroll: UniformListScrollHandle,
    _task: Task<()>,
}

/// A spot in the text: a line, and a byte offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Pos {
    line: usize,
    byte: usize,
}

/// Selected text, from where the mouse went down to where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
    anchor: Pos,
    head: Pos,
}

impl Selection {
    fn ordered(&self) -> (Pos, Pos) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// The selected bytes of line `ix`, if any.
    fn in_line(&self, ix: usize, len: usize) -> Option<std::ops::Range<usize>> {
        let (start, end) = self.ordered();
        if ix < start.line || ix > end.line {
            return None;
        }
        let from = if ix == start.line { start.byte } else { 0 };
        let to = if ix == end.line { end.byte } else { len };
        (from < to).then_some(from..to)
    }

    fn text(&self, lines: &[findest_core::highlight::Line]) -> String {
        let (start, end) = self.ordered();
        let mut out = String::new();
        for ix in start.line..=end.line.min(lines.len().saturating_sub(1)) {
            let text = &lines[ix].text;
            if ix > start.line {
                out.push('\n');
            }
            let from = if ix == start.line { start.byte.min(text.len()) } else { 0 };
            let to = if ix == end.line { end.byte.min(text.len()) } else { text.len() };
            out.push_str(&text[from..to.max(from)]);
        }
        out
    }
}

impl TextPreview {
    /// The text shown: Markdown formatted unless its source is asked for.
    fn shown(&self) -> Option<&Highlighted> {
        let preview = self.content.as_ref()?.as_ref().ok()?;
        Some(preview.formatted.as_ref().filter(|_| !self.show_source).unwrap_or(&preview.source))
    }

    fn has_formatted(&self) -> bool {
        matches!(&self.content, Some(Ok(p)) if p.formatted.is_some())
    }
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
        let read = cx.background_executor().spawn(async move { highlight::preview_file(&file, dark) });
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
        // Keep showing the source while moving between Markdown files.
        let show_source = self.text_preview.as_ref().is_some_and(|p| p.show_source);
        self.text_preview =
            Some(TextPreview { path: path.clone(), content: None, show_source, selection: None, selecting: false, scroll, _task: task });
        cx.notify();
    }

    /// ⌘C in the preview: copies the selected text, or all of it.
    pub(crate) fn copy_preview_text(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &self.text_preview else { return };
        let Some(shown) = preview.shown() else { return };
        let text = match preview.selection {
            Some(selection) => selection.text(&shown.lines),
            None => shown.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n"),
        };
        let lines = text.lines().count().max(1);
        let what = if preview.selection.is_some() { "" } else { "all " };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.set_info(format!("Copied {what}{lines} {}", if lines == 1 { "line" } else { "lines" }), cx);
    }

    /// ⌘A in the preview.
    pub(crate) fn select_all_preview_text(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.text_preview else { return };
        let Some(last) = preview.shown().and_then(|s| s.lines.last().map(|l| l.text.len())) else { return };
        let lines = preview.shown().map_or(0, |s| s.lines.len());
        preview.selection = Some(Selection { anchor: Pos { line: 0, byte: 0 }, head: Pos { line: lines - 1, byte: last } });
        cx.notify();
    }

    /// Markdown: switches between the formatted text and its source.
    pub(crate) fn toggle_preview_source(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.text_preview else { return };
        if !preview.has_formatted() {
            return;
        }
        preview.show_source = !preview.show_source;
        preview.selection = None;
        preview.scroll = UniformListScrollHandle::new();
        cx.notify();
    }

    fn preview_mouse_down(&mut self, ix: usize, layout: &TextLayout, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.text_preview else { return };
        let Some(line) = preview.shown().and_then(|s| s.lines.get(ix)).map(|l| l.text.clone()) else { return };
        let byte = byte_at(layout, event.position, &line);
        let at = Pos { line: ix, byte };
        preview.selection = Some(match event.click_count {
            2 => {
                let (start, end) = word_at(&line, byte);
                Selection { anchor: Pos { line: ix, byte: start }, head: Pos { line: ix, byte: end } }
            }
            n if n >= 3 => Selection { anchor: Pos { line: ix, byte: 0 }, head: Pos { line: ix, byte: line.len() } },
            _ => match preview.selection {
                Some(s) if event.modifiers.shift => Selection { anchor: s.anchor, head: at },
                _ => Selection { anchor: at, head: at },
            },
        });
        preview.selecting = true;
        cx.notify();
    }

    fn preview_mouse_move(&mut self, ix: usize, layout: &TextLayout, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.text_preview else { return };
        if !preview.selecting {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            preview.selecting = false;
            return;
        }
        let Some(line) = preview.shown().and_then(|s| s.lines.get(ix)).map(|l| l.text.clone()) else { return };
        let head = Pos { line: ix, byte: byte_at(layout, event.position, &line) };
        if let Some(selection) = &mut preview.selection
            && selection.head != head
        {
            selection.head = head;
            cx.notify();
        }
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
        if let Some(text) = preview.shown() {
            subtitle = format!("{} · {} lines", text.syntax, text.lines.len());
            notes = text.notes.clone();
        }
        let toggle = preview.has_formatted().then(|| {
            let label = if preview.show_source { "Formatted" } else { "Source" };
            div()
                .id("text-preview-source")
                .flex_none()
                .px_2()
                .py(px(2.))
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .text_size(px(font_size - 1.))
                .hover(|s| s.bg(theme.hover))
                .tooltip(crate::render::tooltip("Show the formatted text or its source (⌘⇧M)"))
                .on_click(cx.listener(|this, _, _, cx| this.toggle_preview_source(cx)))
                .child(label)
        });

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
            .child(div().flex_1().text_color(theme.text_muted).text_size(px(font_size - 1.)).child(subtitle))
            .children(toggle);

        let gutter = theme.text_muted;
        let selected_bg = theme.selection.opacity(0.4);
        let body = match &preview.content {
            None => div().p_4().text_color(theme.text_muted).child("Reading…").into_any_element(),
            Some(Err(message)) => div().p_4().text_color(theme.danger).child(message.clone()).into_any_element(),
            Some(Ok(_)) => {
                let text = Arc::new(preview.shown().expect("read").clone());
                let digits = text.lines.len().max(1).to_string().len();
                let gutter_width = digits as f32 * font_size * 0.62 + 16.;
                // Long lines scroll sideways; the longest one sets the width.
                let widest = text.lines.iter().enumerate().max_by_key(|(_, l)| l.text.chars().count()).map(|(ix, _)| ix);
                uniform_list(
                    "text-preview-lines",
                    text.lines.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let selection = this.text_preview.as_ref().and_then(|p| p.selection);
                        range
                            .map(|ix| {
                                let line = &text.lines[ix];
                                let selected = selection.and_then(|s| s.in_line(ix, line.text.len()));
                                let styled = StyledText::new(line.text.clone())
                                    .with_highlights(highlight_runs(&line.spans, selected, selected_bg));
                                let layout = styled.layout().clone();
                                let move_layout = layout.clone();
                                div()
                                    .id(ix)
                                    .h(px(line_height))
                                    .cursor_text()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                            this.preview_mouse_down(ix, &layout, event, cx)
                                        }),
                                    )
                                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                                        this.preview_mouse_move(ix, &move_layout, event, cx)
                                    }))
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
                                    .child(styled)
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

/// Colors and selection for one line, as the non-overlapping runs
/// `StyledText` takes: the spans are cut where the selection starts and ends.
fn highlight_runs(spans: &[Span], selected: Option<std::ops::Range<usize>>, selected_bg: Hsla) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    let style = |span: &Span| {
        let [r, g, b] = span.rgb;
        HighlightStyle {
            color: Some(Rgba { r: r as f32 / 255., g: g as f32 / 255., b: b as f32 / 255., a: 1. }.into()),
            font_weight: span.bold.then_some(FontWeight::BOLD),
            font_style: span.italic.then_some(FontStyle::Italic),
            ..Default::default()
        }
    };
    let Some(sel) = selected else {
        return spans.iter().map(|s| (s.range.clone(), style(s))).collect();
    };
    let mut cuts: Vec<usize> = spans.iter().flat_map(|s| [s.range.start, s.range.end]).chain([sel.start, sel.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|w| {
            let piece = w[0]..w[1];
            let inside = sel.start <= piece.start && piece.end <= sel.end;
            let span = spans.iter().find(|s| s.range.start <= piece.start && piece.end <= s.range.end);
            let mut style = match span {
                Some(span) => style(span),
                None if inside => HighlightStyle::default(),
                None => return None,
            };
            if inside {
                style.background_color = Some(selected_bg);
            }
            Some((piece, style))
        })
        .collect()
}

/// The byte of `line` nearest to `position` (a window position).
fn byte_at(layout: &TextLayout, position: gpui::Point<gpui::Pixels>, line: &str) -> usize {
    let byte = match layout.index_for_position(position) {
        Ok(ix) | Err(ix) => ix.min(line.len()),
    };
    (0..=byte).rev().find(|&b| line.is_char_boundary(b)).unwrap_or(0)
}

/// The word at `byte` (letters, digits and `_`), or the one character there.
fn word_at(line: &str, byte: usize) -> (usize, usize) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    // Past the end: the last character.
    let byte = if byte >= line.len() { line.char_indices().last().map_or(0, |(i, _)| i) } else { byte };
    let Some(here) = line[byte..].chars().next() else { return (0, 0) };
    if !is_word(here) {
        return (byte, byte + here.len_utf8());
    }
    let start = line[..byte].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map_or(byte, |(i, _)| i);
    let end = line[byte..].char_indices().find(|(_, c)| !is_word(*c)).map_or(line.len(), |(i, _)| byte + i);
    (start, end)
}

/// True if `path` gets Findest's text preview instead of Quick Look.
pub fn handles(path: &Path) -> bool {
    highlight::has_own_preview(path) && !path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use findest_core::highlight::Line;

    fn line(text: &str, spans: Vec<Span>) -> Line {
        Line { text: text.into(), spans }
    }

    fn span(range: std::ops::Range<usize>) -> Span {
        Span { range, rgb: [1, 2, 3], bold: false, italic: false }
    }

    #[test]
    fn copies_the_selected_text_either_way() {
        let lines = [line("first line", vec![]), line("", vec![]), line("third", vec![])];
        let down = Selection { anchor: Pos { line: 0, byte: 6 }, head: Pos { line: 2, byte: 3 } };
        assert_eq!(down.text(&lines), "line\n\nthi");
        let up = Selection { anchor: down.head, head: down.anchor };
        assert_eq!(up.text(&lines), "line\n\nthi");
        assert_eq!(down.in_line(0, 10), Some(6..10));
        assert_eq!(down.in_line(1, 0), None);
        assert_eq!(down.in_line(2, 5), Some(0..3));
        assert_eq!(down.in_line(3, 5), None);
    }

    #[test]
    fn cuts_runs_at_the_selection() {
        let bg = Hsla::default();
        let runs = highlight_runs(&[span(0..4), span(6..10)], Some(2..8), bg);
        let ranges: Vec<_> = runs.iter().map(|(r, s)| (r.clone(), s.background_color.is_some(), s.color.is_some())).collect();
        assert_eq!(
            ranges,
            [(0..2, false, true), (2..4, true, true), (4..6, true, false), (6..8, true, true), (8..10, false, true)]
        );
        assert_eq!(highlight_runs(&[span(0..4)], None, bg).len(), 1);
    }

    #[test]
    fn finds_words() {
        assert_eq!(word_at("key: some_value", 7), (5, 15));
        assert_eq!(word_at("key: some_value", 0), (0, 3));
        assert_eq!(word_at("a: b", 1), (1, 2), "punctuation selects itself");
        assert_eq!(word_at("héllo wörld", 8), (7, 13));
        assert_eq!(word_at("end", 3), (0, 3), "past the end: the last word");
        assert_eq!(word_at("", 0), (0, 0));
    }
}
