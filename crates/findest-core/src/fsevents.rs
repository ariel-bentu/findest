//! that, so this talks to CoreServices directly.

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, OsStr, c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

pub enum Event {
    Changed(PathBuf),
    /// Changes were lost under this folder, or anywhere (`None`): only a
    /// rescan can catch up.
    Rescan(Option<PathBuf>),
    /// Every change since the replayed event id has been delivered.
    HistoryDone,
}

/// Stops the stream when dropped.
pub struct EventStream {
    stream: *mut c_void,
    queue: *mut c_void,
}

type Handler = Box<dyn Fn(Vec<Event>, u64) + Send>;

impl EventStream {
    /// Watches `paths` recursively. With `since`, changes after that event
    /// id are replayed first. `on_events` gets each batch with the id of its
    /// last event, and runs on a private serial queue.
    pub fn start(
        paths: &[PathBuf],
        since: Option<u64>,
        latency_secs: f64,
        on_events: impl Fn(Vec<Event>, u64) + Send + 'static,
    ) -> Option<EventStream> {
        let handler: Box<Handler> = Box::new(Box::new(on_events));
        let context = Context {
            version: 0,
            info: Box::into_raw(handler).cast(),
            retain: None,
            release: Some(release_handler),
            copy_description: None,
        };
        unsafe {
            let strings: Vec<CFRef> = paths
                .iter()
                .map(|p| {
                    let bytes = p.as_os_str().as_bytes();
                    CFStringCreateWithBytes(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, UTF8, 0)
                })
                .filter(|s| !s.is_null())
                .collect();
            let array = CFArrayCreate(std::ptr::null(), strings.as_ptr(), strings.len() as isize, &kCFTypeArrayCallBacks);
            for s in strings {
                CFRelease(s);
            }
            let flags = CREATE_FILE_EVENTS | CREATE_NO_DEFER;
            let stream = FSEventStreamCreate(std::ptr::null(), on_raw_events, &context, array, since.unwrap_or(SINCE_NOW), latency_secs, flags);
            CFRelease(array);
            if stream.is_null() {
                release_handler(context.info);
                return None;
            }
            let queue = dispatch_queue_create(c"findest.fsevents".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            let watched = EventStream { stream, queue };
            (FSEventStreamStart(stream) != 0).then_some(watched)
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream); // frees the handler
            dispatch_release(self.queue);
        }
    }
}

/// The newest event id: changes after now will have larger ids.
pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

/// Identifies the event history of the volume holding `path`. It changes if
/// the history is reset, which invalidates saved event ids.
pub fn history_id(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let dev = std::os::unix::fs::MetadataExt::dev(&meta) as libc::dev_t;
    unsafe {
        let uuid = FSEventsCopyUUIDForDevice(dev);
        if uuid.is_null() {
            return None;
        }
        let bytes = CFUUIDGetUUIDBytes(uuid).0;
        CFRelease(uuid);
        Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }
}

extern "C" fn on_raw_events(
    _stream: *const c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    let handler = unsafe { &*(info as *const Handler) };
    let paths = paths as *const *const c_char;
    let events = (0..count)
        .filter_map(|i| {
            let (flag, path) = unsafe { (*flags.add(i), CStr::from_ptr(*paths.add(i))) };
            let path = PathBuf::from(OsStr::from_bytes(path.to_bytes()));
            if flag & EVENT_HISTORY_DONE != 0 {
                Some(Event::HistoryDone)
            } else if flag & (EVENT_USER_DROPPED | EVENT_KERNEL_DROPPED | EVENT_IDS_WRAPPED | EVENT_ROOT_CHANGED) != 0 {
                Some(Event::Rescan(None))
            } else if flag & EVENT_MUST_SCAN_SUBDIRS != 0 {
                Some(Event::Rescan(Some(path)))
            } else {
                Some(Event::Changed(path))
            }
        })
        .collect();
    // Events arrive in id order.
    let last_id = if count == 0 { 0 } else { unsafe { *ids.add(count - 1) } };
    // Unwinding into CoreServices would abort; drop the batch instead.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(events, last_id)));
}

extern "C" fn release_handler(info: *const c_void) {
    drop(unsafe { Box::from_raw(info as *mut Handler) });
}

// ---- CoreServices / CoreFoundation / libdispatch ------------------------

type CFRef = *const c_void;

#[repr(C)]
struct Context {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CFRef>,
}

#[repr(C)]
struct UuidBytes([u8; 16]);

type Callback = extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

const SINCE_NOW: u64 = u64::MAX;
const UTF8: u32 = 0x0800_0100;
const CREATE_NO_DEFER: u32 = 0x02;
const CREATE_FILE_EVENTS: u32 = 0x10;
const EVENT_MUST_SCAN_SUBDIRS: u32 = 0x01;
const EVENT_USER_DROPPED: u32 = 0x02;
const EVENT_KERNEL_DROPPED: u32 = 0x04;
const EVENT_IDS_WRAPPED: u32 = 0x08;
const EVENT_HISTORY_DONE: u32 = 0x10;
const EVENT_ROOT_CHANGED: u32 = 0x20;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFRef,
        callback: Callback,
        context: *const Context,
        paths: CFRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *mut c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithBytes(alloc: CFRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, count: isize, callbacks: *const c_void) -> CFRef;
    fn CFUUIDGetUUIDBytes(uuid: CFRef) -> UuidBytes;
    fn CFRelease(cf: CFRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(object: *mut c_void);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Waits for a change to `want`, ignoring other events.
    fn wait_for(rx: &mpsc::Receiver<PathBuf>, want: &Path) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(path) if path == want => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }

    fn watch(dir: &Path, since: Option<u64>) -> (EventStream, mpsc::Receiver<PathBuf>) {
        let (tx, rx) = mpsc::channel();
        let stream = EventStream::start(&[dir.to_path_buf()], since, 0.1, move |events, _| {
            for event in events {
                if let Event::Changed(path) = event {
                    let _ = tx.send(path);
                }
            }
        })
        .expect("stream starts");
        (stream, rx)
    }

    #[test]
    fn live_and_replayed_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap(); // events report /private/var/...
        assert!(history_id(&dir).is_some());

        let (live, rx) = watch(&dir, None);
        std::fs::write(dir.join("live.txt"), "x").unwrap();
        assert!(wait_for(&rx, &dir.join("live.txt")), "live change reported");
        drop(live);

        // Changed while nothing watches, then replayed from a saved id.
        let since = current_event_id();
        std::fs::write(dir.join("offline.txt"), "x").unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let (_replay, rx) = watch(&dir, Some(since));
        assert!(wait_for(&rx, &dir.join("offline.txt")), "change made while not watching is replayed");
    }
}
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::{Path, PathBuf};

    #[derive(Clone, Debug)]
    #[allow(dead_code)]
    pub enum Event {
        Changed(PathBuf),
        Rescan(Option<PathBuf>),
        HistoryDone,
    }

    pub struct EventStream;

    impl EventStream {
        pub fn start(
            _paths: &[PathBuf],
            _since: Option<u64>,
            _latency_secs: f64,
            _on_events: impl Fn(Vec<Event>, u64) + Send + 'static,
        ) -> Option<EventStream> {
            None
        }
    }

    pub fn current_event_id() -> u64 {
        0
    }

    pub fn history_id(_path: &Path) -> Option<String> {
        None
    }
}

#[cfg(not(target_os = "macos"))]
pub use imp::*;
