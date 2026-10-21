---
name: findest-search
description: Find files and folders anywhere on this Mac by name, extension, size, date or path, instantly, using Findest's index (`findest search`). Use instead of `find /`, `mdfind` or recursive `ls` whenever you need to locate files outside the current repo, or across a large tree (home folder, Downloads, Documents, other projects, iCloud/OneDrive). Not for searching code inside the current repo (use rg/grep).
---

# Finding files with `findest search`

`findest search` searches an index of every file name under the user's home folder, /Applications and cloud drives. It answers in well under a second, where `find ~` takes minutes. Results are absolute paths sorted by name.

```sh
findest search 'invoice ext:pdf'                     # name contains "invoice", a .pdf
findest search 'ext:pdf invoice dm:2024' --limit 20  # modified in 2024, at most 20 results
findest search 'ext:jpg;png size:>5mb' --in ~/Pictures
findest search 'Cargo.toml' --in ~/dev --json        # NDJSON: path, name, kind, size, modified
findest search 'folder: build' --in . --limit 5
```

Useful flags:
- `--in <dir>`: only results inside this folder. Narrow the search like this whenever you know roughly where to look.
- `--limit <n>`: the default is 100 results; `0` means all. When there are more results, stderr says so: narrow the query rather than raising the limit.
- `--json`: one object per line, for sizes and dates.
- `--hidden`: also searches dotfiles and anything inside hidden folders. These are skipped by default unless `--in` points inside a hidden folder.
- `--live`: walks the disk instead of using the index. Use it for a folder created moments ago, or when you need to be sure. It's fast with a small `--in`.

Exit status: 0 = matches, 1 = no matches, 2 = bad query or error (the message is on stderr).

## Query language

Terms separated by spaces must all match, so quote the whole query for the shell.

| Query | Matches |
|---|---|
| `report` | name contains "report". Lowercase is case-insensitive; any capital letter makes it case-sensitive |
| `*.pdf`, `img_??.jpg` | wildcards match the whole name |
| `"two words"` | literal text |
| `a \| b`, `!a`, `(a \| b) c` | or, not, grouping |
| `ext:jpg;png` | extension(s) |
| `size:>10mb`, `size:1k..5mb`, `size:empty` | size |
| `dm:today`, `dm:week`, `dm:2024-05`, `dm:>2024-05-01` | date modified |
| `file:`, `folder:` | only files or only folders |
| `path:src`, `src/main` | matches against the full path (any term containing `/`) |
| `regex:^\d{4}-` | regular expression on the name |
| `content:"TODO"` | text inside files. This is slow on its own, so always combine it with `ext:` or `--in` |

Run `findest search --help` for the full reference.

## Notes

- The index covers the folders configured in Findest. By default it skips `~/Library`, `.git` and `node_modules`. A folder outside the index is searched on disk automatically, with a note on stderr.
- The index is kept current by the Findest app. If the app hasn't run for a while, the first search catches up with recent changes, which takes a few seconds.
- If `findest` isn't installed: `brew install ariel-bentu/findest/findest`.
