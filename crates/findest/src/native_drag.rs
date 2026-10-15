//! Native (AppKit) drag source. Dragging items out of the file list starts a
//! real macOS drag session carrying file URLs, so items can be dropped into
//! another Findest window, Finder, Mail, or any other app. gpui's own drag
//! state can't leave the window it started in.

use std::cell::RefCell;
use std::path::PathBuf;

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
            NSDragOperation::Copy | NSDragOperation::Move | NSDragOperation::Generic
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
}

/// Starts a native drag of `paths` from `window`. Must be called while gpui
/// is handling the mouse-drag event (AppKit needs that event). Returns false
/// if the session couldn't be started.
pub fn start_file_drag(window: &gpui::Window, paths: &[PathBuf]) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let Ok(handle) = HasWindowHandle::window_handle(window) else { return false };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return false };
    // SAFETY: gpui hands out its live content view; we only use it during this call.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else { return false };
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
        return false;
    }

    let source = DragSource::new(mtm);
    view.beginDraggingSessionWithItems_event_source(
        &NSArray::from_retained_slice(&items),
        &event,
        ProtocolObject::from_ref(&*source),
    );
    SOURCE.with(|s| *s.borrow_mut() = Some(source));
    true
}

/// Whether ⌥ is held right now. gpui's modifier state can be stale after an
/// AppKit drag session, so drops ask the system directly.
pub fn option_key_down() -> bool {
    NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Option)
}
