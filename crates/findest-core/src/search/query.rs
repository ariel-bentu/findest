//! The search query language.
//!
//! ```text
//! report                  name contains "report" (smart case: any capital makes it case-sensitive)
//! *.pdf  img_??.jpg       wildcards match the whole name
//! "two words"             quotes: literal text, no wildcards
//! a b   a AND b           both (AND is implied by a space)
//! a | b   a OR b          either
//! !a  -a  NOT a           not
//! (a | b) c               grouping
//! ext:jpg;png             extension
//! size:>10mb  size:1k..5mb  size:empty
//! dm:today  dm:week  dm:2024  dm:2024-05  dm:>2024-05-01  dm:2024-01..2024-03
//! file:  folder:          only files / only folders (also as prefixes: file:report)
//! path:src  src/main      match against the full path (any term containing "/")
//! regex:^\d{4}-           regular expression on the name (path:regex: for the path)
//! case:Readme  nocase:X   force case-sensitive / insensitive
//! content:"TODO"          text inside files (combine with other terms to stay fast)
//! hidden:                 also match hidden files
//! ```

use chrono::{Duration, Local, NaiveDate, TimeZone};
use regex::Regex;

/// A parsed query, ready to evaluate against index entries.
#[derive(Debug)]
pub struct Query {
    pub(crate) root: Node,
    /// The query looks inside files (slow path).
    pub has_content: bool,
    /// The query explicitly asks for hidden entries.
    pub wants_hidden: bool,
}

#[derive(Debug)]
pub(crate) enum Node {
    All,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Pred),
}

#[derive(Debug)]
pub(crate) enum Pred {
    Name(Text),
    Path(Text),
    Ext(Vec<String>),
    IsDir(bool),
    Size(Range),
    Modified(Range),
    Hidden,
    Content(regex::bytes::Regex),
}

#[derive(Debug)]
pub(crate) enum Text {
    /// Substring; `lower` is set when matching case-insensitively.
    Sub { needle: String, lower: bool },
    Re(Regex),
}

impl Text {
    /// `original` is the name/path as-is; `lowered` is its lowercase form.
    pub(crate) fn matches(&self, original: &str, lowered: &str) -> bool {
        match self {
            Text::Sub { needle, lower: true } => memchr::memmem::find(lowered.as_bytes(), needle.as_bytes()).is_some(),
            Text::Sub { needle, lower: false } => memchr::memmem::find(original.as_bytes(), needle.as_bytes()).is_some(),
            Text::Re(re) => re.is_match(original),
        }
    }
}

/// Half-open range `[lo, hi)`; `None` means unbounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub lo: Option<i64>,
    pub hi: Option<i64>,
}

impl Range {
    pub fn contains(&self, v: i64) -> bool {
        self.lo.is_none_or(|lo| v >= lo) && self.hi.is_none_or(|hi| v < hi)
    }
}

pub fn parse(input: &str) -> Result<Query, String> {
    let tokens = lex(input)?;
    let mut parser = Parser { tokens, pos: 0, has_content: false, wants_hidden: false };
    let root = if parser.tokens.is_empty() { Node::All } else { parser.or()? };
    if parser.pos < parser.tokens.len() {
        return Err("unmatched \")\"".into());
    }
    Ok(Query { root, has_content: parser.has_content, wants_hidden: parser.wants_hidden })
}

// ---- Lexer -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Or,
    Not,
    Word { text: String, quoted: bool },
}

fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Tok::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Tok::RParen);
            }
            '|' => {
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                tokens.push(Tok::Or);
            }
            '!' => {
                chars.next();
                tokens.push(Tok::Not);
            }
            _ => {
                let mut text = String::new();
                let mut quoted = false;
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' || (c == '|' && !text.is_empty()) {
                        break;
                    }
                    chars.next();
                    if c == '"' {
                        quoted = true;
                        let mut closed = false;
                        for c in chars.by_ref() {
                            if c == '"' {
                                closed = true;
                                break;
                            }
                            text.push(c);
                        }
                        if !closed {
                            return Err("unterminated quote".into());
                        }
                    } else if c == '\\' {
                        // Escapes query syntax only, so regexes like \d keep their backslash.
                        match chars.peek() {
                            Some(&n) if n.is_whitespace() || "\"()|\\".contains(n) => {
                                text.push(n);
                                chars.next();
                            }
                            _ => text.push('\\'),
                        }
                    } else {
                        text.push(c);
                    }
                }
                match text.as_str() {
                    "OR" if !quoted => tokens.push(Tok::Or),
                    "NOT" if !quoted => tokens.push(Tok::Not),
                    "AND" | "&&" if !quoted => {}
                    t if !quoted && t.len() > 1 && t.starts_with('-') => {
                        let word = Tok::Word { text: t[1..].to_string(), quoted };
                        tokens.push(Tok::Not);
                        tokens.push(word);
                    }
                    _ => tokens.push(Tok::Word { text, quoted }),
                }
            }
        }
    }
    Ok(tokens)
}

// ---- Parser ------------------------------------------------------------

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
    has_content: bool,
    wants_hidden: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn or(&mut self) -> Result<Node, String> {
        let mut parts = vec![self.and()?];
        while self.peek() == Some(&Tok::Or) {
            self.pos += 1;
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Node::Or(parts) })
    }

    fn and(&mut self) -> Result<Node, String> {
        let mut parts = Vec::new();
        while let Some(tok) = self.peek() {
            if matches!(tok, Tok::Or | Tok::RParen) {
                break;
            }
            parts.push(self.unary()?);
        }
        match parts.len() {
            0 => Err("expected a search term".into()),
            1 => Ok(parts.pop().unwrap()),
            _ => Ok(Node::And(parts)),
        }
    }

    fn unary(&mut self) -> Result<Node, String> {
        match self.tokens.get(self.pos).cloned() {
            Some(Tok::Not) => {
                self.pos += 1;
                Ok(Node::Not(Box::new(self.unary()?)))
            }
            Some(Tok::LParen) => {
                self.pos += 1;
                let inner = self.or()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err("missing \")\"".into());
                }
                self.pos += 1;
                Ok(inner)
            }
            Some(Tok::Word { text, quoted }) => {
                self.pos += 1;
                self.term(&text, quoted)
            }
            _ => Err("expected a search term".into()),
        }
    }

    fn term(&mut self, text: &str, quoted: bool) -> Result<Node, String> {
        let mut rest = text;
        let mut case: Option<bool> = None;
        let mut kind: Option<bool> = None;
        let mut path = false;
        let mut regex = false;
        let mut filters = Vec::new();

        while let Some((key, value)) = split_key(rest) {
            match key.as_str() {
                "case" => case = Some(true),
                "nocase" => case = Some(false),
                "file" | "files" => kind = Some(false),
                "folder" | "folders" | "dir" => kind = Some(true),
                "path" => path = true,
                "regex" => regex = true,
                "hidden" => {
                    self.wants_hidden = true;
                    filters.push(Node::Leaf(Pred::Hidden));
                }
                "ext" => {
                    let exts = value.split([';', ',']).filter(|e| !e.is_empty()).map(|e| e.trim_start_matches('.').to_lowercase()).collect();
                    filters.push(Node::Leaf(Pred::Ext(exts)));
                    rest = "";
                    break;
                }
                "size" => {
                    filters.push(Node::Leaf(Pred::Size(parse_size_range(value)?)));
                    rest = "";
                    break;
                }
                "dm" | "modified" | "date" => {
                    filters.push(Node::Leaf(Pred::Modified(parse_date_range(value)?)));
                    rest = "";
                    break;
                }
                "content" | "text" => {
                    if value.is_empty() {
                        return Err("content: needs some text to look for".into());
                    }
                    let sensitive = case.unwrap_or_else(|| has_upper(value));
                    let pattern = if regex { value.to_string() } else { regex::escape(value) };
                    let re = regex::bytes::RegexBuilder::new(&pattern)
                        .case_insensitive(!sensitive)
                        .build()
                        .map_err(|e| format!("content: {e}"))?;
                    self.has_content = true;
                    filters.push(Node::Leaf(Pred::Content(re)));
                    rest = "";
                    break;
                }
                _ => unreachable!("split_key only returns known keys"),
            }
            rest = value;
        }

        if let Some(dir) = kind {
            filters.push(Node::Leaf(Pred::IsDir(dir)));
        }
        if !rest.is_empty() {
            let sensitive = case.unwrap_or_else(|| has_upper(rest));
            let on_path = path || rest.contains('/');
            let text = if regex {
                Text::Re(regex::RegexBuilder::new(rest).case_insensitive(!sensitive).build().map_err(|e| format!("regex: {e}"))?)
            } else if !quoted && (rest.contains('*') || rest.contains('?')) {
                let pattern = glob_to_regex(rest, on_path);
                Text::Re(regex::RegexBuilder::new(&pattern).case_insensitive(!sensitive).build().map_err(|e| e.to_string())?)
            } else if sensitive {
                Text::Sub { needle: rest.to_string(), lower: false }
            } else {
                Text::Sub { needle: rest.to_lowercase(), lower: true }
            };
            filters.push(Node::Leaf(if on_path { Pred::Path(text) } else { Pred::Name(text) }));
        }
        Ok(match filters.len() {
            0 => Node::All,
            1 => filters.pop().unwrap(),
            _ => Node::And(filters),
        })
    }
}

const KEYS: &[&str] = &[
    "case", "nocase", "file", "files", "folder", "folders", "dir", "path", "regex", "hidden", "ext", "size", "dm",
    "modified", "date", "content", "text",
];

/// Splits a recognized `key:` prefix off a term.
fn split_key(text: &str) -> Option<(String, &str)> {
    let (key, value) = text.split_once(':')?;
    let key = key.to_ascii_lowercase();
    KEYS.contains(&key.as_str()).then_some((key, value))
}

fn has_upper(s: &str) -> bool {
    s.chars().any(char::is_uppercase)
}

/// `*` and `?` wildcards; names are matched whole, paths may match anywhere
/// after the last `/` boundary the pattern starts with.
fn glob_to_regex(glob: &str, on_path: bool) -> String {
    let mut re = String::from(if on_path && !glob.starts_with('/') { "(^|/)" } else { "^" });
    for c in glob.chars() {
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    re
}

// ---- size: and dm: -----------------------------------------------------

fn parse_size(s: &str) -> Result<i64, String> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.parse().map_err(|_| format!("size: can't read \"{s}\""))?;
    let mult = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => return Err(format!("size: unknown unit \"{unit}\" (use kb, mb, gb)")),
    };
    Ok((num * mult) as i64)
}

pub(crate) fn parse_size_range(v: &str) -> Result<Range, String> {
    let v = v.trim();
    if v.eq_ignore_ascii_case("empty") {
        return Ok(Range { lo: Some(0), hi: Some(1) });
    }
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { None } else { Some(parse_size(a)?) };
        let hi = if b.is_empty() { None } else { Some(parse_size(b)? + 1) };
        return Ok(Range { lo, hi });
    }
    Ok(if let Some(x) = v.strip_prefix(">=") {
        Range { lo: Some(parse_size(x)?), hi: None }
    } else if let Some(x) = v.strip_prefix("<=") {
        Range { lo: None, hi: Some(parse_size(x)? + 1) }
    } else if let Some(x) = v.strip_prefix('>') {
        Range { lo: Some(parse_size(x)? + 1), hi: None }
    } else if let Some(x) = v.strip_prefix('<') {
        Range { lo: None, hi: Some(parse_size(x)?) }
    } else {
        let x = parse_size(v.trim_start_matches('='))?;
        Range { lo: Some(x), hi: Some(x + 1) }
    })
}

/// A date expression as a `[start, end)` span of Unix seconds.
fn parse_date_span(s: &str, now: chrono::DateTime<Local>) -> Result<(i64, i64), String> {
    let s = s.trim().to_ascii_lowercase();
    let start_of = |d: NaiveDate| Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp());
    let today = now.date_naive();
    let day = |d: NaiveDate| -> Result<(i64, i64), String> {
        Ok((start_of(d).ok_or("bad date")?, start_of(d + Duration::days(1)).ok_or("bad date")?))
    };
    let ago = |days: i64| Ok((now.timestamp() - days * 86_400, i64::MAX));
    match s.as_str() {
        "today" => return day(today),
        "yesterday" => return day(today - Duration::days(1)),
        "week" | "thisweek" | "lastweek" => return ago(7),
        "month" | "thismonth" | "lastmonth" => return ago(30),
        "year" | "thisyear" | "lastyear" => return ago(365),
        _ => {}
    }
    let parts: Vec<&str> = s.split('-').collect();
    let num = |p: &str| p.parse::<u32>().map_err(|_| format!("dm: can't read \"{s}\" (try today, week, 2024, 2024-05-01)"));
    match parts.as_slice() {
        [y] => {
            let y = num(y)? as i32;
            let a = NaiveDate::from_ymd_opt(y, 1, 1).ok_or("bad year")?;
            let b = NaiveDate::from_ymd_opt(y + 1, 1, 1).ok_or("bad year")?;
            Ok((start_of(a).ok_or("bad date")?, start_of(b).ok_or("bad date")?))
        }
        [y, m] => {
            let (y, m) = (num(y)? as i32, num(m)?);
            let a = NaiveDate::from_ymd_opt(y, m, 1).ok_or("bad month")?;
            let b = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, m + 1, 1) }.ok_or("bad month")?;
            Ok((start_of(a).ok_or("bad date")?, start_of(b).ok_or("bad date")?))
        }
        [y, m, d] => day(NaiveDate::from_ymd_opt(num(y)? as i32, num(m)?, num(d)?).ok_or("bad date")?),
        _ => Err(format!("dm: can't read \"{s}\"")),
    }
}

pub(crate) fn parse_date_range_at(v: &str, now: chrono::DateTime<Local>) -> Result<Range, String> {
    let v = v.trim();
    if let Some((a, b)) = v.split_once("..") {
        let lo = if a.is_empty() { None } else { Some(parse_date_span(a, now)?.0) };
        let hi = if b.is_empty() { None } else { Some(parse_date_span(b, now)?.1) };
        return Ok(Range { lo, hi });
    }
    let (op, rest) = ["<=", ">=", "<", ">", "="]
        .iter()
        .find_map(|op| v.strip_prefix(op).map(|r| (*op, r)))
        .unwrap_or(("", v));
    let (start, end) = parse_date_span(rest, now)?;
    let end = Some(end).filter(|&e| e != i64::MAX);
    Ok(match op {
        ">" => Range { lo: end.or(Some(start)), hi: None },
        ">=" => Range { lo: Some(start), hi: None },
        "<" => Range { lo: None, hi: Some(start) },
        "<=" => Range { lo: None, hi: end },
        _ => Range { lo: Some(start), hi: end },
    })
}

fn parse_date_range(v: &str) -> Result<Range, String> {
    parse_date_range_at(v, Local::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_operators_and_quotes() {
        let toks = lex(r#"a "b c" | !d -e (f OR g) content:"x y" AND h"#).unwrap();
        let words: Vec<String> = toks
            .iter()
            .map(|t| match t {
                Tok::Word { text, .. } => text.clone(),
                Tok::Or => "|".into(),
                Tok::Not => "!".into(),
                Tok::LParen => "(".into(),
                Tok::RParen => ")".into(),
            })
            .collect();
        assert_eq!(words, ["a", "b c", "|", "!", "d", "!", "e", "(", "f", "|", "g", ")", "content:x y", "h"]);
        assert!(lex("\"open").is_err());
        assert_eq!(lex(r"a\ b regex:\d").unwrap()[0], Tok::Word { text: "a b".into(), quoted: false });
        assert_eq!(lex(r"regex:\d+").unwrap()[0], Tok::Word { text: r"regex:\d+".into(), quoted: false });
    }

    #[test]
    fn parses_and_reports_errors() {
        assert!(parse("").is_ok());
        assert!(parse("(a").is_err());
        assert!(parse("a)").is_err());
        assert!(parse("regex:(").is_err());
        assert!(parse("size:>10zb").is_err());
        assert!(parse("dm:someday").is_err());
        let q = parse("file:content:todo ext:rs").unwrap();
        assert!(q.has_content);
        assert!(parse("hidden: x").unwrap().wants_hidden);
        // Unknown "keys" are just text (e.g. a name with a colon).
        assert!(matches!(parse("12:30").unwrap().root, Node::Leaf(Pred::Name(_))));
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size_range(">10mb").unwrap(), Range { lo: Some(10_000_001), hi: None });
        assert_eq!(parse_size_range("<=1k").unwrap(), Range { lo: None, hi: Some(1001) });
        assert_eq!(parse_size_range("1kb..2kb").unwrap(), Range { lo: Some(1000), hi: Some(2001) });
        assert_eq!(parse_size_range("empty").unwrap(), Range { lo: Some(0), hi: Some(1) });
        assert!(parse_size_range("1.5gb").unwrap().contains(1_500_000_000));
    }

    #[test]
    fn dates() {
        let now = Local.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap();
        let ts = |y, m, d| Local.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap().timestamp();
        let today = parse_date_range_at("today", now).unwrap();
        assert!(today.contains(ts(2026, 10, 15) + 5) && !today.contains(ts(2026, 10, 14)));
        let y = parse_date_range_at("2024", now).unwrap();
        assert!(y.contains(ts(2024, 6, 1)) && !y.contains(ts(2025, 1, 1)));
        let after = parse_date_range_at(">2024-05-01", now).unwrap();
        assert!(!after.contains(ts(2024, 5, 1) + 60) && after.contains(ts(2024, 5, 2)));
        let span = parse_date_range_at("2024-01..2024-03", now).unwrap();
        assert!(span.contains(ts(2024, 3, 31)) && !span.contains(ts(2024, 4, 1)));
        assert!(parse_date_range_at("week", now).unwrap().contains(now.timestamp() - 3 * 86_400));
    }

    #[test]
    fn globs() {
        let re = Regex::new(&glob_to_regex("*.pdf", false)).unwrap();
        assert!(re.is_match("report.pdf") && !re.is_match("report.pdf.bak"));
        let re = Regex::new(&glob_to_regex("img_??.jpg", false)).unwrap();
        assert!(re.is_match("img_01.jpg") && !re.is_match("img_001.jpg"));
    }
}
