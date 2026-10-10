//! Embedded WPE WebKit for HTML mail (feature `wpe`, on by default).
//!
//! The embedding — bindings, WPEPlatform subclasses, the GLib source, input
//! translation — is the cce-wpe crate, shared with cce-browser (until
//! 2026-10-10 this module kept a copy of four of its files, which had
//! drifted behind the browser's). Re-exported under the module names
//! `host.rs` uses; everything mail-specific is `host.rs`, which replaces the
//! browser's tabbed `WebKitHost` with the single sandboxed
//! [`host::MailWebView`].

pub use cce_wpe::ffi;
use cce_wpe::{glib_source, input, subclass};
mod host;

pub use host::MailWebView;
