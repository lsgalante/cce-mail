//! `MailWebView` — one sandboxed WPE WebKit view for rendering HTML mail.
//!
//! The mail-shaped sibling of cce-browser's `WebKitHost`: same boot (the
//! GObject subclasses in `subclass.rs`), same frame pipeline (SHM readback →
//! `cce_ui::vk::upload_rgba` → one quad in the detail pane), same calloop
//! bridge (`glib_source.rs`) — but a single view instead of tabs, and locked
//! down for hostile content:
//!
//! * **JavaScript is off.** Mail is not an application platform.
//! * **The network session is ephemeral** — no cookies or cache ever touch
//!   disk.
//! * **All remote loads are blocked by default** by a compiled WebKit content
//!   filter (`data:` stays allowed for inline images). Tracking pixels never
//!   fire. [`MailWebView::set_images_allowed`] lifts the filter for the
//!   current message only — an explicit per-message choice, reset on the
//!   next [`MailWebView::load_html`].
//! * **Navigation never happens in-pane.** A link click is intercepted by
//!   `decide-policy` and handed back through [`MailWebView::take_link_click`]
//!   for the app to open externally; form submissions are dropped.
//!
//! The web process is lazy: constructing the host boots only the WPE display
//! and toplevel (cheap, no child processes). WebKit's processes spawn on the
//! first [`MailWebView::load_html`], so a text-only session pays nothing.

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void, CString};
use std::rc::Rc;

use cce_ui::widget::{KeyEvent, MouseButton};

use super::ffi::*;
use super::glib_source::GlibPoll;
use super::input;
use super::subclass::{types, FRAME_SINK};

unsafe fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior nul")
}

unsafe fn from_cstr(p: *const c_char) -> Option<String> {
    (!p.is_null())
        .then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}

/// Frames handed over by `render_buffer`, drained by `pump`. A slot, not a
/// queue: only the newest frame is worth uploading, and WPE will not produce
/// another until the current one is released anyway.
#[derive(Default)]
struct Pending {
    frame: Option<(Vec<u8>, u32, u32)>,
}

/// The WebKit content filter source: block every URL except `data:`, so a
/// message renders from its own bytes alone. Compiled once (WebKit caches
/// the compiled form in the store directory) and attached to the UCM
/// whenever remote content is disallowed.
const BLOCK_REMOTE_FILTER: &str = r#"[
  {"trigger": {"url-filter": ".*"}, "action": {"type": "block"}},
  {"trigger": {"url-filter": "^data:"}, "action": {"type": "ignore-previous-rules"}}
]"#;

pub struct MailWebView {
    display: *mut WPEDisplay,
    toplevel: *mut WPEToplevel,
    /// Created on the first `load_html`, kept for the life of the host.
    webview: Option<(*mut WebKitWebView, *mut WPEView)>,
    session: *mut WebKitNetworkSession,
    ucm: *mut WebKitUserContentManager,
    /// The compiled block-everything filter; null if compilation failed (in
    /// which case remote loads are stopped by `auto-load-images` alone).
    filter: *mut WebKitUserContentFilter,
    size_px: (u32, u32),
    scale: f32,
    pending: Rc<RefCell<Pending>>,
    /// GLib's pollfd set, mirrored into one epoll fd for calloop.
    poll: Option<GlibPoll>,
    /// Link URIs the page tried to navigate to, stashed by `decide-policy`.
    links: Rc<RefCell<Vec<String>>>,
    /// The message currently loaded, kept so lifting the image block can
    /// re-render the same content.
    html: Option<CString>,
    images_allowed: bool,
    /// Last uploaded frame in the image registry: (id, w px, h px).
    image: Option<(u32, u32, u32)>,
    /// Retained so the headless example can assert on rendered output.
    last_frame: Option<(Vec<u8>, u32, u32)>,
}

impl Drop for MailWebView {
    fn drop(&mut self) {
        unsafe {
            if let Some((wv, _)) = self.webview.take() {
                g_object_unref(wv as *mut _);
            }
        }
        if let Some((id, ..)) = self.image.take() {
            cce_ui::vk::free_image(id);
        }
    }
}

impl MailWebView {
    /// Boot WPE (display + toplevel + content filter). One host per process:
    /// the frame sink and the GType registrations are process-wide.
    pub fn new(size_px: (u32, u32)) -> Self {
        unsafe {
            let t = types();
            let display = g_object_new(t.display, std::ptr::null::<c_char>()) as *mut WPEDisplay;
            let mut err: *mut GError = std::ptr::null_mut();
            assert!(
                wpe_display_connect(display, &mut err) != 0,
                "wpe_display_connect failed"
            );

            // Ephemeral: mail content must leave no cookie jar and no cache.
            let session = webkit_network_session_new_ephemeral();

            let pending = Rc::new(RefCell::new(Pending::default()));
            let sink = pending.clone();
            FRAME_SINK = Some(Box::new(move |buffer: *mut WPEBuffer| {
                if let Some(f) = read_shm(buffer) {
                    // Replace, never accumulate: the newest frame wins.
                    sink.borrow_mut().frame = Some(f);
                }
            }));

            let toplevel = wpe_display_create_toplevel(display, 1);
            wpe_toplevel_resized(toplevel, size_px.0 as i32, size_px.1 as i32);

            let filter = compile_block_filter();

            Self {
                display,
                toplevel,
                webview: None,
                session,
                ucm: webkit_user_content_manager_new(),
                filter,
                size_px,
                scale: 1.0,
                pending,
                poll: GlibPoll::new()
                    .map_err(|e| eprintln!("cce-mail: no GLib epoll bridge ({e}); pump will poll"))
                    .ok(),
                links: Rc::new(RefCell::new(Vec::new())),
                html: None,
                images_allowed: false,
                image: None,
                last_frame: None,
            }
        }
    }

    /// The webview, created on first use — this is what spawns WebKit's
    /// child processes, so it only happens once HTML actually arrives.
    fn ensure_view(&mut self) -> (*mut WebKitWebView, *mut WPEView) {
        if let Some(pair) = self.webview {
            return pair;
        }
        unsafe {
            let (p_display, p_ucm, p_session) = (
                cstr("display"),
                cstr("user-content-manager"),
                cstr("network-session"),
            );
            let wv = g_object_new(
                webkit_web_view_get_type(),
                p_display.as_ptr(),
                self.display,
                p_ucm.as_ptr(),
                self.ucm,
                p_session.as_ptr(),
                self.session,
                std::ptr::null::<c_char>(),
            ) as *mut WebKitWebView;

            // The lockdown. JavaScript stays off for the life of the view;
            // images follow `images_allowed`.
            let settings = webkit_web_view_get_settings(wv);
            webkit_settings_set_enable_javascript(settings, 0);
            webkit_settings_set_auto_load_images(settings, self.images_allowed as gboolean);
            self.apply_filter_policy();

            // Link clicks leave through the app, never navigate in-pane.
            let sig = cstr("decide-policy");
            g_signal_connect_data(
                wv as *mut _,
                sig.as_ptr(),
                Some(std::mem::transmute::<_, unsafe extern "C" fn()>(
                    on_decide_policy
                        as unsafe extern "C" fn(
                            *mut WebKitWebView,
                            *mut WebKitPolicyDecision,
                            WebKitPolicyDecisionType::Type,
                            gpointer,
                        ) -> gboolean,
                )),
                Rc::into_raw(self.links.clone()) as gpointer,
                Some(drop_links_ref),
                0,
            );

            let view = webkit_web_view_get_wpe_view(wv);
            wpe_view_set_toplevel(view, self.toplevel);
            let (lw, lh) = self.logical_size();
            wpe_view_resized(view, lw, lh);
            wpe_view_set_visible(view, 1);
            wpe_view_map(view);
            // Without focus the page has no focused frame and forwarded
            // keyboard input (PageDown, Ctrl+C) is silently dropped.
            wpe_view_focus_in(view);
            self.webview = Some((wv, view));
            (wv, view)
        }
    }

    /// Attach or detach the block-everything filter to match
    /// `images_allowed`. WebKit applies UCM changes to live pages.
    fn apply_filter_policy(&self) {
        if self.filter.is_null() {
            return;
        }
        unsafe {
            if self.images_allowed {
                webkit_user_content_manager_remove_all_filters(self.ucm);
            } else {
                webkit_user_content_manager_add_filter(self.ucm, self.filter);
            }
        }
    }

    /// Show a message. Always re-arms the remote-content block: allowing
    /// images is a per-message decision, never a sticky one.
    pub fn load_html(&mut self, html: &str) {
        self.images_allowed = false;
        // NUL bytes would truncate the CString; they carry no meaning in
        // HTML, so strip rather than fail.
        let owned;
        let clean = if html.contains('\0') {
            owned = html.replace('\0', "");
            owned.as_str()
        } else {
            html
        };
        self.html = Some(unsafe { cstr(clean) });
        self.reload_current();
    }

    /// Drop the shown message (selection cleared / folder switched). The
    /// view and its processes stay for the next message.
    pub fn clear(&mut self) {
        self.html = None;
        self.links.borrow_mut().clear();
        self.pending.borrow_mut().frame = None;
        if let Some((id, ..)) = self.image.take() {
            cce_ui::vk::free_image(id);
        }
        if let Some((wv, _)) = self.webview {
            unsafe {
                let blank = cstr("about:blank");
                webkit_web_view_load_uri(wv, blank.as_ptr());
            }
        }
    }

    /// Lift (or restore) the remote-content block for the current message
    /// and re-render it.
    pub fn set_images_allowed(&mut self, allowed: bool) {
        if allowed == self.images_allowed {
            return;
        }
        self.images_allowed = allowed;
        self.apply_filter_policy();
        self.reload_current();
    }

    pub fn images_allowed(&self) -> bool {
        self.images_allowed
    }

    fn reload_current(&mut self) {
        let Some(html) = self.html.clone() else { return };
        let (wv, _) = self.ensure_view();
        unsafe {
            let settings = webkit_web_view_get_settings(wv);
            webkit_settings_set_auto_load_images(settings, self.images_allowed as gboolean);
            webkit_web_view_load_html(wv, html.as_ptr(), std::ptr::null());
        }
        // The old message's frame must not linger under the new one — the
        // app falls back to the text body until the first frame lands.
        self.pending.borrow_mut().frame = None;
        if let Some((id, ..)) = self.image.take() {
            cce_ui::vk::free_image(id);
        }
    }

    /// A link the user clicked in the message, if any (FIFO).
    pub fn take_link_click(&self) -> Option<String> {
        let mut links = self.links.borrow_mut();
        (!links.is_empty()).then(|| links.remove(0))
    }

    /// The epoll fd carrying GLib's pollfd set, duplicated for calloop.
    /// `None` if the bridge could not be created — fall back to the timer.
    pub fn poll_fd_owned(&self) -> Option<std::os::fd::OwnedFd> {
        let fd = self.poll.as_ref()?.fd();
        rustix::io::dup(fd).ok()
    }

    /// How long calloop may sleep before pumping anyway, per GLib.
    pub fn poll_timeout(&self) -> Option<std::time::Duration> {
        self.poll
            .as_ref()
            .and_then(|p| p.timeout)
            .map(|ms| std::time::Duration::from_millis(ms as u64))
    }

    /// Drain GLib's pending work, then upload any frame it produced.
    /// Returns true when a new frame landed (the pane needs a repaint).
    pub fn pump(&mut self) -> bool {
        // Clear the inner epoll first: calloop is level-triggered on that fd,
        // so leaving it readable across a pump that does not consume the
        // underlying socket would spin the loop.
        if let Some(p) = &self.poll {
            p.drain();
        }
        unsafe {
            while g_main_context_iteration(std::ptr::null_mut(), 0) != 0 {}
        }
        // WebKit opens and drops sockets as it loads, so the set that matters
        // is the one *after* dispatch, not before.
        if let Some(p) = &mut self.poll {
            p.sync();
        }
        let Some((px, w, h)) = self.pending.borrow_mut().frame.take() else {
            return false;
        };
        self.last_frame = Some((px.clone(), w, h));
        let id = cce_ui::vk::upload_rgba(px, w, h);
        if let Some((old, ..)) = self.image.replace((id, w, h)) {
            cce_ui::vk::free_image(old);
        }
        true
    }

    /// The current frame in the image registry: (id, w px, h px).
    pub fn image(&self) -> Option<(u32, u32, u32)> {
        self.image
    }

    /// A pixel of the last frame, for tests asserting on rendered output
    /// (examples/wpe_mail.rs; dead in the app build).
    #[allow(dead_code)]
    pub fn sample_pixel(&self, x: u32, y: u32) -> Option<(u8, u8, u8)> {
        let (px, w, h) = self.last_frame.as_ref()?;
        if x >= *w || y >= *h {
            return None;
        }
        let i = ((y * w + x) * 4) as usize;
        Some((px[i], px[i + 1], px[i + 2]))
    }

    /// Put the page's current selection on the system clipboard (the
    /// clipboard subclass routes it through cce-ui's wl-copy helper).
    pub fn copy_selection(&self) {
        if let Some((wv, _)) = self.webview {
            unsafe {
                let c = cstr("Copy");
                webkit_web_view_execute_editing_command(wv, c.as_ptr());
            }
        }
    }

    // ---- input ----
    //
    // Coordinates are device pixels relative to the view origin (the
    // browser's convention); the host converts to WPE's logical space.

    pub fn mouse_move(&mut self, x_px: f32, y_px: f32) {
        let Some((_, view)) = self.webview else { return };
        unsafe {
            let (x, y) = self.to_logical(x_px, y_px);
            let e = wpe_event_pointer_move_new(
                WPEEventType::WPE_EVENT_POINTER_MOVE,
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                input::now_ms(),
                0,
                x,
                y,
                0.0,
                0.0,
            );
            self.send(view, e);
        }
    }

    pub fn mouse_button_ui(&mut self, button: MouseButton, pressed: bool, x_px: f32, y_px: f32) {
        let Some(n) = input::button_number(button) else {
            return;
        };
        let Some((_, view)) = self.webview else { return };
        unsafe {
            let time = input::now_ms();
            let (x, y) = self.to_logical(x_px, y_px);
            // WPE tracks double/triple clicks for us; a frozen clock here
            // would make every click read as a repeat.
            let press_count = if pressed {
                wpe_view_compute_press_count(view, x, y, n, time)
            } else {
                0
            };
            let e = wpe_event_pointer_button_new(
                if pressed {
                    WPEEventType::WPE_EVENT_POINTER_DOWN
                } else {
                    WPEEventType::WPE_EVENT_POINTER_UP
                },
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                time,
                0,
                n,
                x,
                y,
                press_count,
            );
            self.send(view, e);
        }
    }

    /// Wheel deltas in device pixels, winit-signed (positive = up), passed
    /// through unchanged — WPE inverts on the way to the DOM itself.
    pub fn wheel(&mut self, dx_px: f64, dy_px: f64, x_px: f32, y_px: f32) {
        let Some((_, view)) = self.webview else { return };
        unsafe {
            let (x, y) = self.to_logical(x_px, y_px);
            let e = wpe_event_scroll_new(
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                input::now_ms(),
                0,
                dx_px / self.scale as f64,
                dy_px / self.scale as f64,
                1, // precise deltas: these are pixels, not notches
                0, // not a scroll-stop event
                x,
                y,
            );
            self.send(view, e);
        }
    }

    /// Forward a cce-ui key event (page scrolling, copy chords).
    pub fn key_ui(&mut self, event: &KeyEvent) {
        let Some(keyval) = input::keyval(&event.logical_key) else {
            return;
        };
        let Some((_, view)) = self.webview else { return };
        let pressed = input::is_pressed(event);
        unsafe {
            let e = wpe_event_keyboard_new(
                if pressed {
                    WPEEventType::WPE_EVENT_KEYBOARD_KEY_DOWN
                } else {
                    WPEEventType::WPE_EVENT_KEYBOARD_KEY_UP
                },
                view,
                WPEInputSource::WPE_INPUT_SOURCE_KEYBOARD,
                input::now_ms(),
                input::modifiers(event.ctrl, event.shift, event.alt),
                0, // hardware keycode: unknown to us, WebKit works off keyval
                keyval,
            );
            self.send(view, e);
        }
    }

    unsafe fn send(&self, view: *mut WPEView, event: *mut WPEEvent) {
        if event.is_null() {
            return;
        }
        wpe_view_event(view, event);
        wpe_event_unref(event);
    }

    /// Resize, in **physical** pixels plus the scale. WPE wants a logical
    /// size and produces a buffer of `size * scale` — handing it physical
    /// pixels at scale 1 would lay out double-width CSS on a 2x display.
    pub fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) {
        let size = (width_px.max(1), height_px.max(1));
        let scale = scale.max(0.01);
        if size == self.size_px && (scale - self.scale).abs() < 1.0e-3 {
            return;
        }
        self.size_px = size;
        self.scale = scale;
        let (lw, lh) = self.logical_size();
        unsafe {
            wpe_toplevel_scale_changed(self.toplevel, self.scale as f64);
            wpe_toplevel_resized(self.toplevel, lw, lh);
            if let Some((_, view)) = self.webview {
                wpe_view_resized(view, lw, lh);
            }
        }
    }

    /// The view size WPE works in: physical divided back out by the scale.
    fn logical_size(&self) -> (i32, i32) {
        (
            ((self.size_px.0 as f32 / self.scale).round() as i32).max(1),
            ((self.size_px.1 as f32 / self.scale).round() as i32).max(1),
        )
    }

    fn to_logical(&self, x_px: f32, y_px: f32) -> (f64, f64) {
        ((x_px / self.scale) as f64, (y_px / self.scale) as f64)
    }
}

/// Compile (or load from WebKit's cache) the block-remote content filter.
///
/// The store API is async; the surrounding code is a constructor with a GLib
/// context and nothing else running on it yet, so this blocks on bounded
/// context iterations until the callback lands. Null on failure — the caller
/// degrades to `auto-load-images` alone.
unsafe fn compile_block_filter() -> *mut WebKitUserContentFilter {
    struct Slot {
        done: Cell<bool>,
        filter: Cell<*mut WebKitUserContentFilter>,
    }
    unsafe extern "C" fn on_saved(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
        let slot = &*(data as *const Slot);
        let mut err: *mut GError = std::ptr::null_mut();
        let f = webkit_user_content_filter_store_save_finish(
            source as *mut WebKitUserContentFilterStore,
            res,
            &mut err,
        );
        if f.is_null() {
            let msg = (!err.is_null())
                .then(|| from_cstr((*err).message))
                .flatten()
                .unwrap_or_else(|| "unknown error".into());
            eprintln!("cce-mail: content filter failed to compile ({msg})");
            if !err.is_null() {
                g_error_free(err);
            }
        }
        slot.filter.set(f);
        slot.done.set(true);
    }

    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")))
        .map(|p| p.join("cce/mail/content-filters"));
    let Some(dir) = dir else {
        eprintln!("cce-mail: no HOME; remote-content filter disabled");
        return std::ptr::null_mut();
    };
    let _ = std::fs::create_dir_all(&dir);

    let cdir = cstr(&dir.to_string_lossy());
    let store = webkit_user_content_filter_store_new(cdir.as_ptr());
    let id = cstr("block-remote");
    let bytes = g_bytes_new(
        BLOCK_REMOTE_FILTER.as_ptr() as *const c_void,
        BLOCK_REMOTE_FILTER.len() as u64,
    );
    let slot = Box::new(Slot {
        done: Cell::new(false),
        filter: Cell::new(std::ptr::null_mut()),
    });
    webkit_user_content_filter_store_save(
        store,
        id.as_ptr(),
        bytes,
        std::ptr::null_mut(),
        Some(on_saved),
        slot.as_ref() as *const Slot as gpointer,
    );
    // Blocking iterations; the cap turns a wedged store into a filterless
    // start instead of a hang.
    for _ in 0..10_000 {
        if slot.done.get() {
            break;
        }
        g_main_context_iteration(std::ptr::null_mut(), 1);
    }
    g_bytes_unref(bytes);
    g_object_unref(store as *mut _);
    if !slot.done.get() {
        eprintln!("cce-mail: content filter compile timed out; remote loads gated by image setting only");
        // The callback may still fire later against the leaked slot.
        Box::leak(slot);
        return std::ptr::null_mut();
    }
    slot.filter.get()
}

unsafe extern "C" fn drop_links_ref(data: gpointer, _c: *mut GClosure) {
    drop(Rc::from_raw(data as *const RefCell<Vec<String>>));
}

/// Every navigation decision. The initial `load_html` arrives as type OTHER
/// and passes; a clicked link is stashed for external opening; everything
/// else (forms, window.open targets) is refused outright.
unsafe extern "C" fn on_decide_policy(
    _wv: *mut WebKitWebView,
    decision: *mut WebKitPolicyDecision,
    kind: WebKitPolicyDecisionType::Type,
    data: gpointer,
) -> gboolean {
    let links = &*(data as *const RefCell<Vec<String>>);
    match kind {
        WebKitPolicyDecisionType::WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION
        | WebKitPolicyDecisionType::WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION => {
            let nav = decision as *mut WebKitNavigationPolicyDecision;
            let action = webkit_navigation_policy_decision_get_navigation_action(nav);
            let ty = webkit_navigation_action_get_navigation_type(action);
            let is_click = ty == WebKitNavigationType::WEBKIT_NAVIGATION_TYPE_LINK_CLICKED;
            let in_new_window =
                kind == WebKitPolicyDecisionType::WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION;
            if is_click || in_new_window {
                let req = webkit_navigation_action_get_request(action);
                if let Some(uri) = from_cstr(webkit_uri_request_get_uri(req)) {
                    links.borrow_mut().push(uri);
                }
                webkit_policy_decision_ignore(decision);
            } else if ty == WebKitNavigationType::WEBKIT_NAVIGATION_TYPE_OTHER {
                // The app's own load_html / about:blank clears.
                webkit_policy_decision_use(decision);
            } else {
                // Form submits, reloads, back/forward: nothing a mail pane
                // should ever do.
                webkit_policy_decision_ignore(decision);
            }
        }
        _ => {
            webkit_policy_decision_use(decision);
        }
    }
    1
}

/// Copy an SHM buffer's pixels out as RGBA for `upload_rgba`.
///
/// `WPE_PIXEL_FORMAT_ARGB8888` is B,G,R,A in memory on little-endian, and the
/// stride is not assumed to equal `width * 4`.
unsafe fn read_shm(buffer: *mut WPEBuffer) -> Option<(Vec<u8>, u32, u32)> {
    if g_type_check_instance_is_a(buffer as *mut GTypeInstance, wpe_buffer_shm_get_type()) == 0 {
        return None;
    }
    let shm = buffer as *mut WPEBufferSHM;
    let (w, h) = (
        wpe_buffer_get_width(buffer) as u32,
        wpe_buffer_get_height(buffer) as u32,
    );
    let mut len: u64 = 0;
    let src = g_bytes_get_data(wpe_buffer_shm_get_data(shm), &mut len as *mut u64) as *const u8;
    if src.is_null() || w == 0 || h == 0 {
        return None;
    }
    let stride = wpe_buffer_shm_get_stride(shm) as usize;
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = src.add(y * stride + x * 4);
            let d = (y * w as usize + x) * 4;
            out[d] = *s.add(2);
            out[d + 1] = *s.add(1);
            out[d + 2] = *s;
            out[d + 3] = *s.add(3);
        }
    }
    Some((out, w, h))
}
