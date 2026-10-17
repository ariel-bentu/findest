# Backlog

Ideas that were designed but not built yet.

## Resume transfers after quitting or a crash

**Status:** deferred (concern about copy performance; see "Cost" below).

### Problem

A copy or move that stops for any reason other than cancelling (a network
volume going away, a full disk) can be continued with **Retry**: finished
files are kept and a partly copied file is continued
(`ops::transfer` with `resume`). That state lives only in memory, so if
Findest quits or crashes, there is nothing to resume on the next launch. The
partly copied item stays on disk, and quitting during a transfer stops it
without asking.

### Design

**Journal.** One small JSON file per transfer in
`~/Library/Application Support/findest/transfers/`, holding:

- the planned items, each with where it comes from and where it goes
  (`ops::Planned`), *after* name clashes were settled, so the user isn't
  asked again;
- whether it's a copy or a move;
- how many items are finished (`items_done`);
- the item being written (`partial`).

It's written when the transfer starts and rewritten after each item
finishes. Items are the top-level things being copied, so 10 selected
folders means about 10 small writes, not one per file inside. It's deleted
when the transfer completes, is cancelled, or is discarded.

**At launch.** For each leftover journal, the status bar shows e.g.
*"Unfinished copy to Backup · 3 of 10 items done"* with **Resume** and
**✕ Discard**:

- **Resume:** recounts the remaining items for progress, then runs
  `ops::transfer(items, moving, resume = true)` starting at `items_done`.
  Finished files are kept and the partly copied file is continued. A move
  deletes each original only after its copy is complete, so an interrupted
  move loses nothing.
- **Discard:** removes the partly copied item (`Progress::discard_partial`)
  and the journal.
- **Destination not available** (e.g. the network volume isn't mounted):
  say so and keep offering Resume.
- **Sources gone:** offer only Discard.

**Safeguard after a crash or power cut.** The end of the partly copied file
may not have reached the disk. On resume after an unclean exit (a journal
whose transfer wasn't marked as stopped cleanly), cut the partial file back
by one chunk (4 MB) before continuing, so a damaged tail is copied again.

**Quitting during a transfer.** Ask: *"A copy is in progress. Quit anyway?
You can resume it next time you open Findest."* On quit, mark the journal as
stopped cleanly.

### Cost

- **Copy performance:** one small JSON write per top-level item, next to the
  megabytes of data being copied. This is expected to be negligible, but it
  should be measured, e.g. copying a folder of many small top-level items to
  a slow USB or network volume, with and without the journal.
- **Effort:** about the same as the pause/resume/retry change.

### Tests to add

- The journal is written at start, updated per item, and removed on
  completion, cancel and discard.
- Resuming from a journal after a simulated restart continues from
  `items_done`, keeps finished files and finishes the partial one.
- After an unclean exit, the partial file is cut back one chunk and the
  result matches the source.
- A missing destination keeps the journal and offers Resume; missing
  sources offer only Discard.
- An interrupted move never loses an original.
