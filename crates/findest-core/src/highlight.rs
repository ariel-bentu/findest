//! Syntax-highlighted text for Findest's own preview, for text files Quick
//! Look can't show (YAML, TOML...).

use std::path::Path;
use std::sync::LazyLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Extensions previewed by Findest instead of Quick Look, which shows
/// nothing for them out of the box.
pub const OWN_PREVIEW_EXTENSIONS: &[&str] = &[
    "yaml", "yml", "toml", "ini", "cfg", "conf", "env", "properties", "tf", "hcl", "gradle", "proto", "json", "ndjson", "jsonl",
    "md", "markdown",
];
/// Shown formatted, with the source a toggle away.
const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown"];

/// Files larger than this are cut, and the preview says so.
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINES: usize = 20_000;
/// Longer lines (e.g. base64 blobs) are cut: highlighting them is slow and they
/// can't be read anyway.
const MAX_LINE_CHARS: usize = 20_000;

pub(crate) static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// A run of text in one color: a byte range of its line and an sRGB color.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub range: std::ops::Range<usize>,
    pub rgb: [u8; 3],
    pub bold: bool,
    pub italic: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub text: String,
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Highlighted {
    pub lines: Vec<Line>,
    /// e.g. "YAML", or "Plain Text" when no grammar matched.
    pub syntax: String,
    /// What was left out, if anything ("Showing the first 20,000 lines").
    pub notes: Vec<String>,
    /// Foldable regions, by first line.
    pub folds: Vec<crate::folds::Fold>,
}

/// A file as the preview shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub source: Highlighted,
    /// Markdown, formatted.
    pub formatted: Option<Highlighted>,
}

pub fn has_own_preview(path: &Path) -> bool {
    if has_extension(path, OWN_PREVIEW_EXTENSIONS) {
        return true;
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::info::is_text_file(path)
    }
    #[cfg(target_os = "macos")]
    {
        false
    }
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Reads `path` for the preview: highlighted, and formatted if it's
/// Markdown. `dark` picks the color theme.
pub fn preview_file(path: &Path, dark: bool) -> std::io::Result<Preview> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    file.take(MAX_BYTES as u64).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut source = highlight(&text, path, dark);
    let mut formatted = has_extension(path, MARKDOWN_EXTENSIONS).then(|| crate::markdown::render(&text, dark));
    if size > MAX_BYTES as u64 {
        let note = format!("Showing the first {} MB of {}", MAX_BYTES / (1024 * 1024), crate::listing::format_size(size));
        for h in std::iter::once(&mut source).chain(formatted.as_mut()) {
            h.notes.insert(0, note.clone());
        }
    }
    Ok(Preview { source, formatted })
}

pub fn highlight(text: &str, path: &Path, dark: bool) -> Highlighted {
    let syntax = syntax_for(path, text);
    let theme = theme(dark);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    let mut notes = Vec::new();
    let mut cut_lines = 0;
    let mut failed = None;
    let json = syntax.name == "JSON";
    for raw in syntect::util::LinesWithEndings::from(text) {
        if lines.len() == MAX_LINES {
            notes.push(format!("Showing the first {} lines", group_thousands(MAX_LINES)));
            break;
        }
        let mut raw = raw;
        let cut;
        if raw.chars().nth(MAX_LINE_CHARS).is_some() {
            let end = raw.char_indices().nth(MAX_LINE_CHARS).map_or(raw.len(), |(i, _)| i);
            cut = format!("{}…\n", &raw[..end]);
            raw = &cut;
            cut_lines += 1;
        }
        let text = raw.trim_end_matches(['\n', '\r']).to_string();
        // JSON skips syntect: its own coloring is cheaper on long lines.
        let result = if json { Ok(Vec::new()) } else { highlighter.highlight_line(raw, &SYNTAXES) };
        let spans = match result {
            Ok(_) if json => json_spans(&text, dark),
            Ok(regions) => spans(&regions, text.len()),
            Err(e) => {
                // Keep the text; only the colors are lost from here on.
                failed.get_or_insert_with(|| format!("Highlighting stopped at line {}: {e}", lines.len() + 1));
                Vec::new()
            }
        };
        lines.push(Line { text, spans });
        if failed.is_some() {
            highlighter = HighlightLines::new(SYNTAXES.find_syntax_plain_text(), theme);
        }
    }
    if cut_lines > 0 {
        notes.push(format!(
            "{} {} longer than {} characters {} cut",
            group_thousands(cut_lines),
            if cut_lines == 1 { "line" } else { "lines" },
            group_thousands(MAX_LINE_CHARS),
            if cut_lines == 1 { "is" } else { "are" }
        ));
    }
    notes.extend(failed);
    let folds = match syntax.name.as_str() {
        "Markdown" => crate::folds::by_headings(&crate::folds::markdown_headings(&lines), &lines),
        name => crate::folds::by_indent(&lines, name == "YAML"),
    };
    Highlighted { lines, syntax: syntax.name.clone(), notes, folds }
}

/// JSON colored like a terminal JSON viewer: keys green, string values blue,
/// braces gold. Works line by line, so it suits NDJSON and long lines.
fn json_spans(line: &str, dark: bool) -> Vec<Span> {
    let [key, string, number, literal, brace]: [[u8; 3]; 5] = if dark {
        [[152, 205, 120], [138, 196, 247], [181, 206, 168], [86, 156, 214], [229, 192, 80]]
    } else {
        [[34, 134, 58], [3, 102, 214], [9, 134, 88], [0, 0, 255], [175, 125, 0]]
    };
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut push = |range: std::ops::Range<usize>, rgb| spans.push(Span { range, rgb, bold: false, italic: false });
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(bytes.len());
                let is_key = bytes[i..].iter().find(|b| !b.is_ascii_whitespace()) == Some(&b':');
                push(start..i, if is_key { key } else { string });
            }
            b'{' | b'}' | b'[' | b']' => {
                i += 1;
                push(start..i, brace);
            }
            b'-' | b'0'..=b'9' => {
                while i < bytes.len() && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    i += 1;
                }
                push(start..i, number);
            }
            b't' | b'f' | b'n' if ["true", "false", "null"].iter().any(|w| line[i..].starts_with(w)) => {
                while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                    i += 1;
                }
                push(start..i, literal);
            }
            _ => i += 1,
        }
    }
    spans
}

fn syntax_for(path: &Path, text: &str) -> &'static SyntaxReference {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let by_ext = |ext: &str| SYNTAXES.find_syntax_by_extension(ext);
    // Grammars missing from syntect's defaults borrow a close one.
    let fallback = match ext.to_ascii_lowercase().as_str() {
        "toml" | "cfg" | "conf" | "env" | "properties" | "tf" | "hcl" | "lock" | "editorconfig" => {
            by_ext("ini").or_else(|| by_ext("sh"))
        }
        "gradle" => by_ext("groovy").or_else(|| by_ext("java")),
        "proto" | "swift" => by_ext("c"),
        "ndjson" | "jsonl" | "jsonc" => by_ext("json"),
        "ts" | "tsx" | "jsx" | "mjs" | "cjs" => by_ext("js"),
        "kt" | "kts" => by_ext("java"),
        "vue" | "svelte" => by_ext("html"),
        "scss" | "less" => by_ext("css"),
        "zsh" | "fish" | "gitignore" | "dockerignore" => by_ext("sh"),
        _ => match name.to_ascii_lowercase().as_str() {
            "dockerfile" | "containerfile" | "justfile" => by_ext("sh"),
            "makefile" => by_ext("make").or_else(|| by_ext("sh")),
            _ => None,
        },
    };
    by_ext(ext)
        .or_else(|| by_ext(&ext.to_ascii_lowercase()))
        .or(fallback)
        .or_else(|| SYNTAXES.find_syntax_by_first_line(text.lines().next().unwrap_or("")))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

/// The grammar for a code block's language ("rust", "sh"...), or plain text.
pub(crate) fn syntax_by_token(token: &str) -> &'static SyntaxReference {
    Some(token)
        .filter(|t| !t.is_empty())
        .and_then(|t| SYNTAXES.find_syntax_by_token(t))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

pub(crate) fn theme(dark: bool) -> &'static Theme {
    let name = if dark { "base16-ocean.dark" } else { "InspiredGitHub" };
    THEMES.themes.get(name).or_else(|| THEMES.themes.values().next()).expect("syntect ships themes")
}

pub(crate) fn spans(regions: &[(syntect::highlighting::Style, &str)], len: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut at = 0;
    for (style, piece) in regions {
        let end = (at + piece.len()).min(len);
        if end > at {
            let c = style.foreground;
            let bold = style.font_style.contains(syntect::highlighting::FontStyle::BOLD);
            let italic = style.font_style.contains(syntect::highlighting::FontStyle::ITALIC);
            spans.push(Span { range: at..end, rgb: [c.r, c.g, c.b], bold, italic });
        }
        at += piece.len();
    }
    spans
}

fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_yaml() {
        let h = highlight("type: Folder\nspec:\n  count: 3\n", Path::new("a.yaml"), true);
        assert_eq!(h.syntax, "YAML");
        assert_eq!(h.lines.len(), 3);
        assert_eq!(h.lines[0].text, "type: Folder");
        let line = &h.lines[0];
        let key = line.spans.iter().find(|s| s.range.start == 0).unwrap();
        let value = line.spans.iter().find(|s| line.text[s.range.clone()].contains("Folder")).unwrap();
        assert_ne!(key.rgb, value.rgb, "keys and values get different colors");
        assert!(h.notes.is_empty());
    }

    #[test]
    fn spans_cover_each_line() {
        let h = highlight("a: 1\r\nb: [x, y]\n", Path::new("a.yml"), false);
        for line in &h.lines {
            let covered: usize = line.spans.iter().map(|s| s.range.len()).sum();
            assert_eq!(covered, line.text.len(), "{line:?}");
        }
    }

    #[test]
    fn says_what_it_cut() {
        let long = format!("key: {}\n", "x".repeat(MAX_LINE_CHARS + 10));
        let h = highlight(&long, Path::new("a.yaml"), true);
        assert!(h.lines[0].text.ends_with('…'));
        assert_eq!(h.notes, vec!["1 line longer than 20,000 characters is cut"]);

        let many = "a: 1\n".repeat(MAX_LINES + 5);
        let h = highlight(&many, Path::new("a.yaml"), true);
        assert_eq!(h.lines.len(), MAX_LINES);
        assert_eq!(h.notes, vec!["Showing the first 20,000 lines"]);
    }

    #[test]
    fn falls_back_for_missing_grammars() {
        assert_ne!(highlight("[a]\nb = 1\n", Path::new("x.toml"), true).syntax, "Plain Text");
        assert_eq!(highlight("just text\n", Path::new("x.unknownext"), true).syntax, "Plain Text");
        assert!(has_own_preview(Path::new("/a/B.YAML")));
        assert!(has_own_preview(Path::new("/a/log.ndjson")));
        assert_eq!(highlight("{\"a\": 1}\n{\"a\": 2}\n", Path::new("x.ndjson"), true).syntax, "JSON");
    }

    #[test]
    fn colors_json_keys_values_and_braces() {
        let line = r#"{"id":"a\"b","n":-1.5e3,"ok":true,"x":[null]}"#;
        let spans = json_spans(line, true);
        let text = |s: &Span| &line[s.range.clone()];
        let colored: Vec<(&str, [u8; 3])> = spans.iter().map(|s| (text(s), s.rgb)).collect();
        let key = colored[1].1;
        assert_eq!(colored[0].0, "{");
        assert_eq!(colored[1].0, "\"id\"");
        assert_eq!(colored[2].0, r#""a\"b""#, "escaped quote stays inside the string");
        assert_ne!(colored[2].1, key, "values aren't colored as keys");
        assert!(colored.iter().any(|(t, _)| *t == "-1.5e3"));
        assert!(colored.iter().any(|(t, _)| *t == "true"));
        assert!(colored.iter().any(|(t, _)| *t == "null"));
    }
}
