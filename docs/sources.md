# Sources

A source shows a hierarchy that isn't on disk, such as cloud resources, a bucket or a wiki, in Findest's sidebar. You browse it like folders: list and tiles views, Quick Look, the info panel, Open in…, copy out. Files can't be changed in a source, but the adapter can offer its own [actions](#actions) on items, such as Delete or Reconcile.

Findest doesn't know about any particular system. You provide a small program, an **adapter**, that answers three questions: what's in this folder, what's in this file, and how to sign in. Optionally it answers a fourth: what's in this folder and below that matches a search. Any language works. Two examples in Python, standard library only:

- [`examples/static-source`](examples/static-source) serves a fixed tree in about 120 lines. Copy it to start your own.
- [`examples/k8s-source`](examples/k8s-source) browses a Kubernetes cluster through `kubectl`. Namespaces, then the kinds that have something in them, then each resource as YAML with Type and Status columns (`Pod`, `Running`). It implements `search` across the cluster.

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
| `search` (optional) | `<folder-ref> <hints-json> [<continue>]` | prints matching entries under the folder as JSON (see [Search](#search)) |
| `action` (optional) | `<action-id> <ref>` | has done the action to the item; may print `{"message": "…"}` (see [Actions](#actions)) |

Exit codes, for every verb:

| Exit | Meaning | Findest shows |
|---|---|---|
| `0` | success | the result |
| `77` | not signed in | a **Sign In** button (in place of the folder, in the status bar or the search bar, and a "Sign in" badge in the sidebar), which runs `login` and then retries |
| `64` | this adapter doesn't have that verb | for `search`: Findest lists folder after folder instead |
| anything else | failure | the **last non-empty line of stderr**, so make it a useful sentence |

### `list`

```sh
$ static-source list /
{"entries": [
  {"name": "docs", "kind": "folder", "ref": "/docs"},
  {"name": "hello.txt", "kind": "file", "ref": "hello",
   "label": "Greeting", "attributes": {"owner": "docs-team"},
   "size": 28, "modified": "2026-10-01T12:00:00Z"}
],
 "columns": [{"key": "owner", "title": "Owner"}]}
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Shown, and used as a file name in the cache. Unique within the folder; no `/`. Give files an extension (`.yaml`, `.json`, `.md`): it picks the icon, Quick Look's preview and the app that opens it. |
| `kind` | yes | `"folder"` or `"file"`. |
| `ref` | yes | For a folder: its path, `/`-separated, which is the `name`s from the root joined (`/docs`). For a file: any string you like; it's passed back to `read` unchanged, so put what you need to fetch it there (an id, or a JSON array). |
| `label` | no | Shown in the Kind column instead of "YAML file", e.g. `"Resource Group"`. |
| `attributes` | no | Your own fields, e.g. `{"type": "TenantData", "status": "Ready"}`. Values are strings (numbers and booleans are shown as text). Shown in the columns you declare, and searchable as `status:Ready`. |
| `size` | no | Bytes. Left out, the Size column shows `--`. |
| `modified` | no | RFC 3339 time, e.g. `"2026-10-01T12:00:00Z"`. |
| `has_children` | no | Folders only. `false` hides the disclosure triangle. Default `true`. |
| `actions` | no | What the user can do to this item from its right-click menu, e.g. `[{"id": "delete", "title": "Delete", "guarded": true}]`. See [Actions](#actions). |

**Columns.** Next to `entries`, `"columns": [{"key": "status", "title": "Status"}]` shows those attributes as columns after Kind, in that order, while browsing the source. They can be resized and sorted like the others. Send them with every `list` and `search` response; Findest keeps every column a source has declared. Search matches any attribute, declared or not: `status:Ready` finds entries whose `status` is `Ready` (ignoring case; the whole value must match), and `!status:Ready` the rest. Avoid keys that are built-in search filters (`size`, `path`, `ext`, `dm`, `date`, `modified`, `content`, `text`, `file`, `folder`, `dir`, `hidden`, `case`, `nocase`, `regex`): those can't be searched as attributes.

Next to `entries`, a listing can carry `"warnings": ["…"]`, for problems that shouldn't fail the whole folder ("couldn't list pods.metrics.k8s.io"). They're shown in the status bar. Don't hide partial failures: stderr is only shown when the adapter fails.

Folder refs come from the path, so `list` is also called for folders the user types in the Go to bar or goes back to from history. Answer with an error (not exit 0) if the folder doesn't exist.

**Paging.** If a folder is large, return part of it with `"continue": "<token>"`. Findest ends the folder with a **Load more…** row; opening that row runs `list <folder-ref> <token>` and appends the result. Leave `continue` out on the last page.

### `read`

```sh
$ static-source read hello /Users/me/Library/Caches/findest/sources/example/hello.txt
```

Write the file's content to `dest` (the folder already exists) and exit 0. Print nothing on stdout. Findest calls `read` when a file is selected, opened, previewed or copied, and again if the copy is older than 30 seconds.

### `login`

Sign in and exit 0 once it worked; exit non-zero with a message on stderr if it didn't. While it runs, Findest shows "Signing in…" with **Cancel**. Cancel, like a timeout, stops the adapter and everything it started (Findest runs it in its own process group), so a browser-based login tool you launched is stopped too. There's no terminal: open a browser, or a dialog (`osascript`), for anything interactive. Keep tokens wherever your tools keep them; Findest never sees them. If your system needs no sign-in, exit 0 and never return 77.

Refreshing expired tokens is also the adapter's job. Do it inside `list` and `read`, and return 77 only when the user really has to sign in again.

### `search`

⌘F inside a source searches that folder and everything below it. Findest parses the query, then runs:

```sh
$ static-source search / '{"query": "item ext:txt", "names": ["item"], "extensions": ["txt"], "kind": null}'
{"entries": [
  {"name": "item-1.txt", "kind": "file", "ref": "item-1", "folder": "/big"},
  ...
]}
```

The second argument holds hints you may use to narrow the search:

| Hint | Meaning |
|---|---|
| `query` | what the user typed |
| `names` | lowercase strings that every match's name contains |
| `extensions` | a match's extension is one of these (empty: any) |
| `kind` | `"file"`, `"folder"` or `null` |

Every entry is the same as in `list`, plus **`folder`**: the folder ref it's in, under the searched folder. Use the same `name` that `list` gives it there.

**Returning more than matches is fine.** Findest tests every result against the full query, including `size:`, `dm:`, regexes and `content:` (which fetches files with `read`, up to 200 per search). So an adapter can push down whatever its backend supports and leave the rest to Findest. One that ignores the hints and returns the whole subtree is correct, only slower.

Large result sets can be paged with `continue`, as for `list`. Findest follows up to 50 pages.

**Without `search`**, or when it exits 64, Findest walks the source with `list`, a level at a time with folders listed in parallel. It stops after 1,000 folders or 60 seconds, and only the first page of a paged folder is searched. Each limit it reaches is reported. Implement `search` when your backend can list a subtree in one call: for URM that's one recursive call instead of one call per folder.

### Actions

An entry can list actions, shown in its right-click menu after **Open** and **Open in…**:

```json
{"name": "x.yaml", "kind": "file", "ref": "…",
 "actions": [{"id": "reconcile", "title": "Reconcile"},
             {"id": "delete", "title": "Delete", "guarded": true, "danger": true}]}
```

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | Passed back to `action`. |
| `title` | yes | The menu item. |
| `guarded` | no | Ask first: "Are you sure you want to delete "x.yaml"?" with **Delete** and **Cancel**. The menu item ends in "…". |
| `danger` | no | Shown in red, like Move to Trash. |

Choosing one runs `action <id> <ref>` for each selected item (with several selected, the menu offers only the actions they all have). Exit 0 when it's done, or accepted. Findest shows the message you print, `{"message": "Reconciling x"}`, or "Reconcile: done for "x.yaml"" if you print nothing. Then it lists the folder again, so a deleted item disappears and a changed status shows. Failures work like any verb: the last line of stderr, or 77 to sign in. `action` is stopped after 2 minutes.

Actions are per entry, so offer only what applies: a type that can't be deleted, or a user without permission, needn't see Delete.

### Limits

- `list`, `read` and `search` are stopped after 60 seconds, `login` after 5 minutes. The user sees that it timed out.
- A listing is cached for 60 seconds; ⌘R lists the folder again.
- Problems with single entries (a bad name, an unknown `kind`, a duplicate name, an unreadable `modified`) skip or ignore that field, and they're reported in the status bar. They don't fail the listing.
- A source never changes your files: copying, moving, renaming, deleting and dropping into a source are refused with "Sources are read-only". Changes to the source itself go through its [actions](#actions).

## Test an adapter by hand

```sh
./my-source list /; echo "exit $?"            # valid JSON, exit 0
./my-source list /no-such-folder; echo $?     # exit 1 and a sentence on stderr
./my-source read <a file ref> /tmp/x && cat /tmp/x
./my-source search / '{"names": ["a"]}'      # entries with "folder"; or exit 64
./my-source list / | python3 -m json.tool     # pretty-print
```

Then sign out in your system and check that `list /` exits 77.

## Writing one with a coding agent

Give the agent this file and `examples/static-source`, then describe your system: which command or API lists a folder, how to fetch one item, and how sign-in works. For example: "Write a Findest source adapter like docs/examples/static-source, for S3. `list` uses `aws s3api list-objects-v2` with a delimiter, and `read` uses `aws s3 cp`. Return 77 when the AWS session has expired; `login` runs `aws sso login`."

---

## Protocol v2 — long-lived adapter

The default protocol (v1) spawns a new process per request. Protocol v2 keeps one process alive: Findest sends commands over stdin and the adapter streams responses over stdout. Use it when your adapter has expensive startup (connecting to an API, loading credentials) or when you want to push results to the UI as they arrive.

### Opting in

Add `protocolVersion = 2` to the source's config entry:

```toml
[[sources]]
id = "example-v2"
name = "Example v2"
command = ["~/findest/static-source-v2"]
protocolVersion = 2
```

### Commands → stdin

Findest writes one JSON object per line. Each command has an `id` that the adapter echoes back in every response line.

| Verb | Extra fields | Description |
|---|---|---|
| `list` | `folder`, `continue` (optional) | List a folder |
| `search` | `folder`, `hints`, `continue` (optional) | Search under a folder |
| `read` | `ref`, `dest` | Fetch a file to `dest` |
| `preview` | `ref`, `dest`, `name` | Return an alternative preview (optional verb) |
| `action` | `action`, `ref` | Do one of an entry's [actions](#actions) (optional verb) |
| `login` | — | Sign in |
| `exit` | — | Shut down (no response expected) |

Examples:

```json
{"id": "1", "verb": "list",    "folder": "/docs"}
{"id": "2", "verb": "list",    "folder": "/big",  "continue": "page2"}
{"id": "3", "verb": "search",  "folder": "/",     "hints": {"query": "item ext:txt", "names": ["item"], "extensions": ["txt"], "kind": null}}
{"id": "4", "verb": "read",    "ref": "hello",    "dest": "/Users/me/Library/Caches/findest/sources/ex/hello.txt"}
{"id": "5", "verb": "preview", "ref": "hello",    "dest": "/Users/me/Library/Caches/findest/sources/ex/hello.txt", "name": "hello.txt"}
{"id": "6", "verb": "login"}
{"id": "7", "verb": "exit"}
```

The `hints` object for `search` has the same fields as in v1: `query`, `names`, `extensions`, `kind`.

### Responses → stdout (JSONL)

Every response line carries the originating `id`. Multiple requests may be in flight simultaneously; the adapter may interleave responses as long as each line carries the correct `id`.

#### `list` and `search` — streaming

The adapter emits one entry per line, then a **sentinel** to signal the end. The sentinel is a JSON object with only the `id` field (all other fields absent):

```jsonl
{"id":"1","name":"docs","kind":"folder","ref":"/docs"}
{"id":"1","name":"hello.txt","kind":"file","ref":"hello","label":"Greeting","size":28,"modified":"2026-10-01T12:00:00Z","mime_type":"text/plain"}
{"id":"1"}
```

Entry fields are the same as in v1 (`name`, `kind`, `ref`, `label`, `size`, `modified`, `has_children`), plus the new optional `mime_type` field (see [MIME types](#mime-types-v2)).

For `search`, each entry also carries `"folder"` (the folder ref it's in), same as v1.

**Warnings** may be sent as separate lines at any point before the sentinel:

```jsonl
{"id":"1","warning":"couldn't read permissions for /restricted"}
{"id":"1"}
```

**Paging.** If a folder is large, end the stream with a sentinel that carries a `continue` token. Findest shows a **Load more…** row; opening it sends a new `list` command with `"continue": "<token>"`. Same semantics as v1 paging, just carried on the sentinel instead of in a wrapper object.

```jsonl
{"id":"1","name":"item-1.txt","kind":"file","ref":"item-1"}
{"id":"1","name":"item-2.txt","kind":"file","ref":"item-2"}
{"id":"1","continue":"page2"}
```

For `search`, Findest auto-follows up to 50 continuation pages.

**Attributes and columns** work as in v1: an entry line can carry `"attributes": {…}`, and the sentinel can carry `"columns": [{"key": "status", "title": "Status"}]`.

```jsonl
{"id":"1","name":"x.yaml","kind":"file","ref":"x","attributes":{"status":"Ready"}}
{"id":"1","columns":[{"key":"status","title":"Status"}]}
```

**Notices.** A line without an `id` isn't an answer: the adapter sends it on its own, at any time. `changed` is a folder ref whose contents changed (the root if left out): Findest forgets what it listed there and below, and reloads if it's showing it. `message` is shown in the status bar. Use it when work outlives a request, e.g. answering `list` quickly with a "still building the index" warning, then:

```jsonl
{"changed":"/","message":"The API catalog is ready"}
```

#### `read`

```jsonl
{"id":"4"}
```

Sentinel (id only) = success. The adapter has written the file's content to `dest`.

#### `preview`

The `preview` verb is optional. Findest sends it after `read` when it needs to display the file (Quick Look, the preview pane). The adapter can:

- Return the sentinel with no extra fields — Findest uses the file at `dest` as-is:

```jsonl
{"id":"5"}
```

- Return an alternate file — Findest uses that for display instead:

```jsonl
{"id":"5","mime_type":"image/png","dest":"/tmp/preview-hello.png"}
```

The adapter writes the alternate content to any path it chooses and returns it in `dest`. **The file must have the correct extension** (`.png`, `.pdf`, etc.) — Findest passes the path to Quick Look as-is, and Quick Look determines the format from the extension. The `mime_type` field is accepted but currently unused by Findest.

If the adapter doesn't support `preview`, it can return `"error": "not_supported"` and Findest falls back to the file at `dest`.

#### `action`

```json
{"id": "8", "verb": "action", "action": "reconcile", "ref": "x"}
```

```jsonl
{"id":"8","message":"Reconciling x"}
```

Sentinel = done; `message` is optional. Entries declare their actions with `"actions"` as in v1.

#### `login`

```jsonl
{"id":"6"}
```

Sentinel = signed in.

#### `exit`

No response. The adapter should flush stdout and exit cleanly.

#### Errors

Any response line may carry `"error"` instead of entry data. This terminates the stream for that request (no sentinel needed after an error):

```jsonl
{"id":"1","error":"no such folder /missing"}
```

Two special error strings match the v1 exit codes:

| Error string | Meaning | Findest shows |
|---|---|---|
| `"not_signed_in"` | Equivalent to v1 exit 77 | **Sign In** button |
| `"not_supported"` | Equivalent to v1 exit 64 | Falls back (e.g. folder-walk for `search`) |

Any other string is shown as a failure message in the status bar.

### Timeouts (v2)

| Verb | Timeout |
|---|---|
| `list`, `search` | **30 seconds of inactivity** — if no response line arrives for 30 s, the process is killed and the request fails |
| `read`, `preview` | 60 seconds (wall-clock) |
| `login` | 5 minutes (wall-clock) |

Unlike v1, `list` and `search` may take longer than 60 s in total as long as lines keep arriving within 30 s of each other — useful for large remote datasets.

### MIME types (v2)

File entries in `list` and `search` may carry an optional `"mime_type"` field:

```json
{"id":"1","name":"config","kind":"file","ref":"config-ref","mime_type":"application/json"}
```

Findest uses this to pick the file icon and Quick Look behaviour when the filename has no extension, or when the extension doesn't reflect the actual format. If `mime_type` is absent, behaviour is unchanged (extension-based, same as v1).

### Test an adapter by hand (v2)

Use `printf` to simulate Findest's side:

```sh
printf '{"id":"1","verb":"list","folder":"/"}\n{"id":"2","verb":"exit"}\n' \
  | ./static-source-v2

# Expected: entries one per line, then {"id":"1"} sentinel, then exit
```

Or interactively with `rlwrap`:

```sh
rlwrap ./static-source-v2
# type: {"id":"1","verb":"list","folder":"/"}
# type: {"id":"2","verb":"exit"}
```

### Example

`examples/static-source-v2` is the same fixed tree as `examples/static-source`, rewritten for the v2 protocol. It demonstrates streaming entries, paging, `mime_type`, and `preview`.
