//! The command line besides opening folders: `findest search` (the search
//! index, for scripts and AI agents) and help. Search uses the index the
//! app saved, caught up with changes since if it's over an hour old;
//! folders it doesn't cover (or no index at all) are searched on disk.

use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::path::Path;
use std::time::UNIX_EPOCH;

use findest_core::config::{Config, SearchConfig};
use findest_core::search::saved::{ScanMark, changes_since};
use findest_core::listing::Entry;
use findest_core::search::{Excludes, Index, LiveProgress, SearchOptions, live_search, parse, search};

/// A saved index younger than this (seconds) is used as is; an older one is
/// first caught up with the changes since.
const FRESH: i64 = 60 * 60;

const HELP: &str = r#"Findest: a fast file browser for macOS.

Usage:
  findest [folder]               open Findest (at a folder)
  findest search <query>...      search file names from the command line
  findest search --help          search options and the query language
  findest skill install [dir]    install the agent skill for `findest search`
                                 (into dir, default ~/.claude/skills)
  findest skill show             print the skill (e.g. to paste into AGENTS.md)
  findest help                   this help
"#;

/// Teaches AI agents to use `findest search`.
const SKILL: &str = include_str!("../../../skills/findest-search/SKILL.md");

const USAGE: &str = r#"Usage: findest search [options] <query>...

Searches file names using Findest's index, or the disk directly where
there is no index. Results are sorted by name.

The index is the one the Findest app keeps (saved at least hourly while it
runs). One more than an hour old is first caught up with what changed on
disk since, which is quick; --rescan rebuilds it from scratch instead.

Options:
  --in <dir>      only results inside this folder
  --limit <n>     print at most n results (default 100, 0 = all)
  --json          one JSON object per line: path, name, kind, size, modified
  --hidden        include hidden files and anything in hidden folders
  --live          ignore the index and walk the disk
  --rescan        rebuild the index from disk first (slow: a minute or more;
                  for one folder, --in <dir> --live is much faster)
  -h, --help      this help

Query language:
  report                  name contains "report" (any capital makes it case-sensitive)
  *.pdf  img_??.jpg       wildcards match the whole name
  "two words"             quotes: literal text, no wildcards
  a b   a AND b           both (AND is implied by a space)
  a | b   a OR b          either
  !a  -a  NOT a           not
  (a | b) c               grouping
  ext:jpg;png             extension
  size:>10mb  size:1k..5mb  size:empty
  dm:today  dm:week  dm:2024  dm:2024-05  dm:>2024-05-01  dm:2024-01..2024-03
  file:  folder:          only files / only folders (also as prefixes: file:report)
  path:src  src/main      match against the full path (any term containing "/")
  regex:^\d{4}-           regular expression on the name (path:regex: for the path)
  case:Readme  nocase:X   force case-sensitive / insensitive
  content:"TODO"          text inside files (combine with other terms to stay fast)
  hidden:                 also match hidden files

Exit status: 0 if anything matched, 1 if nothing did, 2 on error.
"#;

struct Args {
    query: String,
    under: Option<PathBuf>,
    limit: usize,
    json: bool,
    hidden: bool,
    live: bool,
    rescan: bool,
}

/// Handles the command line unless it asks to open the app, returning the
/// exit code.
pub fn handle_args() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("search") => Some(run_search(args)),
        Some("skill") => Some(run_skill(args)),
        Some("help" | "--help" | "-h") => {
            print!("{HELP}");
            Some(0)
        }
        // Folders to open: refuse ones that don't exist, rather than
        // starting the app from the terminal.
        _ => std::env::args_os().skip(1).find(|a| !a.to_string_lossy().starts_with('-') && !Path::new(a).exists()).map(
            |missing| {
                eprintln!("findest: {} doesn't exist\n\n{HELP}", Path::new(&missing).display());
                2
            },
        ),
    }
}

fn run_skill(mut args: impl Iterator<Item = String>) -> i32 {
    match (args.next().as_deref(), args.next(), args.next()) {
        (Some("show"), None, _) => {
            print!("{SKILL}");
            0
        }
        (Some("install"), dir, None) => {
            let dir = dir.map(PathBuf::from).unwrap_or_else(|| findest_core::paths::home_dir().join(".claude/skills"));
            let file = dir.join("findest-search/SKILL.md");
            let written = file.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&file, SKILL));
            match written {
                Ok(()) => {
                    println!("Installed {}", file.display());
                    0
                }
                Err(e) => {
                    eprintln!("findest skill: writing {}: {e}", file.display());
                    2
                }
            }
        }
        _ => {
            eprintln!("Usage: findest skill install [dir] | findest skill show");
            2
        }
    }
}

fn run_search(args: impl Iterator<Item = String>) -> i32 {
    match parse_args(args) {
        Ok(Some(args)) => match run(&args) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(e) => {
                eprintln!("findest search: {e}");
                2
            }
        },
        Ok(None) => {
            print!("{USAGE}");
            0
        }
        Err(e) => {
            eprintln!("findest search: {e}\n\n{USAGE}");
            2
        }
    }
}

/// `None` when help was asked for.
fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args =
        Args { query: String::new(), under: None, limit: 100, json: false, hidden: false, live: false, rescan: false };
    let mut terms = Vec::new();
    let mut saw_separator = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--json" => args.json = true,
            "--hidden" => args.hidden = true,
            "--live" => args.live = true,
            "--rescan" => args.rescan = true,
            "--in" => args.under = Some(it.next().ok_or("--in needs a folder")?.into()),
            "--limit" => args.limit = it.next().and_then(|n| n.parse().ok()).ok_or("--limit needs a number")?,
            "--" => {
                saw_separator = true;
                terms.extend(it.by_ref());
            }
            _ if arg.starts_with("--") => return Err(format!("unknown option {arg}")),
            _ => terms.push(arg),
        }
    }
    match terms.as_slice() {
        [] => return Err("no query".into()),
        // `findest search help`; `findest search -- help` searches for "help".
        [only] if only == "help" && !saw_separator => return Ok(None),
        _ => {}
    }
    args.query = terms.join(" ");
    Ok(Some(args))
}

fn run(args: &Args) -> Result<bool, String> {
    let query = parse(&args.query)?;
    let config = Config::load(&Config::path()).unwrap_or_default().search;
    let under = match &args.under {
        Some(dir) => {
            let dir = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
            if !dir.is_dir() {
                return Err(format!("{} is not a folder", dir.display()));
            }
            Some(dir)
        }
        None => None,
    };
    let opts = SearchOptions {
        under: None,
        include_hidden: args.hidden,
        max_content_bytes: config.content_max_mb * 1024 * 1024,
    };
    let excludes = Excludes::new(&config.exclude);
    let roots = config.root_paths();
    let cancel = AtomicBool::new(false);

    // The index, unless the folder asked for isn't in it.
    let index = (!args.live && config.indexing)
        .then(|| current_index(&config, &excludes, args.rescan))
        .flatten()
        .and_then(|index| match &under {
            None => Some((index, None)),
            Some(dir) if excludes.skips(dir, &roots) => None,
            Some(dir) => index.find(dir).map(|id| (index, Some(id))),
        });

    let mut out = BufWriter::new(io::stdout().lock());
    let (total, shown) = match index {
        Some((index, under_id)) => {
            // Inside a hidden folder, everything is hidden: show it anyway.
            let include_hidden = opts.include_hidden || under_id.is_some_and(|id| index.in_hidden_tree(id));
            let opts = SearchOptions { under: under_id, include_hidden, ..opts };
            let found = search(&index, &query, &opts, &AtomicUsize::new(0), &cancel).ok_or("search cancelled")?;
            let shown = take(found.ids.len(), args.limit);
            for &id in &found.ids[..shown] {
                print_entry(&mut out, &index.entry(id), args.json)?;
            }
            (found.ids.len(), shown)
        }
        None => {
            if !args.live {
                eprintln!("findest search: no index for this folder; searching the disk");
            }
            let roots = under.map(|d| vec![d]).unwrap_or(roots);
            let found = live_search(&roots, &query, &opts, &excludes, &LiveProgress::default(), &cancel)
                .ok_or("search cancelled")?;
            let shown = take(found.len(), args.limit);
            for entry in &found[..shown] {
                print_entry(&mut out, entry, args.json)?;
            }
            (found.len(), shown)
        }
    };
    out.flush().map_err(|e| e.to_string())?;
    if shown < total {
        eprintln!("findest search: showing {shown} of {total} results (use --limit, or narrow the query)");
    }
    Ok(total > 0)
}

fn take(len: usize, limit: usize) -> usize {
    if limit == 0 { len } else { len.min(limit) }
}

fn print_entry(out: &mut impl Write, entry: &Entry, json: bool) -> Result<(), String> {
    let line = if json {
        let kind = if entry.is_dir { "folder" } else if entry.is_symlink { "symlink" } else { "file" };
        let modified = entry.modified.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| {
            chrono::DateTime::from_timestamp(d.as_secs() as i64, 0).map(|t| t.to_rfc3339()).unwrap_or_default()
        });
        serde_json::json!({
            "path": entry.path,
            "name": entry.name,
            "kind": kind,
            "size": if entry.is_dir { None } else { entry.size },
            "modified": modified,
        })
        .to_string()
    } else {
        entry.path.display().to_string()
    };
    match writeln!(out, "{line}") {
        // The reader went away (`| head`): stop quietly.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        r => r.map_err(|e| e.to_string()),
    }
}

/// The saved index, caught up with changes since if it's over an hour old,
/// or rebuilt from disk with `rescan`. A rebuilt index isn't saved: this
/// process may not see everything the app can (e.g. without Full Disk
/// Access), and the app's copy would lose those files.
fn current_index(config: &SearchConfig, excludes: &Excludes, rescan: bool) -> Option<Index> {
    if rescan {
        eprintln!("findest search: rescanning (this takes a while)…");
        let cancel = AtomicBool::new(false);
        return Some(Index::build(&config.root_paths(), excludes, &AtomicUsize::new(0), &cancel));
    }
    let index = Index::load(&Index::default_path()).ok()?;
    let age = chrono::Utc::now().timestamp() - index.updated_at;
    if age < FRESH {
        return Some(index);
    }
    match ScanMark::replay_from(config).and_then(|since| changes_since(config, since)) {
        Some(changed) => Some(index.with_changes(&changed, excludes).unwrap_or(index)),
        None => {
            eprintln!(
                "findest search: the index is {} old and couldn't be caught up; recent changes may be missing \
                 (open Findest to update it, or use --rescan / --live)",
                human(age)
            );
            Some(index)
        }
    }
}

fn human(secs: i64) -> String {
    match secs {
        s if s < 2 * 60 * 60 => format!("{} min", s / 60),
        s if s < 2 * 24 * 60 * 60 => format!("{} h", s / 3600),
        s => format!("{} days", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parsed(args: &[&str]) -> Result<Option<Args>, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn parses_options_and_joins_terms() {
        let a = parsed(&["ext:rs", "--in", "/tmp", "main", "--json", "--limit", "5"]).unwrap().unwrap();
        assert_eq!(a.query, "ext:rs main");
        assert_eq!(a.under.as_deref(), Some(Path::new("/tmp")));
        assert_eq!(a.limit, 5);
        assert!(a.json && !a.live && !a.hidden);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parsed(&[]).is_err());
        assert!(parsed(&["--limit", "x", "a"]).is_err());
        assert!(parsed(&["--bogus", "a"]).is_err());
        assert!(parsed(&["-h"]).unwrap().is_none());
        assert!(parsed(&["help"]).unwrap().is_none());
        assert_eq!(parsed(&["--", "help"]).unwrap().unwrap().query, "help");
        assert_eq!(parsed(&["help", "me"]).unwrap().unwrap().query, "help me");
        assert_eq!(parsed(&["--", "--json"]).unwrap().unwrap().query, "--json");
    }
}
