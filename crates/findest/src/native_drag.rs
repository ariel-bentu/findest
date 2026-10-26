//! Native drag source.
//!
//! On macOS, dragging items out of the file list starts a real AppKit drag
//! session carrying file URLs, so items can be dropped into another Findest
//! window, Finder, Mail, or any other app. Other platforms rely on gpui's
//! built-in external drop support and do not implement a native source.

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    use futures::channel::oneshot;

    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{AllocAnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send};
    use objc2_app_kit::{
        NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession, NSDraggingSource,
        NSEvent, NSEventModifierFlags, NSView, NSWorkspace,
    };
    use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL};
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
                if let Some(tx) = ENDED.with(|e| e.borrow_mut().take()) {
                    let _ = tx.send(operation != NSDragOperation::None);
                }
            }
        }
    );

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
    }

    /// Starts a native drag of `paths` from `window`. Must be called while gpui
    /// is handling the mouse-drag event (AppKit needs that event). Returns `None`
    /// if the session couldn't be started, else a receiver that gets `true` if
    /// the items were dropped somewhere and `false` if the drag was cancelled
    /// (Esc) or released where nothing accepts files. `copy_only` stops
    /// destinations from moving the items.
    pub fn start_file_drag(window: &gpui::Window, paths: &[PathBuf], copy_only: bool) -> Option<oneshot::Receiver<bool>> {
        let mtm = MainThreadMarker::new()?;
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
        // SAFETY: gpui hands out its live content view; we only use it during this call.
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let event = NSApplication::sharedApplication(mtm).currentEvent()?;
        let origin = view.convertPoint_fromView(event.locationInWindow(), None);

        let workspace = NSWorkspace::sharedWorkspace();
        let items: Vec<Retained<NSDraggingItem>> = paths
            .iter()
            .filter_map(|path| path.to_str())
            .enumerate()
            .map(|(i, path)| {
                let path = NSString::from_str(path);
                let url = NSURL::fileURLWithPath(&path);
                let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*url));
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
        let source = DragSource::new(mtm);
        view.beginDraggingSessionWithItems_event_source(
            &NSArray::from_retained_slice(&items),
            &event,
            ProtocolObject::from_ref(&*source),
        );
        SOURCE.with(|s| *s.borrow_mut() = Some(source));
        let (tx, rx) = oneshot::channel();
        ENDED.with(|e| *e.borrow_mut() = Some(tx));
        Some(rx)
    }

    /// Whether ⌥ is held right now. gpui's modifier state can be stale after an
    /// AppKit drag session, so drops ask the system directly.
    pub fn option_key_down() -> bool {
        NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Option)
    }
}

#[cfg(target_os = "macos")]
pub use mac::{option_key_down, start_file_drag};

#[cfg(not(target_os = "macos"))]
pub fn start_file_drag(
    _window: &gpui::Window,
    _paths: &[std::path::PathBuf],
    _copy_only: bool,
) -> Option<futures::channel::oneshot::Receiver<bool>> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn option_key_down() -> bool {
    false
}
