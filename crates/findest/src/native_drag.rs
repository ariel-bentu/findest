//! Native drag source.
//!
//! On macOS, dragging items out of the file list starts a real AppKit drag
//! session carrying file URLs, so items can be dropped into another Findest
//! window, Finder, Mail, or any other app. Each item is also a file promise:
//! Finder mishandles file URLs from other apps (a copy drops whatever is
//! selected in Finder, a move does nothing), but fulfils promises by telling
//! Findest where the item went, so Findest copies or moves it there itself.
//! Other platforms rely on gpui's built-in external drop support and do not
//! implement a native source.

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Condvar, Mutex, OnceLock};
    use std::time::Duration;

    use block2::{DynBlock, RcBlock};
    use findest_core::ops;

    use futures::channel::{mpsc, oneshot};

    use objc2::rc::Retained;
    use objc2::ffi::NSUInteger;
    use objc2::runtime::{AnyClass, AnyObject, Imp, ProtocolObject, Sel};
    use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource,
        NSEvent, NSEventModifierFlags, NSEventType, NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSPasteboard,
        NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardWritingOptions, NSView, NSWorkspace,
    };
    use objc2_foundation::{
        NSArray, NSCocoaErrorDomain, NSCopying, NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
    };
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and DragSource has no Drop impl.
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[name = "FindestDragSource"]
        struct DragSource;

        unsafe impl NSObjectProtocol for DragSource {}

        unsafe impl NSDraggingSource for DragSource {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn source_operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
                // Let the destination choose, as Finder does: move within a volume, copy across.
                // Source files are cache copies: moving one would take it out of the source.
                if COPY_ONLY.get() { NSDragOperation::Copy } else { NSDragOperation::Copy | NSDragOperation::Move | NSDragOperation::Generic }
            }

            #[unsafe(method(draggingSession:endedAtPoint:operation:))]
            fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
                let (ops, changed) = &ENDED_OPS;
                ops.lock().unwrap().push((SESSION.get(), operation));
                changed.notify_all();
                if let Some(tx) = ENDED.with(|e| e.borrow_mut().take()) {
                    let _ = tx.send(operation != NSDragOperation::None);
                }
            }
        }

        unsafe impl NSFilePromiseProviderDelegate for DragSource {
            #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
            fn promised_name(&self, provider: &NSFilePromiseProvider, _file_type: &NSString) -> Retained<NSString> {
                let path = &FilePromise::of(provider).ivars().path;
                NSString::from_str(&path.file_name().unwrap_or_default().to_string_lossy())
            }

            #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
            fn write_promise(&self, provider: &NSFilePromiseProvider, url: &NSURL, done: &DynBlock<dyn Fn(*mut NSError)>) {
                let promise = FilePromise::of(provider).ivars();
                let (source, session, delivered) = (promise.path.clone(), promise.session, promise.delivered.clone());
                let target = url.path().map(|p| PathBuf::from(p.to_string()));
                let done = SendBlock(done.copy());
                // Off the main thread: it waits for the drag to end, and copies can be big.
                std::thread::spawn(move || {
                    let result = match target.as_deref().and_then(Path::parent) {
                        Some(dir) => deliver(&source, dir, ended_operation(session)).map(|d| {
                            let _ = delivered.unbounded_send(d);
                        }),
                        None => Err(anyhow::anyhow!("no destination")),
                    };
                    let error = result.err().map(|_| NSError::new(-1, unsafe { NSCocoaErrorDomain }));
                    let done = done;
                    done.0.call((error.as_ref().map_or(std::ptr::null_mut(), |e| Retained::as_ptr(e).cast_mut()),));
                });
            }
        }
    );

    /// A Finder-safe dragged item: a file promise that is also a plain file
    /// URL, for Findest windows and apps that take files.
    struct PromiseIvars {
        path: PathBuf,
        session: u64,
        /// Told what was done, for the window's Undo.
        delivered: mpsc::UnboundedSender<super::Delivered>,
    }

    define_class!(
        // SAFETY: NSFilePromiseProvider may be subclassed; overrides call super.
        #[unsafe(super(NSFilePromiseProvider, NSObject))]
        #[name = "FindestFilePromise"]
        #[ivars = PromiseIvars]
        struct FilePromise;

        impl FilePromise {
            #[unsafe(method_id(writableTypesForPasteboard:))]
            fn writable_types(&self, pasteboard: &NSPasteboard) -> Retained<NSArray<NSPasteboardType>> {
                // SAFETY: calling the superclass implementation with its own arguments.
                let types: Retained<NSArray<NSPasteboardType>> = unsafe { msg_send![super(self), writableTypesForPasteboard: pasteboard] };
                let mut types = types.to_vec();
                types.push(file_url_type().copy());
                NSArray::from_retained_slice(&types)
            }

            #[unsafe(method(writingOptionsForType:pasteboard:))]
            fn writing_options(&self, ty: &NSPasteboardType, pasteboard: &NSPasteboard) -> NSPasteboardWritingOptions {
                if ty.isEqualToString(file_url_type()) {
                    NSPasteboardWritingOptions(0)
                } else {
                    // SAFETY: as above.
                    unsafe { msg_send![super(self), writingOptionsForType: ty, pasteboard: pasteboard] }
                }
            }

            #[unsafe(method_id(pasteboardPropertyListForType:))]
            fn property_list(&self, ty: &NSPasteboardType) -> Option<Retained<AnyObject>> {
                if ty.isEqualToString(file_url_type()) {
                    let path = NSString::from_str(&self.ivars().path.to_string_lossy());
                    NSURL::fileURLWithPath(&path).absoluteString().map(Retained::into_super).map(Retained::into_super)
                } else {
                    // SAFETY: as above.
                    unsafe { msg_send![super(self), pasteboardPropertyListForType: ty] }
                }
            }
        }
    );

    impl FilePromise {
        fn new(path: PathBuf, session: u64, delivered: mpsc::UnboundedSender<super::Delivered>, delegate: &DragSource) -> Retained<Self> {
            let file_type = NSString::from_str(if path.is_dir() { "public.folder" } else { "public.data" });
            let this = Self::alloc().set_ivars(PromiseIvars { path, session, delivered });
            // SAFETY: NSFilePromiseProvider's designated initializer.
            unsafe {
                msg_send![super(this), initWithFileType: &*file_type, delegate: ProtocolObject::<dyn NSFilePromiseProviderDelegate>::from_ref(delegate)]
            }
        }

        /// The promise behind `provider`: only `FilePromise`s name `DragSource` as delegate.
        fn of(provider: &NSFilePromiseProvider) -> &Self {
            // SAFETY: see above; FilePromise only adds ivars to NSFilePromiseProvider.
            unsafe { &*(provider as *const NSFilePromiseProvider).cast::<Self>() }
        }
    }

    fn file_url_type() -> &'static NSPasteboardType {
        // SAFETY: an AppKit constant.
        unsafe { NSPasteboardTypeFileURL }
    }

    /// The completion handler of a promise, called from a worker thread
    /// (AppKit's handlers may be called from any thread).
    struct SendBlock(RcBlock<dyn Fn(*mut NSError)>);
    // SAFETY: see above.
    unsafe impl Send for SendBlock {}

    /// How each drag session ended, by session number.
    static ENDED_OPS: (Mutex<Vec<(u64, NSDragOperation)>>, Condvar) = (Mutex::new(Vec::new()), Condvar::new());
    static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

    /// The operation `session` ended with. Finder asks for promised files
    /// before telling the source how the drop went, so this waits (a while:
    /// without an answer the drop counts as a copy, which loses nothing).
    fn ended_operation(session: u64) -> NSDragOperation {
        let (ops, changed) = &ENDED_OPS;
        let ops = ops.lock().unwrap();
        let (ops, _) = changed
            .wait_timeout_while(ops, Duration::from_secs(10), |ops| !ops.iter().any(|(s, _)| *s == session))
            .unwrap();
        ops.iter().find(|(s, _)| *s == session).map_or(NSDragOperation::Copy, |(_, op)| *op)
    }

    /// Puts `source` into `dir`, where Finder dropped it: moved when Finder
    /// chose to move it (no ⌥) and it stays on its volume, like Finder;
    /// otherwise copied.
    /// A folder another app gave for a drop that isn't somewhere the user
    /// keeps files (WhatsApp, Mail and the like hand out a temporary or
    /// container folder they clear later): those only ever get a copy.
    fn app_private(dir: &Path) -> bool {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let temp = std::env::temp_dir();
        let temp = temp.canonicalize().unwrap_or(temp);
        let library = std::env::home_dir().map(|h| h.join("Library"));
        dir.starts_with(&temp)
            || ["/private/var", "/private/tmp", "/var", "/tmp", "/Library", "/System"].iter().any(|p| dir.starts_with(p))
            || library.is_some_and(|l| dir.starts_with(l))
    }

    fn deliver(source: &Path, dir: &Path, operation: NSDragOperation) -> anyhow::Result<super::Delivered> {
        let progress = ops::Progress::default();
        let moving = operation.intersects(NSDragOperation::Move | NSDragOperation::Generic)
            && !app_private(dir)
            && !crate::browser::drop_copies(&[source.to_path_buf()], dir, false);
        let sources = [source.to_path_buf()];
        Ok(if moving {
            if source.parent() == Some(dir) {
                super::Delivered::Moved(Vec::new())
            } else {
                super::Delivered::Moved(ops::move_into(&sources, dir, ops::OnConflict::KeepBoth, &progress)?)
            }
        } else {
            super::Delivered::Copied(ops::copy_into(&sources, dir, ops::OnConflict::KeepBoth, &progress)?)
        })
    }

    impl DragSource {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(());
            // SAFETY: NSObject's designated initializer.
            unsafe { msg_send![super(this), init] }
        }
    }

    thread_local! {
        /// Keeps the current drag source alive for the duration of the session.
        static SOURCE: RefCell<Option<Retained<DragSource>>> = const { RefCell::new(None) };
        /// Told when the session ends: whether the items were dropped somewhere.
        static ENDED: RefCell<Option<oneshot::Sender<bool>>> = const { RefCell::new(None) };
        /// The current drag may only copy.
        static COPY_ONLY: Cell<bool> = const { Cell::new(false) };
        /// The current drag's number, for its promises.
        static SESSION: Cell<u64> = const { Cell::new(0) };
        /// Per window (NSWindow address): whether a drop where the pointer is
        /// would copy, as last painted (see `set_drop_copies`).
        static DROP_COPIES: RefCell<HashMap<usize, Option<bool>>> = RefCell::new(HashMap::new());
    }

    fn content_view(window: &gpui::Window) -> Option<&NSView> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
        // SAFETY: gpui hands out its live content view, which lives as long as the window.
        Some(unsafe { handle.ns_view.cast().as_ref() })
    }

    /// Tells the drag handlers whether a drop where the pointer is in `window`
    /// would copy (`None`: no drop target there), so AppKit's badge matches.
    pub fn set_drop_copies(window: &gpui::Window, copies: Option<bool>) {
        if let Some(ns_window) = content_view(window).and_then(|view| view.window()) {
            let key = Retained::as_ptr(&ns_window) as usize;
            DROP_COPIES.with(|d| d.borrow_mut().insert(key, copies));
        }
    }

    /// Starts a native drag of `paths` from `window`. Must be called while gpui
    /// is handling the mouse-drag event (AppKit needs that event). Returns `None`
    /// if the session couldn't be started, else a receiver that gets `true` if
    /// the items were dropped somewhere and `false` if the drag was cancelled
    /// (Esc) or released where nothing accepts files, and a stream of what
    /// was copied or moved into other apps (Finder). `copy_only` stops
    /// destinations from moving the items.
    pub fn start_file_drag(window: &gpui::Window, paths: &[PathBuf], copy_only: bool) -> Option<super::DragSession> {
        let mtm = MainThreadMarker::new()?;
        let view = content_view(window)?;
        let event = NSApplication::sharedApplication(mtm).currentEvent()?;
        let origin = view.convertPoint_fromView(event.locationInWindow(), None);

        let workspace = NSWorkspace::sharedWorkspace();
        let source = DragSource::new(mtm);
        let session = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        let (delivered_tx, delivered) = mpsc::unbounded();
        let items: Vec<Retained<NSDraggingItem>> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let promise = FilePromise::new(path.clone(), session, delivered_tx.clone(), &source);
                let path = NSString::from_str(&path.to_string_lossy());
                let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref::<NSFilePromiseProvider>(&promise));
                // A small stack of icons under the cursor.
                let offset = (i.min(4) * 4) as f64;
                let frame = NSRect::new(NSPoint::new(origin.x - 16.0 + offset, origin.y - 16.0 - offset), NSSize::new(32.0, 32.0));
                let icon = workspace.iconForFile(&path);
                // SAFETY: NSImage is valid dragging-frame contents.
                unsafe { item.setDraggingFrame_contents(frame, Some(&icon)) };
                item
            })
            .collect();
        if items.is_empty() {
            return None;
        }

        COPY_ONLY.set(copy_only);
        SESSION.set(session);
        view.beginDraggingSessionWithItems_event_source(
            &NSArray::from_retained_slice(&items),
            &event,
            ProtocolObject::from_ref(&*source),
        );
        SOURCE.with(|s| *s.borrow_mut() = Some(source));
        // AppKit swallows the mouse-up that ends the drag, so gpui would keep
        // replaying this drag event to the window (its autoscroll timer) until
        // the next click, repainting it with the pointer back here. A mouse-up
        // stops that; sent while gpui is handling this event, it goes no
        // further (no click reaches the app).
        let up = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            NSEventType::LeftMouseUp,
            event.locationInWindow(),
            event.modifierFlags(),
            event.timestamp(),
            event.windowNumber(),
            None,
            0,
            1,
            0.0,
        );
        if let Some(up) = up {
            view.mouseUp(&up);
        }
        let (tx, ended) = oneshot::channel();
        ENDED.with(|e| *e.borrow_mut() = Some(tx));
        Some(super::DragSession { ended, delivered })
    }

    type DragHandler = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> NSUInteger;
    static ENTERED: OnceLock<DragHandler> = OnceLock::new();
    static UPDATED: OnceLock<DragHandler> = OnceLock::new();

    /// gpui's windows answer every file drag with "copy", so the cursor
    /// always shows a green + however the drop will go. This wraps their
    /// handlers to answer "move" instead, unless the source only allows
    /// copying, or ⌥ is held (AppKit then narrows the source's offer to copy),
    /// matching what `Browser::drop_paths` will do. Call once at startup.
    pub fn install_drop_feedback() {
        let Some(class) = AnyClass::get(c"GPUIWindow") else { return };
        for (name, original, wrapper) in [
            (sel!(draggingEntered:), &ENTERED, dragging_entered as DragHandler),
            (sel!(draggingUpdated:), &UPDATED, dragging_updated as DragHandler),
        ] {
            let Some(method) = class.instance_method(name) else { continue };
            if original.get().is_some() {
                continue;
            }
            // SAFETY: both are `(self, _cmd, id<NSDraggingInfo>) -> NSDragOperation` IMPs.
            unsafe {
                let _ = original.set(std::mem::transmute::<Imp, DragHandler>(method.implementation()));
                method.set_implementation(std::mem::transmute::<DragHandler, Imp>(wrapper));
            }
        }
    }

    unsafe extern "C-unwind" fn dragging_entered(this: *mut AnyObject, cmd: Sel, info: *mut AnyObject) -> NSUInteger {
        // SAFETY: set before this wrapper was installed; same arguments as AppKit passed us.
        let op = unsafe { ENTERED.get().expect("installed")(this, cmd, info) };
        unsafe { drop_operation(op, this, info) }
    }

    unsafe extern "C-unwind" fn dragging_updated(this: *mut AnyObject, cmd: Sel, info: *mut AnyObject) -> NSUInteger {
        // SAFETY: as above.
        let op = unsafe { UPDATED.get().expect("installed")(this, cmd, info) };
        unsafe { drop_operation(op, this, info) }
    }

    /// The operation to show for a drag gpui accepted (`op` non-zero) over
    /// `ns_window`: copy where Findest's drop would copy, else move.
    unsafe fn drop_operation(op: NSUInteger, ns_window: *mut AnyObject, info: *mut AnyObject) -> NSUInteger {
        let Some(info) = (unsafe { info.as_ref() }).filter(|_| op != NSDragOperation::None.0) else { return op };
        // SAFETY: `info` is the NSDraggingInfo AppKit passed to the handler.
        let offered: NSDragOperation = unsafe { msg_send![info, draggingSourceOperationMask] };
        let copies = DROP_COPIES.with(|d| d.borrow().get(&(ns_window as usize)).copied().flatten());
        if offered.contains(NSDragOperation::Move) && copies != Some(true) {
            NSDragOperation::Move.0
        } else if offered.contains(NSDragOperation::Copy) {
            NSDragOperation::Copy.0
        } else {
            op
        }
    }

    /// Whether ⌥ is held right now. gpui's modifier state can be stale after an
    /// AppKit drag session, so drops ask the system directly.
    pub fn option_key_down() -> bool {
        NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Option)
    }
}

#[cfg(target_os = "macos")]
pub use mac::{install_drop_feedback, option_key_down, set_drop_copies, start_file_drag};

#[cfg(not(target_os = "macos"))]
pub fn start_file_drag(_window: &gpui::Window, _paths: &[std::path::PathBuf], _copy_only: bool) -> Option<DragSession> {
    None
}

/// A drag started by `start_file_drag`.
pub struct DragSession {
    /// Gets whether the items were dropped somewhere.
    pub ended: futures::channel::oneshot::Receiver<bool>,
    /// What other apps had Findest copy or move, item by item.
    pub delivered: futures::channel::mpsc::UnboundedReceiver<Delivered>,
}

/// What a drop into another app (Finder) did with one dragged item.
pub enum Delivered {
    /// `(from, to)`, as `ops::move_into` returns it.
    Moved(Vec<(std::path::PathBuf, std::path::PathBuf)>),
    /// The copies made.
    Copied(Vec<std::path::PathBuf>),
}

#[cfg(not(target_os = "macos"))]
pub fn install_drop_feedback() {}

#[cfg(not(target_os = "macos"))]
pub fn set_drop_copies(_window: &gpui::Window, _copies: Option<bool>) {}

#[cfg(not(target_os = "macos"))]
pub fn option_key_down() -> bool {
    false
}
