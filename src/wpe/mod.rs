//! Embedded WPE WebKit for HTML mail (feature `wpe`, on by default).
//!
//! `subclass.rs`, `glib_source.rs`, `input.rs` and `wrapper.h` are verbatim
//! copies of cce-browser's `src/wpe/` — the proven embedding pattern (see
//! that crate's WPE-PORT.md). Keep them byte-identical to ease a future
//! extraction into a shared crate; anything mail-specific belongs in
//! `host.rs`, which replaces the browser's tabbed `WebKitHost` with the
//! single sandboxed [`host::MailWebView`].

pub mod ffi {
    #![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
    include!(concat!(env!("OUT_DIR"), "/wpe_bindings.rs"));
}

mod glib_source;
mod host;
mod input;
// dead_code: the copy stays verbatim; mail never pastes into a page, so the
// browser's clipboard-sync direction goes unused here.
#[allow(dead_code)]
mod subclass;

pub use host::MailWebView;
