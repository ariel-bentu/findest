//! Findest's own preview for text files Quick Look shows nothing for (YAML,
//! TOML...): a syntax-highlighted panel over the file list. Space opens and
//! closes it, like Quick Look; ↑/↓ follow the selection. Markdown shows
//! formatted, with its source a toggle away. Text can be selected with the
//! mouse and copied (⌘A, ⌘C), and searched (⌘F, then ⌘G or Enter for the
//! next match). Blocks fold from chevrons by the line numbers. PDFs show
//! their pages, drawn by macOS as they scroll into view.

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use findest_core::highlight::{self, Highlighted, Preview, Span};
use gpui::{
    AppContext, ClipboardItem, Context, Entity, Focusable, FontStyle, FontWeight, HighlightStyle, Hsla,
    InteractiveElement, IntoElement, ListAlignment, ListState, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, ParentElement, Rgba,
    ScrollStrategy, StatefulInteractiveElement, Styled, StyledImage, StyledText, Subscription, Task, TextLayout,
    RenderImage, UniformListScrollHandle, Window, div, hsla, img, list, ListHorizontalSizingBehavior, prelude::FluentBuilder,
    px, uniform_list,
};

use crate::browser::Browser;
use crate::text_input::{TextInput, TextInputEvent};

/// Matches counted at most; past this the count says "10,000+".
const MAX_MATCHES: usize = 10_000;
/// Drawn PDF pages kept; the ones shown longest ago go first.
const MAX_PDF_IMAGES: usize = 16;

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
    find: Option<Find>,
    /// First lines of the folds that are collapsed.
    folded: BTreeSet<usize>,
    /// The mouse is over the line numbers (fold chevrons show then).
    gutter_hover: bool,
    /// A PDF's pages, once its page sizes are read.
    pdf: Option<PdfPages>,
    /// An image preview (used on platforms without a native Quick Look panel).
    image: Option<ImagePreview>,
    _task: Task<()>,
}

struct ImagePreview {
    image: Arc<gpui::Image>,
    dimensions: (u32, u32),
}

/// A PDF's pages, drawn as they scroll into view.
struct PdfPages {
    /// Each page's size in points.
    sizes: Vec<(f32, f32)>,
    list: ListState,
    /// Drawn pages: page, pixel width and image, the one shown last at the end.
    images: Vec<(usize, u32, Arc<RenderImage>)>,
    /// Pages being drawn (or that couldn't be).
    drawing: HashSet<usize>,
}

impl Drop for PdfPages {
    fn drop(&mut self) {
        for (_, _, image) in self.images.drain(..) {
            crate::pdf::retire(image);
        }
    }
}

/// The find bar (⌘F) and what it found in the text shown.
struct Find {
    input: Entity<TextInput>,
    /// (line, bytes), in order.
    matches: Vec<(usize, Range<usize>)>,
    /// More matched than `MAX_MATCHES`.
    capped: bool,
    /// Index into `matches` of the one scrolled to.
    current: usize,
    _subscriptions: Vec<Subscription>,
}

/// Line `ix`'s matches, each with whether it's the `current` one.
fn matches_in_line(matches: &[(usize, Range<usize>)], current: usize, ix: usize) -> Vec<(Range<usize>, bool)> {
    let start = matches.partition_point(|(line, _)| *line < ix);
    matches[start..]
        .iter()
        .take_while(|(line, _)| *line == ix)
        .enumerate()
        .map(|(i, (_, range))| (range.clone(), start + i == current))
        .collect()
}

/// Where `query` occurs in `lines`, ignoring case; at most `MAX_MATCHES`
/// (the bool says more were left out).
fn find_matches(lines: &[findest_core::highlight::Line], query: &str) -> (Vec<(usize, Range<usize>)>, bool) {
    let Ok(re) = regex::RegexBuilder::new(&regex::escape(query)).case_insensitive(true).build() else {
        return (Vec::new(), false);
    };
    let mut matches = Vec::new();
    for (ix, line) in lines.iter().enumerate() {
        for m in re.find_iter(&line.text) {
            if matches.len() == MAX_MATCHES {
                return (matches, true);
            }
            matches.push((ix, m.range()));
        }
    }
    (matches, false)
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
    pub fn is_pdf(&self) -> bool {
        crate::pdf::is_pdf(&self.path)
    }

    /// The text shown: Markdown formatted unless its source is asked for.
    fn shown(&self) -> Option<&Highlighted> {
        let preview = self.content.as_ref()?.as_ref().ok()?;
        Some(preview.formatted.as_ref().filter(|_| !self.show_source).unwrap_or(&preview.source))
    }

    fn has_formatted(&self) -> bool {
        matches!(&self.content, Some(Ok(p)) if p.formatted.is_some())
    }

    /// The last line hidden by the fold starting at `line`, if one does.
    fn fold_at(&self, line: usize) -> Option<usize> {
        let folds = &self.shown()?.folds;
        folds.binary_search_by_key(&line, |f| f.0).ok().map(|i| folds[i].1)
    }

    /// The lines shown, in order, when some are folded away.
    fn rows(&self) -> Option<Vec<usize>> {
        if self.folded.is_empty() {
            return None;
        }
        let count = self.shown()?.lines.len();
        let mut rows = Vec::new();
        let mut ix = 0;
        while ix < count {
            rows.push(ix);
            ix = match self.fold_at(ix) {
                Some(end) if self.folded.contains(&ix) => end + 1,
                _ => ix + 1,
            };
        }
        Some(rows)
    }

    /// Unfolds whatever hides `line`, and returns the row it's shown in.
    fn reveal(&mut self, line: usize) -> usize {
        let hiding: Vec<usize> =
            self.folded.iter().copied().filter(|&start| start < line && self.fold_at(start).is_some_and(|end| line <= end)).collect();
        for start in hiding {
            self.folded.remove(&start);
        }
        self.rows().map_or(line, |rows| rows.partition_point(|&l| l < line))
    }

    fn toggle_fold(&mut self, line: usize) {
        if !self.folded.remove(&line) {
            self.folded.insert(line);
            // A selection end out of sight would be copied unseen; drop it.
            self.selection = None;
        }
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
            Some(path) if handles_sniffed(&path) => {
                self.open_text_preview(path, cx);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn open_text_preview(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let dark = self.theme.background.l < 0.5;
        let file = path.clone();
        let opened = path.clone();
        let task = if crate::pdf::is_pdf(&path) {
            let read = cx.background_executor().spawn(async move { crate::pdf::page_sizes(&file) });
            cx.spawn(async move |this, cx| {
                let path = opened;
                let result = read.await;
                this.update(cx, |this, cx| {
                    let Some(preview) = this.text_preview.as_mut().filter(|p| p.path == path) else { return };
                    match result {
                        Ok(sizes) => {
                            let list = ListState::new(sizes.len(), ListAlignment::Top, px(1000.));
                            preview.pdf = Some(PdfPages { sizes, list, images: Vec::new(), drawing: HashSet::new() });
                        }
                        Err(e) => preview.content = Some(Err(format!("Can't show {}: {e}", findest_core::paths::display_name(&path)))),
                    }
                    cx.notify();
                })
                .ok();
            })
        } else if is_image(&path) {
            let read = cx.background_executor().spawn(async move {
                let dims = crate::system::image_dimensions(&file).ok_or_else(|| "not a readable image".to_string())?;
                let png = crate::system::image_preview_png(&file, 2048.0).ok_or_else(|| "can't decode image".to_string())?;
                Ok::<_, String>((Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)), dims))
            });
            cx.spawn(async move |this, cx| {
                let path = opened;
                let result = read.await;
                this.update(cx, |this, cx| {
                    let Some(preview) = this.text_preview.as_mut().filter(|p| p.path == path) else { return };
                    match result {
                        Ok((image, dimensions)) => preview.image = Some(ImagePreview { image, dimensions }),
                        Err(e) => preview.content = Some(Err(format!("Can't show {}: {e}", findest_core::paths::display_name(&path)))),
                    }
                    cx.notify();
                })
                .ok();
            })
        } else {
            self.open_text_task(file, opened, dark, cx)
        };
        self.install_text_preview(path, task, cx);
    }

    fn open_text_task(&mut self, file: PathBuf, opened: PathBuf, dark: bool, cx: &mut Context<Self>) -> Task<()> {
        let read = cx.background_executor().spawn(async move { highlight::preview_file(&file, dark) });
        cx.spawn(async move |this, cx| {
            let path = opened;
            let result = read.await;
            this.update(cx, |this, cx| {
                if let Some(preview) = this.text_preview.as_mut().filter(|p| p.path == path) {
                    preview.content = Some(result.map(Arc::new).map_err(|e| format!("Can't read {}: {e}", findest_core::paths::display_name(&path))));
                    this.refresh_preview_find(cx);
                    cx.notify();
                }
            })
            .ok();
        })
    }

    fn install_text_preview(&mut self, path: PathBuf, task: Task<()>, cx: &mut Context<Self>) {
        let scroll = UniformListScrollHandle::new();
        // Keep showing the source, and searching, while moving between files.
        let show_source = self.text_preview.as_ref().is_some_and(|p| p.show_source);
        let find = self.text_preview.as_mut().and_then(|p| p.find.take());
        self.text_preview = Some(TextPreview {
            path: path.clone(),
            content: None,
            show_source,
            selection: None,
            selecting: false,
            scroll,
            find,
            folded: BTreeSet::new(),
            gutter_hover: false,
            pdf: None,
            image: None,
            _task: task,
        });
        cx.notify();
    }

    /// ⌘F in the preview: opens the find bar, or selects what's in it.
    pub(crate) fn open_preview_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.text_preview.as_mut().filter(|p| !p.is_pdf() && p.image.is_none()) else { return };
        if let Some(find) = &preview.find {
            let input = find.input.clone();
            input.update(cx, |input, cx| {
                let len = input.text().len();
                input.set_text(input.text().to_string(), 0..len, cx);
            });
            window.focus(&input.focus_handle(cx));
            return;
        }
        let theme = self.theme.clone();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(cx);
            input.cursor_color = theme.accent;
            input.selection_color = theme.accent.opacity(0.35);
            input.placeholder_color = theme.text_muted;
            input.emit_vertical = true;
            input.set_placeholder("Find in file");
            input
        });
        let subscriptions = vec![
            cx.observe(&input, |this, _, cx| this.refresh_preview_find(cx)),
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Confirm | TextInputEvent::Down => this.step_preview_find(1, cx),
                TextInputEvent::Up => this.step_preview_find(-1, cx),
                TextInputEvent::Cancel => this.close_preview_find(window, cx),
                TextInputEvent::Tab => {}
            }),
        ];
        window.focus(&input.focus_handle(cx));
        preview.find = Some(Find { input, matches: Vec::new(), capped: false, current: 0, _subscriptions: subscriptions });
        cx.notify();
    }

    pub(crate) fn close_preview_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(preview) = &mut self.text_preview
            && preview.find.take().is_some()
        {
            window.focus(&self.focus);
            cx.notify();
        }
    }

    /// The find field has the keyboard (so keys go to it, not to shortcuts).
    pub(crate) fn preview_find_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        self.text_preview.as_ref().and_then(|p| p.find.as_ref()).is_some_and(|f| f.input.focus_handle(cx).is_focused(window))
    }

    /// Searches the text shown for what's in the find field, and scrolls to the first match.
    fn refresh_preview_find(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.text_preview else { return };
        let Some(find) = &preview.find else { return };
        let query = find.input.read(cx).text().to_string();
        let (matches, capped) = match preview.shown() {
            Some(shown) if !query.is_empty() => find_matches(&shown.lines, &query),
            _ => (Vec::new(), false),
        };
        if let Some(&(line, _)) = matches.first() {
            let row = preview.reveal(line);
            preview.scroll.scroll_to_item(row, ScrollStrategy::Center);
        }
        if let Some(find) = &mut preview.find {
            (find.matches, find.capped, find.current) = (matches, capped, 0);
        }
        cx.notify();
    }

    /// The next (`1`) or previous (`-1`) match, wrapping around.
    pub(crate) fn step_preview_find(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.text_preview else { return };
        let Some(find) = &mut preview.find else { return };
        if find.matches.is_empty() {
            return;
        }
        let n = find.matches.len() as isize;
        find.current = (find.current as isize + delta).rem_euclid(n) as usize;
        let line = find.matches[find.current].0;
        let row = preview.reveal(line);
        preview.scroll.scroll_to_item(row, ScrollStrategy::Center);
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
        preview.folded.clear();
        preview.scroll = UniformListScrollHandle::new();
        self.refresh_preview_find(cx);
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
        let in_gutter = event.position.x < layout.bounds().left();
        if preview.gutter_hover != in_gutter {
            preview.gutter_hover = in_gutter;
            cx.notify();
        }
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

    /// Page `ix` of the open PDF, drawn about `width` pixels wide if it has
    /// been; asks for it if not (or if it was drawn at another size).
    fn pdf_page(&mut self, ix: usize, width: u32, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let preview = self.text_preview.as_mut()?;
        let path = preview.path.clone();
        let pages = preview.pdf.as_mut()?;
        let image = pages.images.iter().position(|(page, ..)| *page == ix).map(|pos| {
            let entry = pages.images.remove(pos);
            pages.images.push(entry.clone());
            entry
        });
        let stale = image.as_ref().is_none_or(|(_, drawn, _)| drawn.abs_diff(width) * 10 > width);
        if stale && pages.drawing.insert(ix) {
            let file = path.clone();
            let draw = cx.background_executor().spawn(async move { crate::pdf::render_page(&file, ix, width) });
            cx.spawn(async move |this, cx| {
                let image = draw.await;
                this.update(cx, |this, cx| {
                    let Some(pages) = this.text_preview.as_mut().filter(|p| p.path == path).and_then(|p| p.pdf.as_mut()) else { return };
                    // A page that can't be drawn stays in `drawing`, so it isn't asked for again.
                    let Some(image) = image else { return };
                    pages.drawing.remove(&ix);
                    if let Some(pos) = pages.images.iter().position(|(page, ..)| *page == ix) {
                        crate::pdf::retire(pages.images.remove(pos).2);
                    }
                    pages.images.push((ix, width, Arc::new(image)));
                    while pages.images.len() > MAX_PDF_IMAGES {
                        crate::pdf::retire(pages.images.remove(0).2);
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        image.map(|(_, _, image)| image)
    }

    fn render_pdf_pages(&self, pages: &PdfPages, cx: &mut Context<Self>) -> gpui::AnyElement {
        let sizes = Arc::new(pages.sizes.clone());
        let last = sizes.len().saturating_sub(1);
        let paper = gpui::white();
        list(
            pages.list.clone(),
            cx.processor(move |this, ix: usize, window, cx| {
                // The panel is inset 48pt from the window's sides; pages get its
                // width less a margin, up to twice their own size.
                let room = f32::from(window.viewport_size().width) - 2. * 48. - 2. - 32.;
                let (pw, ph) = sizes[ix];
                let width = room.min(pw * 2.).max(48.);
                let height = width * ph / pw;
                let pixels = (width * window.scale_factor()).round() as u32;
                let image = this.pdf_page(ix, pixels, cx);
                div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .pt_4()
                    .when(ix == last, |d| d.pb_4())
                    .child(
                        div()
                            .w(px(width))
                            .h(px(height))
                            .flex_none()
                            .bg(paper)
                            .shadow_md()
                            .children(image.map(|image| img(image).size_full())),
                    )
                    .into_any_element()
            }),
        )
        .size_full()
        .into_any_element()
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
            Some(path) if handles_sniffed(&path) => self.open_text_preview(path, cx),
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
        } else if let Some(pages) = &preview.pdf {
            let n = pages.sizes.len();
            subtitle = format!("PDF · {n} {}", if n == 1 { "page" } else { "pages" });
        } else if let Some(img_prev) = &preview.image {
            subtitle = format!("Image · {} × {}", img_prev.dimensions.0, img_prev.dimensions.1);
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
                .tooltip(crate::render::tooltip(if cfg!(target_os = "macos") {
                    "Show the formatted text or its source (⌘⇧M)"
                } else {
                    "Show the formatted text or its source (Ctrl+Shift+M)"
                }))
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
            .on_click(cx.listener(|this, _, window, cx| {
                this.text_preview = None;
                // The find field may have had the keyboard.
                window.focus(&this.focus);
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

        let find_bar = preview.find.as_ref().map(|find| {
            let count = match (find.matches.len(), find.capped) {
                (0, _) if find.input.read(cx).text().is_empty() => String::new(),
                (0, _) => "No matches".to_string(),
                (n, true) => format!("{} of {}+", find.current + 1, findest_core::info::group_digits(n as u64)),
                (n, false) => format!("{} of {}", find.current + 1, findest_core::info::group_digits(n as u64)),
            };
            let button = |id: &'static str, label: &'static str, tip: &'static str, delta: isize| {
                div()
                    .id(id)
                    .px_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|s| s.bg(theme.hover))
                    .tooltip(crate::render::tooltip(tip))
                    .on_click(cx.listener(move |this, _, _, cx| this.step_preview_find(delta, cx)))
                    .child(label)
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py(px(2.))
                        .rounded_md()
                        .border_1()
                        .border_color(theme.accent)
                        .bg(theme.background)
                        .child(find.input.clone()),
                )
                .child(div().flex_none().text_size(px(font_size - 1.)).text_color(theme.text_muted).child(count))
                .child(button(
                    "text-preview-find-prev",
                    "↑",
                    if cfg!(target_os = "macos") { "Previous match (⌘⇧G or ↑)" } else { "Previous match (Ctrl+Shift+G or ↑)" },
                    -1,
                ))
                .child(button(
                    "text-preview-find-next",
                    "↓",
                    if cfg!(target_os = "macos") { "Next match (⌘G, Enter or ↓)" } else { "Next match (Ctrl+G, Enter or ↓)" },
                    1,
                ))
                .child(
                    div()
                        .id("text-preview-find-close")
                        .px_2()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(theme.text_muted)
                        .hover(|s| s.bg(theme.hover))
                        .tooltip(crate::render::tooltip("Close the find bar (Esc)"))
                        .on_click(cx.listener(|this, _, window, cx| this.close_preview_find(window, cx)))
                        .child("✕"),
                )
        });

        let gutter = theme.text_muted;
        let marks = Marks {
            selected: theme.selection.opacity(0.4),
            found: hsla(0.12, 0.95, 0.55, 0.3),
            current: hsla(0.12, 0.95, 0.55, 0.75),
        };
        let body = match &preview.content {
            None if let Some(pages) = &preview.pdf => self.render_pdf_pages(pages, cx),
            None if let Some(img_prev) = &preview.image => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(img(img_prev.image.clone()).max_w_full().max_h_full().object_fit(ObjectFit::Contain))
                .into_any_element(),
            None => div().p_4().text_color(theme.text_muted).child("Reading…").into_any_element(),
            Some(Err(message)) => div().p_4().text_color(theme.danger).child(message.clone()).into_any_element(),
            Some(Ok(_)) => {
                let text = Arc::new(preview.shown().expect("read").clone());
                let digits = text.lines.len().max(1).to_string().len();
                let gutter_width = digits as f32 * font_size * 0.62 + 8.;
                let rows = preview.rows().map(Arc::new);
                let count = rows.as_ref().map_or(text.lines.len(), |r| r.len());
                // Long lines scroll sideways; the longest one sets the width.
                let widest = (0..count)
                    .max_by_key(|&row| text.lines[rows.as_ref().map_or(row, |r| r[row])].text.chars().count());
                let fold_color = theme.text_muted;
                let badge_bg = theme.hover;
                uniform_list(
                    "text-preview-lines",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                        let Some(preview) = this.text_preview.as_ref() else { return Vec::new() };
                        let selection = preview.selection;
                        let find = preview.find.as_ref();
                        range
                            .map(|row| {
                                let ix = rows.as_ref().map_or(row, |r| r[row]);
                                let line = &text.lines[ix];
                                let selected = selection.and_then(|s| s.in_line(ix, line.text.len()));
                                let found = find.map(|f| matches_in_line(&f.matches, f.current, ix)).unwrap_or_default();
                                let styled = StyledText::new(line.text.clone())
                                    .with_highlights(highlight_runs(&line.spans, selected, &found, &marks));
                                let layout = styled.layout().clone();
                                let move_layout = layout.clone();
                                let folded = preview.folded.contains(&ix);
                                let chevron = preview.fold_at(ix).map(|_| {
                                    div()
                                        .id(("fold", ix))
                                        .size_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .when(!folded && !preview.gutter_hover, |d| d.invisible())
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                                cx.stop_propagation();
                                                if let Some(preview) = &mut this.text_preview {
                                                    preview.toggle_fold(ix);
                                                    cx.notify();
                                                }
                                            }),
                                        )
                                        .child(if folded { "▸" } else { "▾" })
                                });
                                let badge = folded.then(|| {
                                    div()
                                        .id(("folded", ix))
                                        .flex_none()
                                        .ml_2()
                                        .px_1()
                                        .rounded_sm()
                                        .bg(badge_bg)
                                        .text_color(fold_color)
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                                                cx.stop_propagation();
                                                if let Some(preview) = &mut this.text_preview {
                                                    preview.toggle_fold(ix);
                                                    cx.notify();
                                                }
                                            }),
                                        )
                                        .child("⋯")
                                });
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
                                            .flex()
                                            .justify_end()
                                            .text_color(gutter)
                                            .child((ix + 1).to_string()),
                                    )
                                    .child(
                                        div()
                                            .w(px(font_size * 1.4))
                                            .flex_none()
                                            .mr_1()
                                            .text_color(fold_color)
                                            .children(chevron),
                                    )
                                    .child(styled)
                                    .children(badge)
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
                .children(find_bar)
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
                .child(
                    div()
                        .id("text-preview-body")
                        .flex_1()
                        .min_h_0()
                        .font_family(crate::render::monospace_font())
                        .text_size(px(font_size - 1.))
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            if let Some(preview) = &mut this.text_preview
                                && !hovered
                                && preview.gutter_hover
                            {
                                preview.gutter_hover = false;
                                cx.notify();
                            }
                        }))
                        .child(body),
                ),
        )
    }
}

/// Backgrounds for selected text and find matches.
struct Marks {
    selected: Hsla,
    found: Hsla,
    /// The match scrolled to.
    current: Hsla,
}

/// Colors, selection and find matches for one line, as the non-overlapping
/// runs `StyledText` takes: the spans are cut wherever a mark starts or ends.
fn highlight_runs(
    spans: &[Span],
    selected: Option<Range<usize>>,
    found: &[(Range<usize>, bool)],
    marks: &Marks,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let style = |span: &Span| {
        let [r, g, b] = span.rgb;
        HighlightStyle {
            color: Some(Rgba { r: r as f32 / 255., g: g as f32 / 255., b: b as f32 / 255., a: 1. }.into()),
            font_weight: span.bold.then_some(FontWeight::BOLD),
            font_style: span.italic.then_some(FontStyle::Italic),
            ..Default::default()
        }
    };
    if selected.is_none() && found.is_empty() {
        return spans.iter().map(|s| (s.range.clone(), style(s))).collect();
    }
    let within = |piece: &Range<usize>, r: &Range<usize>| r.start <= piece.start && piece.end <= r.end;
    let mut cuts: Vec<usize> = spans
        .iter()
        .map(|s| &s.range)
        .chain(selected.iter())
        .chain(found.iter().map(|(r, _)| r))
        .flat_map(|r| [r.start, r.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|w| {
            let piece = w[0]..w[1];
            // The selection shows over matches, the current match over the others.
            let background = if selected.as_ref().is_some_and(|r| within(&piece, r)) {
                Some(marks.selected)
            } else {
                found.iter().find(|(r, _)| within(&piece, r)).map(|(_, current)| if *current { marks.current } else { marks.found })
            };
            let span = spans.iter().find(|s| within(&piece, &s.range));
            let mut style = match span {
                Some(span) => style(span),
                None if background.is_some() => HighlightStyle::default(),
                None => return None,
            };
            style.background_color = background;
            Some((piece, style))
        })
        .collect()
}

/// The byte of `line` nearest to `position` (a window position).
fn byte_at(layout: &TextLayout, position: gpui::Point<gpui::Pixels>, line: &str) -> usize {
    // Rows don't wrap, so only x matters; gpui reads a row's bottom edge as
    // the start of the line, so ask at mid-height.
    let y = layout.bounds().top() + layout.line_height() / 2.;
    let byte = match layout.index_for_position(gpui::point(position.x, y)) {
        Ok(ix) | Err(ix) => ix.min(line.len()),
    };
    let byte = (0..=byte).rev().find(|&b| line.is_char_boundary(b)).unwrap_or(0);
    // That's the character under the pointer; the caret goes on its nearer side.
    let Some(next) = line[byte..].chars().next().map(|c| byte + c.len_utf8()) else { return byte };
    match (layout.position_for_index(byte), layout.position_for_index(next)) {
        (Some(start), Some(end)) if position.x - start.x > end.x - position.x => next,
        _ => byte,
    }
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

/// Image formats the `image` crate is built to decode (see Cargo.toml).
#[cfg(not(target_os = "macos"))]
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tiff", "tif"];

/// Whether `path` is an image handled by Findest's built-in viewer (on platforms
/// without a native Quick Look panel).
pub fn is_image(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
    }
}

/// Like [`handles`], also for text files with unknown names. Reads the file's
/// start, so only for what the user asked for, not on every frame.
pub fn handles_sniffed(path: &Path) -> bool {
    handles(path) || (cfg!(not(target_os = "macos")) && !path.is_dir() && findest_core::info::is_text_file(path))
}

/// True if `path` gets Findest's preview instead of Quick Look.
pub fn handles(path: &Path) -> bool {
    (highlight::has_own_preview(path) || crate::pdf::is_pdf(path) || is_image(path)) && !path.is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn every_listed_image_type_can_be_decoded() {
        for ext in IMAGE_EXTENSIONS {
            let format = image::ImageFormat::from_extension(ext).unwrap_or_else(|| panic!("{ext} isn't an image format"));
            assert!(format.reading_enabled(), "{ext} is listed but the image crate can't read it");
        }
        assert!(is_image(Path::new("/a/B.JPG")));
        assert!(!is_image(Path::new("/a/b.tga")));
    }
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

    fn marks() -> Marks {
        Marks { selected: hsla(0., 0., 0., 1.), found: hsla(0.1, 0., 0., 1.), current: hsla(0.2, 0., 0., 1.) }
    }

    #[test]
    fn cuts_runs_at_the_selection() {
        let runs = highlight_runs(&[span(0..4), span(6..10)], Some(2..8), &[], &marks());
        let ranges: Vec<_> = runs.iter().map(|(r, s)| (r.clone(), s.background_color.is_some(), s.color.is_some())).collect();
        assert_eq!(
            ranges,
            [(0..2, false, true), (2..4, true, true), (4..6, true, false), (6..8, true, true), (8..10, false, true)]
        );
        assert_eq!(highlight_runs(&[span(0..4)], None, &[], &marks()).len(), 1);
    }

    #[test]
    fn marks_matches_under_the_selection() {
        let m = marks();
        let runs = highlight_runs(&[span(0..10)], Some(0..2), &[(1..3, false), (5..7, true)], &m);
        let backgrounds: Vec<_> = runs.iter().map(|(r, s)| (r.clone(), s.background_color)).collect();
        assert_eq!(
            backgrounds,
            [(0..1, Some(m.selected)), (1..2, Some(m.selected)), (2..3, Some(m.found)), (3..5, None), (5..7, Some(m.current)), (7..10, None)]
        );
        let bare = highlight_runs(&[], None, &[(2..4, false)], &m);
        assert_eq!(bare.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(), [2..4], "matches in uncolored text");
    }

    #[test]
    fn finds_text_ignoring_case() {
        let lines = [line("Hello hello", vec![]), line("nothing", vec![]), line("ÉCOLE école", vec![])];
        assert_eq!(find_matches(&lines, "hello").0, [(0, 0..5), (0, 6..11)]);
        assert_eq!(find_matches(&lines, "école").0, [(2, 0..6), (2, 7..13)], "non-ASCII, by byte");
        assert_eq!(find_matches(&lines, "a.b").0, [], "taken literally");
        let (matches, _) = find_matches(&lines, "hello");
        assert_eq!(matches_in_line(&matches, 1, 0), [(0..5, false), (6..11, true)]);
        assert_eq!(matches_in_line(&matches, 1, 1), []);
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
