//! Sources v2 protocol: long-lived adapter process communicating over stdin/stdout.
//!
//! Findest writes one JSON command per line to the adapter's stdin; the adapter
//! streams back one JSON object per line on stdout.  See `docs/sources.md` for
//! the full protocol spec.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::sources::{Source, SourceError, WireColumn};

/// How long `list` and `search` may be idle (no line received) before timing out.
pub(crate) const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
/// Wall-clock limit for `read` and `preview`.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// Wall-clock limit for `login`.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Wall-clock limit for an entry's action.
const ACTION_TIMEOUT: Duration = Duration::from_secs(2 * 60);

// ---- Wire types sent to the adapter -----------------------------------

#[derive(Serialize)]
struct Cmd {
    id: String,
    verb: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    continue_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hints: Option<Value>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
}

// ---- Wire types received from the adapter -----------------------------

#[derive(Deserialize)]
struct RawLine {
    id: String,
    #[serde(rename = "ref")]
    reference: Option<String>,
    #[serde(rename = "name")]
    entry_name: Option<String>,
    kind: Option<String>,
    label: Option<String>,
    size: Option<u64>,
    modified: Option<String>,
    has_children: Option<bool>,
    folder: Option<String>,
    mime_type: Option<String>,
    #[serde(default)]
    attributes: serde_json::Map<String, Value>,
    #[serde(default)]
    actions: Vec<crate::sources::SourceAction>,
    /// On the sentinel of `action`: what to tell the user.
    message: Option<String>,
    /// On the sentinel: attributes to show as columns.
    columns: Option<Vec<WireColumn>>,
    #[serde(rename = "continue")]
    continue_token: Option<String>,
    warning: Option<String>,
    error: Option<String>,
    dest: Option<String>,
}

// ---- Public decoded types ---------------------------------------------

pub(crate) struct WireEntry {
    pub(crate) reference: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) label: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) modified: Option<String>,
    pub(crate) has_children: Option<bool>,
    pub(crate) folder: Option<String>,
    pub(crate) mime_type: Option<String>,
    pub(crate) attributes: serde_json::Map<String, Value>,
    pub(crate) actions: Vec<crate::sources::SourceAction>,
}

pub(crate) enum V2Line {
    Entry(Box<WireEntry>),
    Warning(String),
    /// End-of-stream sentinel; carries the continue token for paged list/search.
    Done { continue_token: Option<String>, columns: Vec<WireColumn>, message: Option<String> },
    Error(V2Error),
}

pub(crate) enum V2Error {
    NotSignedIn,
    NotSupported,
    Message(String),
}

impl V2Error {
    pub(crate) fn into_source_error(self, source: &Source, verb: &str) -> SourceError {
        match self {
            V2Error::NotSignedIn => SourceError::NotSignedIn { id: source.id.clone(), name: source.name.clone() },
            V2Error::NotSupported => SourceError::NotSupported { verb: verb.to_string(), name: source.name.clone() },
            V2Error::Message(m) => SourceError::Failed(format!("{}: {m}", source.name)),
        }
    }
}

pub(crate) enum PreviewResult {
    SameAsContent,
    Alternate { dest: String, mime_type: String },
}

// ---- V2Process --------------------------------------------------------

type PendingMap = Mutex<HashMap<String, std::sync::mpsc::SyncSender<V2Line>>>;

pub(crate) struct V2Process {
    stdin: Mutex<BufWriter<ChildStdin>>,
    pending: Arc<PendingMap>,
    next_id: AtomicU64,
    child: Mutex<Child>,
}

impl Drop for V2Process {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

impl V2Process {
    pub(crate) fn spawn(source: &Source) -> Result<Arc<V2Process>, SourceError> {
        use std::os::unix::process::CommandExt;
        let Some((program, own_args)) = source.command.split_first() else {
            return Err(SourceError::Failed(format!("{}: no command configured", source.name)));
        };
        let mut child = Command::new(crate::paths::expand_tilde(program))
            .env("PATH", crate::sources::adapter_path())
            .args(own_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| SourceError::Failed(format!("{}: can't run {program}: {e}", source.name)))?;

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();
        let writer = BufWriter::new(child_stdin);

        let reader = BufReader::new(child_stdout);
        let pending: Arc<PendingMap> = Arc::new(Mutex::new(HashMap::new()));
        let pending_bg = pending.clone();
        let src_name = source.name.clone();
        let src_id = source.id.clone();
        std::thread::spawn(move || reader_loop(reader, pending_bg, src_name, src_id));

        Ok(Arc::new(V2Process {
            stdin: Mutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            child: Mutex::new(child),
        }))
    }

    fn next_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn send(&self, cmd: &Cmd) -> Result<std::sync::mpsc::Receiver<V2Line>, SourceError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        self.pending.lock().unwrap().insert(cmd.id.clone(), tx);
        let mut w = self.stdin.lock().unwrap();
        let result = (|| -> std::io::Result<()> {
            serde_json::to_writer(&mut *w, cmd).map_err(std::io::Error::other)?;
            w.write_all(b"\n")?;
            w.flush()
        })();
        if let Err(e) = result {
            self.pending.lock().unwrap().remove(&cmd.id);
            return Err(SourceError::Failed(format!("v2 write error: {e}")));
        }
        Ok(rx)
    }

    pub(crate) fn list(
        &self,
        source: &Source,
        folder: String,
        continue_token: Option<String>,
        on_entry: &mut impl FnMut(WireEntry),
        on_warning: &mut impl FnMut(String),
    ) -> Result<(Option<String>, Vec<WireColumn>), SourceError> {
        let rx = self.send(&Cmd {
            id: self.next_id(),
            verb: "list",
            folder: Some(folder),
            continue_token,
            hints: None,
            reference: None,
            dest: None,
            name: None,
            action: None,
        })?;
        drain_stream(rx, source, "list", INACTIVITY_TIMEOUT, on_entry, on_warning)
    }

    pub(crate) fn search(
        &self,
        source: &Source,
        folder: String,
        hints: Value,
        continue_token: Option<String>,
        on_entry: &mut impl FnMut(WireEntry),
        on_warning: &mut impl FnMut(String),
    ) -> Result<(Option<String>, Vec<WireColumn>), SourceError> {
        let rx = self.send(&Cmd {
            id: self.next_id(),
            verb: "search",
            folder: Some(folder),
            continue_token,
            hints: Some(hints),
            reference: None,
            dest: None,
            name: None,
            action: None,
        })?;
        drain_stream(rx, source, "search", INACTIVITY_TIMEOUT, on_entry, on_warning)
    }

    pub(crate) fn read(&self, source: &Source, reference: String, dest: String) -> Result<(), SourceError> {
        let rx = self.send(&Cmd {
            id: self.next_id(),
            verb: "read",
            folder: None,
            continue_token: None,
            hints: None,
            reference: Some(reference),
            dest: Some(dest),
            name: None,
            action: None,
        })?;
        wait_done(rx, source, "read", READ_TIMEOUT)
    }

    pub(crate) fn preview(
        &self,
        source: &Source,
        reference: String,
        dest: String,
        name: String,
    ) -> Result<PreviewResult, SourceError> {
        let rx = self.send(&Cmd {
            id: self.next_id(),
            verb: "preview",
            folder: None,
            continue_token: None,
            hints: None,
            reference: Some(reference),
            dest: Some(dest),
            name: Some(name),
            action: None,
        })?;
        wait_preview(rx, source, READ_TIMEOUT)
    }

    pub(crate) fn login(&self, source: &Source) -> Result<(), SourceError> {
        let rx = self.send(&Cmd {
            id: self.next_id(),
            verb: "login",
            folder: None,
            continue_token: None,
            hints: None,
            reference: None,
            dest: None,
            name: None,
            action: None,
        })?;
        wait_done(rx, source, "login", LOGIN_TIMEOUT)
    }

    /// Runs one of an entry's actions; returns the adapter's message, if any.
    pub(crate) fn action(&self, source: &Source, action: String, reference: String) -> Result<Option<String>, SourceError> {
        let rx = self.send(&Cmd {
            id: self.next_id(),
            verb: "action",
            folder: None,
            continue_token: None,
            hints: None,
            reference: Some(reference),
            dest: None,
            name: None,
            action: Some(action),
        })?;
        loop {
            match rx.recv_timeout(ACTION_TIMEOUT) {
                Ok(V2Line::Done { message, .. }) => return Ok(message),
                Ok(V2Line::Entry(_) | V2Line::Warning(_)) => {}
                Ok(V2Line::Error(e)) => return Err(e.into_source_error(source, "action")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(SourceError::Failed(format!(
                        "{}: the action didn't finish within {ACTION_TIMEOUT:?} and was stopped",
                        source.name
                    )));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(SourceError::Failed(format!("{}: adapter exited unexpectedly", source.name)));
                }
            }
        }
    }

    pub(crate) fn exit(&self) {
        let id = self.next_id();
        let mut w = self.stdin.lock().unwrap();
        let _ = serde_json::to_writer(&mut *w, &serde_json::json!({"id": id, "verb": "exit"}));
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    }
}

// ---- Background reader -----------------------------------------------

/// A line without an `id`: the adapter tells Findest something on its own.
#[derive(Deserialize)]
struct NoticeLine {
    changed: Option<String>,
    message: Option<String>,
}

fn reader_loop(mut reader: BufReader<ChildStdout>, pending: Arc<PendingMap>, src_name: String, src_id: String) {
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) | Err(_) => break, // EOF or read error
            Ok(_) => {}
        }
        let value: Value = match serde_json::from_str(buf.trim()) {
            Ok(v) => v,
            Err(_) => continue, // malformed line: skip
        };
        if value.get("id").is_none() {
            if let Ok(notice) = serde_json::from_value::<NoticeLine>(value) {
                crate::sources::global().notice(&src_id, notice.changed.as_deref(), notice.message.as_deref());
            }
            continue;
        }
        let raw: RawLine = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(_) => continue, // malformed line: skip
        };
        let id = raw.id.clone();
        let (is_terminal, msg) = decode_line(raw);
        if let Some(tx) = pending.lock().unwrap().get(&id) {
            let _ = tx.send(msg);
        }
        if is_terminal {
            pending.lock().unwrap().remove(&id);
        }
    }
    // Adapter exited: wake all waiting callers.
    let err_msg = format!("{src_name}: adapter exited unexpectedly");
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(V2Line::Error(V2Error::Message(err_msg.clone())));
    }
}

fn decode_line(raw: RawLine) -> (bool, V2Line) {
    if let Some(err) = raw.error {
        let e = match err.as_str() {
            "not_signed_in" => V2Error::NotSignedIn,
            "not_supported" => V2Error::NotSupported,
            _ => V2Error::Message(err),
        };
        return (true, V2Line::Error(e));
    }
    if let Some(w) = raw.warning {
        return (false, V2Line::Warning(w));
    }
    if let (Some(name), Some(kind), Some(reference)) = (raw.entry_name, raw.kind, raw.reference) {
        return (
            false,
            V2Line::Entry(Box::new(WireEntry {
                reference,
                name,
                kind,
                label: raw.label,
                size: raw.size,
                modified: raw.modified,
                has_children: raw.has_children,
                folder: raw.folder,
                mime_type: raw.mime_type,
                attributes: raw.attributes,
                actions: raw.actions,
            })),
        );
    }
    // Sentinel: done, optionally with a continue token or preview fields.
    // For preview: adapter sends {"id":"N","mime_type":"...","dest":"..."} with no kind/name.
    // We route this through Done and let wait_preview inspect the raw fields — but we've
    // already consumed them.  Instead, treat a line with mime_type+dest but no name/kind
    // as a special preview sentinel by re-using Entry with a synthetic name.
    if let (Some(mt), Some(d)) = (raw.mime_type, raw.dest) {
        // preview alternate response
        return (
            true,
            V2Line::Entry(Box::new(WireEntry {
                reference: String::new(),
                name: d,   // dest stored in name for convenience
                kind: String::new(),
                label: None,
                size: None,
                modified: None,
                has_children: None,
                folder: None,
                mime_type: Some(mt),
                attributes: Default::default(),
                actions: Vec::new(),
            })),
        );
    }
    (true, V2Line::Done { continue_token: raw.continue_token, columns: raw.columns.unwrap_or_default(), message: raw.message })
}

// ---- Receive helpers -------------------------------------------------

fn drain_stream(
    rx: std::sync::mpsc::Receiver<V2Line>,
    source: &Source,
    verb: &str,
    timeout: Duration,
    on_entry: &mut impl FnMut(WireEntry),
    on_warning: &mut impl FnMut(String),
) -> Result<(Option<String>, Vec<WireColumn>), SourceError> {
    loop {
        match rx.recv_timeout(timeout) {
            Ok(V2Line::Entry(e)) => on_entry(*e),
            Ok(V2Line::Warning(w)) => on_warning(w),
            Ok(V2Line::Done { continue_token, columns, .. }) => return Ok((continue_token, columns)),
            Ok(V2Line::Error(e)) => return Err(e.into_source_error(source, verb)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(SourceError::Failed(format!(
                    "{}: `{verb}` was idle for {timeout:?} and was stopped",
                    source.name
                )));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(SourceError::Failed(format!("{}: adapter exited unexpectedly", source.name)));
            }
        }
    }
}

fn wait_done(
    rx: std::sync::mpsc::Receiver<V2Line>,
    source: &Source,
    verb: &str,
    timeout: Duration,
) -> Result<(), SourceError> {
    loop {
        match rx.recv_timeout(timeout) {
            Ok(V2Line::Done { .. }) => return Ok(()),
            Ok(V2Line::Entry(_) | V2Line::Warning(_)) => {} // unexpected but harmless
            Ok(V2Line::Error(e)) => return Err(e.into_source_error(source, verb)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(SourceError::Failed(format!(
                    "{}: `{verb}` didn't finish within {timeout:?} and was stopped",
                    source.name
                )));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(SourceError::Failed(format!("{}: adapter exited unexpectedly", source.name)));
            }
        }
    }
}

fn wait_preview(
    rx: std::sync::mpsc::Receiver<V2Line>,
    source: &Source,
    timeout: Duration,
) -> Result<PreviewResult, SourceError> {
    loop {
        match rx.recv_timeout(timeout) {
            Ok(V2Line::Done { .. }) => return Ok(PreviewResult::SameAsContent),
            Ok(V2Line::Entry(e)) if e.mime_type.is_some() => {
                return Ok(PreviewResult::Alternate { dest: e.name, mime_type: e.mime_type.unwrap() });
            }
            Ok(V2Line::Entry(_) | V2Line::Warning(_)) => {}
            Ok(V2Line::Error(e)) => return Err(e.into_source_error(source, "preview")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(SourceError::Failed(format!(
                    "{}: `preview` didn't finish within {timeout:?} and was stopped",
                    source.name
                )));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(SourceError::Failed(format!("{}: adapter exited unexpectedly", source.name)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn v2_source(dir: &Path, script: &str) -> Source {
        let p = dir.join("adapter");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        Source {
            id: "t".into(),
            name: "Test".into(),
            command: vec![p.to_string_lossy().into()],
            root: "/".into(),
            protocol_version: 2,
        }
    }

    #[test]
    fn list() {
        let tmp = tempfile::tempdir().unwrap();
        let source = v2_source(
            tmp.path(),
            r#"#!/usr/bin/env python3
import json, sys
for cmd in (json.loads(l) for l in sys.stdin):
    v = cmd["verb"]
    if v == "list":
        print(json.dumps({"id": cmd["id"], "name": "a.txt", "kind": "file", "ref": "a"}), flush=True)
        print(json.dumps({"id": cmd["id"], "name": "b.txt", "kind": "file", "ref": "b", "mime_type": "text/plain"}), flush=True)
        print(json.dumps({"id": cmd["id"]}), flush=True)
    elif v == "exit":
        break
"#,
        );
        let proc = V2Process::spawn(&source).unwrap();
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let (cont, _) = proc.list(&source, "/".into(), None, &mut |e| entries.push(e), &mut |w| warnings.push(w)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[1].mime_type.as_deref(), Some("text/plain"));
        assert!(cont.is_none());
        assert!(warnings.is_empty());
        proc.exit();
    }

    #[test]
    fn list_paging() {
        let tmp = tempfile::tempdir().unwrap();
        let source = v2_source(
            tmp.path(),
            r#"#!/usr/bin/env python3
import json, sys
for cmd in (json.loads(l) for l in sys.stdin):
    v, id = cmd["verb"], cmd["id"]
    tok = cmd.get("continue")
    if v == "list" and tok is None:
        print(json.dumps({"id": id, "name": "p1.txt", "kind": "file", "ref": "p1"}), flush=True)
        print(json.dumps({"id": id, "continue": "page2"}), flush=True)
    elif v == "list" and tok == "page2":
        print(json.dumps({"id": id, "name": "p2.txt", "kind": "file", "ref": "p2"}), flush=True)
        print(json.dumps({"id": id}), flush=True)
    elif v == "exit":
        break
"#,
        );
        let proc = V2Process::spawn(&source).unwrap();
        let mut entries = Vec::new();
        let (cont, _) = proc.list(&source, "/".into(), None, &mut |e| entries.push(e.name.clone()), &mut |_| {}).unwrap();
        assert_eq!(entries, vec!["p1.txt"]);
        assert_eq!(cont.as_deref(), Some("page2"));

        entries.clear();
        let (cont2, _) = proc.list(&source, "/".into(), Some("page2".into()), &mut |e| entries.push(e.name.clone()), &mut |_| {}).unwrap();
        assert_eq!(entries, vec!["p2.txt"]);
        assert!(cont2.is_none());
        proc.exit();
    }

    #[test]
    fn not_signed_in_error() {
        let tmp = tempfile::tempdir().unwrap();
        let source = v2_source(
            tmp.path(),
            r#"#!/usr/bin/env python3
import json, sys
for cmd in (json.loads(l) for l in sys.stdin):
    if cmd["verb"] == "exit": break
    print(json.dumps({"id": cmd["id"], "error": "not_signed_in"}), flush=True)
"#,
        );
        let proc = V2Process::spawn(&source).unwrap();
        let err = proc.list(&source, "/".into(), None, &mut |_| {}, &mut |_| {}).unwrap_err();
        assert!(matches!(err, SourceError::NotSignedIn { .. }), "{err}");
        proc.exit();
    }

    #[test]
    fn inactivity_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let source = v2_source(
            tmp.path(),
            r#"#!/usr/bin/env python3
import json, sys, time
for cmd in (json.loads(l) for l in sys.stdin):
    if cmd["verb"] == "list":
        time.sleep(60)  # stall
    elif cmd["verb"] == "exit":
        break
"#,
        );
        // Override timeout for the test via a shorter inactivity window.
        // We use the internal drain_stream directly with a short timeout.
        let proc = V2Process::spawn(&source).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        proc.pending.lock().unwrap().insert("1".into(), tx);
        {
            let mut w = proc.stdin.lock().unwrap();
            let _ = w.write_all(b"{\"id\":\"1\",\"verb\":\"list\",\"folder\":\"/\"}\n");
            let _ = w.flush();
        }
        let short = Duration::from_millis(200);
        let err = drain_stream(rx, &source, "list", short, &mut |_| {}, &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("idle"), "{err}");
    }

    #[test]
    fn read_and_preview() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out.txt");
        let dest_s = dest.to_string_lossy().to_string();
        let alt_dest = tmp.path().join("preview.png");
        let alt_dest_s = alt_dest.to_string_lossy().to_string();
        let source = v2_source(
            tmp.path(),
            &format!(
                r#"#!/usr/bin/env python3
import json, sys
for cmd in (json.loads(l) for l in sys.stdin):
    v, id = cmd["verb"], cmd["id"]
    if v == "read":
        with open(cmd["dest"], "w") as f: f.write("hello")
        print(json.dumps({{"id": id}}), flush=True)
    elif v == "preview":
        with open("{alt_dest_s}", "w") as f: f.write("img")
        print(json.dumps({{"id": id, "mime_type": "image/png", "dest": "{alt_dest_s}"}}), flush=True)
    elif v == "exit":
        break
"#
            ),
        );
        let proc = V2Process::spawn(&source).unwrap();
        proc.read(&source, "ref1".into(), dest_s.clone()).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello");

        let prev = proc.preview(&source, "ref1".into(), dest_s, "out.txt".into()).unwrap();
        assert!(matches!(prev, PreviewResult::Alternate { mime_type, .. } if mime_type == "image/png"));
        proc.exit();
    }
}
