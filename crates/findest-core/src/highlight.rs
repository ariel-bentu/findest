//! Syntax-highlighted text for Findest's own preview, for text files Quick
//! Look can't show (YAML, TOML...).

use std::path::Path;
use std::sync::LazyLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// Extensions previewed by Findest instead of Quick Look, which shows
/// nothing for them out of the box.
pub const OWN_PREVIEW_EXTENSIONS: &[&str] =
    &["yaml", "yml", "toml", "ini", "cfg", "conf", "env", "properties", "tf", "hcl", "gradle", "proto"];

/// Files larger than this are cut, and the preview says so.
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINES: usize = 20_000;
/// Longer lines (e.g. base64 blobs) are cut: highlighting them is slow and they
/// can't be read anyway.
const MAX_LINE_CHARS: usize = 2_000;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);

/// A run of text in one color: a byte range of its line and an sRGB color.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub range: std::ops::Range<usize>,
    pub rgb: [u8; 3],
    pub bold: bool,
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
}

pub fn has_own_preview(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| OWN_PREVIEW_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Reads and highlights `path`. `dark` picks the color theme.
pub fn highlight_file(path: &Path, dark: bool) -> std::io::Result<Highlighted> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    file.take(MAX_BYTES as u64).read_to_end(&mut bytes)?;
    let mut highlighted = highlight(&String::from_utf8_lossy(&bytes), path, dark);
    if size > MAX_BYTES as u64 {
        highlighted.notes.insert(0, format!("Showing the first {} MB of {}", MAX_BYTES / (1024 * 1024), crate::listing::format_size(size)));
    }
    Ok(highlighted)
}

pub fn highlight(text: &str, path: &Path, dark: bool) -> Highlighted {
    let syntax = syntax_for(path, text);
    let theme = theme(dark);
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    let mut notes = Vec::new();
    let mut cut_lines = 0;
    let mut failed = None;
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
        let spans = match highlighter.highlight_line(raw, &SYNTAXES) {
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
    Highlighted { lines, syntax: syntax.name.clone(), notes }
}

fn syntax_for(path: &Path, text: &str) -> &'static SyntaxReference {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let by_ext = |ext: &str| SYNTAXES.find_syntax_by_extension(ext);
    // Grammars missing from syntect's defaults borrow a close one.
    let fallback = match ext.to_ascii_lowercase().as_str() {
        "toml" | "cfg" | "conf" | "env" | "properties" | "tf" | "hcl" => by_ext("ini").or_else(|| by_ext("sh")),
        "gradle" => by_ext("groovy").or_else(|| by_ext("java")),
        "proto" => by_ext("c"),
        _ => None,
    };
    by_ext(ext)
        .or_else(|| by_ext(&ext.to_ascii_lowercase()))
        .or(fallback)
        .or_else(|| SYNTAXES.find_syntax_by_first_line(text.lines().next().unwrap_or("")))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

fn theme(dark: bool) -> &'static Theme {
    let name = if dark { "base16-ocean.dark" } else { "InspiredGitHub" };
    THEMES.themes.get(name).or_else(|| THEMES.themes.values().next()).expect("syntect ships themes")
}

fn spans(regions: &[(syntect::highlighting::Style, &str)], len: usize) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut at = 0;
    for (style, piece) in regions {
        let end = (at + piece.len()).min(len);
        if end > at {
            let c = style.foreground;
            let bold = style.font_style.contains(syntect::highlighting::FontStyle::BOLD);
            spans.push(Span { range: at..end, rgb: [c.r, c.g, c.b], bold });
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
        assert_eq!(h.notes, vec!["1 line longer than 2,000 characters is cut"]);

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
        assert!(!has_own_preview(Path::new("/a/b.json")), "Quick Look shows JSON itself");
    }
}
