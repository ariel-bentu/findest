//! Markdown formatted for the text preview: headings, emphasis, lists,
//! quotes, code blocks, links and tables, as styled monospace lines (the
//! preview's `Line`s, so selecting and copying work the same). Not a full
//! renderer: no images, wrapping or proportional fonts.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;

use crate::highlight::{self, Highlighted, Line, Span};

type Rgb = [u8; 3];

struct Colors {
    text: Rgb,
    muted: Rgb,
    heading: Rgb,
    link: Rgb,
    code: Rgb,
}

impl Colors {
    fn new(dark: bool) -> Colors {
        let fg = highlight::theme(dark).settings.foreground.map_or(if dark { [192, 197, 206] } else { [36, 41, 46] }, |c| [c.r, c.g, c.b]);
        if dark {
            Colors { text: fg, muted: [128, 136, 150], heading: [229, 192, 80], link: [138, 196, 247], code: [152, 205, 120] }
        } else {
            Colors { text: fg, muted: [110, 118, 129], heading: [175, 90, 0], link: [3, 102, 214], code: [34, 134, 58] }
        }
    }
}

/// Text written at the start of each line inside a block (a quote's bar, a
/// list item's indent).
struct Prefix {
    text: String,
    rgb: Rgb,
}

#[derive(Default)]
struct Table {
    /// Cells as plain text; the first row is the header.
    rows: Vec<Vec<String>>,
}

struct Writer {
    colors: Colors,
    dark: bool,
    lines: Vec<Line>,
    line: Line,
    /// The current line has its prefixes written.
    started: bool,
    /// A blank line goes before the next one (written then, so it gets the
    /// prefixes of where it lands: inside a quote or not).
    blank_pending: bool,
    prefixes: Vec<Prefix>,
    bold: usize,
    italic: usize,
    muted: usize,
    heading: Option<HeadingLevel>,
    links: Vec<String>,
    /// Counters of the open lists (`None`: bulleted).
    lists: Vec<Option<u64>>,
    /// A code block being collected: its language and text.
    code: Option<(String, String)>,
    table: Option<Table>,
}

/// Formats `text` as Markdown.
pub fn render(text: &str, dark: bool) -> Highlighted {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut w = Writer {
        colors: Colors::new(dark),
        dark,
        lines: Vec::new(),
        line: empty_line(),
        started: false,
        blank_pending: false,
        prefixes: Vec::new(),
        bold: 0,
        italic: 0,
        muted: 0,
        heading: None,
        links: Vec::new(),
        lists: Vec::new(),
        code: None,
        table: None,
    };
    for event in Parser::new_ext(text, options) {
        w.event(event);
    }
    w.end_line();
    Highlighted { lines: w.lines, syntax: "Markdown".into(), notes: Vec::new() }
}

fn empty_line() -> Line {
    Line { text: String::new(), spans: Vec::new() }
}

impl Writer {
    fn event(&mut self, event: Event) {
        if let Some((_, code)) = &mut self.code {
            match event {
                Event::Text(text) => return code.push_str(&text),
                Event::End(TagEnd::CodeBlock | TagEnd::MetadataBlock(_)) => return self.end_code_block(),
                _ => return,
            }
        }
        if let Some(table) = &mut self.table {
            match event {
                Event::Start(Tag::TableHead | Tag::TableRow) => table.rows.push(Vec::new()),
                Event::Start(Tag::TableCell) => table.rows.last_mut().into_iter().for_each(|r| r.push(String::new())),
                Event::Text(t) | Event::Code(t) => {
                    if let Some(cell) = table.rows.last_mut().and_then(|r| r.last_mut()) {
                        cell.push_str(&t);
                    }
                }
                Event::End(TagEnd::Table) => self.end_table(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.write(&code, self.colors.code, self.bold > 0, false),
            Event::InlineMath(math) | Event::DisplayMath(math) => self.write(&math, self.colors.code, false, false),
            Event::Html(html) | Event::InlineHtml(html) => {
                let mut parts = html.split('\n').peekable();
                while let Some(part) = parts.next() {
                    self.write(part, self.colors.muted, false, false);
                    if parts.peek().is_some() {
                        self.end_line();
                    }
                }
            }
            Event::FootnoteReference(name) => self.write(&format!("[^{name}]"), self.colors.muted, false, false),
            Event::SoftBreak | Event::HardBreak => self.end_line(),
            Event::Rule => {
                self.end_line();
                self.write(&"─".repeat(40), self.colors.muted, false, false);
                self.blank();
            }
            Event::TaskListMarker(done) => self.write(if done { "☑ " } else { "☐ " }, self.colors.muted, false, false),
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.end_line();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.end_line();
                self.flush_blank();
                self.prefixes.push(Prefix { text: "│ ".into(), rgb: self.colors.muted });
                self.muted += 1;
            }
            Tag::CodeBlock(kind) => {
                self.end_line();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split([' ', ',', '{']).next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::MetadataBlock(_) => {
                self.end_line();
                self.code = Some(("yaml".into(), String::new()));
            }
            Tag::List(start) => {
                self.end_line();
                self.lists.push(start);
            }
            Tag::Item => {
                self.end_line();
                let depth = self.lists.len().saturating_sub(1);
                let bullet = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}. ", *n - 1)
                    }
                    _ => format!("{} ", ["•", "◦", "▪"][depth % 3]),
                };
                self.write(&bullet, self.colors.muted, false, false);
                self.prefixes.push(Prefix { text: " ".repeat(bullet.chars().count()), rgb: self.colors.muted });
            }
            Tag::Table(_) => {
                self.end_line();
                self.table = Some(Table::default());
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.muted += 1,
            Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
            Tag::Image { .. } => {
                self.write("[image: ", self.colors.muted, false, false);
                self.muted += 1;
            }
            Tag::FootnoteDefinition(name) => {
                self.end_line();
                self.write(&format!("[^{name}]: "), self.colors.muted, false, false);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::FootnoteDefinition => {
                // In a list, items stay together; elsewhere blocks are spaced.
                if self.lists.is_empty() { self.blank() } else { self.end_line() }
            }
            TagEnd::Heading(level) => {
                self.heading = None;
                let width = self.line.text.chars().count();
                self.end_line();
                // Underlined like setext headings, to tell the levels apart.
                match level {
                    HeadingLevel::H1 => self.write(&"═".repeat(width), self.colors.heading, false, false),
                    HeadingLevel::H2 => self.write(&"─".repeat(width), self.colors.heading, false, false),
                    _ => {}
                }
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.end_line();
                self.prefixes.pop();
                self.muted -= 1;
                self.blank();
            }
            TagEnd::List(_) => {
                self.end_line();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.end_line();
                self.prefixes.pop();
            }
            TagEnd::Emphasis => self.italic -= 1,
            TagEnd::Strong => self.bold -= 1,
            TagEnd::Strikethrough => self.muted -= 1,
            TagEnd::Link => {
                let url = self.links.pop().unwrap_or_default();
                // The address, unless the text already shows it or it's a jump within the page.
                let shown = self.line.text.ends_with(url.as_str()) || url.starts_with('#') || url.is_empty();
                if !shown {
                    self.write(&format!(" ({url})"), self.colors.muted, false, false);
                }
            }
            TagEnd::Image => {
                self.muted -= 1;
                self.write("]", self.colors.muted, false, false);
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        let c = &self.colors;
        let (rgb, bold) = if self.heading.is_some() {
            (c.heading, true)
        } else if !self.links.is_empty() {
            (c.link, self.bold > 0)
        } else if self.muted > 0 {
            (c.muted, self.bold > 0)
        } else {
            (c.text, self.bold > 0)
        };
        self.write(text, rgb, bold, self.italic > 0);
    }

    /// Appends to the current line, starting it with the prefixes first.
    fn write(&mut self, text: &str, rgb: Rgb, bold: bool, italic: bool) {
        if !self.started {
            self.flush_blank();
            self.started = true;
            self.write_prefixes();
        }
        self.append(text, rgb, bold, italic);
    }

    /// Writes the pending blank line, if any. Blocks that add a prefix call
    /// this first, so the blank line before them goes without it.
    fn flush_blank(&mut self) {
        if std::mem::take(&mut self.blank_pending) {
            self.write_prefixes();
            let trimmed = self.line.text.trim_end().len();
            self.line.text.truncate(trimmed);
            self.line.spans.retain_mut(|s| {
                s.range.end = s.range.end.min(trimmed);
                s.range.start < s.range.end
            });
            self.lines.push(std::mem::replace(&mut self.line, empty_line()));
        }
    }

    fn write_prefixes(&mut self) {
        for i in 0..self.prefixes.len() {
            let (prefix, rgb) = (self.prefixes[i].text.clone(), self.prefixes[i].rgb);
            self.append(&prefix, rgb, false, false);
        }
    }

    fn append(&mut self, text: &str, rgb: Rgb, bold: bool, italic: bool) {
        let start = self.line.text.len();
        self.line.text.push_str(text);
        if !text.is_empty() {
            self.line.spans.push(Span { range: start..self.line.text.len(), rgb, bold, italic });
        }
    }

    fn end_line(&mut self) {
        if self.started {
            self.lines.push(std::mem::replace(&mut self.line, empty_line()));
            self.started = false;
        }
    }

    /// Ends the line, with a blank line before the next.
    fn blank(&mut self) {
        self.end_line();
        self.blank_pending = !self.lines.is_empty();
    }

    fn end_code_block(&mut self) {
        let Some((lang, code)) = self.code.take() else { return };
        let syntax = highlight::syntax_by_token(&lang);
        let mut highlighter = HighlightLines::new(syntax, highlight::theme(self.dark));
        self.flush_blank();
        let bar = Prefix { text: "▎ ".into(), rgb: self.colors.muted };
        self.prefixes.push(bar);
        for raw in syntect::util::LinesWithEndings::from(&code) {
            let text = raw.trim_end_matches(['\n', '\r']);
            self.write("", self.colors.text, false, false);
            let offset = self.line.text.len();
            self.line.text.push_str(text);
            match highlighter.highlight_line(raw, &highlight::SYNTAXES) {
                Ok(regions) => self.line.spans.extend(highlight::spans(&regions, text.len()).into_iter().map(|mut s| {
                    s.range = s.range.start + offset..s.range.end + offset;
                    s
                })),
                Err(_) => self.line.spans.push(Span { range: offset..self.line.text.len(), rgb: self.colors.code, bold: false, italic: false }),
            }
            self.end_line();
        }
        self.prefixes.pop();
        if self.lists.is_empty() { self.blank() } else { self.end_line() }
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else { return };
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> =
            (0..columns).map(|c| table.rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
        let muted = self.colors.muted;
        for (i, row) in table.rows.iter().enumerate() {
            for (c, width) in widths.iter().enumerate() {
                let cell = row.get(c).map_or("", String::as_str);
                self.write(if c == 0 { "│ " } else { " │ " }, muted, false, false);
                let pad = " ".repeat(width - cell.chars().count());
                self.write(&format!("{cell}{pad}"), self.colors.text, i == 0, false);
            }
            self.write(" │", muted, false, false);
            self.end_line();
            if i == 0 {
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
                self.write(&format!("├{}┤", rule.join("┼")), muted, false, false);
                self.end_line();
            }
        }
        self.blank();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(md: &str) -> Vec<String> {
        render(md, true).lines.into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn formats_blocks() {
        let md = "# Title\n\nSome *em* and **bold** with `code`.\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n> quoted\n> text\n\n```rust\nfn main() {}\n```\n\n---\n\n- [x] done\n";
        assert_eq!(
            text(md),
            [
                "Title",
                "═════",
                "",
                "Some em and bold with code.",
                "",
                "• one",
                "• two",
                "  ◦ nested",
                "",
                "1. first",
                "2. second",
                "",
                "│ quoted",
                "│ text",
                "",
                "▎ fn main() {}",
                "",
                "────────────────────────────────────────",
                "",
                "• ☑ done",
            ]
        );
    }

    #[test]
    fn styles_inline_text() {
        let h = render("Some *em* and **bold** [link](https://x.y).\n", true);
        let line = &h.lines[0];
        assert_eq!(line.text, "Some em and bold link (https://x.y).");
        let span = |word: &str| line.spans.iter().find(|s| &line.text[s.range.clone()] == word).unwrap().clone();
        assert!(span("em").italic && !span("em").bold);
        assert!(span("bold").bold);
        assert_eq!(span("link").rgb, Colors::new(true).link);
        for s in &line.spans {
            assert!(line.text.is_char_boundary(s.range.start) && line.text.is_char_boundary(s.range.end));
        }
    }

    #[test]
    fn lines_up_tables() {
        assert_eq!(
            text("| Name | Size |\n|---|---|\n| a.txt | 1 KB |\n| b | 20 MB |\n"),
            ["│ Name  │ Size  │", "├───────┼───────┤", "│ a.txt │ 1 KB  │", "│ b     │ 20 MB │"]
        );
    }

    #[test]
    fn keeps_links_that_show_their_address() {
        assert_eq!(text("<https://x.y> and [docs](#install)"), ["https://x.y and docs"]);
    }

    #[test]
    fn shows_front_matter_as_code() {
        assert_eq!(text("---\ntitle: x\n---\n\nBody\n"), ["▎ title: x", "", "Body"]);
    }
}
