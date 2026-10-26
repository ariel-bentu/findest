//! Quick Look (Space): the system preview panel Finder uses, so every file
//! type the system (or an installed Quick Look extension) can preview works.
//!
//! The panel looks for a controller in the responder chain, so each window
//! gets a small responder inserted after its `NSWindow`. Keys the panel
//! doesn't use itself (arrows, Space) are passed back to the window, so the
//! file list keeps working and the panel follows the selection.
//!
//! AppKit calls here can make another window key, which gpui handles
//! synchronously; callers inside a gpui update must go through
//! [`toggle`] / [`follow`], which defer the AppKit work.

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use objc2::rc::{Retained, Weak};
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::{ClassType, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
    use objc2_app_kit::{NSEvent, NSEventType, NSResponder, NSView, NSWindowDelegate};
    use objc2_foundation::{NSInteger, NSNotification, NSObjectProtocol, NSString, NSURL};
    use objc2_quick_look_ui::{QLPreviewItem, QLPreviewPanel, QLPreviewPanelDataSource, QLPreviewPanelDelegate};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

#[derive(Default)]
struct State {
    /// What the panel shows, and the same as URLs for the data source.
    paths: Vec<PathBuf>,
    urls: Vec<Retained<NSURL>>,
    /// The content view of the window the panel belongs to; unhandled keys go here.
    view: Option<Weak<NSView>>,
    /// Responders inserted into windows' chains (`nextResponder` doesn't retain).
    controllers: Vec<Retained<Controller>>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

define_class!(
    // SAFETY: NSResponder has no subclassing requirements and Controller has no Drop impl.
    #[unsafe(super(NSResponder))]
    #[thread_kind = MainThreadOnly]
    #[name = "FindestQuickLookController"]
    struct Controller;

    impl Controller {
        #[unsafe(method(acceptsPreviewPanelControl:))]
        fn accepts(&self, _panel: Option<&QLPreviewPanel>) -> bool {
            true
        }

        #[unsafe(method(beginPreviewPanelControl:))]
        fn begin(&self, panel: Option<&QLPreviewPanel>) {
            let Some(panel) = panel else { return };
            let delegate: &AnyObject = self.as_ref();
            // SAFETY: Controller implements both protocols and outlives the panel's use of it.
            unsafe {
                panel.setDataSource(Some(ProtocolObject::from_ref(self)));
                panel.setDelegate(Some(delegate));
            }
        }

        #[unsafe(method(endPreviewPanelControl:))]
        fn end(&self, panel: Option<&QLPreviewPanel>) {
            let Some(panel) = panel else { return };
            // SAFETY: clearing is always valid.
            unsafe {
                panel.setDataSource(None);
                panel.setDelegate(None);
            }
        }
    }

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl QLPreviewPanelDataSource for Controller {
        #[unsafe(method(numberOfPreviewItemsInPreviewPanel:))]
        fn count(&self, _panel: Option<&QLPreviewPanel>) -> NSInteger {
            STATE.with(|s| s.borrow().urls.len() as NSInteger)
        }

        #[unsafe(method_id(previewPanel:previewItemAtIndex:))]
        fn item(&self, _panel: Option<&QLPreviewPanel>, index: NSInteger) -> Option<Retained<ProtocolObject<dyn QLPreviewItem>>> {
            let url = usize::try_from(index).ok().and_then(|i| STATE.with(|s| s.borrow().urls.get(i).cloned()));
            url.map(ProtocolObject::from_retained)
        }
    }

    unsafe impl NSWindowDelegate for Controller {
        // A popup: clicking anywhere else (or switching apps) closes it.
        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _notification: &NSNotification) {
            if let Some(mtm) = MainThreadMarker::new() {
                close(mtm);
            }
        }
    }

    unsafe impl QLPreviewPanelDelegate for Controller {
        #[unsafe(method(previewPanel:handleEvent:))]
        fn handle_event(&self, _panel: Option<&QLPreviewPanel>, event: Option<&NSEvent>) -> bool {
            event.is_some_and(forward_key)
        }
    }
);

const KEY_ESCAPE: u16 = 53;

fn close(mtm: MainThreadMarker) {
    if let Some(panel) = panel_if_open(mtm) {
        panel.orderOut(None);
    }
}

/// Passes a key the panel didn't use to the window it belongs to.
fn forward_key(event: &NSEvent) -> bool {
    let kind = event.r#type();
    if kind != NSEventType::KeyDown && kind != NSEventType::KeyUp {
        return false;
    }
    if kind == NSEventType::KeyDown && event.keyCode() == KEY_ESCAPE {
        // Close here rather than in the window, where Esc clears the selection.
        if let Some(mtm) = MainThreadMarker::new() {
            close(mtm);
        }
        return true;
    }
    let Some(view) = STATE.with(|s| s.borrow().view.as_ref().and_then(Weak::load)) else { return false };
    if kind == NSEventType::KeyDown { view.keyDown(event) } else { view.keyUp(event) }
    true
}

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: NSResponder's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// The content view of a gpui window.
fn content_view(window: &gpui::Window) -> Option<Retained<NSView>> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };
    // SAFETY: gpui hands out its live content view; retaining it keeps it valid.
    let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
    Some(view.retain())
}

fn panel_if_open(mtm: MainThreadMarker) -> Option<Retained<QLPreviewPanel>> {
    // SAFETY: plain class methods; checking first avoids creating the panel.
    unsafe {
        if !QLPreviewPanel::sharedPreviewPanelExists(mtm) {
            return None;
        }
        QLPreviewPanel::sharedPreviewPanel(mtm).filter(|p| p.isVisible())
    }
}

/// Whether the panel is open for `window`. Cheap; called every frame.
pub fn is_open_for(window: &gpui::Window) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    if panel_if_open(mtm).is_none() {
        return false;
    }
    let Some(view) = content_view(window) else { return false };
    STATE.with(|s| s.borrow().view.as_ref().and_then(Weak::load).is_some_and(|v| v == view))
}

/// Opens the panel on `paths` for `window`, or closes it if it's open there.
pub fn toggle(window: &gpui::Window, paths: Vec<PathBuf>, cx: &gpui::App) {
    let close = is_open_for(window);
    let view = content_view(window);
    cx.foreground_executor()
        .spawn(async move {
            let Some(mtm) = MainThreadMarker::new() else { return };
            match view {
                _ if close => self::close(mtm),
                Some(view) if !paths.is_empty() => show(&view, paths, mtm),
                _ => {}
            }
        })
        .detach();
}

/// Keeps the open panel showing `paths` (the window's selection); closes it
/// when nothing is selected.
pub fn follow(paths: Vec<PathBuf>, cx: &gpui::App) {
    if STATE.with(|s| s.borrow().paths == paths) {
        return;
    }
    set_items(paths.clone());
    cx.foreground_executor()
        .spawn(async move {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let Some(panel) = panel_if_open(mtm) else { return };
            if paths.is_empty() {
                panel.orderOut(None);
            } else {
                // SAFETY: the data source is ours and reads the new items.
                unsafe { panel.reloadData() };
            }
        })
        .detach();
}

fn set_items(paths: Vec<PathBuf>) {
    let urls = paths.iter().filter_map(|p| p.to_str()).map(|p| NSURL::fileURLWithPath(&NSString::from_str(p))).collect();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.paths = paths;
        s.urls = urls;
    });
}

fn show(view: &NSView, paths: Vec<PathBuf>, mtm: MainThreadMarker) {
    let Some(window) = view.window() else { return };
    // Put a controller in this window's responder chain, once.
    // SAFETY: the controller is kept alive in STATE (`nextResponder` doesn't retain).
    unsafe {
        let next = window.nextResponder();
        if !next.as_deref().is_some_and(|r| r.isKindOfClass(Controller::class())) {
            let controller = Controller::new(mtm);
            controller.setNextResponder(next.as_deref());
            window.setNextResponder(Some(&controller));
            STATE.with(|s| s.borrow_mut().controllers.push(controller));
        }
    }
    set_items(paths);
    STATE.with(|s| s.borrow_mut().view = Some(Weak::from_retained(&view.retain())));
    // SAFETY: plain panel calls on the main thread; our controller is in the chain.
    unsafe {
        let Some(panel) = QLPreviewPanel::sharedPreviewPanel(mtm) else { return };
        if panel.isVisible() {
            // Open for another window: take it over.
            panel.updateController();
            panel.reloadData();
        } else {
            panel.makeKeyAndOrderFront(None);
        }
    }
}
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    thread_local! {
        static OPEN_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    pub fn is_open_for(_window: &gpui::Window) -> bool {
        OPEN_PATH.with(|p| p.borrow().is_some())
    }

    pub fn toggle(_window: &gpui::Window, paths: Vec<PathBuf>, cx: &gpui::App) {
        let current = OPEN_PATH.with(|p| p.borrow().clone());
        let next = paths.into_iter().next();
        if current.is_some() && (next.is_none() || current == next) {
            OPEN_PATH.with(|p| *p.borrow_mut() = None);
            cx.background_executor().spawn(async move { close_sushi() }).detach();
            return;
        }
        if let Some(path) = next {
            OPEN_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));
            cx.background_executor().spawn(async move { show_sushi(&path) }).detach();
        }
    }

    pub fn follow(paths: Vec<PathBuf>, cx: &gpui::App) {
        if !OPEN_PATH.with(|p| p.borrow().is_some()) {
            return;
        }
        let next = paths.into_iter().next();
        if OPEN_PATH.with(|p| *p.borrow() == next) {
            return;
        }
        OPEN_PATH.with(|p| *p.borrow_mut() = next.clone());
        cx.background_executor()
            .spawn(async move {
                match next {
                    Some(path) => show_sushi(&path),
                    None => close_sushi(),
                }
            })
            .detach();
    }

    fn show_sushi(path: & std::path::Path) {
        let uri = findest_core::paths::file_url(path);
        // GNOME Sushi exposes org.gnome.NautilusPreviewer on the session bus.
        let status = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.gnome.NautilusPreviewer",
                "--object-path",
                "/org/gnome/NautilusPreviewer",
                "--method",
                "org.gnome.NautilusPreviewer.ShowFile",
                &uri,
                "0",
                "true",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !status.is_ok_and(|s| s.success()) {
            let _ = Command::new("sushi")
                .arg(path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }

    fn close_sushi() {
        let _ = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.gnome.NautilusPreviewer",
                "--object-path",
                "/org/gnome/NautilusPreviewer",
                "--method",
                "org.gnome.NautilusPreviewer.Close",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(not(target_os = "macos"))]
pub use imp::*;

