# Sources

A source shows a hierarchy that isn't on disk, such as cloud resources, a bucket or a wiki, in Findest's sidebar. You browse it like folders: list and tiles views, Quick Look, the info panel, Open in…, copy out. Sources are read-only.

Findest doesn't know about any particular system. You provide a small program, an **adapter**, that answers three questions: what's in this folder, what's in this file, and how to sign in. Any language works. [`examples/static-source`](examples/static-source) is a complete adapter in about 100 lines of Python that serves a fixed tree. Copy it to start your own.

## Configure

In `~/.config/findest/config.toml`:

```toml
[[sources]]
id = "example"      # stable name for its cache folder; no "/"
name = "Example"    # shown in the sidebar and the path bar
command = ["~/findest/static-source", "--some-flag"]   # the adapter, plus its own arguments
root = "/"          # the folder the sidebar opens (default "/")
```

The source appears under **Sources** in the sidebar as soon as you save. Mistakes in this section, such as a duplicate `id` or an empty `command`, are shown in the status bar.

## The protocol

Findest runs `command` followed by a verb and its arguments, one process per request:

| Verb | Arguments | On success (exit 0) |
|---|---|---|
| `list` | `<folder-ref> [<continue>]` | prints a listing as JSON on stdout |
| `read` | `<file-ref> <dest>` | writes the file's content to the path `dest` |
| `login` | none | has signed the user in (it may open a browser) |

Exit codes, for every verb:

| Exit | Meaning | Findest shows |
|---|---|---|
| `0` | success | the result |
| `77` | not signed in | a **Sign In** button, which runs `login` and then retries |
| anything else | failure | the **last non-empty line of stderr**, so make it a useful sentence |

### `list`

```sh
$ static-source list /
{"entries": [
  {"name": "docs", "kind": "folder", "ref": "/docs"},
  {"name": "hello.txt", "kind": "file", "ref": "hello",
   "label": "Greeting", "size": 28, "modified": "2026-10-01T12:00:00Z"}
]}
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Shown, and used as a file name in the cache. Unique within the folder; no `/`. Give files an extension (`.yaml`, `.json`, `.md`): it picks the icon, Quick Look's preview and the app that opens it. |
| `kind` | yes | `"folder"` or `"file"`. |
| `ref` | yes | For a folder: its path, `/`-separated, which is the `name`s from the root joined (`/docs`). For a file: any string you like; it's passed back to `read` unchanged, so put what you need to fetch it there (an id, or a JSON array). |
| `label` | no | Shown in the Kind column instead of "YAML file", e.g. `"ManagedService · Ready"`. |
| `size` | no | Bytes. |
| `modified` | no | RFC 3339 time, e.g. `"2026-10-01T12:00:00Z"`. |
| `has_children` | no | Folders only. `false` hides the disclosure triangle. Default `true`. |

Next to `entries`, a listing can carry `"warnings": ["…"]`, for problems that shouldn't fail the whole folder ("couldn't list pods.metrics.k8s.io"). They're shown in the status bar. Don't hide partial failures: stderr is only shown when the adapter fails.

Folder refs come from the path, so `list` is also called for folders the user types in the Go to bar or goes back to from history. Answer with an error (not exit 0) if the folder doesn't exist.

**Paging.** If a folder is large, return part of it with `"continue": "<token>"`. Findest ends the folder with a **Load more…** row; opening that row runs `list <folder-ref> <token>` and appends the result. Leave `continue` out on the last page.

### `read`

```sh
$ static-source read hello /Users/me/Library/Caches/findest/sources/example/hello.txt
```

Write the file's content to `dest` (the folder already exists) and exit 0. Print nothing on stdout. Findest calls `read` when a file is selected, opened, previewed or copied, and again if the copy is older than 30 seconds.

### `login`

Sign in and exit 0 once it worked; exit non-zero with a message on stderr if it didn't. There's no terminal: open a browser, or a dialog (`osascript`), for anything interactive. Keep tokens wherever your tools keep them; Findest never sees them. If your system needs no sign-in, exit 0 and never return 77.

Refreshing expired tokens is also the adapter's job. Do it inside `list` and `read`, and return 77 only when the user really has to sign in again.

### Limits

- `list` and `read` are stopped after 60 seconds, `login` after 5 minutes. The user sees that it timed out.
- A listing is cached for 60 seconds; ⌘R lists the folder again.
- Problems with single entries (a bad name, an unknown `kind`, a duplicate name, an unreadable `modified`) skip or ignore that field, and they're reported in the status bar. They don't fail the listing.
- A source never changes your files: copying, moving, renaming, deleting and dropping into a source are refused with "Sources are read-only".

## Test an adapter by hand

```sh
./my-source list /; echo "exit $?"            # valid JSON, exit 0
./my-source list /no-such-folder; echo $?     # exit 1 and a sentence on stderr
./my-source read <a file ref> /tmp/x && cat /tmp/x
./my-source list / | python3 -m json.tool     # pretty-print
```

Then sign out in your system and check that `list /` exits 77.

## Writing one with a coding agent

Give the agent this file and `examples/static-source`, then describe your system: which command or API lists a folder, how to fetch one item, and how sign-in works. For example: "Write a Findest source adapter like docs/examples/static-source, for S3. `list` uses `aws s3api list-objects-v2` with a delimiter, and `read` uses `aws s3 cp`. Return 77 when the AWS session has expired; `login` runs `aws sso login`."
