//! Where the preview's text folds, like an editor's: nested blocks by
//! indentation (JSON, YAML, code), Markdown sections by heading.

use crate::highlight::Line;

/// A foldable region: the line that stays visible, and the last one it hides.
pub type Fold = (usize, usize);

/// A tab indents as far as this many spaces.
const TAB: usize = 4;

/// Folds where lines are indented deeper than the one before them; the
/// fold ends at the last deeper line, so a closing `}` stays visible.
/// `yaml`: a `- ` list item counts as one deeper than its key, so a list
/// at the key's indentation still folds under it.
pub fn by_indent(lines: &[Line], yaml: bool) -> Vec<Fold> {
    let mut folds = Vec::new();
    // Open blocks: their first line and indentation.
    let mut open: Vec<(usize, usize)> = Vec::new();
    let mut last = 0;
    for (ix, line) in lines.iter().enumerate() {
        let Some(depth) = indent(&line.text, yaml) else { continue };
        while let Some(&(start, d)) = open.last()
            && d >= depth
        {
            open.pop();
            if last > start {
                folds.push((start, last));
            }
        }
        open.push((ix, depth));
        last = ix;
    }
    for (start, _) in open {
        if last > start {
            folds.push((start, last));
        }
    }
    folds.sort_unstable();
    folds
}

/// How deep `text` is indented; `None` for a blank line.
fn indent(text: &str, yaml: bool) -> Option<usize> {
    let mut width = 0;
    for (i, c) in text.char_indices() {
        match c {
            ' ' => width += 1,
            '\t' => width = (width / TAB + 1) * TAB,
            _ => {
                let item = yaml && (text[i..].starts_with("- ") || &text[i..] == "-");
                return Some(width + item as usize);
            }
        }
    }
    None
}

/// Folds each heading's section: up to the next heading as high or higher,
/// leaving out the blank lines before it. `headings`: (line, level 1–6).
pub fn by_headings(headings: &[(usize, u8)], lines: &[Line]) -> Vec<Fold> {
    let mut folds = Vec::new();
    for (i, &(start, level)) in headings.iter().enumerate() {
        let next = headings[i + 1..].iter().find(|(_, l)| *l <= level).map_or(lines.len(), |(ix, _)| *ix);
        let end = (start + 1..next).rev().find(|&ix| !lines[ix].text.trim().is_empty());
        if let Some(end) = end {
            folds.push((start, end));
        }
    }
    folds
}

/// The `#` headings of Markdown source, skipping code blocks and front matter.
pub fn markdown_headings(lines: &[Line]) -> Vec<(usize, u8)> {
    let mut headings = Vec::new();
    let mut fence: Option<&str> = None;
    let mut front_matter = lines.first().is_some_and(|l| l.text.trim_end() == "---");
    for (ix, line) in lines.iter().enumerate() {
        let text = line.text.trim_start();
        if front_matter {
            front_matter = ix == 0 || !matches!(text.trim_end(), "---" | "...");
            continue;
        }
        if let Some(open) = fence {
            if text.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if text.starts_with("```") || text.starts_with("~~~") {
            fence = Some(&text[..3]);
            continue;
        }
        let level = text.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&level) && text[level..].chars().next().is_none_or(|c| c == ' ' || c == '\t') {
            headings.push((ix, level as u8));
        }
    }
    headings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<Line> {
        text.lines().map(|t| Line { text: t.into(), spans: Vec::new() }).collect()
    }

    #[test]
    fn folds_json_blocks() {
        let json = "{\n  \"a\": {\n    \"b\": 1,\n\n    \"c\": [\n      2\n    ]\n  },\n  \"d\": 3\n}";
        assert_eq!(by_indent(&lines(json), false), vec![(0, 8), (1, 6), (4, 5)]);
    }

    #[test]
    fn folds_yaml_lists_under_their_key() {
        let yaml = "spec:\n  containers:\n  - name: a\n    image: x\n  - name: b\nkind: Pod";
        assert_eq!(by_indent(&lines(yaml), true), vec![(0, 4), (1, 4), (2, 3)]);
    }

    #[test]
    fn folds_markdown_sections() {
        let md = lines("---\n# not a heading\n---\n# One\ntext\n\n## Two\n```\n# code\n```\n\n# Three\nend");
        let headings = markdown_headings(&md);
        assert_eq!(headings, vec![(3, 1), (6, 2), (11, 1)]);
        assert_eq!(by_headings(&headings, &md), vec![(3, 9), (6, 9), (11, 12)]);
    }
}
