/// Embedded WPE WebKit for the HTML mail view — see src/wpe/mod.rs.
#[cfg(feature = "wpe")]
mod ipc;
mod wpe;
mod accounts_file;
mod imap_client;
mod layout;
mod mime;
mod smtp;
mod store;
use imap_client::*;
use layout::*;
use mime::*;
use smtp::*;
use store::*;
use cce_ui::fmt::percent_decode;
use cce_ui::process::spawn_detached;
use cce_ui::widget::Handle;
use cce_ui::widget::ScrollRegion;
use cce_ui::cosmic_text::FontSystem;
use cce_ui::engine::{Application, CursorIcon, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::widget::{
    MouseButton, ElementState, MouseScrollDelta, KeyEvent, WidgetHost,
    TextBox, Button, TextLabel, Key, Dropdown, Bounds, ScrollMotion, LINE_PX
};
use cce_ui::context::UiContext;
use native_tls::TlsConnector;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AccountInfo {
    email: String,
    imap: String,
    smtp: String,
    is_default: bool,
    password: String,
    #[serde(default)]
    is_oauth: bool,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_expiry: Option<u64>,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    /// True when the password came from (or was migrated into) the Secret
    /// Service keyring — save_accounts blanks it on disk. Never serialized.
    #[serde(skip)]
    keyring_backed: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Email {
    id: usize,
    from: String,
    to: String,
    subject: String,
    body: String,
    date: String,
    read: bool,
    folder: String, // "inbox", "sent", "drafts", "trash"
    /// Cc line of locally-composed mail (sent copies and drafts); fetched
    /// mail leaves it empty. Defaults keep pre-existing caches loading.
    #[serde(default)]
    cc: String,
    /// Bcc is only ever populated on drafts — a sent copy records who was
    /// bcc'd nowhere, which is the point of bcc.
    #[serde(default)]
    bcc: String,
    /// Drafts: absolute paths still to be attached. Sent copies: just the
    /// file names, for display.
    #[serde(default)]
    attachments: Vec<String>,
    /// Attachments discovered on the server (fetched mail only): metadata
    /// from the BODYSTRUCTURE pass, enough to fetch the part on demand.
    /// Locally-created mail leaves this empty.
    #[serde(default)]
    remote_attachments: Vec<RemoteAttachment>,
    /// Server-side IMAP UID (INBOX message); None for mock or locally-created
    /// mail. Server operations (delete) key on this, never on `id` — `id` is
    /// the fetch-time sequence number, which shifts after any expunge.
    #[serde(default)]
    uid: Option<u32>,
    /// Sortable arrival time (epoch seconds) from the message's own Date
    /// header. `date` is a display string and cannot be ordered; uid order is
    /// only a proxy for arrival WITHIN one mailbox, and for a Gmail label it
    /// reflects when mail was labelled, which is why a folder could come out
    /// oldest-first. None on rows cached before this existed — they fall back
    /// to uid order.
    #[serde(default)]
    ts: Option<i64>,
    /// For mail moved to the local Trash, the server folder it came from.
    /// Without it the next sync sees the uid missing from that folder, calls
    /// it new, and downloads the message straight back — deleted mail
    /// reappearing while a copy also sits in Trash.
    #[serde(default)]
    origin_folder: Option<String>,
}

#[derive(Debug, Clone)]
enum AppMessage {
    SwitchFolder(String),
    /// The folder list the server reported during a sync.
    FoldersDiscovered(String, Vec<FolderInfo>),
    SelectEmail(usize),
    SearchChanged,
    ComposeNew,
    ComposeCancel,
    ComposeSend,
    /// Attach button in the compose dialog: pick a file, append a chip.
    ComposeAttach,
    /// Outcome of a send: `None` = delivered (record in Sent); `Some(err)` =
    /// failed (save the content to Drafts and say why).
    SendResult(OutgoingMail, Option<String>),
    /// A detail-pane attachment chip was clicked: fetch part `1` of the
    /// email with id `0` from the server.
    OpenAttachment(usize, usize),
    /// Fetch outcome: Ok(saved path) — open it; Err(why) — status line.
    AttachmentFetched(Result<String, String>),
    Reply,
    DeleteSelected,
    ToggleUnread,
    SelectAccount(usize),
    /// Open cce-system-interface on its Accounts page — account add/remove/
    /// default/OAuth all live there now; this app only reads accounts.json.
    ManageAccounts,
    SyncNow,
    /// One line from the control socket (`cce-mail ctl …`), answered from
    /// the app's live state and written back on the request's own stream.
    Ipc(ipc::Request),
    Quit,
    Status(String),
    /// A failure a user must not miss: shown as a sticky red toast (no
    /// timer) where [`AppMessage::Status`] is a green 4-second one.
    StatusError(String),
    /// One mailbox's pass, folded in the moment it lands. Sent per folder
    /// rather than once per pass: the inbox answers in about a second while
    /// `[Gmail]/All Mail`'s `UID SEARCH ALL` walks the whole account, and
    /// holding the inbox's result until that finished is what made a sync
    /// look like it had done nothing.
    EmailsSynced(String, Vec<FolderSync>),
    /// The sync worker has left — every exit, including the ones that never
    /// reached a mailbox (no session, no password, mock account). Clears
    /// `sync_in_flight`, so a failed pass can't wedge the app against ever
    /// syncing again. Carries the pass's generation: one presumed lost and
    /// replaced (see [`SYNC_STUCK_AFTER`]) must not hand back a wire that now
    /// belongs to its replacement.
    SyncFinished(u64),
    UpdateAccountTokens(String, Option<String>, Option<u64>),
    /// GLib has work (or produced a frame): pump the embedded webview.
    #[cfg(feature = "wpe")]
    Spin,
    /// The on-demand HTML part fetch for email `0` finished: `Some` carries
    /// the decoded HTML plus its inline images, `None` means no HTML part
    /// (or the fetch failed) — the pane stays on the text body.
    #[cfg(feature = "wpe")]
    HtmlFetched(usize, Option<HtmlMail>),
}

/// One message's renderable HTML: the decoded body plus the cid: inline
/// attachments it references — (content-id, mime type, decoded bytes),
/// served to the page by the webview's `cid:` scheme handler.
#[cfg(feature = "wpe")]
#[derive(Debug, Clone)]
struct HtmlMail {
    html: String,
    inline: Vec<(String, String, Vec<u8>)>,
}

/// The single status slot at the bottom of the window. Info toasts expire on
/// a deadline checked in `tick`; errors carry no timer — they stay until
/// clicked away or replaced, so a failed sync can't vanish unseen.
///
/// The deadline is a wall clock rather than a `ttl` counted down by `tick`'s
/// `dt`, because `dt` is animation time: the runner clamps it to one frame
/// after an idle sleep, and a window whose only pending work is a toast's own
/// expiry never leaves that sleep. Counted in `dt`, a 4 s toast sat on screen
/// for about four minutes.
#[derive(Debug, Clone)]
enum StatusToast {
    Info { text: String, expires_at: std::time::Instant },
    Error { text: String },
}

impl StatusToast {
    /// `ttl` is in seconds, resolved to a deadline here so every call site
    /// keeps reading as a duration.
    fn info(text: impl Into<String>, ttl: f32) -> Self {
        StatusToast::Info {
            text: text.into(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs_f32(ttl),
        }
    }
    fn error(text: impl Into<String>) -> Self {
        StatusToast::Error { text: text.into() }
    }
    fn text(&self) -> &str {
        match self {
            StatusToast::Info { text, .. } | StatusToast::Error { text } => text,
        }
    }
}

/// App shortcuts, resolved once at startup from input.kdl
/// (`cce-mail` domain → `cce-ui` domain), defaulting to the historical keys.
struct EmailKeys {
    compose: String,
    open_search: String,
    /// Pane focus, under the toolkit's own names (`focus_next_group` /
    /// `focus_prev_group`, the chords the runner would claim for plate
    /// navigation if this app opted in): rebinding them DE-wide in the
    /// `cce-ui` domain rebinds them here too, and a `cce-mail` entry still
    /// overrides that.
    focus_next_pane: String,
    focus_prev_pane: String,
}

impl EmailKeys {
    fn load() -> Self {
        Self {
            compose: cce_ui::input::app_chord("compose", "ctrl+n"),
            open_search: cce_ui::input::app_chord("open_search", "/"),
            focus_next_pane: cce_ui::input::app_chord("focus_next_group", "ctrl+tab"),
            focus_prev_pane: cce_ui::input::app_chord("focus_prev_group", "ctrl+shift+tab"),
        }
    }
}

/// The two pane plates keyboard focus walks between (cce-designer's
/// `focused_pane`, at this app's scale): the message list on the left and
/// the preview on the right. Ctrl+Tab cycles, `h` / `l` jump, and `j` / `k`
/// act inside whichever holds it — the selection in the list, the body
/// scroll in the preview. Up/Down and Delete stay global: they moved the
/// selection from anywhere before there was a focused pane, and still do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    List,
    Detail,
}

impl Pane {
    fn other(self) -> Self {
        match self {
            Pane::List => Pane::Detail,
            Pane::Detail => Pane::List,
        }
    }
}

struct ClearEmailApp {
    keys: EmailKeys,
    /// When the active account's cache first changed since its last save;
    /// `None` when it is saved. See `save_emails`.
    emails_dirty_since: Option<std::time::Instant>,
    /// Bumped wherever `emails` is changed; the key of `filter_cache`.
    emails_rev: u64,
    /// `filtered_emails`' last answer, as indices into `emails`, with the
    /// (revision, folder, search) it was computed for. The list asks for it
    /// several times a frame, and each ask lowercased every message's sender,
    /// subject and BODY to match the search — the whole 13.5 MB cache per
    /// ask, per frame, while a query was typed.
    filter_cache: std::cell::RefCell<Option<(u64, String, String, Vec<usize>)>>,

    // Navigation / Sidebar (MenuBar retired): a mail-icon button on the
    // left whose click pops the app menu (New Message / Sync Now / Quit)
    // through the toolkit context menu, and two selection dropdowns on
    // the right.
    /// The app menu trigger: the cce-icons `mail` glyph, plateless. Its
    /// rows fire through `context_menu_actions`, the same path as a card's
    /// right-click menu, so the bar owns no second menu implementation.
    mail_button: Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>,
    /// Folder switcher: options[0] carries the live inbox unread count
    /// ("Inbox (6)"), refreshed each rebuild, so rows and trigger agree.
    folder_dropdown: Handle<cce_ui::widget::Adapted<Dropdown>>,
    /// Account switcher beside the folder dropdown: options are the account
    /// emails plus a trailing "Manage Accounts…" pseudo-entry (management
    /// lives in cce-system-interface). Options refresh from accounts.json on
    /// every open — the job the retired Accounts page did on entry.
    account_dropdown: Handle<cce_ui::widget::Adapted<Dropdown>>,

    // Search and List View
    search_box: Handle<cce_ui::widget::Adapted<TextBox>>,
    email_list: ScrollRegion,
    email_buttons: Vec<Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>>,

    // Details View (Reply/Delete/Mark Read/Unread live in the Message menu)
    detail_body: Handle<cce_ui::widget::Adapted<TextBox>>,

    // Compose Dialog
    compose_to: Handle<cce_ui::widget::Adapted<TextBox>>,
    compose_cc: Handle<cce_ui::widget::Adapted<TextBox>>,
    compose_bcc: Handle<cce_ui::widget::Adapted<TextBox>>,
    compose_subject: Handle<cce_ui::widget::Adapted<TextBox>>,
    compose_body: Handle<cce_ui::widget::Adapted<TextBox>>,
    /// Absolute paths queued for the next send; drawn as removable chips.
    compose_attachments: Vec<String>,
    btn_compose_send: Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>,
    btn_compose_cancel: Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>,
    btn_compose_attach: Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>,

    // Accounts (switch via the bar dropdown — management lives in cce-system-interface)
    accounts: Vec<AccountInfo>,
    selected_account_idx: usize,

    // Application state
    emails: Vec<Email>,
    /// Tag of the folder on show. A String rather than an enum because the
    /// set is whatever the account actually has.
    current_folder: String,
    /// Every folder the switcher offers: the built-in five, then whatever the
    /// server reported. Persisted per account so the switcher is populated
    /// before the first sync answers.
    folders: Vec<FolderInfo>,
    /// Folders synced at least once this run. A folder opened for the first
    /// time syncs immediately instead of waiting out the throttle, which is
    /// what stops it looking empty.
    synced_tags: std::collections::HashSet<String>,
    selected_email_id: Option<usize>,
    /// Detail-pane body scroll offset (logical px) and the measured height of
    /// the wrapped body text, refreshed each display_list; hover scopes the
    /// wheel/keyboard scrolling to the pane. The sb_* pair is the scrollbar
    /// thumb drag (grab offset within the thumb), mirroring ScrollRegion.
    body_scroll: f32,
    /// Drives `body_scroll` (the DRAWN offset): wheel notches glide it, a
    /// trackpad flick coasts it. Direct writes — selection reset, thumb drag,
    /// the paint-time clamp — are adopted by the motion on its next step.
    body_motion: ScrollMotion,
    body_content_h: f32,
    detail_hovered: bool,
    body_sb_dragging: bool,
    body_sb_drag_offset: f32,
    /// Raise/sink hysteresis for the detail-pane bar (shared toolkit type).
    /// The window bg behind the pane is opaque, so "sunk" here is fully
    /// hidden — an overlay bar that appears on scroll and idles away.
    body_sb_activity: cce_ui::widget::ScrollbarActivity,

    // HTML mail view (feature `wpe`). The host boots at startup (cheap: no
    // WebKit processes until the first message renders); the fetched HTML is
    // keyed by email id and pulled on demand, since the synced `body` is
    // only a 1200-char text preview.
    #[cfg(feature = "wpe")]
    webview: wpe::MailWebView,
    /// Whether a renderer has been handed over yet — the first one is the
    /// process's own, any later one is a replacement after a reconnect. See
    /// `renderer_init`.
    seen_renderer: bool,
    /// Email id whose HTML the webview currently shows.
    #[cfg(feature = "wpe")]
    html_loaded: Option<usize>,
    /// Email id with an HTML fetch in flight.
    #[cfg(feature = "wpe")]
    html_pending: Option<usize>,
    #[cfg(feature = "wpe")]
    html_cache: std::collections::HashMap<usize, HtmlMail>,
    /// The user asked for the text body of the current message.
    #[cfg(feature = "wpe")]
    show_text: bool,
    /// HTML-view toggles in the detail header band (text/HTML, remote
    /// images); laid out each frame by `layout_html_buttons`.
    #[cfg(feature = "wpe")]
    btn_html_view: Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>,
    #[cfg(feature = "wpe")]
    btn_html_images: Handle<cce_ui::widget::Adapted<cce_ui::widget::Button>>,
    /// Where the page was last drawn (logical px), for routing input to it.
    #[cfg(feature = "wpe")]
    webview_rect: (f32, f32, f32, f32),
    /// A press went to the page; the matching release must follow it there.
    #[cfg(feature = "wpe")]
    webview_mouse_down: bool,
    /// Width of the email-list band (the rows), user-draggable via the
    /// list/detail separator. The stored preference survives narrow windows
    /// un-clobbered — [`Self::split_geom`] clamps at use, not here.
    list_w: f32,
    split_dragging: bool,
    /// Which pane plate keyboard focus is on — see [`Pane`]. Set by the
    /// chords and by a left press inside either plate; drawn as the plate's
    /// highlight ring.
    focused_pane: Pane,
    /// Message per row of the open card context menu, built beside its
    /// labels — index 0 is the inert subject header, hence Option.
    context_menu_actions: Vec<Option<AppMessage>>,
    /// Whether the search band is revealed. Closed is the resting state:
    /// the box is laid out, painted and routed only while this is set.
    search_open: bool,
    compose_open: bool,
    compose_title: String,
    /// When the most recent IMAP sync was spawned — folder switches re-sync
    /// through [`Self::start_sync`], throttled against tab-hopping.
    last_sync_start: Option<std::time::Instant>,
    /// Set when a sync was skipped because the account had no password: the
    /// vault was not readable yet. `tick` retries at this instant, so an
    /// unlock heals the account on its own rather than waiting for the user
    /// to go looking for a Sync Now.
    secret_retry_at: Option<std::time::Instant>,
    /// Whether the missing password has already been reported. Separate from
    /// `secret_retry_at`, which `tick` clears before each attempt — reusing
    /// that as the "already said this" flag made every retry look like the
    /// first and logged the same line every few seconds.
    secret_missing_reported: bool,
    /// Set while a mailbox still has history to download: `tick` runs the next
    /// backfill pass at this instant. Cleared once the server holds nothing
    /// the cache is missing.
    backfill_at: Option<std::time::Instant>,
    /// The pass currently out on the wire, and what it covers. Nothing starts
    /// a second one while this is set — two passes race, and the loser is
    /// whichever finishes LAST: each carries its own `UID SEARCH` snapshot,
    /// and a pass that searched before a message arrived deletes it again
    /// through [`merge_sync`]'s deleted-elsewhere sweep. That is what made
    /// mail need a second Sync Now: the first press fetched it and a passing
    /// straggler threw it away.
    sync_in_flight: Option<SyncScope>,
    /// Generation of the last pass started, bumped per `start_sync`.
    sync_generation: u64,
    /// A sync that had to be turned away because one was already out, and
    /// which the running pass does not cover — a different account, or a
    /// folder outside its `want_tags`. Run once the wire is free, so
    /// switching account mid-sync still ends up synced instead of silently
    /// skipped.
    resync_pending: bool,
    /// What each folder of the pass now running still had to come, as of its
    /// latest report. Read once the worker leaves: a later folder reporting
    /// nothing outstanding must not cancel an earlier one that did, while a
    /// folder's own later report (its history batches) supersedes its first.
    backfill_remaining: std::collections::HashMap<String, usize>,
    status_message: Option<StatusToast>,
    sender: calloop::channel::Sender<AppMessage>,

    // UI state
    width: u32,
    height: u32,
    scale_factor: f64,
    font_system: FontSystem,
    needs_rebuild: bool,
    ui_context: UiContext,
}

fn get_default_mock_emails() -> Vec<Email> {
    vec![
        Email {
            id: 1,
            from: "System Daemon <daemon@cce-ui.org>".to_string(),
            to: "lsgalante@cce-ui.org".to_string(),
            subject: "Welcome to Clear Mail".to_string(),
            body: "Welcome to the Clear Computing Environment (CCE) email client!\n\nThis application is built entirely using the cce-ui Rust framework, running on top of Wayland and wgpu. It provides a premium, responsive dark slate interface.\n\nEnjoy the clean lines and micro-animations!\n\nBest,\nSystem Daemon".to_string(),
            date: "12:15 PM".to_string(),
            read: false,
            folder: "inbox".to_string(),
            uid: None,
            ts: None,
            origin_folder: None,
            cc: String::new(),
            bcc: String::new(),
            attachments: Vec::new(),
            remote_attachments: Vec::new(),
        },
        Email {
            id: 2,
            from: "Design Team <design@cce-ui.org>".to_string(),
            to: "lsgalante@cce-ui.org".to_string(),
            subject: "Slate Dark Theme Palette Guidelines".to_string(),
            body: "Hi Lucas,\n\nWe have finalized the core Slate Dark styling color values for CCE application builders:\n- Window Background: [0.05, 0.05, 0.07, 1.0] (deep slate blue)\n- Sidebar Background: [0.08, 0.08, 0.12, 1.0]\n- Selection Highlight: [0.20, 0.45, 0.85, 0.50] (high contrast premium blue)\n- Text Bright: [0.88, 0.88, 0.92, 1.0]\n- Text Muted: [0.51, 0.51, 0.54, 1.0]\n\nPlease align all interface panels and layouts to use these values.\n\nThanks,\nDesign Team".to_string(),
            date: "Yesterday".to_string(),
            read: false,
            folder: "inbox".to_string(),
            uid: None,
            ts: None,
            origin_folder: None,
            cc: String::new(),
            bcc: String::new(),
            attachments: Vec::new(),
            remote_attachments: Vec::new(),
        },
        Email {
            id: 3,
            from: "Codeberg CI <ci@codeberg.org>".to_string(),
            to: "lsgalante@cce-ui.org".to_string(),
            subject: "Build Success: cce-mail (main)".to_string(),
            body: "Repository: lsgalante/cce-mail\nBranch: main\nCommit: da8cf20fcb2c993c1c048ced4020\nStatus: SUCCESS\n\nAll unit tests passed. Binary compiled in 48.2s.\n\n---\nCodeberg Actions".to_string(),
            date: "June 3".to_string(),
            read: true,
            folder: "inbox".to_string(),
            uid: None,
            ts: None,
            origin_folder: None,
            cc: String::new(),
            bcc: String::new(),
            attachments: Vec::new(),
            remote_attachments: Vec::new(),
        },
        Email {
            id: 4,
            from: "lsgalante@cce-ui.org".to_string(),
            to: "Design Team <design@cce-ui.org>".to_string(),
            subject: "Re: Slate Dark Theme Palette Guidelines".to_string(),
            body: "Thanks for sending the palette! I am implementing the email client right now using these specifications. The three-pane layout feels very premium.\n\n- Lucas".to_string(),
            date: "Yesterday".to_string(),
            read: true,
            folder: "sent".to_string(),
            uid: None,
            ts: None,
            origin_folder: None,
            cc: String::new(),
            bcc: String::new(),
            attachments: Vec::new(),
            remote_attachments: Vec::new(),
        },
    ]
}

impl ClearEmailApp {
    /// Effective To/Cc/Bcc/Subject/Body/attachments as composed right now.
    /// Each box reads whichever side its `editing` flag selects — the same
    /// split ComposeSend always honored.
    fn gather_compose(&self) -> OutgoingMail {
        fn val(tb: &TextBox) -> String {
            if tb.editing { tb.edit_buffer.trim().to_string() } else { tb.text.trim().to_string() }
        }
        OutgoingMail {
            to: val(&self.ui_context[self.compose_to]),
            cc: val(&self.ui_context[self.compose_cc]),
            bcc: val(&self.ui_context[self.compose_bcc]),
            subject: val(&self.ui_context[self.compose_subject]),
            body: if self.ui_context[self.compose_body].editing {
                self.ui_context[self.compose_body].edit_buffer.clone()
            } else {
                self.ui_context[self.compose_body].text.clone()
            },
            attachments: self.compose_attachments.clone(),
        }
    }

    /// Reset every compose field (text and edit_buffer both — the reader
    /// picks a side by the editing flag, so they must always agree).
    fn clear_compose(&mut self) {
        for tb in [
            self.compose_to,
            self.compose_cc,
            self.compose_bcc,
            self.compose_subject,
            self.compose_body,
        ] {
            let tb = &mut self.ui_context[tb];
            tb.text = String::new();
            tb.edit_buffer = String::new();
        }
        self.compose_attachments.clear();
    }

    /// File content in the local Drafts folder (never synced — drafts have
    /// no uid and a folder the retention filters don't touch).
    fn file_as_draft(&mut self, mail: &OutgoingMail) {
        let new_id = self.emails.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        self.emails_rev = self.emails_rev.wrapping_add(1);
        self.emails.push(Email {
            id: new_id,
            from: self
                .accounts
                .get(self.selected_account_idx)
                .map(|a| a.email.clone())
                .unwrap_or_default(),
            to: mail.to.clone(),
            subject: mail.subject.clone(),
            body: mail.body.clone(),
            date: "Draft".to_string(),
            read: true,
            folder: "drafts".to_string(),
            uid: None,
            ts: None,
            origin_folder: None,
            cc: mail.cc.clone(),
            bcc: mail.bcc.clone(),
            attachments: mail.attachments.clone(),
            remote_attachments: Vec::new(),
        });
        self.save_emails();
    }


    /// Spawn an IMAP sync for the selected account. Unforced calls (folder
    /// switches) are throttled so tab-hopping doesn't stack connections;
    /// forced calls (explicit account selection) always run.
    /// Re-read a keyring-backed account's password when it has gone missing.
    ///
    /// Secrets otherwise resolve exactly once, at startup — and the compositor
    /// restores every saved window at once, so cce-mail can read the vault in
    /// the same second KeePassXC is still claiming org.freedesktop.secrets.
    /// The account is then left holding an empty password and every sync fails
    /// with the server's `Empty username or password` until the app is
    /// restarted (diagnosed 2026-08-22 from the mirrored stderr). Re-reading
    /// before a sync heals that race, and the mid-session re-lock with it.
    ///
    /// Deliberately only when the password is MISSING: the resolver's other
    /// branch writes a non-empty password back INTO the keyring, which has no
    /// business running on every sync.
    fn refresh_account_secret(&mut self, idx: usize) {
        let Some(acc) = self.accounts.get_mut(idx) else {
            return;
        };
        if !acc.password.is_empty() || is_mock_account(acc) {
            return;
        }
        let email = acc.email.clone();
        let migrated = resolve_account_secrets(std::slice::from_mut(acc));
        if !self.accounts[idx].password.is_empty() {
            eprintln!("cce-mail: recovered {} credentials from the keyring", email);
        }
        if migrated {
            persist_keyring_migration(&self.accounts[idx..=idx]);
        }
    }

    fn start_sync(&mut self, force: bool) {
        const MIN_SYNC_GAP: std::time::Duration = std::time::Duration::from_secs(30);
        if !force && self.last_sync_start.is_some_and(|t| t.elapsed() < MIN_SYNC_GAP) {
            return;
        }
        // One pass at a time, forced or not. Two overlapping passes do not
        // merely duplicate the work: each took its own `UID SEARCH` snapshot,
        // and the one that finishes LAST decides what the cache holds, so a
        // straggler that searched before a message arrived deletes it out
        // from under the pass that just fetched it (see `merge_sync`). A
        // press while a pass is out says so rather than silently stacking a
        // second session — the whole "press Sync twice" symptom was this.
        //
        // Turning a request away is not the same as dropping it: a pass
        // already on the wire is fetching the wrong account's mail as far as
        // an account switch is concerned, and knows nothing about a folder
        // opened since it left. Those are queued for the moment it lands —
        // only a request the running pass already covers is answered with
        // "it is happening".
        if let Some(scope) = self.sync_in_flight.as_ref().filter(|s| {
            let stuck = s.started.elapsed() >= SYNC_STUCK_AFTER;
            if stuck {
                eprintln!(
                    "cce-mail: the sync started {}s ago has not reported back; starting another",
                    s.started.elapsed().as_secs()
                );
            }
            !stuck
        }) {
            let covered = self
                .accounts
                .get(self.selected_account_idx)
                .is_some_and(|a| a.email == scope.account)
                && scope.tags.contains(&self.current_folder);
            // Past its folders the pass is only downloading history, and a
            // Sync Now wants what has arrived since it searched: stop it at
            // the next batch and go again. History resumes from that pass.
            let backfilling = scope.progress.backfilling.load(std::sync::atomic::Ordering::Relaxed);
            if backfilling && (force || !covered) {
                scope.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                self.resync_pending = true;
            } else if covered {
                if force {
                    self.status_message = Some(StatusToast::info("Already syncing...", 2.0));
                }
            } else {
                self.resync_pending = true;
            }
            return;
        }
        self.refresh_account_secret(self.selected_account_idx);

        let Some((email, is_oauth, no_password, mock)) = self
            .accounts
            .get(self.selected_account_idx)
            .map(|a| (a.email.clone(), a.is_oauth, a.password.is_empty(), is_mock_account(a)))
        else {
            return;
        };

        // Still nothing to log in with: say what is actually wrong rather than
        // relaying the server's reply, which reads as a bug in the app. OAuth
        // accounts legitimately carry no password — the token comes from the
        // refresh — so they are exempt.
        if !is_oauth && no_password && !mock {
            // First miss only. The retry below runs until the vault opens, so
            // saying this every few seconds would spam the session log for as
            // long as it stays shut, and keep clobbering whatever toast the
            // user is actually reading.
            if !self.secret_missing_reported {
                self.secret_missing_reported = true;
                let msg = format!("No password for {} — is the keyring unlocked?", email);
                eprintln!("cce-mail: {} (retrying every {}s)", msg, SECRET_RETRY_SECS);
                self.status_message = Some(StatusToast::error(msg));
            }
            self.secret_retry_at =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(SECRET_RETRY_SECS));
            // last_sync_start is left alone: unlocking the vault should not
            // then have to wait out the throttle.
            return;
        }

        self.secret_retry_at = None;
        self.secret_missing_reported = false;

        // What the cache already holds, per server-backed folder. The worker
        // fetches only what is missing from this, which is what turns a sync
        // from "refetch the newest 50" into "download what I do not have".
        let known: std::collections::HashMap<String, std::collections::HashSet<u32>>;
        known = known_uids(&self.emails, &self.folders);

        // The three the app always keeps current, plus whatever folder is on
        // show — a discovered folder is only fetched once you actually open
        // it, which is what keeps 17 mailboxes from all downloading at once.
        let mut want_tags: Vec<String> = vec![
            TAG_INBOX.to_string(),
            TAG_ARCHIVE.to_string(),
            TAG_SENT.to_string(),
        ];
        if !want_tags.contains(&self.current_folder) {
            want_tags.push(self.current_folder.clone());
        }

        let account = self.accounts[self.selected_account_idx].clone();
        // Cleared by `AppMessage::SyncFinished`, which the worker sends from
        // a drop guard — so every early return and every panic still hands
        // the wire back.
        self.sync_generation = self.sync_generation.wrapping_add(1);
        let progress = SyncProgress::default();
        self.sync_in_flight = Some(SyncScope {
            generation: self.sync_generation,
            account: account.email.clone(),
            tags: want_tags.clone(),
            started: std::time::Instant::now(),
            progress: progress.clone(),
        });
        self.backfill_remaining.clear();
        sync_imap(account, known, want_tags, self.sync_generation, progress, self.sender.clone());
        self.last_sync_start = Some(std::time::Instant::now());
    }

    /// The active account's cache changed: save it shortly. A run of
    /// changes (a sync's folders, a burst of flag toggles) is one save,
    /// [`EMAILS_SAVE_DELAY`] after the first; `tick` makes it.
    fn save_emails(&mut self) {
        if self.emails_dirty_since.is_none() {
            self.emails_dirty_since = Some(std::time::Instant::now());
        }
    }

    /// Hand the active account's cache to the writer now, if it changed.
    fn flush_emails(&mut self) {
        if self.emails_dirty_since.take().is_none() {
            return;
        }
        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
            save_emails_for_account(&acc.email, self.emails.clone());
        }
    }

    /// The list/detail split, clamped to the current window: (list band
    /// width, separator x, detail pane x). The single source for paint,
    /// layout, and every hit-test — the stored `list_w` preference is never
    /// mutated by a window resize, only re-clamped here.
    fn split_geom(&self) -> (f32, f32, f32) {
        let (edge, gap) = (window_pad(), pane_gap());
        // What is left for the detail plate once both outer gutters and the
        // split gap are taken: it may not fall under DETAIL_W_MIN.
        let max_w = (self.width as f32 - 2.0 * edge - gap - DETAIL_W_MIN).max(LIST_W_MIN);
        let lw = self.list_w.clamp(LIST_W_MIN, max_w);
        // The grab band rides the gap's centre line; the detail plate starts a
        // whole gap past the list, and its content one plate padding inside.
        let sep = edge + lw + gap / 2.0;
        (lw, sep, edge + lw + gap + pane_inner_pad())
    }

    /// The email list's band: (top y, height). The search row above it only
    /// exists while the band is open, and the list takes that strip back when
    /// it closes — so nothing hardcodes the top.
    fn list_geom(&self) -> (f32, f32) {
        // One padding below the bar's items (an interior gap); the open search
        // band and its own gap push the plate further down. The bottom is a
        // WINDOW edge, so it clears the plate's roll as well.
        let band = if self.search_open { cce_ui::layout::textbox_height() + pane_gap() } else { 0.0 };
        let top = menubar_h() + pane_pad() + band;
        (top, (self.height as f32 - top - window_pad()).max(50.0))
    }

    /// The detail column: (x, w), the list's vertical band's right-hand
    /// side. It mirrors the list's inset from the window's left edge on
    /// the right — plates either side of the separator, which the equal
    /// gaps leave centred between them.
    fn detail_column(&self) -> (f32, f32) {
        let (lw, _, _) = self.split_geom();
        let x = window_pad() + lw + pane_gap();
        let w = ((self.width as f32 - window_pad()) - x).max(DETAIL_W_MIN);
        (x, w)
    }

    /// Height of the header plate's content: down to the date line, or to
    /// the chip row when the message on show has server attachments. The
    /// buttons never add to it — they sit within the lines' band.
    fn detail_header_content_h(&self) -> f32 {
        let chips = self
            .selected_email_id
            .and_then(|id| self.emails.iter().find(|e| e.id == id))
            .is_some_and(|e| !e.remote_attachments.is_empty());
        if chips {
            DETAIL_CHIPS_Y + cce_ui::layout::button_height()
        } else {
            DETAIL_LINES_BOTTOM
        }
    }

    /// The header plate: (x, y, w, h), at the top of the detail column. It
    /// holds the subject, the address lines, the attachment chips and the
    /// HTML-view buttons — everything about the message that is not the
    /// message — and is exactly as tall as that content plus its padding,
    /// so it shrinks when there are no chips.
    fn detail_header_geom(&self) -> (f32, f32, f32, f32) {
        let (x, w) = self.detail_column();
        let (top, _) = self.list_geom();
        (x, top, w, self.detail_header_content_h() + 2.0 * pane_inner_pad())
    }

    /// The left edge of the HTML-view buttons on show, if any — the header
    /// lines sharing their band are ellipsized short of it.
    #[cfg(feature = "wpe")]
    fn html_buttons_left(&self) -> Option<f32> {
        [&self.ui_context[self.btn_html_view], &self.ui_context[self.btn_html_images]]
            .into_iter()
            .map(|b| b.rect().0)
            .filter(|x| *x > -9000.0)
            .min_by(|a, b| a.total_cmp(b))
    }
    #[cfg(not(feature = "wpe"))]
    fn html_buttons_left(&self) -> Option<f32> {
        None
    }

    /// The preview plate: (x, y, w, h), the rest of the detail column below
    /// the header plate and one pane gap — the seam between the two is the
    /// same as the seam between the list and them.
    fn detail_pane_geom(&self) -> (f32, f32, f32, f32) {
        let (x, w) = self.detail_column();
        let (top, h) = self.list_geom();
        let (_, hy, _, hh) = self.detail_header_geom();
        let y = hy + hh + pane_gap();
        (x, y, w, (top + h - y).max(50.0))
    }

    /// The y the header's own offsets are measured from: the top of its
    /// content, which is the header plate's top edge taken in by the plate
    /// padding.
    fn detail_origin_y(&self) -> f32 {
        self.detail_header_geom().1 + pane_inner_pad()
    }

    /// The body scrollbar's lane: (x, width), on the plate's CENTRE line, as
    /// the list's bar rides its plate's.
    fn detail_sb_lane(&self) -> (f32, f32) {
        let (px, _, pw, _) = self.detail_pane_geom();
        // The DE's centred-bar width, the list's and the params pane's.
        let sb_w = cce_ui::layout::centred_scrollbar_width();
        (px + (pw - sb_w) / 2.0, sb_w)
    }

    /// Width of the detail pane's text column: the whole plate inside its
    /// padding. No lane is reserved for the bar any more — it rides over the
    /// text the way the list's rides over its rows, and is only in front
    /// while scrolling.
    fn detail_content_w(&self) -> f32 {
        let (px, _, pw, _) = self.detail_pane_geom();
        let (_, _, detail_x) = self.split_geom();
        ((px + pw - pane_inner_pad()) - detail_x).max(100.0)
    }

    /// The detail pane's bar as pills at `alpha`. Emitted TWICE a frame: once
    /// under the plate at full strength (the copy seen through the frost),
    /// once over the body as the fore copy fades in and out.
    fn push_body_scrollbar(&self, pc: &mut cce_ui::scene::paint::PaintCtx, alpha: f32) {
        let a = alpha.clamp(0.0, 1.0);
        if a <= 0.001 {
            return;
        }
        let Some((sb_x, track_y, sb_w, track_h, thumb_y, thumb_h)) = self.body_scrollbar_geom()
        else {
            return;
        };
        let dim = |mut c: [f32; 4]| {
            c[3] *= a;
            c
        };
        use cce_ui::scene::layout::Rect;
        let all = (true, true, true, true);
        // Pills, as `ScrollRegion::push_scrollbar_prims` draws them: radius is
        // half the shorter extent.
        pc.rounded_rect(
            Rect { x: sb_x, y: track_y, width: sb_w, height: track_h },
            sb_w.min(track_h) * 0.5,
            all,
            dim(cce_ui::color::scrollbar_track_color()),
        );
        pc.rounded_rect(
            Rect { x: sb_x, y: thumb_y, width: sb_w, height: thumb_h },
            sb_w.min(thumb_h) * 0.5,
            all,
            dim(cce_ui::color::scrollbar_thumb_color()),
        );
    }

    /// Move the selection `delta` rows through the list as it is displayed,
    /// scrolling to keep the new row on screen. Returns the message to
    /// dispatch, if any.
    ///
    /// A draft is selected in place rather than through SelectEmail, which
    /// resumes it in the compose dialog — arrowing down a Drafts folder must
    /// not fling a dialog open on every keypress.
    fn move_selection(&mut self, delta: isize) -> Option<AppMessage> {
        let rows: Vec<(usize, bool)> = self
            .filtered_emails()
            .iter()
            .map(|e| (e.id, e.folder == "drafts"))
            .collect();
        if rows.is_empty() {
            return None;
        }

        let current = self
            .selected_email_id
            .and_then(|id| rows.iter().position(|(rid, _)| *rid == id));
        let next = match current {
            Some(i) => (i as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
            // Nothing selected yet: Down lands on the newest, Up on the oldest.
            None if delta > 0 => 0,
            None => rows.len() - 1,
        };
        let (id, is_draft) = rows[next];
        self.scroll_row_into_view(next);

        if is_draft {
            self.selected_email_id = Some(id);
            self.body_scroll = 0.0;
            None
        } else {
            Some(AppMessage::SelectEmail(id))
        }
    }

    /// One line of preview scroll, `j` / `k` with the preview focused. The
    /// text body glides a wheel notch the way the list's own arrows do; the
    /// HTML page gets the equivalent arrow key, since the engine owns that
    /// scroll (press and release both, so no key is left held in WebKit).
    /// `body_h` is the text body's visible height, read by the caller
    /// before any borrow of the context.
    fn step_body(&mut self, down: bool, body_h: f32) {
        #[cfg(feature = "wpe")]
        if self.html_on_show() {
            use cce_ui::widget::NamedKey;
            let key = Key::Named(if down { NamedKey::ArrowDown } else { NamedKey::ArrowUp });
            for state in [ElementState::Pressed, ElementState::Released] {
                self.webview.key_ui(&KeyEvent {
                    state,
                    logical_key: key.clone(),
                    text: None,
                    repeat: false,
                    ctrl: false,
                    shift: false,
                    alt: false,
                });
            }
            return;
        }
        if self.selected_email_id.is_none() {
            return;
        }
        let max = (self.body_content_h - body_h).max(0.0);
        let b = Bounds::max(max);
        let s = cce_ui::widget::scroll_motion::scroll_settings();
        self.body_motion.reconcile(0.0, self.body_scroll);
        self.body_motion.y.wheel(if down { LINE_PX } else { -LINE_PX }, b, &s);
        self.body_scroll = self.body_motion.y.pos();
    }

    /// Scroll the list the least amount that puts row `idx` fully on screen.
    fn scroll_row_into_view(&mut self, idx: usize) {
        let (_, height) = self.list_geom();
        // Row pitch is the region's own row height plus its gap, so this
        // cannot drift from where the rows are actually painted.
        let pitch = self.email_list.item_height + self.email_list.item_gap;
        let row_top = idx as f32 * pitch;
        let row_bottom = row_top + self.email_list.item_height;
        let view = self.email_list.scroll_y;
        if row_top < view {
            self.email_list.set_scroll_y(row_top);
        } else if row_bottom > view + height {
            self.email_list.set_scroll_y(row_bottom - height);
        }
    }

    /// The IMAP mailbox a folder tag names, from the discovered list. None for
    /// the folders this app keeps locally — and, importantly, for a tag the
    /// account no longer has, so a stale row cannot address some other
    /// mailbox's uid space.
    fn mailbox_for(&self, tag: &str) -> Option<String> {
        self.folders
            .iter()
            .find(|f| f.tag == tag)
            .filter(|f| f.is_server_backed())
            .map(|f| f.mailbox.clone())
    }

    /// The rows the list is currently showing, in paint order — the folder
    /// filter plus the search box. A row index means nothing without this:
    /// it is what maps the card under the pointer to its message.
    fn filtered_emails(&self) -> Vec<&Email> {
        let search_text = if self.ui_context[self.search_box].editing {
            &self.ui_context[self.search_box].edit_buffer
        } else {
            &self.ui_context[self.search_box].text
        };
        filtered_from(&self.emails, self.emails_rev, &self.current_folder, search_text, &self.filter_cache)
    }

    /// The app menu, hung off the bottom of the bar under the mail-icon
    /// button. Same context-menu plumbing as a card's right-click menu:
    /// labels to the toolkit, messages into the parallel
    /// `context_menu_actions`, fired by `context_menu_press`.
    fn open_mail_menu(&mut self) {
        let (bx, _, _, _) = self.ui_context[self.mail_button].rect();
        let options = vec!["New Message".to_string(), "Sync Now".to_string(), "Quit".to_string()];
        self.context_menu_actions = vec![
            Some(AppMessage::ComposeNew),
            Some(AppMessage::SyncNow),
            Some(AppMessage::Quit),
        ];
        cce_ui::widget::context_menu::show(bx, menubar_h(), options, 0, self.mail_button.id());
    }

    /// Right-press on a message card: open its context menu at the pointer.
    /// Returns false when the press missed every card.
    fn open_card_context_menu(&mut self, px: f32, py: f32) -> bool {
        let hit = self.email_buttons.iter().position(|&b| {
            let (bx, by, bw, bh) = self.ui_context[b].rect();
            bx > -9000.0 && px >= bx && px <= bx + bw && py >= by && py <= by + bh
        });
        let Some(idx) = hit else { return false };
        let Some((id, read, subject, folder)) = self
            .filtered_emails()
            .get(idx)
            .map(|e| (e.id, e.read, e.subject.clone(), e.folder.clone()))
        else {
            return false;
        };

        // Select the card the menu acts on, but NOT through SelectEmail:
        // that marks the message read and pushes the flag to the server, and
        // sends a draft into the compose dialog — all wrong when the next
        // click might be "Mark Unread".
        self.selected_email_id = Some(id);
        self.body_scroll = 0.0;

        // Row 0 is the subject header (header_count = 1) — dimmed and inert,
        // hence the leading None. A draft is not a message that arrived:
        // there is nobody to reply to and no read state, so it gets Edit —
        // which is SelectEmail, the same path a left-click takes to resume it
        // in the compose dialog.
        // A delete that cannot be undone should say so where it is invoked.
        let delete_label = if folder == "trash" || folder == "archive" {
            "Delete Forever"
        } else {
            "Delete"
        };
        let (labels, actions): (Vec<&str>, Vec<Option<AppMessage>>) = if folder == "drafts" {
            (
                vec!["Edit", delete_label],
                vec![Some(AppMessage::SelectEmail(id)), Some(AppMessage::DeleteSelected)],
            )
        } else {
            (
                vec!["Reply", delete_label, if read { "Mark Unread" } else { "Mark Read" }],
                vec![
                    Some(AppMessage::Reply),
                    Some(AppMessage::DeleteSelected),
                    Some(AppMessage::ToggleUnread),
                ],
            )
        };
        let mut options = vec![ellipsize(&subject, 28)];
        options.extend(labels.into_iter().map(str::to_string));
        self.context_menu_actions = std::iter::once(None).chain(actions).collect();

        let target = self.email_buttons[idx].id();
        cce_ui::widget::context_menu::show(px, py, options.clone(), 1, target);

        // Re-show clamped rather than duplicating the toolkit's sizing rule:
        // `show` is what computes w/h, and this app publishes no overflow
        // margin, so a menu opened near the right or bottom rim has to be
        // pulled back inside the window.
        let mw = cce_ui::widget::context_menu::w();
        let mh = cce_ui::widget::context_menu::h();
        let cx = (px).min(self.width as f32 - mw - 2.0).max(0.0);
        let cy = (py).min(self.height as f32 - mh - 2.0).max(0.0);
        if (cx - px).abs() > 0.5 || (cy - py).abs() > 0.5 {
            cce_ui::widget::context_menu::show(cx, cy, options, 1, target);
        }
        true
    }

    /// A press while a card menu is open: an item fires its action, anything
    /// else only dismisses. The row's message comes from the parallel
    /// `context_menu_actions` built with the labels, so the two menu shapes
    /// (mail vs draft) can't drift out of step with a positional map. The
    /// toolkit's own `context_menu::mouse_input` is deliberately NOT used —
    /// it dispatches labels through a fixed map into the target widget,
    /// where "Delete" means DeleteKey, not this app's message.
    ///
    /// A menu with NO app actions behind it is the toolkit's own — the
    /// Cut / Copy / Paste menu a text box opens — and that one dispatches
    /// through `context_menu::mouse_input`, or its rows paint and do
    /// nothing (which is what they did until 2026-09-22).
    fn context_menu_press(&mut self, px: f32, py: f32) -> Option<AppMessage> {
        if self.context_menu_actions.is_empty() {
            cce_ui::widget::context_menu::mouse_input(
                MouseButton::Left,
                ElementState::Pressed,
                px,
                py,
                Some(&mut self.ui_context),
            );
            cce_ui::widget::context_menu::hide();
            return None;
        }
        let msg = cce_ui::widget::context_menu::row_at(px, py)
            .and_then(|row| self.context_menu_actions.get(row).cloned().flatten());
        cce_ui::widget::context_menu::hide();
        self.context_menu_actions.clear();
        msg
    }

    /// Whether the detail pane is showing (or about to show) the rendered
    /// HTML view of the selected message.
    #[cfg(feature = "wpe")]
    fn html_active(&self) -> bool {
        !self.show_text
            && self.selected_email_id.is_some()
            && self.html_loaded == self.selected_email_id
            && !self.compose_open
    }

    /// HTML view with an actual frame on screen — the gate for routing
    /// input to the page (before the first frame the text body is still
    /// what the user sees and scrolls).
    #[cfg(feature = "wpe")]
    fn html_on_show(&self) -> bool {
        self.html_active() && self.webview.image().is_some()
    }

    #[cfg(feature = "wpe")]
    fn in_webview(&self, px: f32, py: f32) -> bool {
        let (x, y, w, h) = self.webview_rect;
        w > 0.0 && px >= x && px <= x + w && py >= y && py <= y + h
    }

    /// Window-logical position → device pixels relative to the page origin
    /// (the webview's input convention).
    #[cfg(feature = "wpe")]
    fn webview_px(&self, px: f32, py: f32) -> (f32, f32) {
        let (x, y, ..) = self.webview_rect;
        let s = self.scale_factor as f32;
        ((px - x) * s, (py - y) * s)
    }

    /// Place the HTML-view buttons in the header plate, right-aligned with
    /// their bottoms on the date line, so they never collide with the
    /// attachment chips growing from the left below. A button that does not
    /// apply is parked off-screen (the email_buttons convention), which
    /// keeps it out of hit-testing too. Runs before every paint, so events
    /// hit the rects on screen.
    #[cfg(feature = "wpe")]
    fn layout_html_buttons(&mut self) {
        let shown = !self.compose_open
            && self.selected_email_id.is_some()
            && self.html_loaded == self.selected_email_id;
        let btn_h = cce_ui::layout::button_height();
        let y = self.detail_origin_y() + DETAIL_LINES_BOTTOM - btn_h;
        let (px, _, pw, _) = self.detail_pane_geom();
        let mut right = px + pw - pane_inner_pad();
        let view_label = if self.show_text { "View: Text" } else { "View: HTML" };
        let images_label = if self.webview.images_allowed() { "Images: On" } else { "Load Images" };
        let images_shown = shown && !self.show_text;
        // Built right-to-left: the view toggle holds the corner.
        for (btn, label, visible) in [
            (self.btn_html_view, view_label, shown),
            (self.btn_html_images, images_label, images_shown),
        ] {
            let btn = &mut self.ui_context[btn];
            if !visible {
                btn.set_rect(-9999.0, -9999.0, 0.0, 0.0);
                continue;
            }
            btn.set_label(label);
            // The compose buttons' size, so every button in the app matches
            // (the floor also stops the toggles jittering between labels).
            let w = btn.intrinsic_size().map_or(0.0, |s| s.width).max(COMPOSE_BTN_W);
            right -= w;
            btn.set_rect(right, y, w, btn_h);
            right -= cce_ui::layout::plate_gap();
        }
    }

    /// Show `id`'s HTML: from the cache immediately, else a background
    /// fetch. Called on every non-draft selection.
    #[cfg(feature = "wpe")]
    fn request_html(&mut self, id: usize) {
        self.show_text = false;
        if self.html_loaded == Some(id) {
            return;
        }
        if self.html_loaded.take().is_some() {
            self.webview.clear();
        }
        if let Some(mail) = self.html_cache.get(&id) {
            let mail = mail.clone();
            self.show_html_mail(&mail);
            self.html_loaded = Some(id);
            return;
        }
        if self.html_pending == Some(id) {
            return;
        }
        // Debug affordance (CCE_* env-var convention): the mock account has
        // no server to fetch HTML from, so this renders any message's plain
        // body through the HTML view — how a shadow session verifies the
        // in-app pipeline without credentials (examples/wpe_mail.rs covers
        // the engine alone). Trusted local data; escaping is not the point.
        if std::env::var_os("CCE_MAIL_HTML_DEMO").is_some() {
            if let Some(email) = self.emails.iter().find(|e| e.id == id) {
                let html = format!(
                    "<!doctype html><html><body style=\"font-family:sans-serif;margin:16px\">\
                     <img src=\"cid:demo-logo@cce-mail\" width=\"48\" height=\"48\" style=\"float:right\">\
                     <h2 style=\"color:#204060\">{}</h2><p>{}</p>\
                     <p><a href=\"https://example.com/\">an external link</a></p></body></html>",
                    email.subject,
                    email.body.replace('\n', "<br>")
                );
                let logo = br##"<svg xmlns="http://www.w3.org/2000/svg" width="48" height="48"><circle cx="24" cy="24" r="22" fill="#00a040"/></svg>"##;
                let mail = HtmlMail {
                    html,
                    inline: vec![(
                        "demo-logo@cce-mail".to_string(),
                        "image/svg+xml".to_string(),
                        logo.to_vec(),
                    )],
                };
                self.show_html_mail(&mail);
                self.html_loaded = Some(id);
                return;
            }
        }
        let Some((folder, Some(uid))) = self
            .emails
            .iter()
            .find(|e| e.id == id)
            .map(|e| (e.folder.clone(), e.uid))
        else {
            eprintln!("cce-mail: html view: message {id} has no server uid; staying on text");
            return;
        };
        // Same mailbox rule as OpenAttachment: a uid only means anything
        // inside the mailbox the sync pulled it from.
        let Some(mailbox) = self.mailbox_for(&folder) else {
            eprintln!("cce-mail: html view: folder {folder:?} has no server mailbox; staying on text");
            return;
        };
        let Some(acc) = self.accounts.get(self.selected_account_idx) else { return };
        self.html_pending = Some(id);
        fetch_html_part(acc.clone(), mailbox, uid, id, self.sender.clone());
    }
    #[cfg(not(feature = "wpe"))]
    fn request_html(&mut self, _id: usize) {}

    /// Hand a message to the webview: inline images first (the `cid:`
    /// handler must be able to answer the page's very first request), then
    /// the HTML.
    #[cfg(feature = "wpe")]
    fn show_html_mail(&mut self, mail: &HtmlMail) {
        self.webview.set_inline_parts(mail.inline.clone());
        self.webview.load_html(&mail.html);
    }

    /// Drop the HTML view state (selection cleared, folder or account
    /// switched, message deleted). The webview and its processes stay.
    #[cfg(feature = "wpe")]
    fn reset_html(&mut self) {
        if self.html_loaded.take().is_some() {
            self.webview.clear();
        }
        self.html_pending = None;
        self.show_text = false;
    }
    #[cfg(not(feature = "wpe"))]
    fn reset_html(&mut self) {}

    /// The preview body box: (top y, height) — the preview plate taken in by
    /// its padding top and bottom. Paint, layout, the scrollbar and the
    /// scroll clamps all derive from this one pair.
    fn detail_body_geom(&self) -> (f32, f32) {
        let (_, py, _, ph) = self.detail_pane_geom();
        let top = py + pane_inner_pad();
        let h = ((py + ph - pane_inner_pad()) - top).max(100.0);
        (top, h)
    }

    /// Detail-pane body scrollbar geometry, mirroring `ScrollRegion::scrollbar_geom`:
    /// (sb_x, track_y, sb_w, track_h, thumb_y, thumb_h). None when the body fits
    /// (no scrollbar drawn). The single source for display_list and the drag path.
    fn body_scrollbar_geom(&self) -> Option<(f32, f32, f32, f32, f32, f32)> {
        let (body_y, body_h) = self.detail_body_geom();
        let max_scroll = (self.body_content_h - body_h).max(0.0);
        if max_scroll <= 0.0 {
            return None;
        }
        // The bar rides the pane plate's centre line — it belongs to the
        // pane, not to the window.
        let (sb_x, sb_w) = self.detail_sb_lane();
        // The track stops 4px short at each end, which is what every toolkit
        // bar does (`ScrollRegion::scrollbar_geom`, `paint_relief_scrollbar`,
        // `parameters_bg`); this one alone used to run the full pane height.
        let track_y = body_y + 4.0;
        let track_h = (body_h - 8.0).max(0.0);
        let visible_ratio = body_h / self.body_content_h.max(1.0);
        let thumb_h = if track_h <= 20.0 {
            track_h
        } else {
            (track_h * visible_ratio).clamp(20.0, track_h)
        };
        let thumb_y = track_y + (self.body_scroll / max_scroll) * (track_h - thumb_h);
        Some((sb_x, track_y, sb_w, track_h, thumb_y, thumb_h))
    }

    /// Left press on the scrollbar strip (±4px slop like ScrollRegion): grab the
    /// thumb where it was clicked, or jump the track and drag from the thumb center.
    fn body_sb_press(&mut self, px: f32, py: f32) -> bool {
        // A sunk bar is not drawn and takes no input; the press falls through.
        if !self.body_sb_activity.raised() {
            return false;
        }
        let Some((sb_x, track_y, sb_w, track_h, thumb_y, thumb_h)) = self.body_scrollbar_geom() else {
            return false;
        };
        if px < sb_x - 4.0 || px > sb_x + sb_w + 4.0 || py < track_y || py > track_y + track_h {
            return false;
        }
        self.body_sb_dragging = true;
        let click_offset = py - thumb_y;
        if click_offset >= 0.0 && click_offset <= thumb_h {
            self.body_sb_drag_offset = click_offset;
        } else {
            self.body_sb_drag_offset = thumb_h / 2.0;
            self.body_sb_drag_to(py);
        }
        true
    }

    fn body_sb_drag_to(&mut self, py: f32) -> bool {
        let Some((_, track_y, _, track_h, _, thumb_h)) = self.body_scrollbar_geom() else {
            return false;
        };
        let (_, body_h) = self.detail_body_geom();
        let max_scroll = (self.body_content_h - body_h).max(0.0);
        let target = py - self.body_sb_drag_offset;
        let ratio = if track_h - thumb_h > 0.0 {
            ((target - track_y) / (track_h - thumb_h)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let old = self.body_scroll;
        self.body_scroll = ratio * max_scroll;
        (self.body_scroll - old).abs() > 0.01
    }

    /// Advance the detail body's wheel glide / flick coast; true while the
    /// offset is moving, so the frame loop keeps drawing until it settles.
    fn tick_body_scroll(&mut self, dt: f32) -> bool {
        self.body_motion.reconcile(0.0, self.body_scroll);
        if !self.body_motion.is_animating() {
            return false;
        }
        let (_, body_h) = self.detail_body_geom();
        let max = (self.body_content_h - body_h).max(0.0);
        let moved = self.body_motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
        self.body_scroll = self.body_motion.y.pos();
        moved || self.body_motion.is_animating()
    }

    fn emit_text_prims(&mut self, pc: &mut cce_ui::scene::paint::PaintCtx) {
        let mut labels = Vec::new();

        let w_f32 = self.width as f32;
        let h_f32 = self.height as f32;

        let list_x = window_pad();
        let (list_w, _separator_x, detail_x) = self.split_geom();

        // Widget text rides along with chrome in display_list's paint_root_into
        // walk — only app-composed labels are emitted here.

        // 3. Email List Labels.
        // Skipped while a modal is up: text always renders above geometry, and these
        // hand-emitted labels carry no bounds, so they'd bleed straight through the
        // modal panel (the popover-occlusion clamp only knows registered popovers).
        let modal_open = self.compose_open;
        if modal_open {
        } else {
            let filtered: Vec<&Email> = self.filtered_emails();

            // Row labels carry the list-viewport bounds, unlike the other
            // hand-emitted labels: `get_item_draw_y` returns PARTIALLY visible
            // cards (the toolkit ScrollRegion's intersection contract), so an
            // edge card's text must be cut at the viewport instead of bleeding
            // into the menubar above or the frame below. Emitted straight into
            // `pc` — the shared `labels` drain is boundless.
            let list_bounds = Some([
                list_x,
                self.email_list.viewport_y,
                list_x + list_w,
                self.email_list.viewport_y + self.email_list.viewport_h,
            ]);
            for (idx, email) in filtered.iter().enumerate() {
                if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                    // Sender — recipient on sent rows (every sent mail is
                    // "from" ourselves; the interesting party is the other end)
                    let row_head = if email.folder == "sent" {
                        format!("To: {}", email.to)
                    } else {
                        email.from.clone()
                    };
                    // The date is right-aligned on the same row, so the
                    // sender — drawn in full, like the subject below and the
                    // header's lines — is clipped one gap short of it rather
                    // than at the card's edge.
                    let date_w = TextLabel::estimate_width(&email.date, 9.0);
                    let date_x = list_x + list_w - pane_inner_pad() - date_w;
                    let head_bounds = list_bounds.map(|mut b| {
                        b[2] = date_x - cce_ui::layout::plate_gap();
                        b
                    });
                    pc.text_with(
                        row_head,
                        list_x + pane_inner_pad(),
                        draw_y + 6.0,
                        11.0,
                        if !email.read { [0xff, 0xff, 0xff] } else { [0xb0, 0xb0, 0xb8] },
                        None,
                        head_bounds,
                    );

                    // Date — right-aligned inside the row, clear of the scrollbar strip
                    pc.text_with(
                        email.date.clone(),
                        date_x,
                        draw_y + 7.0,
                        9.0,
                        [0x70, 0x70, 0x75],
                        None,
                        list_bounds,
                    );

                    // Subject — in full, as the header draws it: only the
                    // clip bounds it, at the card's content edge rather
                    // than the plate's, so a long one is cut clean of the
                    // padding and the scrollbar strip instead of shortened.
                    let subject_bounds = list_bounds.map(|mut b| {
                        b[2] = list_x + list_w - pane_inner_pad();
                        b
                    });
                    pc.text_with(
                        email.subject.clone(),
                        list_x + pane_inner_pad(),
                        draw_y + 20.0,
                        10.0,
                        if !email.read { [0x3a, 0x9a, 0xff] } else { [0x83, 0x83, 0x8a] },
                        None,
                        subject_bounds,
                    );

                    // Snippet — collapse ALL whitespace: CRLF bodies leave bare '\r'
                    // after a plain '\n' replace, and the renderer treats it as a
                    // line break, bleeding preview lines into the next row.
                    // Drawn in full like the subject and clipped at the same
                    // content edge; the cached body is a 1200-char preview,
                    // so the one run is bounded either way.
                    let snippet = email.body.split_whitespace().collect::<Vec<_>>().join(" ");
                    pc.text_with(
                        snippet,
                        list_x + pane_inner_pad(),
                        draw_y + 34.0,
                        9.0,
                        [0x60, 0x60, 0x65],
                        None,
                        subject_bounds,
                    );
                }
            }
        }

        // 4. Detail View Content (same modal gate as the list labels above)
        if modal_open {
        } else if let Some(selected_id) = self.selected_email_id {
            if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                // Subject header and metadata lines: single-line, so a long
                // subject (or address list) is ellipsized to the pane's text
                // column, and the label is clipped to the plate's content rect
                // as the backstop for the width estimate — these used to ride
                // the boundless `labels` drain and ran off the plate.
                let (plate_x, plate_y, plate_w, plate_h) = self.detail_header_geom();
                let header_bounds = Some([
                    plate_x + pane_inner_pad(),
                    plate_y,
                    plate_x + plate_w - pane_inner_pad(),
                    plate_y + plate_h,
                ]);
                // Every line is drawn in full — a subject or an address is
                // read end to end, and an ellipsis in the middle hides
                // exactly the part that tells one apart from another — so
                // only the clip rect bounds a line: the plate's content
                // edge, or the HTML-view buttons' left edge for a line whose
                // box reaches into their band (they stand on the date line
                // at the right edge).
                let header_lines: [(String, f32, f32, [u8; 3]); 4] = [
                    (email.subject.clone(), DETAIL_SUBJECT_Y, 15.0, [0xff, 0xff, 0xff]),
                    (format!("From: {}", email.from), DETAIL_FROM_Y, 11.0, [0xb0, 0xb0, 0xb8]),
                    (format!("To:   {}", email.to), DETAIL_TO_Y, 11.0, [0x83, 0x83, 0x8a]),
                    (format!("Date: {}", email.date), DETAIL_DATE_Y, 11.0, [0x83, 0x83, 0x8a]),
                ];
                let buttons_left = self.html_buttons_left();
                for (text, dy, font_size, color) in header_lines {
                    let mut bounds = header_bounds;
                    if let Some(bl) = buttons_left {
                        let buttons_top = DETAIL_LINES_BOTTOM - cce_ui::layout::button_height();
                        if dy + DETAIL_LINE_H > buttons_top {
                            if let Some(b) = bounds.as_mut() {
                                b[2] = b[2].min(bl - cce_ui::layout::plate_gap());
                            }
                        }
                    }
                    pc.text_with(
                        text,
                        detail_x,
                        self.detail_origin_y() + dy,
                        font_size,
                        color,
                        None,
                        bounds,
                    );
                }

                // Server-attachment chip labels (quads paint in display_list;
                // both sides lay out via detail_chip_rects).
                for (att, (cx, cy, _, ch)) in email
                    .remote_attachments
                    .iter()
                    .zip(detail_chip_rects(&email.remote_attachments, detail_x, self.detail_origin_y()))
                {
                    labels.push(TextLabel {
                        text: detail_chip_label(att),
                        x: cx + cce_ui::layout::CONTROL_TEXT_INSET,
                        y: cy + (ch - 12.0) / 2.0,
                        font_size: 10.0,
                        color: [0xc8, 0xc8, 0xd2],
                    });
                }
            }
        } else {
            let placeholder = "Select an email to view its content".to_string();
            let est_w = TextLabel::estimate_width(&placeholder, 13.0);
            let (plate_x, plate_y, plate_w, plate_h) = self.detail_pane_geom();
            let px = plate_x + (plate_w - est_w) / 2.0;
            labels.push(TextLabel {
                text: placeholder,
                x: px.max(plate_x + 4.0),
                y: plate_y + plate_h / 2.0 - 10.0,
                font_size: 13.0,
                color: [0x60, 0x60, 0x65],
            });
        }

        // 5. Status Banner Message — green timed info, red sticky error
        // (errors persist until clicked or replaced; see StatusToast).
        if let Some(ref toast) = self.status_message {
            let color = match toast {
                StatusToast::Info { .. } => [0x3a, 0xff, 0x80],
                StatusToast::Error { .. } => [0xff, 0x5c, 0x5c],
            };
            labels.push(TextLabel {
                text: toast.text().to_string(),
                x: 200.0,
                y: h_f32 - 25.0,
                font_size: 11.0,
                color,
            });
        }

        // 6. Compose Dialog Content
        if self.compose_open {
            let (modal_x, modal_y) = compose_modal_origin(w_f32, h_f32);
            let l = compose_layout(modal_x, modal_y);

            labels.push(TextLabel {
                text: self.compose_title.clone(),
                x: l.label_x,
                y: l.title_y,
                font_size: 13.0,
                color: [0xff, 0xff, 0xff],
            });

            for (name, row_y) in ["To:", "Cc:", "Bcc:", "Subject:"].iter().zip(l.rows_y) {
                // The label's baseline sits on the field's (the field centres
                // its text): 4px below its top at a 26px field and this size.
                let label_y = row_y + (cce_ui::layout::textbox_height() - 18.0) / 2.0;
                labels.push(TextLabel { text: name.to_string(), x: l.label_x, y: label_y, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            }

            // Attachment chips: the name (its `x` glyph is drawn with the
            // chip's plate), clickable to remove (hit-test in
            // handle_mouse_input via the same compose_chip_rects).
            for (path, (cx, cy, _cw, ch)) in self
                .compose_attachments
                .iter()
                .zip(compose_chip_rects(&self.compose_attachments, modal_x, modal_y))
            {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("attachment");
                labels.push(TextLabel {
                    text: ellipsize(name, 22),
                    x: cx + cce_ui::layout::CONTROL_TEXT_INSET,
                    y: cy + (ch - 12.0) / 2.0,
                    font_size: 10.0,
                    color: [0xc8, 0xc8, 0xd2],
                });
            }
        }

        // Emit accumulated static labels as text prims.
        for label in labels {
            pc.text_with(label.text.clone(), label.x, label.y, label.font_size, label.color, None, None);
        }
    }
}

/// Adapts the legacy `quads.push((x,y,w,h,color))` / `quads.extend(extra_quads())` calls in
/// the ported view() body to the single paint path: each tuple becomes a `PaintCtx::quad`.
struct __EmailQuadSink<'a> {
    pc: &'a mut cce_ui::scene::paint::PaintCtx,
}
impl<'a> __EmailQuadSink<'a> {
    fn push(&mut self, q: (f32, f32, f32, f32, [f32; 4])) {
        self.pc.quad(cce_ui::scene::layout::Rect { x: q.0, y: q.1, width: q.2, height: q.3 }, q.4);
    }
}

/// The control socket's commands (src/ipc.rs parses and renders; this is
/// the half that touches state). Actions go through `update` with the same
/// messages the widgets send, so a driven window behaves exactly as a
/// clicked one — including the side effects (`open` marks read, switching
/// folders may start a sync).
impl ClearEmailApp {
    /// A folder named the way the switcher shows it or by its tag,
    /// case-insensitively.
    fn folder_by_name(&self, name: &str) -> Option<&FolderInfo> {
        let n = name.trim().to_lowercase();
        self.folders
            .iter()
            .find(|f| f.tag.to_lowercase() == n || f.label.to_lowercase() == n)
    }

    /// Set one message's read flag, mirrored to the server for inbox mail
    /// (only inbox uids are known to be the INBOX's). False when no message
    /// has that id.
    fn set_read(&mut self, id: usize, read: bool) -> bool {
        let mut push = None;
        self.emails_rev = self.emails_rev.wrapping_add(1);
        let Some(email) = self.emails.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        if email.read != read {
            email.read = read;
            if email.folder == TAG_INBOX {
                push = email.uid.map(|u| (u, read));
            }
        }
        if let Some((uid, seen)) = push {
            if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                set_seen_on_server(acc.clone(), uid, seen, self.sender.clone());
            }
        }
        self.save_emails();
        true
    }

    /// Open the compose dialog with a mailto:'s fields filled in.
    fn prefill_compose(&mut self, m: &MailtoPrefill) {
        self.clear_compose();
        for (tb, v) in [
            (self.compose_to, &m.to),
            (self.compose_cc, &m.cc),
            (self.compose_bcc, &m.bcc),
            (self.compose_subject, &m.subject),
            (self.compose_body, &m.body),
        ] {
            let tb = &mut self.ui_context[tb];
            tb.text = v.clone();
            tb.edit_buffer = v.clone();
        }
        self.compose_title = "New Message".to_string();
        self.compose_open = true;
        self.needs_rebuild = true;
    }

    fn handle_ipc(&mut self, line: &str, needs_rebuild: &mut bool, exit: &mut bool) -> String {
        use ipc::Command;
        let ipc::Parsed { command, json } = match ipc::parse(line) {
            Ok(p) => p,
            Err(e) => return ipc::error(&e),
        };
        match command {
            Command::Help => ipc::HELP.to_string(),
            Command::Status => {
                let folder_label = self
                    .folders
                    .iter()
                    .find(|f| f.tag == self.current_folder)
                    .map(|f| f.label.clone())
                    .unwrap_or_default();
                let in_folder = self.emails.iter().filter(|e| e.folder == self.current_folder);
                let info = ipc::StatusInfo {
                    account: self.accounts.get(self.selected_account_idx).map(|a| a.email.clone()),
                    accounts: self.accounts.len(),
                    folder_tag: self.current_folder.clone(),
                    folder_label,
                    cached: self.emails.len(),
                    in_folder: in_folder.clone().count(),
                    unread_in_folder: in_folder.filter(|e| !e.read).count(),
                    selected: self.selected_email_id,
                    syncing: self.sync_in_flight.is_some(),
                    last_sync_secs_ago: self.last_sync_start.map(|t| t.elapsed().as_secs()),
                };
                ipc::render_status(&info, json)
            }
            Command::Accounts => ipc::render_accounts(&self.accounts, self.selected_account_idx, json),
            Command::Folders => ipc::render_folders(&self.folders, &self.emails, &self.current_folder, json),
            Command::List { folder, limit, unread } => {
                let tag = match folder {
                    None => self.current_folder.clone(),
                    Some(name) => match self.folder_by_name(&name) {
                        Some(f) => f.tag.clone(),
                        None => return ipc::error(&format!("no folder {name:?} (see `folders`)")),
                    },
                };
                let rows: Vec<&Email> = self
                    .emails
                    .iter()
                    .filter(|e| e.folder == tag && (!unread || !e.read))
                    .take(limit)
                    .collect();
                ipc::render_rows(&rows, json)
            }
            Command::Search { query, limit } => {
                // Same match as the window's search box, across every folder.
                let q = query.to_lowercase();
                let rows: Vec<&Email> = self
                    .emails
                    .iter()
                    .filter(|e| {
                        e.from.to_lowercase().contains(&q)
                            || e.subject.to_lowercase().contains(&q)
                            || e.body.to_lowercase().contains(&q)
                    })
                    .take(limit)
                    .collect();
                ipc::render_rows(&rows, json)
            }
            Command::Get(id) => match self.emails.iter().find(|e| e.id == id) {
                Some(e) => ipc::render_email(e, json),
                None => ipc::error(&format!("no message with id {id}")),
            },
            Command::Open(id) => {
                let Some(folder) = self.emails.iter().find(|e| e.id == id).map(|e| e.folder.clone()) else {
                    return ipc::error(&format!("no message with id {id}"));
                };
                if folder != self.current_folder {
                    self.update(AppMessage::SwitchFolder(folder), needs_rebuild, exit);
                }
                self.update(AppMessage::SelectEmail(id), needs_rebuild, exit);
                ipc::ok(&format!("opened {id}"), json)
            }
            Command::MarkRead(id, read) => {
                if !self.set_read(id, read) {
                    return ipc::error(&format!("no message with id {id}"));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                ipc::ok(&format!("{id} marked {}", if read { "read" } else { "unread" }), json)
            }
            Command::SelectAccount(email) => {
                let wanted = email.to_lowercase();
                match self.accounts.iter().position(|a| a.email.to_lowercase() == wanted) {
                    Some(idx) => {
                        self.update(AppMessage::SelectAccount(idx), needs_rebuild, exit);
                        ipc::ok(&format!("selected {}", self.accounts[idx].email), json)
                    }
                    None => ipc::error(&format!("no account {email:?} (see `accounts`)")),
                }
            }
            Command::SwitchFolder(name) => match self.folder_by_name(&name).map(|f| f.tag.clone()) {
                Some(tag) => {
                    self.update(AppMessage::SwitchFolder(tag.clone()), needs_rebuild, exit);
                    ipc::ok(&format!("switched to {tag}"), json)
                }
                None => ipc::error(&format!("no folder {name:?} (see `folders`)")),
            },
            Command::Sync => {
                self.update(AppMessage::SyncNow, needs_rebuild, exit);
                ipc::ok("sync started", json)
            }
            Command::Compose(None) => {
                self.update(AppMessage::ComposeNew, needs_rebuild, exit);
                ipc::ok("compose opened", json)
            }
            Command::Compose(Some(url)) => match parse_mailto(&url) {
                Some(m) => {
                    self.prefill_compose(&m);
                    *needs_rebuild = true;
                    ipc::ok("compose opened", json)
                }
                None => ipc::error(&format!("{url:?} is not a mailto: URL")),
            },
            Command::Quit => {
                *exit = true;
                ipc::ok("quitting", json)
            }
        }
    }
}

impl Application for ClearEmailApp {
    type Message = AppMessage;

    fn ui_context(&self) -> Option<&cce_ui::context::UiContext> {
        Some(&self.ui_context)
    }

    fn create(_sender: cce_ui::engine::AppSender<Self::Message>) -> Self {
        // The app keeps calloop's sender; `AppSender` converts into it.
        let _sender: calloop::channel::Sender<Self::Message> = _sender.into();
        cce_ui::scale::set_scale_factor(1.0);
        ipc::spawn_listener(_sender.clone());

        // The bar: the mail-icon app-menu button (its commands pop as a
        // context menu, see `open_mail_menu`) and two selection dropdowns
        // for folder + account. Per-message actions are not here: they live
        // on each card's right-click context menu, where the target is
        // unambiguous. The recessed bar chrome itself is carved in
        // display_list.
        let bar_font = cce_ui::layout::parse_font_string(&cce_ui::layout::menubar_font()).0;
        // The bar's items are plates of the panes' material, in the same
        // stance: flat, so the face is a quad and carries the frost, and the
        // silhouette is the rect so they line up with the panes.
        let mail_button = Button::new_icon("mail", "Mail", 0.0, 0.0, bar_item_h(), bar_item_h())
            .with_flat(true)
            .with_bg(pane_fill());
        let folder_dropdown = Dropdown::new(
            vec!["Inbox".to_string()],
            0,
        )
        .with_font_family(&bar_font)
        .with_flat(true)
        .with_face(cce_ui::scene::Material::from_fill(pane_fill()));

        let mut search_box = TextBox::new(String::new())
            .with_multiline(false)
            .with_draw_bg_border(true)
            .with_placeholder("Search mail — sender, subject or body");
        search_box.font_size = 11.0;

        // Designer raise/sink treatment: idle the bar sinks under the list's
        // plate (dimly visible through it), a scroll raises it over the
        // cards. Sinking is centring: the toolkit puts a sink-behind bar on
        // the plate's centre line, wherever the splitter leaves it.
        let email_list = ScrollRegion::new(54.0, 4.0)
            // Frameless: the cards sit straight on the plate. Explicit because
            // the prim paint path below honors `draw_frame` (default true) and
            // would otherwise draw the legacy 1px bordered box the old tuple
            // path silently ignored.
            .with_frame(false)
            .with_sink_behind(true);

        let accounts = load_accounts();
        // Last-used account wins (sidecar file), else the configured default:
        // the on-start sync below should hit the account the user actually
        // reads, not whichever entry carries the is_default flag.
        let selected_account_idx = load_selected_account_email()
            .and_then(|e| accounts.iter().position(|a| a.email == e))
            .or_else(|| accounts.iter().position(|a| a.is_default))
            .unwrap_or(0);

        let account_dropdown = Dropdown::new(
            account_dropdown_options(&accounts),
            selected_account_idx,
        )
        .with_font_family(&bar_font)
        .with_flat(true)
        .with_face(cce_ui::scene::Material::from_fill(pane_fill()));

        let mut detail_body = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(false);
        detail_body.font_size = 12.0;
        detail_body.font_family = "sans-serif".to_string();

        let mut compose_to = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_to.font_size = 12.0;
        let mut compose_cc = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_cc.font_size = 12.0;
        let mut compose_bcc = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_bcc.font_size = 12.0;
        let mut compose_subject = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_subject.font_size = 12.0;
        let mut compose_body = TextBox::new(String::new()).with_multiline(true).with_line_wrap(true).with_draw_bg_border(true);
        compose_body.font_size = 12.0;
        compose_body.font_family = "sans-serif".to_string();

        let btn_h = cce_ui::layout::button_height();
        let btn_compose_send = Button::new(0.0, 0.0, COMPOSE_BTN_W, btn_h).with_label("Send");
        let btn_compose_cancel = Button::new_reset(0.0, 0.0, COMPOSE_BTN_W, btn_h).with_label("Cancel");
        let btn_compose_attach = Button::new(0.0, 0.0, COMPOSE_ATTACH_W, btn_h).with_label("Attach...");

        // A mailto: argv (this is the x-scheme-handler/mailto handler) opens
        // the compose dialog prefilled. Both text and edit_buffer are set,
        // same as ComposeNew/Reply: ComposeSend reads whichever side the
        // editing flag selects, so a box left untouched must agree with one
        // the user clicked into.
        let mailto = std::env::args().nth(1).and_then(|a| parse_mailto(&a));


        let emails = if let Some(acc) = accounts.get(selected_account_idx) {
            load_emails_for_account(&acc.email)
        } else {
            Vec::new()
        };
        let initial_folders = accounts
            .get(selected_account_idx)
            .map(|a| load_folders(&a.email))
            .unwrap_or_else(default_folders);

        // The context owns the widgets; the app keeps their handles.
        let mut ui_context = cce_ui::context::UiContext::new();
        let mut app = Self {
            last_sync_start: None,
            secret_retry_at: None,
            secret_missing_reported: false,
            backfill_at: None,
            sync_in_flight: None,
            sync_generation: 0,
            backfill_remaining: std::collections::HashMap::new(),
            resync_pending: false,
            keys: EmailKeys::load(),
            mail_button: ui_context.insert(mail_button),
            folder_dropdown: ui_context.insert(folder_dropdown),
            account_dropdown: ui_context.insert(account_dropdown),
            search_box: ui_context.insert(search_box),
            email_list,
            email_buttons: Vec::new(),
            detail_body: ui_context.insert(detail_body),
            compose_to: ui_context.insert(compose_to),
            compose_cc: ui_context.insert(compose_cc),
            compose_bcc: ui_context.insert(compose_bcc),
            compose_subject: ui_context.insert(compose_subject),
            compose_body: ui_context.insert(compose_body),
            compose_attachments: Vec::new(),
            btn_compose_send: ui_context.insert(btn_compose_send),
            btn_compose_cancel: ui_context.insert(btn_compose_cancel),
            btn_compose_attach: ui_context.insert(btn_compose_attach),
            accounts,
            selected_account_idx,
            emails,
            emails_dirty_since: None,
            emails_rev: 0,
            filter_cache: std::cell::RefCell::new(None),
            current_folder: TAG_INBOX.to_string(),
            folders: initial_folders,
            synced_tags: std::collections::HashSet::new(),
            selected_email_id: None,
            body_scroll: 0.0,
            body_motion: ScrollMotion::new(),
            body_content_h: 0.0,
            detail_hovered: false,
            body_sb_dragging: false,
            body_sb_drag_offset: 0.0,
            body_sb_activity: cce_ui::widget::ScrollbarActivity::new(),
            #[cfg(feature = "wpe")]
            webview: wpe::MailWebView::new((800, 600)),
            seen_renderer: false,
            #[cfg(feature = "wpe")]
            html_loaded: None,
            #[cfg(feature = "wpe")]
            html_pending: None,
            #[cfg(feature = "wpe")]
            html_cache: std::collections::HashMap::new(),
            #[cfg(feature = "wpe")]
            show_text: false,
            #[cfg(feature = "wpe")]
            btn_html_view: ui_context.insert(Button::new(-9999.0, -9999.0, 0.0, 0.0).with_label("View: HTML")),
            #[cfg(feature = "wpe")]
            btn_html_images: ui_context.insert(Button::new(-9999.0, -9999.0, 0.0, 0.0).with_label("Load Images")),
            #[cfg(feature = "wpe")]
            webview_rect: (0.0, 0.0, 0.0, 0.0),
            #[cfg(feature = "wpe")]
            webview_mouse_down: false,
            list_w: load_list_w().unwrap_or(LIST_W_DEFAULT),
            split_dragging: false,
            focused_pane: Pane::List,
            context_menu_actions: Vec::new(),
            search_open: false,
            compose_open: false,
            compose_title: String::new(),
            status_message: None,
            sender: _sender.clone(),
            width: 1000,
            height: 600,
            scale_factor: 1.0,
            font_system: cce_ui::create_font_system(),
            needs_rebuild: true,
            ui_context,
        };

        // A mailto: launch opens the compose dialog prefilled — the same
        // path `compose <url>` on the control socket takes at runtime.
        if let Some(m) = &mailto {
            app.prefill_compose(m);
        }

        // The first sync goes through start_sync like every other one, so it
        // gets the same keyring refresh and missing-password guard. It used to
        // call sync_imap inline here — the one sync that bypassed both, and
        // precisely the one that fails when the vault is not up yet.
        app.start_sync(true);
        app
    }

    /// Wake on GLib activity rather than polling for it — cce-browser's
    /// bridge: the epoll fd carrying WPE's pollfd set, plus a timer for the
    /// timeout GLib asks for. Both fire `Spin`, which pumps the webview.
    /// With no page loaded GLib schedules nothing, so the timer settles at
    /// its 1s ceiling and the app stays effectively idle.
    #[cfg(feature = "wpe")]
    fn register_sources(&mut self, handle: &calloop::LoopHandle<'_, EngineState<Self>>) {
        use calloop::{generic::Generic, Interest, Mode, PostAction};

        if let Some(fd) = self.webview.poll_fd_owned() {
            let tx = self.sender.clone();
            // Level-triggered: `pump` drains the epoll, so an un-consumed
            // socket re-arms rather than being missed.
            let source = Generic::new(fd, Interest::READ, Mode::Level);
            if let Err(e) = handle.insert_source(source, move |_, _, _| {
                let _ = tx.send(AppMessage::Spin);
                Ok(PostAction::Continue)
            }) {
                eprintln!("cce-mail: could not watch the GLib fd ({e}); falling back to the timer alone");
            }
        }

        let tx = self.sender.clone();
        let timer = calloop::timer::Timer::from_duration(std::time::Duration::from_millis(100));
        if let Err(e) = handle.insert_source(timer, move |_, _, state| {
            let _ = tx.send(AppMessage::Spin);
            let next = state
                .inner
                .as_ref()
                .and_then(|app| app.webview.poll_timeout())
                .unwrap_or(std::time::Duration::from_millis(1000))
                .clamp(
                    std::time::Duration::from_millis(8),
                    std::time::Duration::from_millis(1000),
                );
            calloop::timer::TimeoutAction::ToDuration(next)
        }) {
            eprintln!("cce-mail: could not arm the GLib timer ({e})");
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Mail".to_string(),
            app_id: "cce-mail".to_string(),
            width: 1000,
            height: 600,
            fullscreen: false,
            min_size: Some((800, 500)),
        }
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, exit: &mut bool) {
        match msg {
            AppMessage::SwitchFolder(f) => {
                // A folder opened for the first time this run syncs now rather
                // than waiting out the throttle: it would otherwise sit there
                // looking empty with nothing to say it had not been fetched.
                let first_visit = !self.synced_tags.contains(&f);
                self.current_folder = f;
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                self.reset_html();
                self.start_sync(first_visit);
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectEmail(id) => {
                // A draft doesn't open in the read pane — it resumes in the
                // compose dialog and leaves the folder (Cancel re-files it,
                // Send delivers it, a send failure re-files it too).
                if let Some(pos) = self.emails.iter().position(|e| e.id == id && e.folder == "drafts") {
                    self.emails_rev = self.emails_rev.wrapping_add(1);
                    let draft = self.emails.remove(pos);
                    self.save_emails();
                    self.clear_compose();
                    for (tb, v) in [
                        (self.compose_to, &draft.to),
                        (self.compose_cc, &draft.cc),
                        (self.compose_bcc, &draft.bcc),
                        (self.compose_subject, &draft.subject),
                        (self.compose_body, &draft.body),
                    ] {
                        let tb = &mut self.ui_context[tb];
                        tb.text = v.clone();
                        tb.edit_buffer = v.clone();
                    }
                    self.compose_attachments = draft.attachments;
                    self.compose_title = "Draft".to_string();
                    self.compose_open = true;
                    self.selected_email_id = None;
                    *needs_rebuild = true;
                    self.needs_rebuild = true;
                    return;
                }
                self.selected_email_id = Some(id);
                self.body_scroll = 0.0;
                self.request_html(id);
                let mut push_seen_uid = None;
                self.emails_rev = self.emails_rev.wrapping_add(1);
                if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                    if !email.read {
                        email.read = true;
                        if email.folder == "inbox" {
                            push_seen_uid = email.uid;
                        }
                        self.save_emails();
                    }
                }
                if let Some(uid) = push_seen_uid {
                    if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                        set_seen_on_server(acc.clone(), uid, true, self.sender.clone());
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SearchChanged => {
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                self.reset_html();
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeNew => {
                self.clear_compose();
                self.compose_title = "New Message".to_string();
                self.compose_open = true;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeCancel => {
                // Cancel is "put it down", not "throw it away": any content
                // goes to Drafts, where clicking it resumes the compose.
                let mail = self.gather_compose();
                if !mail.to.is_empty()
                    || !mail.cc.is_empty()
                    || !mail.bcc.is_empty()
                    || !mail.subject.is_empty()
                    || !mail.body.is_empty()
                    || !mail.attachments.is_empty()
                {
                    self.file_as_draft(&mail);
                    self.status_message = Some(StatusToast::info("Saved to Drafts", 3.0));
                }
                self.clear_compose();
                self.compose_open = false;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeAttach => {
                // rfd blocks this thread until the chooser closes — the same
                // trade cce-data-editor and cce-preview already make.
                if let Some(path) = cce_ui::file_dialog::pick_file("Attach File", &[]) {
                    self.compose_attachments.push(path.to_string_lossy().into_owned());
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeSend => {
                let mail = self.gather_compose();
                if !mail.to.is_empty() || !mail.cc.is_empty() || !mail.bcc.is_empty() {
                    let active_acc = self.accounts[self.selected_account_idx].clone();
                    // The outcome comes back as SendResult: Sent on success,
                    // Drafts on failure. Nothing is recorded optimistically —
                    // the old flow filed a Sent copy before SMTP even ran, so
                    // a failed send looked exactly like a delivered one.
                    send_smtp(active_acc, mail, self.sender.clone());
                    self.clear_compose();
                    self.compose_open = false;
                } else {
                    self.status_message = Some(StatusToast::error("Recipient is required"));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::OpenAttachment(email_id, att_idx) => {
                let target = self
                    .emails
                    .iter()
                    .find(|e| e.id == email_id)
                    .and_then(|e| {
                        e.remote_attachments.get(att_idx).map(|a| (e.folder.clone(), e.uid, a.clone()))
                    });
                match target {
                    Some((folder, Some(uid), att)) => {
                        let mailbox = self.mailbox_for(&folder);
                        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                            self.status_message = Some(StatusToast::info(format!("Fetching {}...", att.name), 4.0));
                            if let Some(mailbox) = mailbox {
                                fetch_attachment(acc.clone(), mailbox, uid, att, self.sender.clone());
                            }
                        }
                    }
                    Some((_, None, _)) => {
                        self.status_message =
                            Some(StatusToast::error("No server copy for this message"));
                    }
                    None => {}
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AttachmentFetched(outcome) => {
                match outcome {
                    Ok(path) => {
                        let name = std::path::Path::new(&path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("attachment")
                            .to_string();
                        self.status_message = Some(StatusToast::info(format!("Saved {} — opening...", name), 4.0));
                        // Route through the XDG default — which, since the
                        // Default Apps work, is a cce app for pdf/images.
                        // `gio open` reads the same mimeapps.list as xdg-open
                        // but resolves in C (~8ms) instead of shell (~60ms),
                        // all of it before the handler even spawns.
                        let mut cmd = std::process::Command::new("gio");
                        cmd.arg("open").arg(&path);
                        let _ = spawn_detached(cmd);
                    }
                    Err(e) => {
                        self.status_message = Some(StatusToast::error(format!("Attachment: {}", e)));
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SendResult(mail, outcome) => {
                let new_id = self.emails.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                let from = self
                    .accounts
                    .get(self.selected_account_idx)
                    .map(|a| a.email.clone())
                    .unwrap_or_default();
                match outcome {
                    None => {
                        self.emails_rev = self.emails_rev.wrapping_add(1);
                        self.emails.push(Email {
                            id: new_id,
                            from,
                            to: mail.to,
                            subject: if mail.subject.is_empty() { "(No Subject)".to_string() } else { mail.subject },
                            body: mail.body,
                            date: "Just now".to_string(),
                            read: true,
                            folder: "sent".to_string(),
                            uid: None,
                            ts: None,
                            origin_folder: None,
                            cc: mail.cc,
                            // A sent copy records who was bcc'd nowhere; the
                            // attachment paths shrink to names for display.
                            bcc: String::new(),
                            attachments: mail
                                .attachments
                                .iter()
                                .map(|p| {
                                    std::path::Path::new(p)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("attachment")
                                        .to_string()
                                })
                                .collect(),
                            remote_attachments: Vec::new(),
                        });
                        self.save_emails();
                        self.status_message = Some(StatusToast::info("Email Sent Successfully", 4.0));
                    }
                    Some(err) => {
                        self.file_as_draft(&mail);
                        self.status_message = Some(StatusToast::error(format!("Send failed — saved to Drafts: {}", err)));
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::Reply => {
                if let Some(id) = self.selected_email_id {
                    if let Some(email) = self.emails.iter().find(|e| e.id == id).cloned() {
                        // Start from a clean slate: a Cc/Bcc/attachment left
                        // over from an earlier compose must not ride along.
                        self.clear_compose();
                        self.ui_context[self.compose_to].text = email.from.clone();
                        self.ui_context[self.compose_to].edit_buffer = email.from.clone();
                        self.ui_context[self.compose_subject].text = if email.subject.starts_with("Re:") {
                            email.subject.clone()
                        } else {
                            format!("Re: {}", email.subject)
                        };
                        self.ui_context[self.compose_subject].edit_buffer = self.ui_context[self.compose_subject].text.clone();

                        let reply_intro = format!("\n\nOn {}, {} wrote:\n> {}", email.date, email.from, email.body.replace('\n', "\n> "));
                        self.ui_context[self.compose_body].text = reply_intro.clone();
                        self.ui_context[self.compose_body].edit_buffer = reply_intro;
                        self.ui_context[self.compose_body].cursor_idx = 0;
                        self.compose_title = "Reply".to_string();
                        self.compose_open = true;
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::DeleteSelected => {
                if let Some(id) = self.selected_email_id {
                    // Where it sits in the visible list, read before the
                    // delete changes what that list contains.
                    let deleted_row =
                        self.filtered_emails().iter().position(|e| e.id == id);
                    let mut permanently_deleted = false;
                    // (mailbox tag, uid, permanent)
                    let mut server_op: Option<(String, u32, bool)> = None;
                    self.emails_rev = self.emails_rev.wrapping_add(1);
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        if email.folder == "trash" {
                            // Already in the local Trash: this is the second
                            // delete, so it is the final one. The copy on the
                            // server lives in whichever mailbox it came from.
                            permanently_deleted = true;
                            if let (Some(origin), Some(uid)) =
                                (email.origin_folder.clone(), email.uid)
                            {
                                server_op = Some((origin, uid, true));
                            }
                        } else if email.folder == "archive" {
                            // All Mail is the last label holding a message, so
                            // there is no lesser delete to offer here: removing
                            // it from this mailbox removes it from the account.
                            permanently_deleted = true;
                            if let Some(uid) = email.uid {
                                server_op = Some(("archive".to_string(), uid, true));
                            }
                        } else {
                            if email.folder == "inbox" {
                                server_op = email.uid.map(|u| ("inbox".to_string(), u, false));
                            }
                            // Remember where it came from. Deletion only
                            // reaches the server for INBOX (expunging from
                            // All Mail would be a permanent delete, not a
                            // label removal), so for every other folder the
                            // message is still there — and without this the
                            // next sync would see the uid missing from that
                            // folder, call it new, and download it back.
                            if email.uid.is_some() {
                                email.origin_folder = Some(email.folder.clone());
                            }
                            email.folder = "trash".to_string();
                        }
                    }
                    if let Some((tag, uid, permanent)) = server_op {
                        let mailbox = self.mailbox_for(&tag);
                        if let (Some(mailbox), Some(acc)) =
                            (mailbox, self.accounts.get(self.selected_account_idx))
                        {
                            delete_on_server(
                                acc.clone(),
                                mailbox,
                                uid,
                                permanent,
                                self.sender.clone(),
                            );
                        }
                    }
                    if permanently_deleted {
                        self.emails_rev = self.emails_rev.wrapping_add(1);
                        self.emails.retain(|e| e.id != id);
                    }
                    self.save_emails();

                    // Land on whatever slid into the deleted row's place, so a
                    // run of deletions needs no reaching for the mouse. Set
                    // directly rather than through SelectEmail: that marks a
                    // message read, and deleting past unread mail should not
                    // quietly mark it read — nor resume the next draft.
                    let rows: Vec<usize> =
                        self.filtered_emails().iter().map(|e| e.id).collect();
                    self.selected_email_id = match deleted_row {
                        Some(i) => rows.get(i).or_else(|| rows.last()).copied(),
                        None => None,
                    };
                    self.body_scroll = 0.0;
                    // The webview still shows the deleted message; swap to
                    // the new selection's HTML (request_html marks nothing
                    // read — that stays SelectEmail's job).
                    self.reset_html();
                    if let Some(next_id) = self.selected_email_id {
                        self.request_html(next_id);
                    }
                    if let Some(next_id) = self.selected_email_id {
                        if let Some(i) = rows.iter().position(|id| *id == next_id) {
                            self.scroll_row_into_view(i);
                        }
                    }

                    self.status_message = Some(StatusToast::info(
                        if permanently_deleted { "Email Deleted Permanently" } else { "Moved to Trash" },
                        4.0,
                    ));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ToggleUnread => {
                if let Some(id) = self.selected_email_id {
                    if let Some(read) = self.emails.iter().find(|e| e.id == id).map(|e| !e.read) {
                        self.set_read(id, read);
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectAccount(idx) => {
                // The outgoing account's unsaved changes go first.
                self.flush_emails();
                self.selected_account_idx = idx;
                self.ui_context[self.account_dropdown].selected = idx.min(self.accounts.len().saturating_sub(1));
                if let Some(email) = self.accounts.get(idx).map(|a| a.email.clone()) {
                    self.emails_rev = self.emails_rev.wrapping_add(1);
                    self.emails = load_emails_for_account(&email);
                    save_selected_account_email(&email);
                    self.start_sync(true);
                } else {
                    self.emails_rev = self.emails_rev.wrapping_add(1);
                    self.emails = Vec::new();
                }
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                self.reset_html();
                #[cfg(feature = "wpe")]
                self.html_cache.clear();
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ManageAccounts => {
                let mut cmd = std::process::Command::new("cce-system-interface");
                cmd.arg("accounts");
                let _ = spawn_detached(cmd);
                self.status_message = Some(StatusToast::info("Opening System Settings...", 4.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SyncNow => {
                self.start_sync(true);
            }
            AppMessage::Ipc(req) => {
                let reply = self.handle_ipc(&req.line, needs_rebuild, exit);
                req.respond(&reply);
            }
            AppMessage::Quit => {
                *exit = true;
            }
            AppMessage::Status(msg) => {
                self.status_message = Some(StatusToast::info(msg, 4.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::StatusError(msg) => {
                self.status_message = Some(StatusToast::error(msg));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::FoldersDiscovered(email, discovered) => {
                let active = self
                    .accounts
                    .get(self.selected_account_idx)
                    .map(|a| a.email.clone());
                save_folders(&email, &discovered);
                if active.as_deref() == Some(email.as_str()) && self.folders != discovered {
                    // Keep showing whatever is on screen if it survived.
                    self.folders = discovered;
                    if !self.folders.iter().any(|f| f.tag == self.current_folder) {
                        self.current_folder = TAG_INBOX.to_string();
                    }
                    *needs_rebuild = true;
                    self.needs_rebuild = true;
                }
            }
            AppMessage::EmailsSynced(email, folders) => {
                for f in &folders {
                    self.synced_tags.insert(f.folder.clone());
                }
                let remaining: usize = folders.iter().map(|f| f.remaining).sum();
                let fetched: usize = folders.iter().map(|f| f.fetched.len()).sum();
                let active = self
                    .accounts
                    .get(self.selected_account_idx)
                    .map(|a| a.email.clone());

                if active.as_deref() == Some(email.as_str()) {
                    self.emails_rev = self.emails_rev.wrapping_add(1);
                    let prior = std::mem::take(&mut self.emails);
                    self.emails = merge_sync(prior, &folders);
                    self.save_emails();
                    // The merge can re-key ids, so id-keyed HTML is no
                    // longer trustworthy. The loaded view stays (the user
                    // is reading it); a re-open refetches.
                    #[cfg(feature = "wpe")]
                    self.html_cache.clear();
                } else if active.is_some() {
                    // A background account: fold into its cache on disk only,
                    // off the UI thread — the read and parse are the whole
                    // cache.
                    let folders = folders.clone();
                    let email = email.clone();
                    std::thread::spawn(move || {
                        let prior = load_emails_for_account(&email);
                        save_emails_for_account(&email, merge_sync(prior, &folders));
                    });
                }

                // Keep pulling while history is still coming down. Each pass
                // is already saved, so this can stop at any point without
                // losing what arrived. The decision is deferred to
                // `SyncFinished`: results arrive one folder at a time now, and
                // scheduling from here would start the next pass on top of the
                // one still walking the folders behind this message — exactly
                // the overlap `sync_in_flight` exists to prevent. A folder
                // with nothing outstanding must not clear the flag either; the
                // inbox being current says nothing about All Mail's history.
                for f in &folders {
                    self.backfill_remaining.insert(f.folder.clone(), f.remaining);
                }
                // News from the pass on the wire: it is not stuck.
                if let Some(scope) = self.sync_in_flight.as_mut().filter(|s| s.account == email) {
                    scope.started = std::time::Instant::now();
                }
                if remaining > 0 {
                    self.status_message = Some(StatusToast::info(
                        format!("Fetched {} — {} older messages still to come...", fetched, remaining),
                        4.0,
                    ));
                }

                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SyncFinished(generation) => {
                // A pass presumed lost, reporting back after its replacement
                // went out: it has nothing to hand over.
                if self.sync_in_flight.as_ref().is_none_or(|s| s.generation != generation) {
                    return;
                }
                self.sync_in_flight = None;
                // Now that the wire is free: a sync turned away while this
                // one was out goes first — it is there because the user
                // changed account or opened a folder this pass never covered,
                // and it supersedes backfilling history nobody asked for.
                // `start_sync` cannot be turned away twice, the flag having
                // just been cleared.
                let backfill_pending =
                    std::mem::take(&mut self.backfill_remaining).values().any(|&n| n > 0);
                if std::mem::take(&mut self.resync_pending) {
                    self.start_sync(true);
                    *needs_rebuild = true;
                    self.needs_rebuild = true;
                } else {
                    self.backfill_at = backfill_pending.then(|| {
                        std::time::Instant::now()
                            + std::time::Duration::from_secs(BACKFILL_DELAY_SECS)
                    });
                }
            }
            AppMessage::UpdateAccountTokens(email, access_token, expiry) => {
                if let Some(acc) = self.accounts.iter_mut().find(|a| a.email == email) {
                    persist_account_tokens(&email, access_token.as_deref(), expiry);
                    acc.access_token = access_token;
                    acc.token_expiry = expiry;
                }
            }
            #[cfg(feature = "wpe")]
            AppMessage::Spin => {
                if self.webview.pump() {
                    *needs_rebuild = true;
                    self.needs_rebuild = true;
                }
                // Link clicks never navigate the pane; they leave through
                // the XDG default browser.
                while let Some(uri) = self.webview.take_link_click() {
                    if uri.starts_with("http://")
                        || uri.starts_with("https://")
                        || uri.starts_with("mailto:")
                    {
                        // gio, not xdg-open: same mimeapps resolution, minus
                        // ~50ms of shell script on the click-to-tab path.
                        let mut cmd = std::process::Command::new("gio");
                        cmd.arg("open").arg(&uri);
                        let _ = spawn_detached(cmd);
                        self.status_message =
                            Some(StatusToast::info(format!("Opening {}", ellipsize(&uri, 60)), 4.0));
                        *needs_rebuild = true;
                        self.needs_rebuild = true;
                    }
                }
            }
            #[cfg(feature = "wpe")]
            AppMessage::HtmlFetched(id, html) => {
                if self.html_pending == Some(id) {
                    self.html_pending = None;
                }
                if let Some(mail) = html {
                    if self.html_cache.len() >= HTML_CACHE_CAP {
                        self.html_cache.clear();
                    }
                    self.html_cache.insert(id, mail.clone());
                    if self.selected_email_id == Some(id) {
                        self.show_html_mail(&mail);
                        self.html_loaded = Some(id);
                        *needs_rebuild = true;
                        self.needs_rebuild = true;
                    }
                }
            }
        }
    }

    /// A live info toast's only pending work is its own expiry, which the
    /// runner cannot see: nothing redraws, so the loop would park on its
    /// default idle sleep and retire the toast up to a second late. Poll
    /// while one is up; the scheduled backfill is happy with the default.
    fn idle_poll_interval(&self) -> Option<std::time::Duration> {
        let toast = matches!(self.status_message, Some(StatusToast::Info { .. }))
            .then(|| std::time::Duration::from_millis(100));
        // A pending cache save is due EMAILS_SAVE_DELAY after it was marked.
        let save = self
            .emails_dirty_since
            .map(|t| EMAILS_SAVE_DELAY.saturating_sub(t.elapsed()).max(std::time::Duration::from_millis(10)));
        match (toast, save) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn on_exit(&mut self) {
        self.flush_emails();
        email_store::flush();
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        if self.emails_dirty_since.is_some_and(|t| t.elapsed() >= EMAILS_SAVE_DELAY) {
            self.flush_emails();
        }
        // Pump the widget tick walk: animating widgets (dropdown menus'
        // expand/contract) register as tick receivers and report changed
        // until their transition lands — without this a closing menu freezes
        // fully open.
        if self.ui_context.tick(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }

        // Raise/sink upkeep for the two scrollbars (email list, detail pane):
        // true while the post-scroll hold runs or on the depth flip, keeping
        // frames coming so the sink actually renders.
        if self.email_list.tick(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
        // The body's own glide/coast: the wheel only moves the target, this
        // is what carries the drawn offset there.
        if self.tick_body_scroll(dt) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
        let body_holding = self.body_sb_activity.holding();
        let body_visible = self.body_scrollbar_geom().is_some();
        if self.body_sb_activity.tick(dt, body_visible, self.body_sb_dragging) || body_holding {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }

        // A sync skipped for a missing password retries itself, so unlocking
        // the vault brings mail in without the user having to ask again.
        // No redraw is requested here: a failed retry changes nothing on
        // screen (the toast is already up), and asking for one every few
        // seconds would keep this demand-driven loop from ever idling. A
        // successful one repaints when its results arrive.
        if self.secret_retry_at.is_some_and(|t| std::time::Instant::now() >= t) {
            self.secret_retry_at = None;
            self.start_sync(true);
        }

        // Next backfill pass. Same reasoning on the redraw: the pass repaints
        // when its results land, so nothing is requested here. Held back
        // while a pass is out rather than put to `start_sync`: history that
        // waits a few seconds longer costs nothing, where a forced call it
        // would only turn away announces "Already syncing..." over whatever
        // the user is reading — and this one is the app's idea, not theirs.
        if self.sync_in_flight.is_none()
            && self.backfill_at.is_some_and(|t| std::time::Instant::now() >= t)
        {
            self.backfill_at = None;
            self.start_sync(true);
        }

        // Only info toasts expire; an error stays until clicked or replaced.
        if let Some(StatusToast::Info { expires_at, .. }) = self.status_message {
            if std::time::Instant::now() >= expires_at {
                self.status_message = None;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
        }
    }

    /// Hand the rendered message back to a replacement renderer.
    ///
    /// The detail pane draws the WPE page as an image id, and an id belongs to
    /// a renderer, not to the process: `cce-ui`'s `window_runner` repairs a
    /// lost Wayland transport by opening a new session around the same
    /// `Application`, which rebuilds the renderer and with it the image table.
    /// A draw for an unknown id is skipped rather than reported, and the
    /// webview only re-uploads when WPE produces a new frame — which a
    /// finished page never does. So a reconnect left the message list and the
    /// headers intact beside an empty body pane.
    ///
    /// `reupload_frame` replays the pixels the webview already holds, so the
    /// page comes back on the next frame with no reload and no refetch of
    /// remote content.
    ///
    /// Not on the first renderer: nothing has been uploaded yet, and the
    /// webview has not rendered a frame.
    fn renderer_init(&mut self, _renderer: &mut cce_ui::vk::VkRenderer) {
        if !std::mem::replace(&mut self.seen_renderer, true) {
            return;
        }
        #[cfg(feature = "wpe")]
        if self.webview.reupload_frame() {
            eprintln!("cce-mail: renderer replaced; re-uploaded the rendered message");
            self.needs_rebuild = true;
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<cce_ui::scene::paint::DisplayList> {
        // Phase 6ai single paint path: the view() geometry (all plain quads) and the text
        // (the old rebuild_text_items assembly, now prims via emit_text_prims) are this one
        // list. The app FontSystem stays for the TextBoxes' prepare_text measurement, but is
        // now bundled create_font_system() (was _with_system_fonts) — the render path is the
        // engine's bundled cache, so measurement matches and no face-ID mismatch remains.
        let mut __pc = cce_ui::scene::paint::PaintCtx::new();
        let quads = &mut __EmailQuadSink { pc: &mut __pc };
        let size_changed = self.width != size.width as u32 || self.height != size.height as u32 || self.scale_factor != scale;
        if self.needs_rebuild || size_changed {
            self.width = size.width as u32;
            self.height = size.height as u32;
            self.scale_factor = scale;
        }

        let w_f32 = self.width as f32;
        let h_f32 = self.height as f32;

        let list_x = window_pad();
        let (list_w, _separator_x, detail_x) = self.split_geom();

        if self.needs_rebuild || size_changed {
            // Bar layout — every open dropdown is a popover: registration
            // feeds the dl-text occlusion clamp, and the popover pass at the
            // end of this function draws it on top of everything.
            self.ui_context.clear_popovers();
            if self.ui_context[self.folder_dropdown].popover_rect().is_some() {
                self.ui_context.register_popover_id(self.folder_dropdown.id());
            }
            if self.ui_context[self.account_dropdown].popover_rect().is_some() {
                self.ui_context.register_popover_id(self.account_dropdown.id());
            }
            // Labels and the index the switcher reports both come from the
            // one list, so a discovered folder cannot leave them disagreeing.
            self.ui_context[self.folder_dropdown].options =
                self.folders.iter().map(|f| f.label.clone()).collect();

            // Folder rows and trigger both carry the live inbox unread count.
            // Applied AFTER the rebuild above and found by tag rather than
            // index 0 — written before it, the rebuild simply erased it.
            let inbox_unread = self
                .emails
                .iter()
                .filter(|e| e.folder == TAG_INBOX && !e.read)
                .count();
            if inbox_unread > 0 {
                if let Some(i) = self.folders.iter().position(|f| f.tag == TAG_INBOX) {
                    self.ui_context[self.folder_dropdown].options[i] =
                        format!("{} ({})", self.folders[i].label, inbox_unread);
                }
            }
            self.ui_context[self.folder_dropdown].selected = self
                .folders
                .iter()
                .position(|f| f.tag == self.current_folder)
                .unwrap_or(0);
            // Menu button left; selectors right at fixed offsets from the
            // edge — anchoring to the folder label would make the account
            // switcher drift as the folder name changes length.
            // The bar row on the same two spacings as everything else: one
            // padding in from each rim, one gap between siblings (which is
            // the case `root_plate_gap` names as "control rows"). The items
            // hang from the band's top padding, so their bottoms are flush
            // with it and the plates below clear them by one padding.
            // Flat plates, so each rect IS its silhouette and the bar sits on
            // the same grid as the panes with no compensation at all.
            let (edge, gap) = (window_pad(), pane_gap());
            // Bottoms on the band's, should button and dropdown heights differ.
            let (btn_h, dd_h) = (cce_ui::layout::button_height(), cce_ui::layout::dropdown_height());
            let bottom = edge + bar_item_h();
            self.ui_context[self.mail_button].set_rect(edge, bottom - btn_h, btn_h, btn_h);
            let folder_x = w_f32 - edge - FOLDER_W;
            self.ui_context[self.folder_dropdown].set_rect(folder_x, bottom - dd_h, FOLDER_W, dd_h);
            self.ui_context[self.account_dropdown]
                .set_rect(folder_x - gap - ACCOUNT_W, bottom - dd_h, ACCOUNT_W, dd_h);

            cce_ui::scale::set_scale_factor(scale as f32);

            // The search box spans the band when open, and parks off-screen
            // when closed so a stale rect can't be hit by anything that still
            // routes to it.
            if self.search_open {
                self.ui_context[self.search_box].set_rect(list_x, menubar_h() + pane_pad(), list_w, cce_ui::layout::textbox_height());
            } else {
                self.ui_context[self.search_box].set_rect(-9999.0, -9999.0, 0.0, 0.0);
            }

            // Get filtered emails count for bounds setup
            let list_count = self.filtered_emails().len();

            let (list_top, list_h) = self.list_geom();
            self.email_list.set_rect(list_x, list_top, list_w, list_h);
            self.email_list.update_bounds(list_count, list_top, list_h);

            if self.email_buttons.len() != list_count {
                // The context owns the rows: the outgoing ones are taken out of it
                // (ids are never reused) and the fresh ones put in, registered from
                // the moment they exist.
                for h in std::mem::take(&mut self.email_buttons) {
                    self.ui_context.remove(h);
                }
                let ctx = &mut self.ui_context;
                self.email_buttons = (0..list_count)
                    .map(|_| ctx.insert(Button::new_list_row(0.0, 0.0, 0.0, 0.0)))
                    .collect();
            }

            // Get selected email state to avoid borrowing self while mutating
            let selected_email_state = if let Some(selected_id) = self.selected_email_id {
                self.emails.iter().find(|e| e.id == selected_id).map(|e| e.body.clone())
            } else {
                None
            };

            // Get filtered email IDs and selection states
            let filtered_email_ids: Vec<(usize, bool)> = self
                .filtered_emails()
                .iter()
                .map(|e| (e.id, Some(e.id) == self.selected_email_id))
                .collect();

            for (idx, &(_email_id, is_selected)) in filtered_email_ids.iter().enumerate() {
                let btn = &mut self.ui_context[self.email_buttons[idx]];
                btn.selected = is_selected;
                if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                    btn.set_rect(list_x, draw_y, list_w, 54.0);
                } else {
                    btn.set_rect(-9999.0, -9999.0, 0.0, 0.0);
                }
            }

            // Detail View
            if let Some(body) = selected_email_state {
                let detail_w = self.detail_content_w();
                let (body_y, body_h) = self.detail_body_geom();
                self.ui_context[self.detail_body].set_rect(detail_x, body_y, detail_w, body_h);
                self.ui_context[self.detail_body].text = body;
            }

            // Compose inputs layout
            if self.compose_open {
                let (modal_x, modal_y) = compose_modal_origin(w_f32, h_f32);
                let l = compose_layout(modal_x, modal_y);

                let field_h = cce_ui::layout::textbox_height();
                self.ui_context[self.compose_to].set_rect(l.field_x, l.rows_y[0], l.field_w, field_h);
                self.ui_context[self.compose_cc].set_rect(l.field_x, l.rows_y[1], l.field_w, field_h);
                self.ui_context[self.compose_bcc].set_rect(l.field_x, l.rows_y[2], l.field_w, field_h);
                self.ui_context[self.compose_subject].set_rect(l.field_x, l.rows_y[3], l.field_w, field_h);
                let (bx, by, bw, bh) = l.body;
                self.ui_context[self.compose_body].set_rect(bx, by, bw, bh);

                let btn_h = cce_ui::layout::button_height();
                self.ui_context[self.btn_compose_attach].set_rect(l.attach_x, l.buttons_y, COMPOSE_ATTACH_W, btn_h);
                self.ui_context[self.btn_compose_send].set_rect(l.send_x, l.buttons_y, COMPOSE_BTN_W, btn_h);
                self.ui_context[self.btn_compose_cancel].set_rect(l.cancel_x, l.buttons_y, COMPOSE_BTN_W, btn_h);
            }



            self.needs_rebuild = false;
        }
        // Outside the rebuild gate: the labels follow webview state (the
        // images toggle) that does not always raise needs_rebuild.
        #[cfg(feature = "wpe")]
        self.layout_html_buttons();

        // Now compute `filtered` only for rendering (immutable borrow of self)
        let filtered: Vec<&Email> = {
            let search_text = if self.ui_context[self.search_box].editing { &self.ui_context[self.search_box].edit_buffer } else { &self.ui_context[self.search_box].text };
            filtered_from(&self.emails, self.emails_rev, &self.current_folder, search_text, &self.filter_cache)
        };

        // 1. The standard root plate (cce-ui `PlateSpec::window`): the DE root
        // material at its opacity, the shared silhouette arc, the rolled rim.
        // Replaces a hardcoded deep-slate quad of this app's own.
        quads.pc.root_plate(w_f32, h_f32);

        // Anything that must show THROUGH a frosted plate has to exist before
        // the FIRST blur plate in the frame: that one takes the backdrop
        // snapshot and every later one reuses it, even with ordinary draws in
        // between. So both scrollbars' behind copies go down here, at full
        // strength, ahead of the bar's items as well as the panes' plates —
        // the bar items became plates too, which made THEM the frame's first
        // blur plate and quietly flattened the panes until this moved up.
        //
        // Each plate ends up carrying the other pane's bar in its sampled
        // backdrop; they do not overlap, so that costs nothing.
        self.push_body_scrollbar(&mut *quads.pc, 1.0);
        self.email_list.push_scrollbar_prims(&mut *quads.pc);

        // The bar carries no chrome of its own: it is flush with the root
        // plate, the same face as the content below it, and its widgets are
        // the only thing marking the band. It used to be a plateau one step
        // down, carved with a single bottom wall (`recess_edges`, the
        // inherited MenuBar paint) — that wall is gone.
        cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.mail_button], &mut *quads.pc);
        cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.folder_dropdown], &mut *quads.pc);
        cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.account_dropdown], &mut *quads.pc);

        // 3. The detail column's two plates: the header above, the preview
        // below. Same fill and same radius as the list: the three are one
        // set, and the gaps between them are the seams — there is no drawn
        // separator line. The split is still draggable in the vertical gap;
        // `cursor_icon` puts resize arrows over the grab band, which is what
        // advertises it now that nothing is painted there. Both are drawn
        // whether or not a message is on show, so the layout does not jump
        // on the first selection.
        // Nothing here reaches the plates' corners (the body carries its own
        // text bounds), so this needs no rounded clip the way the list — whose
        // cards span its full width — does.
        for (px, py, pw, ph) in [self.detail_header_geom(), self.detail_pane_geom()] {
            quads.pc.rounded_rect(
                cce_ui::scene::layout::Rect { x: px, y: py, width: pw, height: ph },
                cce_ui::layout::list_corner_radius(),
                (true, true, true, true),
                pane_fill(),
            );
        }

        // Search band (only while open) and the list
        if self.search_open {
            self.ui_context[self.search_box].prepare_text(&mut self.font_system);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.search_box], &mut *quads.pc);
        }
        {
            // The list's behind copy is NOT emitted here — it goes down with
            // the detail pane's, ahead of both plates; see there.
            // The list is a plate, so it wears the DE's list radius rather
            // than square corners — `ScrollRegion`'s own framed paint draws
            // its surface at exactly this getter.
            quads.pc.rounded_rect(
                cce_ui::scene::layout::Rect {
                    x: self.email_list.x,
                    y: self.email_list.y,
                    width: self.email_list.w,
                    height: self.email_list.h,
                },
                cce_ui::layout::list_corner_radius(),
                (true, true, true, true),
                pane_fill(),
            );
        }

        // Visible List Item Buttons — `get_item_draw_y` returns PARTIALLY
        // visible cards too (the toolkit ScrollRegion's intersection
        // contract), so the whole card pass runs under the list viewport clip:
        // an edge card renders cut by the clip instead of vanishing. The
        // unread dots ride the same PaintCtx (the tuple sink forwards to it),
        // so the clip covers them too.
        // Rounded, not square: a card spans the plate's full width, so at the
        // first and last row the clip is what carries the plate's corner onto
        // the card riding in it — concentric, the way a plate cuts its children.
        quads.pc.push_clip_rounded(
            cce_ui::scene::layout::Rect {
                x: self.email_list.x,
                y: self.email_list.viewport_y,
                width: self.email_list.w,
                height: self.email_list.viewport_h,
            },
            cce_ui::layout::list_corner_radius(),
        );
        for idx in 0..filtered.len() {
            if self.email_list.get_item_draw_y(idx, 0.0).is_some() {
                cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.email_buttons[idx]], &mut *quads.pc);

                // Unread: the `circle` glyph in blue, a 6px dot (the glyph's
                // disc is three quarters of its box) centred where the
                // square quad it replaced stood.
                if !filtered[idx].read {
                    if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                        let side = 8.0;
                        let dot = cce_ui::scene::layout::Rect {
                            x: list_x + (pane_inner_pad() - side) / 2.0,
                            y: draw_y + 15.0 - side / 2.0,
                            width: side,
                            height: side,
                        };
                        // Prims take linear colour; a glyph is tinted in sRGB.
                        quads.pc.icon("circle", dot, cce_ui::colors::to_srgb(UNREAD_DOT));
                    }
                }
            }
        }
        quads.pc.pop_clip_rounded();
        // The bar's fore copy after the rows, so the thumb rides on top of
        // them instead of peeking through the inter-row gaps — at the fade,
        // over the idle copy that went down under the plates above.
        self.email_list.push_scrollbar_fore(&mut *quads.pc);

        // The focused pane's highlight ring: cce-designer's flat-style pane
        // focus (`append_context_border`), at the plates' own radius. Both
        // plates here are flat fills with no relief to tint, so the ring is
        // the cue in either relief style. Over the cards, which span the
        // list plate's full width and would otherwise cover it.
        {
            let (x, y, w, h) = match self.focused_pane {
                Pane::List => (self.email_list.x, self.email_list.y, self.email_list.w, self.email_list.h),
                Pane::Detail => self.detail_pane_geom(),
            };
            let mut color = cce_ui::colors::highlight_primary_color();
            color[3] = 0.9;
            let r = cce_ui::layout::list_corner_radius();
            quads.pc.border(
                cce_ui::scene::layout::Rect { x, y, width: w, height: h },
                (r, r, r, r),
                [0.0; 4],
                color,
                2.0,
            );
        }

        // 4. Detail View Area
        if let Some(selected_id) = self.selected_email_id {
            if self.emails.iter().any(|e| e.id == selected_id) {
                // Body as one boxed text prim: word-wrapped at the pane width and
                // clipped to the pane (the TextBox walk drew each logical line as a
                // single run, so long paragraphs truncated at the pane edge). Skipped
                // while a modal is up — boxed text still renders above the panel.
                // Body geometry is read before the email/font_system borrows —
                // detail_body_geom takes &self whole.
                let (body_y, body_h) = self.detail_body_geom();
                if !self.compose_open {
                    if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                        // Server-attachment chips in the header band (labels
                        // ride in the labels pass; same rect fn both places).
                        for (cx, cy, cw, ch) in detail_chip_rects(&email.remote_attachments, detail_x, self.detail_origin_y()) {
                            quads.push((cx, cy, cw, ch, [0.14, 0.14, 0.20, 1.0]));
                            quads.push((cx, cy, cw, 1.0, [0.25, 0.35, 0.50, 0.40]));
                            quads.push((cx, cy + ch - 1.0, cw, 1.0, [0.25, 0.35, 0.50, 0.40]));
                            quads.push((cx, cy, 1.0, ch, [0.25, 0.35, 0.50, 0.40]));
                            quads.push((cx + cw - 1.0, cy, 1.0, ch, [0.25, 0.35, 0.50, 0.40]));
                        }

                        // HTML-view buttons, right-aligned in the same band
                        // (placed by layout_html_buttons; parked ones paint
                        // off-screen).
                        #[cfg(feature = "wpe")]
                        for btn in [&self.ui_context[self.btn_html_view], &self.ui_context[self.btn_html_images]] {
                            cce_ui::widget::painter::paint_root_into(&self.ui_context, btn, &mut *quads.pc);
                        }

                        let body_w = self.detail_content_w();
                        let line_h = 12.0 * 1.4; // get_text_buffer_laid_out's placed-text metric

                        // The rendered HTML view: one image quad where the
                        // text body would go. WebKit owns scrolling inside
                        // the page, so the app's body scroll state is
                        // disarmed while this is up.
                        #[cfg(feature = "wpe")]
                        let html_img = if self.html_active() {
                            let s = self.scale_factor as f32;
                            self.webview.resize(
                                (body_w * s) as u32,
                                (body_h * s) as u32,
                                s,
                            );
                            self.webview_rect = (detail_x, body_y, body_w, body_h);
                            self.webview.image()
                        } else {
                            None
                        };
                        #[cfg(not(feature = "wpe"))]
                        let html_img: Option<(u32, u32, u32)> = None;

                        if let Some((img_id, ..)) = html_img {
                            self.body_content_h = 0.0;
                            self.body_scroll = 0.0;
                            // White ground: frames lag a resize by a beat,
                            // and mail HTML assumes a white canvas.
                            quads.push((detail_x, body_y, body_w, body_h, [1.0, 1.0, 1.0, 1.0]));
                            quads.pc.image(
                                img_id,
                                cce_ui::scene::layout::Rect {
                                    x: detail_x,
                                    y: body_y,
                                    width: body_w,
                                    height: body_h,
                                },
                                1.0,
                            );
                        } else {

                        // Measure with the exact shaping the renderer will use, so the
                        // scroll clamp and the thumb track the real wrapped height.
                        let (buffer, _) = cce_ui::engine::get_text_buffer_laid_out(
                            &mut self.font_system,
                            &email.body,
                            12.0,
                            Some("sans-serif"),
                            cce_ui::scene::paint::TextAttrs::default(),
                            cce_ui::scene::paint::TextLayout {
                                wrap_width: Some(body_w),
                                box_height: 1.0e6,
                                align_h: cce_ui::scene::paint::AlignH::Left,
                                align_v: cce_ui::scene::paint::AlignV::Top,
                            },
                        );
                        let content_h = buffer.layout_runs().count() as f32 * line_h;
                        self.body_content_h = content_h;
                        let max_scroll = (content_h - body_h).max(0.0);
                        self.body_scroll = self.body_scroll.clamp(0.0, max_scroll);

                        // The fore copy, faded in over the body (the idle copy
                        // went down under the plates, above). Driven by the
                        // activity's fade rather than its latch, so it keeps
                        // drawing all the way out instead of being cut off the
                        // instant the bar stops counting as raised.
                        self.push_body_scrollbar(&mut *quads.pc, self.body_sb_activity.fade());

                        quads.pc.text_boxed(
                            email.body.clone(),
                            detail_x,
                            body_y - self.body_scroll,
                            12.0,
                            [0xc8, 0xc8, 0xd0],
                            Some("sans-serif".to_string()),
                            Some([detail_x, body_y, detail_x + body_w, body_y + body_h]),
                            cce_ui::scene::paint::TextAttrs::default(),
                            cce_ui::scene::paint::TextLayout {
                                wrap_width: Some(body_w),
                                box_height: content_h.max(body_h),
                                align_h: cce_ui::scene::paint::AlignH::Left,
                                align_v: cce_ui::scene::paint::AlignV::Top,
                            },
                        );
                        }
                    }
                }
            }
        }

        // 5. Compose Dialog Overlay
        if self.compose_open {
            let (modal_x, modal_y) = compose_modal_origin(w_f32, h_f32);

            // Semitransparent modal backdrop
            quads.push((0.0, 0.0, w_f32, h_f32, [0.0, 0.0, 0.0, 0.6]));

            // The dialog: a raised popover plate (the menu material, frosted
            // over the scrim) at the menu radius — the toolkit's own raised
            // popover idiom, in place of an opaque quad with four hairlines.
            {
                let r = cce_ui::layout::menu_corner_radius();
                quads.pc.plate(
                    cce_ui::scene::layout::Rect { x: modal_x, y: modal_y, width: COMPOSE_W, height: compose_h() },
                    (r, r, r, r),
                    &cce_ui::scene::Material::popover(cce_ui::color::page_low_color()),
                    cce_ui::layout::bevel_width(),
                );
            }

            // Attachment chips: control plates and their `x` glyphs here,
            // names in the labels pass — all laid out by compose_chip_rects.
            // The glyph sits in the room the chip keeps past the name; the
            // whole chip is still the remove target (handle_mouse_input).
            for (cx, cy, cw, ch) in compose_chip_rects(&self.compose_attachments, modal_x, modal_y) {
                let r = cce_ui::layout::control_corner_radius();
                quads.pc.bevel(
                    cce_ui::scene::layout::Rect { x: cx, y: cy, width: cw, height: ch },
                    (r, r, r, r),
                    &cce_ui::scene::Material::control(),
                    cce_ui::layout::bevel_width().min(ch * 0.2),
                );
                let side = 8.0;
                quads.pc.icon(
                    "x",
                    cce_ui::scene::layout::Rect {
                        x: cx + cw - cce_ui::layout::CONTROL_TEXT_INSET - side,
                        y: cy + (ch - side) / 2.0,
                        width: side,
                        height: side,
                    },
                    [0xc8 as f32 / 255.0, 0xc8 as f32 / 255.0, 0xd2 as f32 / 255.0, 1.0],
                );
            }

            self.ui_context[self.compose_to].prepare_text(&mut self.font_system);
            self.ui_context[self.compose_cc].prepare_text(&mut self.font_system);
            self.ui_context[self.compose_bcc].prepare_text(&mut self.font_system);
            self.ui_context[self.compose_subject].prepare_text(&mut self.font_system);
            self.ui_context[self.compose_body].prepare_text(&mut self.font_system);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.compose_to], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.compose_cc], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.compose_bcc], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.compose_subject], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.compose_body], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.btn_compose_send], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.btn_compose_cancel], &mut *quads.pc);
            cce_ui::widget::painter::paint_root_into(&self.ui_context, &self.ui_context[self.btn_compose_attach], &mut *quads.pc);
        }



        // Popover pass: on top of everything, exactly where they hit-test,
        // painted into the frame's own PaintCtx (a RenderTarget) as the
        // gallery and the designer paint theirs. A dropdown's labels carry
        // the drawn popover rect as their bounds (the is-overlay-text
        // convention). Until 2026-10-06 this went through a
        // `PopoverCollector`, which keeps only rects and text: an open
        // dropdown lost its arrow and its option marks, both cce-icons
        // glyphs, and its plate came out flat and square-cornered.
        {
            for &pop_id in &self.ui_context.active_popovers {
                let Some(popover) = self.ui_context.get_widget(pop_id) else { continue };
                if popover.popover_rect().is_none() {
                    continue;
                }
                popover.render_popover(&mut __pc);
            }
        }

        self.emit_text_prims(&mut __pc);

        // The card context menu draws last, over everything. Its labels carry
        // bounds EXACTLY equal to the menu rect: the engine's popover
        // occlusion clamp exempts only an exact match, and pushes this same
        // rect as an overlay so the row text underneath is clamped away.
        // The lit plate and the menu font in one call — the flat
        // `extra_quads` look this drew was the pre-frost menu every other
        // app has moved off.
        cce_ui::widget::context_menu::paint_with_labels(&mut __pc);

        Some(__pc.finish())
    }

    fn cursor_icon(&self, x: f32, y: f32) -> Option<CursorIcon> {
        // Resize arrows over the split's grab band — the only thing marking
        // the split now that the separator line is gone — and for the whole
        // drag: mid-drag the pointer legally outruns the clamped split, and
        // the cursor must not flicker back to the arrow there.
        if !self.compose_open
            && !cce_ui::widget::context_menu::is_visible()
            && (self.split_dragging
                || ((x - self.split_geom().1).abs() <= SPLIT_GRAB_SLOP && y > menubar_h()))
        {
            return Some(CursorIcon::EwResize);
        }
        None
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let mut changed = false;
        let px = pos.x as f32;
        let py = pos.y as f32;

        // An open card menu owns the pointer: it takes the hover highlight,
        // and nothing beneath it re-hovers under the covered rows.
        if cce_ui::widget::context_menu::is_visible() {
            if cce_ui::widget::context_menu::cursor_moved(px, py) {
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            return;
        }
        // Active scrollbar-thumb drag tracks the pointer — before the
        // ui_context borrow (the sb helper takes &mut self).
        if !self.compose_open && self.body_sb_dragging {
            if self.body_sb_drag_to(py) {
                changed = true;
            }
        }

        // Detail-bar hover bookkeeping: hover only SUSTAINS a raised bar (the
        // hysteresis contract), so raw strip geometry is the right input.
        if !self.compose_open {
            let over = self
                .body_scrollbar_geom()
                .is_some_and(|(sb_x, track_y, sb_w, track_h, _, _)| {
                    px >= sb_x - 4.0
                        && px <= sb_x + sb_w + 4.0
                        && py >= track_y
                        && py <= track_y + track_h
                });
            self.body_sb_activity.set_hover(over);
        }

        // Active split drag: the separator follows the pointer, clamped so
        // neither pane collapses. Clamp into the stored value (not just at
        // paint) so the release persists what the user actually sees.
        if !self.compose_open && self.split_dragging {
            let (edge, gap) = (window_pad(), pane_gap());
            let max_w = (self.width as f32 - 2.0 * edge - gap - DETAIL_W_MIN).max(LIST_W_MIN);
            // The pointer holds the gap's centre line, which is where the grab
            // band is — so the list ends half a gap back from it.
            let new_w = (px - edge - gap / 2.0).clamp(LIST_W_MIN, max_w);
            if (new_w - self.list_w).abs() > 0.5 {
                self.list_w = new_w;
                changed = true;
            }
        }

        // Routed dispatch (6bd shrink): one Event per widget root through the router.
        // separator_x is read before ctx: split_geom(&self) cannot run while
        // ui_context is mutably borrowed.
        let separator_x = self.split_geom().1;
        let mv = cce_ui::widget::Event::PointerMove { x: px, y: py, local_x: px, local_y: py };
        let ctx = &mut self.ui_context;

        if ctx.propagate_event(&mv, self.mail_button.id()) { changed = true; }
        if ctx.propagate_event(&mv, self.folder_dropdown.id()) { changed = true; }

        if self.compose_open {
            if ctx.propagate_event(&mv, self.compose_to.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_cc.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_bcc.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_subject.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_body.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_send.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_cancel.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_attach.id()) { changed = true; }
        } else {
            if ctx.propagate_event(&mv, self.account_dropdown.id()) { changed = true; }

            // Search and lists
            if self.search_open && ctx.propagate_event(&mv, self.search_box.id()) { changed = true; }
            if self.email_list.cursor_moved(px, py) { changed = true; }
            // Hover scope for the detail-pane body scroll (wheel + keys).
            self.detail_hovered = px > separator_x;

            for &btn in &self.email_buttons {
                if ctx[btn].rect().0 > -9000.0 {
                    if ctx.propagate_event(&mv, btn.id()) { changed = true; }
                }
            }

            #[cfg(feature = "wpe")]
            {
                if ctx.propagate_event(&mv, self.btn_html_view.id()) { changed = true; }
                if ctx.propagate_event(&mv, self.btn_html_images.id()) { changed = true; }
            }

            // Detail pane: the HTML-view buttons above — and the rendered HTML
            // page tracks the pointer (hover states, drag selection). While
            // a press is held the page keeps the pointer even outside the
            // rect, so selections drag naturally.
            #[cfg(feature = "wpe")]
            if self.html_on_show() && (self.webview_mouse_down || self.in_webview(px, py)) {
                let (wx, wy) = self.webview_px(px, py);
                self.webview.mouse_move(wx, wy);
            }
        }

        if changed {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn handle_mouse_input(&mut self, button: MouseButton, state: ElementState, pos: LogicalPosition, needs_rebuild: &mut bool) -> Option<Self::Message> {
        let mut changed = false;
        let mut msg_out = None;
        let px = pos.x as f32;
        let py = pos.y as f32;
        let ev = cce_ui::widget::Event::MouseButton { button, state, x: px, y: py, local_x: px, local_y: py };

        // An open card menu swallows the press before any widget routing.
        if cce_ui::widget::context_menu::is_visible() {
            if state != ElementState::Pressed {
                return None;
            }
            let msg = self.context_menu_press(px, py);
            *needs_rebuild = true;
            self.needs_rebuild = true;
            if msg.is_some() {
                return msg;
            }
            // A right-press elsewhere goes on to open another card's menu.
            if button != MouseButton::Right {
                return None;
            }
        }

        // Right-press on a message card opens that card's context menu.
        if button == MouseButton::Right && state == ElementState::Pressed && !self.compose_open {
            // Both the engine's outside-press sweep and Dropdown's own event
            // arm match Left only, so without this a right press strands an
            // open bar menu hanging over the list.
            self.ui_context.close_popovers_missed_by_press(px, py);
            if self.open_card_context_menu(px, py) {
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return None;
            }
        }

        // A sticky error toast dismisses on a direct press (info toasts
        // expire on their own). Bounds mirror the paint site — x=200,
        // y=h-25, 11px — with a forgiving band; the toast draws over
        // whatever is beneath, so consuming the press is right.
        if button == MouseButton::Left && state == ElementState::Pressed {
            if let Some(StatusToast::Error { ref text }) = self.status_message {
                let tw = TextLabel::estimate_width(text, 11.0);
                let ty = self.height as f32 - 25.0;
                if px >= 196.0 && px <= 204.0 + tw && py >= ty - 6.0 && py <= ty + 16.0 {
                    self.status_message = None;
                    *needs_rebuild = true;
                    self.needs_rebuild = true;
                    return None;
                }
            }
        }

        // Detail-pane body scrollbar drag — before the ui_context borrow (the
        // sb helpers take &mut self).
        if !self.compose_open && button == MouseButton::Left {
            match state {
                ElementState::Pressed => {
                    // A fresh press always supersedes a stale drag — a lost
                    // release must not leave the thumb glued to the pointer.
                    self.body_sb_dragging = false;
                    self.split_dragging = false;
                    // Grab the list/detail separator (±slop, below the bar).
                    let separator_x = self.split_geom().1;
                    if (px - separator_x).abs() <= SPLIT_GRAB_SLOP && py > menubar_h() {
                        self.split_dragging = true;
                        *needs_rebuild = true;
                        self.needs_rebuild = true;
                        return None;
                    }
                    // A press in either plate moves pane focus there, the
                    // way a click focuses a designer pane; the bar above
                    // the plates leaves it alone.
                    if py > menubar_h() {
                        let pane = if px < separator_x { Pane::List } else { Pane::Detail };
                        if pane != self.focused_pane {
                            self.focused_pane = pane;
                            changed = true;
                        }
                    }
                    if self.selected_email_id.is_some() && self.body_sb_press(px, py) {
                        changed = true;
                    }
                    // Server-attachment chips: hit-test against the same
                    // rects the paint pass laid out.
                    let detail_x = self.split_geom().2;
                    if let Some(email) = self
                        .selected_email_id
                        .and_then(|id| self.emails.iter().find(|e| e.id == id))
                    {
                        if !email.remote_attachments.is_empty() {
                            let hit = detail_chip_rects(&email.remote_attachments, detail_x, self.detail_origin_y())
                                .iter()
                                .position(|&(cx, cy, cw, ch)| {
                                    px >= cx && px <= cx + cw && py >= cy && py <= cy + ch
                                });
                            if let Some(i) = hit {
                                msg_out = Some(AppMessage::OpenAttachment(email.id, i));
                                changed = true;
                            }
                        }
                    }
                }
                ElementState::Released => {
                    if std::mem::take(&mut self.body_sb_dragging) {
                        // Drag release starts the hold before the bar hides.
                        self.body_sb_activity.bump();
                        changed = true;
                    }
                    if std::mem::take(&mut self.split_dragging) {
                        // Persist the split where the drag left it.
                        save_list_w(self.list_w);
                        changed = true;
                    }
                }
            }
        }

        let ctx = &mut self.ui_context;

        // Account dropdown (the bar dropdowns route before the pane content —
        // an open menu overlays the panes, so a handled press must not fall
        // through to what's beneath it).
        {
            let was_open = ctx[self.account_dropdown].open;
            if ctx.propagate_event(&ev, self.account_dropdown.id()) {
                if !was_open && ctx[self.account_dropdown].open {
                    // Freshly opened: re-read the shared accounts.json so
                    // cce-system-interface edits appear without a restart (the
                    // job the retired Accounts page did on entry). Keep the
                    // selection by email; if that account is gone, fall to the
                    // default. Field-level accesses only — `ctx` still borrows
                    // self.ui_context.
                    let prev_email = self
                        .accounts
                        .get(self.selected_account_idx)
                        .map(|a| a.email.clone());
                    self.accounts = load_accounts();
                    let new_idx = prev_email
                        .as_deref()
                        .and_then(|e| self.accounts.iter().position(|a| a.email == e))
                        .or_else(|| self.accounts.iter().position(|a| a.is_default))
                        .unwrap_or(0);
                    let changed_account =
                        self.accounts.get(new_idx).map(|a| a.email.as_str()) != prev_email.as_deref();
                    ctx[self.account_dropdown].options = account_dropdown_options(&self.accounts);
                    ctx[self.account_dropdown].selected = new_idx;
                    if changed_account {
                        // The account under the selection vanished — switch to
                        // what the trigger now shows.
                        *needs_rebuild = true;
                        self.needs_rebuild = true;
                        return Some(AppMessage::SelectAccount(new_idx));
                    }
                }
                let mut msg_out = None;
                if ctx[self.account_dropdown].take_change() {
                    let idx = ctx[self.account_dropdown].selected;
                    if idx < self.accounts.len() {
                        if idx != self.selected_account_idx {
                            msg_out = Some(AppMessage::SelectAccount(idx));
                        }
                    } else {
                        // The trailing "Manage Accounts…" pseudo-entry: not an
                        // account — restore the trigger to the active one.
                        ctx[self.account_dropdown].selected = self.selected_account_idx;
                        msg_out = Some(AppMessage::ManageAccounts);
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return msg_out;
            }
        }

        // The mail-icon button: a click pops the app menu under it. The
        // rows fire on the NEXT press, through the context-menu gate at the
        // top of this function.
        if ctx.propagate_event(&ev, self.mail_button.id()) {
            if state == ElementState::Released && ctx[self.mail_button].take_click() {
                self.open_mail_menu();
            }
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return None;
        }
        // Folder switcher.
        if ctx.propagate_event(&ev, self.folder_dropdown.id()) {
            if ctx[self.folder_dropdown].take_change() {
                if let Some(f) = self.folders.get(ctx[self.folder_dropdown].selected) {
                    msg_out = Some(AppMessage::SwitchFolder(f.tag.clone()));
                }
            }
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return msg_out;
        }

        if self.compose_open {
            if ctx.propagate_event(&ev, self.compose_to.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused_id(self.compose_to.id()); }
            }
            if ctx.propagate_event(&ev, self.compose_cc.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused_id(self.compose_cc.id()); }
            }
            if ctx.propagate_event(&ev, self.compose_bcc.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused_id(self.compose_bcc.id()); }
            }
            if ctx.propagate_event(&ev, self.compose_subject.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused_id(self.compose_subject.id()); }
            }
            if ctx.propagate_event(&ev, self.compose_body.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused_id(self.compose_body.id()); }
            }

            if ctx.propagate_event(&ev, self.btn_compose_send.id()) {
                changed = true;
                if state == ElementState::Released && ctx[self.btn_compose_send].take_click() {
                    msg_out = Some(AppMessage::ComposeSend);
                }
            }
            if ctx.propagate_event(&ev, self.btn_compose_cancel.id()) {
                changed = true;
                if state == ElementState::Released && ctx[self.btn_compose_cancel].take_click() {
                    msg_out = Some(AppMessage::ComposeCancel);
                }
            }
            if ctx.propagate_event(&ev, self.btn_compose_attach.id()) {
                changed = true;
                if state == ElementState::Released && ctx[self.btn_compose_attach].take_click() {
                    msg_out = Some(AppMessage::ComposeAttach);
                }
            }

            // Attachment chips are painted, not widgets: hit-test against the
            // same rects the paint pass used and remove the clicked one.
            if !changed && state == ElementState::Pressed && button == MouseButton::Left {
                let (modal_x, modal_y) = compose_modal_origin(self.width as f32, self.height as f32);
                let hit = compose_chip_rects(&self.compose_attachments, modal_x, modal_y)
                    .iter()
                    .position(|&(cx, cy, cw, ch)| px >= cx && px <= cx + cw && py >= cy && py <= cy + ch);
                if let Some(i) = hit {
                    self.compose_attachments.remove(i);
                    changed = true;
                }
            }

            // Click outside the modal clears focus or behaves neutrally
            if !changed && state == ElementState::Pressed && button == MouseButton::Left {
                let w_f32 = self.width as f32;
                let h_f32 = self.height as f32;
                let (modal_x, modal_y) = compose_modal_origin(w_f32, h_f32);

                if px < modal_x || px > modal_x + COMPOSE_W || py < modal_y || py > modal_y + compose_h() {
                    ctx.clear_focus();
                    ctx[self.compose_to].unfocus();
                    ctx[self.compose_cc].unfocus();
                    ctx[self.compose_bcc].unfocus();
                    ctx[self.compose_subject].unfocus();
                    ctx[self.compose_body].unfocus();
                    changed = true;
                }
            }
        } else {
            // Search input — only reachable while the band is open.
            if self.search_open && ctx.propagate_event(&ev, self.search_box.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused_id(self.search_box.id()); }
                if ctx[self.search_box].take_change() {
                    msg_out = Some(AppMessage::SearchChanged);
                }
            } else if state == ElementState::Pressed && button == MouseButton::Left {
                ctx.clear_focus();
                ctx[self.search_box].unfocus();
                // Clicking away from an EMPTY search collapses the band —
                // an unfocused box with no query is dead chrome. One holding
                // a query stays: it is the visible reason the list is short.
                if self.search_open && ctx[self.search_box].text.is_empty() && ctx[self.search_box].edit_buffer.is_empty() {
                    self.search_open = false;
                }
                changed = true;
            }

            // The scrollbar strip sits inside the row rects (rows span x 10..310, the strip
            // ~294..310), so a press the scrollbar consumed must not also reach the row
            // underneath it — grabbing the thumb used to press+release the row behind it and
            // fire SelectEmail.
            let mut scrollbar_took_press = false;
            if button == MouseButton::Left {
                let handled = match state {
                    ElementState::Pressed => self.email_list.press(px, py),
                    ElementState::Released => self.email_list.release(),
                };
                if handled {
                    changed = true;
                    if state == ElementState::Pressed {
                        scrollbar_took_press = true;
                    }
                }
            }


            if scrollbar_took_press {
                // fall through to the rest of the handler, but not to the rows
            } else {
                // Mirrors the paint pass's folder filter — this decides which
                // email a row click lands on. No wildcard: a new folder
                // absorbed into "inbox" here routes clicks to the wrong list
                // (Drafts was, briefly).
                let search_text = if ctx[self.search_box].editing { &ctx[self.search_box].edit_buffer } else { &ctx[self.search_box].text };
                let filtered: Vec<&Email> =
                    filtered_from(&self.emails, self.emails_rev, &self.current_folder, search_text, &self.filter_cache);

                for (idx, email) in filtered.iter().enumerate() {
                    if idx < self.email_buttons.len() {
                        let btn = self.email_buttons[idx];
                        if ctx[btn].rect().0 > -9000.0 {
                            if ctx.propagate_event(&ev, btn.id()) {
                                changed = true;
                                if state == ElementState::Released && ctx[btn].take_click() {
                                    msg_out = Some(AppMessage::SelectEmail(email.id));
                                }
                            }
                        }
                    }
                }
            }

            // HTML-view buttons (a release commits the click; parked ones
            // never take a press).
            #[cfg(feature = "wpe")]
            {
                if ctx.propagate_event(&ev, self.btn_html_view.id()) {
                    changed = true;
                    if state == ElementState::Released && ctx[self.btn_html_view].take_click() {
                        self.show_text = !self.show_text;
                    }
                }
                if ctx.propagate_event(&ev, self.btn_html_images.id()) {
                    changed = true;
                    if state == ElementState::Released && ctx[self.btn_html_images].take_click() {
                        let lift = !self.webview.images_allowed();
                        self.webview.set_images_allowed(lift);
                    }
                }
            }

            // The detail pane takes no widget events besides those — but the
            // rendered HTML page does take raw pointer input (text selection, link
            // clicks). Presses reach here only after every overlay (card
            // menu, bar dropdowns) had its chance and returned; the release
            // follows the press wherever the pointer went, so a drag out of
            // the pane still ends cleanly.
            #[cfg(feature = "wpe")]
            if button == MouseButton::Left {
                match state {
                    ElementState::Pressed => {
                        if !scrollbar_took_press
                            && !self.body_sb_dragging
                            && !self.split_dragging
                            && self.html_on_show()
                            && self.in_webview(px, py)
                        {
                            let (wx, wy) = self.webview_px(px, py);
                            self.webview.mouse_button_ui(button, true, wx, wy);
                            self.webview_mouse_down = true;
                            changed = true;
                        }
                    }
                    ElementState::Released => {
                        if std::mem::take(&mut self.webview_mouse_down) {
                            let (wx, wy) = self.webview_px(px, py);
                            self.webview.mouse_button_ui(button, false, wx, wy);
                            changed = true;
                        }
                    }
                }
            }
        }

        if changed {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }

        msg_out
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let mut changed = false;
        let px = pos.x as f32;
        let py = pos.y as f32;

        let separator_x = self.split_geom().1;
        if !self.compose_open {
            if px < separator_x {
                if self.email_list.wheel(delta, px, py) {
                    changed = true;
                }
            }
        }

        // Detail-pane body scroll — or, in the HTML view, the page's own
        // scrolling: the wheel goes to the engine (winit-signed device px,
        // the webview's convention) and WebKit moves the document.
        if !self.compose_open && self.selected_email_id.is_some() {
            if px > separator_x {
                #[cfg(feature = "wpe")]
                let html_wheel = self.html_on_show() && self.in_webview(px, py);
                #[cfg(not(feature = "wpe"))]
                let html_wheel = false;
                if html_wheel {
                    #[cfg(feature = "wpe")]
                    {
                        let s = self.scale_factor;
                        let (dx, dy) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => {
                                ((*x as f64) * 24.0 * s, (*y as f64) * 24.0 * s)
                            }
                            MouseScrollDelta::PixelDelta(p) => (p.x * s, p.y * s),
                        };
                        let (wx, wy) = self.webview_px(px, py);
                        self.webview.wheel(dx, dy, wx, wy);
                    }
                } else {
                    let (_, body_h) = self.detail_body_geom();
                    let max = (self.body_content_h - body_h).max(0.0);
                    // A wheel notch moves the target (tick_body_scroll glides
                    // the offset there); a trackpad finger moves the offset now.
                    self.body_motion.reconcile(0.0, self.body_scroll);
                    let moved = self.body_motion.apply(delta, (LINE_PX, LINE_PX), Bounds::max(0.0), Bounds::max(max));
                    self.body_scroll = self.body_motion.y.pos();
                    if moved {
                        // The scroll raises the bar in the same frame.
                        self.body_sb_activity.bump();
                        self.body_sb_activity.recompute(true, self.body_sb_dragging);
                        changed = true;
                    }
                }
            }
        }

        if changed {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        let mut handled = false;
        let mut msg_out = None;
        let kev = cce_ui::widget::Event::KeyInput(event.clone());
        // Read before ctx: detail_body_geom takes &self whole.
        let (_, body_h) = self.detail_body_geom();

        // A visible context menu takes Escape — it was mouse-dismiss only.
        if event.state == ElementState::Pressed
            && event.logical_key == Key::Named(cce_ui::widget::NamedKey::Escape)
            && cce_ui::widget::context_menu::is_visible()
        {
            cce_ui::widget::context_menu::hide();
            self.context_menu_actions.clear();
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return None;
        }

        // An open bar dropdown takes the keyboard — Escape closes it, arrows
        // move the hover, Enter selects — routed and drained exactly like its
        // mouse events (see handle_mouse_input; the freshly-opened accounts
        // reload stays mouse-only, since a key cannot open a closed
        // dropdown). No key ever reached these before: every propagate below
        // is gated on compose/search state that an open dropdown never
        // satisfies (the cce-files bug).
        if self.ui_context[self.account_dropdown].open {
            if self.ui_context.propagate_event(&kev, self.account_dropdown.id()) {
                if self.ui_context[self.account_dropdown].take_change() {
                    let idx = self.ui_context[self.account_dropdown].selected;
                    if idx < self.accounts.len() {
                        if idx != self.selected_account_idx {
                            msg_out = Some(AppMessage::SelectAccount(idx));
                        }
                    } else {
                        // The trailing "Manage Accounts…" pseudo-entry.
                        self.ui_context[self.account_dropdown].selected = self.selected_account_idx;
                        msg_out = Some(AppMessage::ManageAccounts);
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return msg_out;
            }
        } else if self.ui_context[self.folder_dropdown].open {
            if self.ui_context.propagate_event(&kev, self.folder_dropdown.id()) {
                if self.ui_context[self.folder_dropdown].take_change() {
                    if let Some(f) = self.folders.get(self.ui_context[self.folder_dropdown].selected) {
                        msg_out = Some(AppMessage::SwitchFolder(f.tag.clone()));
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return msg_out;
            }
        }

        // Arrow keys move the selection and Delete removes it. Handled up
        // here, ahead of the ui_context borrow and everything routed through
        // it: the list would otherwise eat Up/Down to scroll itself, and
        // hover-scoping the arrows to whichever pane the pointer happens to
        // rest over makes "the arrows do nothing" a matter of where the mouse
        // is. move_selection takes &mut self, so it cannot run while ctx is
        // borrowed anyway.
        if !self.compose_open
            && !self.ui_context[self.search_box].editing
            && event.state == ElementState::Pressed
        {
            // Pane focus first: the chords carry Ctrl, so they cannot be
            // mistaken for the bare letters below, and with two panes next
            // and previous are the same move.
            if cce_ui::widget::match_key_shortcut(event, &self.keys.focus_next_pane)
                || cce_ui::widget::match_key_shortcut(event, &self.keys.focus_prev_pane)
            {
                self.focused_pane = self.focused_pane.other();
                handled = true;
            }
            if !handled {
                match &event.logical_key {
                    Key::Named(cce_ui::widget::NamedKey::ArrowDown) => {
                        msg_out = self.move_selection(1);
                        handled = true;
                    }
                    Key::Named(cce_ui::widget::NamedKey::ArrowUp) => {
                        msg_out = self.move_selection(-1);
                        handled = true;
                    }
                    Key::Named(cce_ui::widget::NamedKey::Delete) => {
                        if self.selected_email_id.is_some() {
                            msg_out = Some(AppMessage::DeleteSelected);
                            handled = true;
                        }
                    }
                    // Vim keys, bare only: a Ctrl or Alt chord on these
                    // letters is somebody else's binding.
                    Key::Character(c) if !event.ctrl && !event.alt => match c.as_str() {
                        "h" => {
                            self.focused_pane = Pane::List;
                            handled = true;
                        }
                        "l" => {
                            self.focused_pane = Pane::Detail;
                            handled = true;
                        }
                        "j" | "k" => {
                            let down = c == "j";
                            match self.focused_pane {
                                Pane::List => {
                                    msg_out = self.move_selection(if down { 1 } else { -1 });
                                }
                                Pane::Detail => self.step_body(down, body_h),
                            }
                            handled = true;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if handled {
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return msg_out;
            }
        }

        // Read before ctx: html_on_show(&self) cannot run while ui_context
        // is mutably borrowed, and nothing below changes what it reads.
        #[cfg(feature = "wpe")]
        let html_on_show = self.html_on_show();

        let ctx = &mut self.ui_context;

        if self.compose_open {
            if ctx[self.compose_to].editing {
                if ctx.propagate_event(&kev, self.compose_to.id()) { handled = true; }
            } else if ctx[self.compose_cc].editing {
                if ctx.propagate_event(&kev, self.compose_cc.id()) { handled = true; }
            } else if ctx[self.compose_bcc].editing {
                if ctx.propagate_event(&kev, self.compose_bcc.id()) { handled = true; }
            } else if ctx[self.compose_subject].editing {
                if ctx.propagate_event(&kev, self.compose_subject.id()) { handled = true; }
            } else if ctx[self.compose_body].editing {
                if ctx.propagate_event(&kev, self.compose_body.id()) { handled = true; }
            }

            // Escape closes compose dialog
            if !handled && event.state == ElementState::Pressed && event.logical_key == Key::Named(cce_ui::widget::NamedKey::Escape) {
                msg_out = Some(AppMessage::ComposeCancel);
                handled = true;
            }
        } else {
            // Escape closes the search band and drops the query with it, so
            // the list is unfiltered again — leaving a hidden filter behind
            // would look like mail had gone missing.
            //
            // This runs BEFORE the box is offered the key: TextBox consumes
            // Escape itself (clearing and unfocusing), which marked the event
            // handled and left the band standing open forever.
            if self.search_open
                && event.state == ElementState::Pressed
                && event.logical_key == Key::Named(cce_ui::widget::NamedKey::Escape)
            {
                ctx.clear_focus();
                ctx[self.search_box].unfocus();
                ctx[self.search_box].text.clear();
                ctx[self.search_box].edit_buffer.clear();
                self.search_open = false;
                msg_out = Some(AppMessage::SearchChanged);
                handled = true;
            }

            // General keyboard shortcuts (input.kdl `cce-mail` domain)
            if !handled && event.state == ElementState::Pressed {
                if cce_ui::widget::match_key_shortcut(event, &self.keys.compose) {
                    msg_out = Some(AppMessage::ComposeNew);
                    handled = true;
                } else if !ctx[self.search_box].editing
                    && cce_ui::widget::match_key_shortcut(event, &self.keys.open_search)
                {
                    // The !editing guard is load-bearing now the chord is a
                    // bare "/": without it, typing a slash into the open box
                    // would re-match here and never reach the text.
                    self.search_open = true;
                    ctx.set_focused_id(self.search_box.id());
                    ctx[self.search_box].focus();
                    handled = true;
                }
            }

            // HTML view: page keys and the copy chord go to the engine
            // (hover-scoped like the text body's keys below). Up/Down stay
            // with the list selection either way.
            #[cfg(feature = "wpe")]
            if !handled
                && (self.detail_hovered || self.focused_pane == Pane::Detail)
                && html_on_show
                && !ctx[self.search_box].editing
                && event.state == ElementState::Pressed
            {
                use cce_ui::widget::NamedKey;
                let forward = match &event.logical_key {
                    Key::Named(
                        NamedKey::PageDown | NamedKey::PageUp | NamedKey::Home | NamedKey::End,
                    ) => true,
                    Key::Character(c) if event.ctrl && c.eq_ignore_ascii_case("c") => {
                        self.webview.copy_selection();
                        handled = true;
                        false
                    }
                    _ => false,
                };
                if forward {
                    self.webview.key_ui(event);
                    handled = true;
                }
            }

            // Detail-pane body scroll: hover-scoped like ScrollRegion's
            // keyboard path, and the focused pane counts as hovered.
            if !handled
                && (self.detail_hovered || self.focused_pane == Pane::Detail)
                && self.selected_email_id.is_some()
                && !ctx[self.search_box].editing
                && event.state == ElementState::Pressed
            {
                let max = (self.body_content_h - body_h).max(0.0);
                let b = Bounds::max(max);
                let s = cce_ui::widget::scroll_motion::scroll_settings();
                // Pages glide from the current TARGET, so a held key
                // accumulates into one motion rather than restarting it.
                self.body_motion.reconcile(0.0, self.body_scroll);
                let moved = match &event.logical_key {
                    // Up/Down belong to the selection; the body still has
                    // Page keys, Home/End, the wheel and its scrollbar.
                    Key::Named(cce_ui::widget::NamedKey::PageDown) => {
                        let t = self.body_motion.y.target() + body_h;
                        self.body_motion.y.scroll_to(t, b, &s)
                    }
                    Key::Named(cce_ui::widget::NamedKey::PageUp) => {
                        let t = self.body_motion.y.target() - body_h;
                        self.body_motion.y.scroll_to(t, b, &s)
                    }
                    Key::Named(cce_ui::widget::NamedKey::Home) => self.body_motion.y.scroll_to(0.0, b, &s),
                    Key::Named(cce_ui::widget::NamedKey::End) => self.body_motion.y.scroll_to(max, b, &s),
                    _ => false,
                };
                self.body_scroll = self.body_motion.y.pos();
                if moved {
                    handled = true;
                }
            }

            // Email/accounts list keyboard scrolling. ScrollRegion scopes itself to
            // hover-or-focus (a row click focuses the region, a press elsewhere
            // unfocuses); the search box owns the keys while editing, and a
            // body-scroll above wins when the detail pane is hovered.
            if !handled && !ctx[self.search_box].editing && self.email_list.keyboard(event) {
                handled = true;
            }

            if !handled && ctx[self.search_box].editing {
                if ctx.propagate_event(&kev, self.search_box.id()) {
                    handled = true;
                    if ctx[self.search_box].take_change() {
                        msg_out = Some(AppMessage::SearchChanged);
                    }
                }
            }

        }

        if handled {
            *needs_rebuild = true;
            self.needs_rebuild = true;
        }

        msg_out
    }
}

fn main() {
    // `cce-mail ctl <command…>` talks to the running window over its
    // control socket and exits; see src/ipc.rs for the command list.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("ctl") {
        std::process::exit(ipc::run_client(&args[1..]));
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();
    let _ = RUNTIME.set(rt.handle().clone());

    cce_ui::engine::run::<ClearEmailApp>();
    ipc::cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailto_bare_address() {
        let m = parse_mailto("mailto:a@example.com").unwrap();
        assert_eq!(m.to, "a@example.com");
        assert_eq!(m.subject, "");
        assert_eq!(m.body, "");
    }

    #[test]
    fn mailto_full_query() {
        // %20 decodes; '+' stays literal (mailto is not form encoding);
        // header names are case-insensitive; cc/bcc keep their own fields.
        let m = parse_mailto(
            "mailto:a@x.org,b@y.org?Subject=Hello%20W%C3%B6rld&body=line1%0Aline2+plus&cc=c@z.org&BCC=d@w.org&to=e@v.org",
        )
        .unwrap();
        assert_eq!(m.to, "a@x.org, b@y.org, e@v.org");
        assert_eq!(m.cc, "c@z.org");
        assert_eq!(m.bcc, "d@w.org");
        assert_eq!(m.subject, "Hello Wörld");
        assert_eq!(m.body, "line1\nline2+plus");
    }

    fn outgoing(to: &str, cc: &str, bcc: &str, attachments: Vec<String>) -> OutgoingMail {
        OutgoingMail {
            to: to.to_string(),
            cc: cc.to_string(),
            bcc: bcc.to_string(),
            subject: "S".to_string(),
            body: "B".to_string(),
            attachments,
        }
    }

    #[test]
    fn outgoing_bcc_delivers_without_a_header() {
        let msg = build_outgoing(
            "me@example.org",
            &outgoing("a@x.org, b@y.org", "c@z.org", "hidden@w.org", vec![]),
        )
        .unwrap();

        // Envelope (actual delivery) covers To + Cc + Bcc…
        let rcpts: Vec<String> = msg.envelope().to().iter().map(|a| a.to_string()).collect();
        assert!(rcpts.contains(&"a@x.org".to_string()));
        assert!(rcpts.contains(&"b@y.org".to_string()));
        assert!(rcpts.contains(&"c@z.org".to_string()));
        assert!(rcpts.contains(&"hidden@w.org".to_string()));

        // …but the transmitted bytes never name the bcc recipient.
        let formatted = String::from_utf8_lossy(&msg.formatted()).into_owned();
        assert!(formatted.contains("To: a@x.org, b@y.org"), "{formatted}");
        assert!(formatted.contains("Cc: c@z.org"), "{formatted}");
        assert!(!formatted.contains("hidden@w.org"), "bcc leaked into headers: {formatted}");
    }

    #[test]
    fn outgoing_rejects_empty_and_bad_addresses() {
        assert!(build_outgoing("me@example.org", &outgoing("", "", "", vec![])).is_err());
        assert!(build_outgoing("me@example.org", &outgoing("not-an-address", "", "", vec![])).is_err());
        // Bcc-only is a legitimate message.
        assert!(build_outgoing("me@example.org", &outgoing("", "", "b@y.org", vec![])).is_ok());
    }

    #[test]
    fn outgoing_attachment_builds_multipart() {
        let dir = std::env::temp_dir().join("cce-mail-attach-test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("note.txt");
        std::fs::write(&p, "hello attachment").unwrap();

        let msg = build_outgoing(
            "me@example.org",
            &outgoing("a@x.org", "", "", vec![p.to_string_lossy().into_owned()]),
        )
        .unwrap();
        let formatted = String::from_utf8_lossy(&msg.formatted()).into_owned();
        assert!(formatted.contains("multipart/mixed"), "{formatted}");
        assert!(formatted.contains("note.txt"), "{formatted}");

        // Unreadable path = build error = the send fails into Drafts rather
        // than silently mailing without the file.
        assert!(build_outgoing(
            "me@example.org",
            &outgoing("a@x.org", "", "", vec!["/nonexistent/gone.pdf".to_string()]),
        )
        .is_err());
        std::fs::remove_file(&p).unwrap();
    }

    #[test]
    fn mailto_rejects_non_mailto_and_keeps_bad_escapes() {
        assert!(parse_mailto("https://example.com").is_none());
        assert!(parse_mailto("a@example.com").is_none());
        // Truncated/invalid %-escapes stay literal rather than erroring.
        let m = parse_mailto("mailto:a@x.org?subject=100%25%2").unwrap();
        assert_eq!(m.subject, "100%%2");
    }

    fn attach_part<'a>(
        ty: &'a str,
        subtype: &'a str,
        ty_params: imap_proto::types::BodyParams<'a>,
        disposition: Option<(&'a str, imap_proto::types::BodyParams<'a>)>,
        encoding: imap_proto::types::ContentEncoding<'a>,
        octets: u32,
    ) -> imap_proto::types::BodyStructure<'a> {
        use imap_proto::types::*;
        BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType { ty, subtype, params: ty_params },
                disposition: disposition
                    .map(|(t, params)| ContentDisposition { ty: t, params }),
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: encoding,
                octets,
            },
            extension: None,
        }
    }

    #[test]
    fn attachment_parts_found_with_paths_and_name_fallbacks() {
        use imap_proto::types::*;
        // multipart/mixed: [text/plain body, pdf w/ disposition filename,
        // image w/ only a Content-Type name, csv w/ disposition but NO name].
        let text = BodyStructure::Text {
            common: BodyContentCommon {
                ty: ContentType { ty: "TEXT", subtype: "PLAIN", params: None },
                disposition: None,
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::SevenBit,
                octets: 10,
            },
            lines: 1,
            extension: None,
        };
        let pdf = attach_part(
            "APPLICATION",
            "PDF",
            None,
            Some(("ATTACHMENT", Some(vec![("FILENAME", "report.pdf")]))),
            ContentEncoding::Base64,
            2048,
        );
        let img = attach_part(
            "IMAGE",
            "PNG",
            Some(vec![("NAME", "shot.png")]),
            None,
            ContentEncoding::Base64,
            4096,
        );
        let csv = attach_part(
            "TEXT",
            "CSV",
            None,
            Some(("ATTACHMENT", None)),
            ContentEncoding::QuotedPrintable,
            100,
        );
        let root = BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType { ty: "MULTIPART", subtype: "MIXED", params: None },
                disposition: None,
                language: None,
                location: None,
            },
            bodies: vec![text, pdf, img, csv],
            extension: None,
        };

        let atts = find_attachment_parts(&root);
        assert_eq!(atts.len(), 3, "body text part must not be listed: {atts:?}");
        assert_eq!(atts[0].name, "report.pdf");
        assert_eq!(atts[0].section, vec![2]);
        assert_eq!(atts[0].mime, "application/pdf");
        assert_eq!(atts[0].encoding, "base64");
        assert_eq!(atts[1].name, "shot.png"); // Content-Type name fallback
        assert_eq!(atts[1].section, vec![3]);
        assert_eq!(atts[2].name, "attachment.csv"); // synthesized
        assert_eq!(atts[2].section, vec![4]);

        // A bare single-part attachment is section 1.
        let solo = attach_part(
            "APPLICATION",
            "ZIP",
            Some(vec![("NAME", "a.zip")]),
            None,
            ContentEncoding::Base64,
            9,
        );
        assert_eq!(find_attachment_parts(&solo)[0].section, vec![1]);
    }

    #[test]
    fn decode_part_bytes_handles_the_transfer_encodings() {
        // "hello attachment" in base64, with the line-wrap noise real
        // servers emit.
        let b64 = b"aGVsbG8g\r\nYXR0YWNobWVudA==";
        assert_eq!(decode_part_bytes("base64", b64), b"hello attachment");
        assert_eq!(
            decode_part_bytes("quoted-printable", b"gr=C3=BC=C3=9Fe"),
            "grüße".as_bytes()
        );
        // Identity encodings pass through untouched.
        assert_eq!(decode_part_bytes("7bit", b"plain"), b"plain");
        assert_eq!(decode_part_bytes("", b"raw"), b"raw");
    }

    fn parse(raw: &str) -> mail_parser::Message<'_> {
        mail_parser::MessageParser::default()
            .parse(raw.as_bytes())
            .expect("fixture parses")
    }

    #[test]
    fn rfc2047_headers_decode() {
        // Subject and From display-name as UTF-8 B encoded-words.
        let raw = "From: =?UTF-8?B?SsO8cmdlbiBNw7xsbGVy?= <juergen@example.de>\r\n\
                   To: me@example.org\r\n\
                   Subject: =?UTF-8?B?UsOpdW5pb24gZ8OpbsOpcmFsZSDigJQgw4RuZGVydW5nZW4=?=\r\n\
                   Date: Wed, 23 Jul 2026 10:50:12 +0200\r\n\
                   \r\n\
                   plain body\r\n";
        let msg = parse(raw);
        assert_eq!(msg.subject(), Some("Réunion générale — Änderungen"));
        assert_eq!(format_from(&msg), "Jürgen Müller <juergen@example.de>");
        assert_eq!(format_date(msg.date()), "Jul 23 10:50");
    }

    #[test]
    fn multipart_prefers_decoded_text_part() {
        // multipart/alternative: quoted-printable text + base64 HTML; the text
        // part must win and arrive transfer-decoded with its charset applied.
        let raw = "From: a@b.c\r\n\
                   Subject: mp\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/alternative; boundary=\"XX\"\r\n\
                   \r\n\
                   --XX\r\n\
                   Content-Type: text/plain; charset=utf-8\r\n\
                   Content-Transfer-Encoding: quoted-printable\r\n\
                   \r\n\
                   Gr=C3=BC=C3=9Fe aus M=C3=BCnchen!\r\n\
                   Caf=C3=A9 =E2=80=94 na=C3=AFve r=C3=A9sum=C3=A9.\r\n\
                   --XX\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   PGh0bWw+PGJvZHk+PHA+SMOpbGxvIDxiPndvcmxkPC9iPiAmYW1wOyBmcmllbmRzPC9wPjxwPlp3w6lpdGUgWmVpbGU8L3A+PC9ib2R5PjwvaHRtbD4=\r\n\
                   --XX--\r\n";
        let body = extract_body(&parse(raw));
        assert!(body.contains("Grüße aus München!"), "qp not decoded: {body:?}");
        assert!(body.contains("Café — naïve résumé."));
        assert!(!body.contains("<b>"), "html part leaked: {body:?}");
    }

    #[test]
    fn html_only_message_converts_to_text() {
        let raw = "From: a@b.c\r\n\
                   Subject: html\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   PGh0bWw+PGJvZHk+PHA+SMOpbGxvIDxiPndvcmxkPC9iPiAmYW1wOyBmcmllbmRzPC9wPjxwPlp3w6lpdGUgWmVpbGU8L3A+PC9ib2R5PjwvaHRtbD4=\r\n";
        let body = extract_body(&parse(raw));
        assert!(body.contains("Héllo"), "not decoded: {body:?}");
        assert!(body.contains("world & friends"), "entities/tags mishandled: {body:?}");
        assert!(!body.contains('<'), "tags leaked: {body:?}");
    }

    #[test]
    fn extract_body_caps_at_1200_chars() {
        let long = "é".repeat(1500);
        let raw = format!(
            "From: a@b.c\r\nSubject: long\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{long}"
        );
        let body = extract_body(&parse(&raw));
        assert_eq!(body.chars().count(), 1203); // 1200 + "..."
        assert!(body.ends_with("..."));
    }

    #[test]
    fn missing_headers_fall_back() {
        let msg = parse("X-Nothing: here\r\n\r\nbody\r\n");
        assert_eq!(format_from(&msg), "Unknown");
        assert_eq!(format_date(msg.date()), "Unknown");
    }

    use imap_proto::types::{
        BodyContentCommon, BodyContentSinglePart, BodyStructure, ContentEncoding, ContentType,
    };

    fn text_part<'a>(
        subtype: &'a str,
        enc: ContentEncoding<'a>,
        charset: Option<(&'a str, &'a str)>,
    ) -> BodyStructure<'a> {
        BodyStructure::Text {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: "TEXT",
                    subtype,
                    params: charset.map(|kv| vec![kv]),
                },
                disposition: None,
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: enc,
                octets: 0,
            },
            lines: 0,
            extension: None,
        }
    }

    fn multipart<'a>(subtype: &'a str, bodies: Vec<BodyStructure<'a>>) -> BodyStructure<'a> {
        BodyStructure::Multipart {
            common: BodyContentCommon {
                ty: ContentType {
                    ty: "MULTIPART",
                    subtype,
                    params: None,
                },
                disposition: None,
                language: None,
                location: None,
            },
            bodies,
            extension: None,
        }
    }

    fn basic_part<'a>(ty: &'a str, subtype: &'a str) -> BodyStructure<'a> {
        BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType {
                    ty,
                    subtype,
                    params: None,
                },
                disposition: None,
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: None,
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::Base64,
                octets: 0,
            },
            extension: None,
        }
    }

    #[test]
    fn text_part_found_through_nested_multipart() {
        // multipart/mixed( multipart/alternative( text/plain qp, text/HTML ), application/pdf )
        let bs = multipart(
            "MIXED",
            vec![
                multipart(
                    "ALTERNATIVE",
                    vec![
                        text_part("PLAIN", ContentEncoding::QuotedPrintable, Some(("CHARSET", "UTF-8"))),
                        text_part("HTML", ContentEncoding::Base64, None),
                    ],
                ),
                basic_part("APPLICATION", "PDF"),
            ],
        );
        let spec = find_text_part(&bs).expect("finds the plain part");
        assert_eq!(spec.path, vec![1, 1]);
        assert_eq!(section_str(&spec.path), "1.1");
        assert_eq!(spec.subtype, "plain");
        assert_eq!(spec.encoding, "quoted-printable");
        assert_eq!(spec.charset.as_deref(), Some("UTF-8"));
    }

    #[test]
    fn html_only_and_toplevel_paths() {
        // top-level (non-multipart) text/html → section 1, html rank
        let spec = find_text_part(&text_part("HTML", ContentEncoding::Base64, None)).unwrap();
        assert_eq!(spec.path, vec![1]);
        assert_eq!(spec.subtype, "html");
        // attachments only → None (caller falls back to a capped full fetch)
        let bs = multipart("MIXED", vec![basic_part("APPLICATION", "OCTET-STREAM")]);
        assert!(find_text_part(&bs).is_none());
    }

    #[cfg(feature = "wpe")]
    #[test]
    fn html_part_walk_prefers_html_over_plain() {
        // The same alternative the sync reads plain out of: the HTML view
        // must land on the other branch.
        let bs = multipart(
            "MIXED",
            vec![
                multipart(
                    "ALTERNATIVE",
                    vec![
                        text_part("PLAIN", ContentEncoding::QuotedPrintable, Some(("CHARSET", "UTF-8"))),
                        text_part("HTML", ContentEncoding::Base64, Some(("CHARSET", "ISO-8859-1"))),
                    ],
                ),
                basic_part("APPLICATION", "PDF"),
            ],
        );
        let spec = find_html_part(&bs).expect("finds the html part");
        assert_eq!(section_str(&spec.path), "1.2");
        assert_eq!(spec.subtype, "html");
        assert_eq!(spec.encoding, "base64");
        assert_eq!(spec.charset.as_deref(), Some("ISO-8859-1"));
    }

    #[cfg(feature = "wpe")]
    fn cid_part<'a>(ty: &'a str, subtype: &'a str, cid: &'a str) -> BodyStructure<'a> {
        BodyStructure::Basic {
            common: BodyContentCommon {
                ty: ContentType { ty, subtype, params: None },
                disposition: None,
                language: None,
                location: None,
            },
            other: BodyContentSinglePart {
                id: Some(cid.into()),
                md5: None,
                description: None,
                transfer_encoding: ContentEncoding::Base64,
                octets: 0,
            },
            extension: None,
        }
    }

    #[cfg(feature = "wpe")]
    #[test]
    fn inline_part_walk_finds_content_ids() {
        // multipart/related( text/HTML, image/PNG cid ) inside mixed with a
        // plain attachment — the classic inline-logo shape.
        let bs = multipart(
            "MIXED",
            vec![
                multipart(
                    "RELATED",
                    vec![
                        text_part("HTML", ContentEncoding::QuotedPrintable, None),
                        cid_part("IMAGE", "PNG", "<logo@corp>"),
                    ],
                ),
                basic_part("APPLICATION", "PDF"),
            ],
        );
        let parts = find_inline_parts(&bs);
        assert_eq!(
            parts,
            vec![InlinePartSpec {
                cid: "logo@corp".to_string(), // brackets stripped
                section: vec![1, 2],
                encoding: "base64".to_string(),
                mime: "image/png".to_string(),
            }]
        );
        // A structure with no Content-IDs yields nothing — basic_part and
        // text_part both carry id: None.
        assert!(find_inline_parts(&multipart(
            "MIXED",
            vec![text_part("PLAIN", ContentEncoding::SevenBit, None)]
        ))
        .is_empty());
    }

    #[cfg(feature = "wpe")]
    #[test]
    fn html_part_walk_toplevel_and_absent() {
        // Non-multipart text/html is section 1 (RFC 3501).
        let spec = find_html_part(&text_part("HTML", ContentEncoding::SevenBit, None)).unwrap();
        assert_eq!(spec.path, vec![1]);
        // Plain-only mail has no HTML view: the pane stays on text.
        let bs = multipart(
            "MIXED",
            vec![text_part("PLAIN", ContentEncoding::SevenBit, None)],
        );
        assert!(find_html_part(&bs).is_none());
    }

    #[test]
    fn synthesized_part_decodes_via_mail_parser() {
        let spec = TextPartSpec {
            path: vec![1, 1],
            subtype: "plain".to_string(),
            charset: Some("utf-8".to_string()),
            encoding: "quoted-printable".to_string(),
        };
        // "Grüße aus München!" as quoted-printable part bytes
        let raw = spec.synthesize(b"Gr=C3=BC=C3=9Fe aus M=C3=BCnchen!");
        let msg = mail_parser::MessageParser::default()
            .parse(&raw[..])
            .expect("synthetic message parses");
        assert_eq!(extract_body(&msg), "Grüße aus München!");
    }

    // ---- incremental sync merge -------------------------------------------

    fn msg(id: usize, uid: Option<u32>, folder: &str, subject: &str, read: bool) -> Email {
        Email {
            id,
            uid,
            from: "someone@example.org".to_string(),
            to: "me@example.org".to_string(),
            subject: subject.to_string(),
            body: String::new(),
            date: "today".to_string(),
            read,
            folder: folder.to_string(),
            cc: String::new(),
            bcc: String::new(),
            attachments: Vec::new(),
            remote_attachments: Vec::new(),
            ts: None,
            origin_folder: None,
        }
    }

    fn folder_sync(folder: &str, fetched: Vec<Email>, server: Vec<u32>) -> FolderSync {
        FolderSync {
            folder: folder.to_string(),
            fetched,
            server_uids: server,
            seen_uids: Vec::new(),
            remaining: 0,
        }
    }

    #[test]
    fn merge_keeps_history_and_adds_the_new() {
        // The whole point: a later pass must not discard what an earlier one
        // downloaded. This is what pinned the cache at FETCH_COUNT before.
        let prior = vec![msg(1, Some(100), "inbox", "old", true)];
        let f = folder_sync(
            "inbox",
            vec![msg(0, Some(101), "inbox", "new", false)],
            vec![100, 101],
        );
        let out = merge_sync(prior, &[f]);
        assert_eq!(out.len(), 2, "prior mail must survive the merge");
        assert!(out.iter().any(|e| e.subject == "old"));
        assert!(out.iter().any(|e| e.subject == "new"));
    }

    #[test]
    fn merge_assigns_fresh_ids_without_collision() {
        let prior = vec![msg(7, Some(100), "inbox", "old", true)];
        let f = folder_sync(
            "inbox",
            vec![
                msg(0, Some(101), "inbox", "a", false),
                msg(0, Some(102), "inbox", "b", false),
            ],
            vec![100, 101, 102],
        );
        let out = merge_sync(prior, &[f]);
        let mut ids: Vec<usize> = out.iter().map(|e| e.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), out.len(), "ids must stay unique across the cache");
        assert!(ids.iter().all(|&i| i != 0), "every message gets a real id");
    }

    #[test]
    fn merge_keeps_mail_that_postdates_a_stale_pass() {
        // The "press Sync twice" bug. Two passes overlap: the quick one
        // fetches uid 102 that has just arrived, the slow one searched before
        // it existed and reports the mailbox as {100, 101}. Finishing last
        // used to make the straggler authoritative, and 102 was deleted again
        // — fetched, merged, saved, gone, until another Sync Now went out.
        let prior = vec![
            msg(1, Some(100), "inbox", "old", true),
            msg(2, Some(102), "inbox", "just arrived", false),
        ];
        let stale = folder_sync("inbox", vec![], vec![100, 101]);
        let out = merge_sync(prior, &[stale]);
        assert!(
            out.iter().any(|e| e.subject == "just arrived"),
            "a pass cannot delete mail newer than anything its search saw"
        );
        assert!(out.iter().any(|e| e.subject == "old"));
    }

    #[test]
    fn merge_still_drops_a_gap_below_what_the_pass_saw() {
        // The guard above is a ceiling, not an amnesty: a uid the pass could
        // have seen and did not is genuinely gone, and must still go.
        let prior = vec![
            msg(1, Some(100), "inbox", "gone", true),
            msg(2, Some(103), "inbox", "kept", true),
        ];
        let f = folder_sync("inbox", vec![], vec![101, 103]);
        let out = merge_sync(prior, &[f]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subject, "kept");
    }

    #[test]
    fn merge_drops_mail_deleted_on_the_server() {
        let prior = vec![
            msg(1, Some(100), "inbox", "gone", true),
            msg(2, Some(101), "inbox", "kept", true),
        ];
        let f = folder_sync("inbox", vec![], vec![101]);
        let out = merge_sync(prior, &[f]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].subject, "kept");
    }

    #[test]
    fn merge_leaves_other_folders_alone() {
        // An inbox pass says nothing about drafts, trash, or a sent mailbox
        // that could not be selected.
        let prior = vec![
            msg(1, None, "drafts", "draft", true),
            msg(2, Some(50), "trash", "trashed", true),
            msg(3, Some(60), "sent", "sent mail", true),
        ];
        let f = folder_sync("inbox", vec![], vec![]);
        let out = merge_sync(prior, &[f]);
        assert_eq!(out.len(), 3, "only the synced folder is reconciled");
    }

    #[test]
    fn merge_keeps_local_read_over_server_unread() {
        // The seen-push is best-effort; a message the user opened here must
        // not flip back to unread because the server has not caught up.
        let prior = vec![msg(1, Some(100), "inbox", "read here", true)];
        let f = folder_sync(
            "inbox",
            vec![msg(0, Some(100), "inbox", "read here", false)],
            vec![100],
        );
        let out = merge_sync(prior, &[f]);
        assert_eq!(out.len(), 1, "a refetched message is updated, not duplicated");
        assert!(out[0].read, "local read state wins");
        assert_eq!(out[0].id, 1, "identity survives a refetch");
    }

    #[test]
    fn merge_adopts_server_seen_for_cached_mail() {
        let prior = vec![msg(1, Some(100), "inbox", "read elsewhere", false)];
        let mut f = folder_sync("inbox", vec![], vec![100]);
        f.seen_uids = vec![100];
        let out = merge_sync(prior, &[f]);
        assert!(out[0].read, "read on another client shows as read here");
    }

    #[test]
    fn merge_drops_the_local_sent_copy_once_the_server_has_it() {
        let prior = vec![msg(1, None, "sent", "hello", true)];
        let f = folder_sync(
            "sent",
            vec![msg(0, Some(9), "sent", "hello", true)],
            vec![9],
        );
        let out = merge_sync(prior, &[f]);
        assert_eq!(out.len(), 1, "the same message must not show twice");
        assert_eq!(out[0].uid, Some(9));
    }

    #[test]
    fn imap_utf7_decodes_the_literal_ampersand() {
        // Real folder names from the author's account.
        assert_eq!(decode_imap_utf7("Events &- Tickets"), "Events & Tickets");
        assert_eq!(decode_imap_utf7("Orders &- Payments"), "Orders & Payments");
    }

    #[test]
    fn imap_utf7_decodes_non_ascii_runs() {
        // "&<base64>-" with ',' standing in for '/'.
        assert_eq!(decode_imap_utf7("&AOk-t&AOk-"), "été");
        assert_eq!(decode_imap_utf7("Re&AOc-us"), "Reçus");
        assert_eq!(decode_imap_utf7("plain"), "plain");
    }

    #[test]
    fn imap_utf7_keeps_a_run_it_cannot_read() {
        // Better a visibly odd name than a silently dropped one.
        assert_eq!(decode_imap_utf7("bad&!!!-run"), "bad&!!!run");
    }

    #[test]
    fn folder_label_drops_the_provider_namespace() {
        assert_eq!(folder_label("[Gmail]/Spam"), "Spam");
        assert_eq!(folder_label("Finance"), "Finance");
        assert_eq!(folder_label("Legal &- Insurence"), "Legal & Insurence");
    }

    #[test]
    fn known_uids_counts_trash_under_where_it_came_from() {
        // Otherwise the next pass calls a deleted message new and downloads
        // it back, leaving a copy in Trash and a copy in the folder.
        let mut trashed = msg(1, Some(77), "trash", "deleted from All Mail", true);
        trashed.origin_folder = Some("archive".to_string());
        let live = msg(2, Some(78), "archive", "still there", true);
        // As after discovery: the archive slot has been given its mailbox.
        let mut folders = default_folders();
        folders.iter_mut().find(|f| f.tag == TAG_ARCHIVE).unwrap().mailbox = "Archive".into();
        let known = known_uids(&[trashed, live], &folders);
        let archive = known.get("archive").expect("archive tracked");
        assert!(archive.contains(&77), "a trashed message stays known");
        assert!(archive.contains(&78));
    }

    #[test]
    fn known_uids_ignores_local_only_mail() {
        let draft = msg(1, None, "drafts", "draft", true);
        let local_sent = msg(2, None, "sent", "unsent copy", true);
        assert!(known_uids(&[draft, local_sent], &default_folders()).is_empty());
    }

    #[test]
    fn default_folders_lead_the_switcher() {
        // The switcher builds its labels and reads its index from this one
        // list, so order and identity have to hold.
        let f = default_folders();
        assert_eq!(f.len(), 5);
        assert_eq!(f[0].tag, TAG_INBOX);
        assert_eq!(f[0].label, "Inbox");
        assert!(f[0].is_server_backed(), "INBOX is the one guaranteed mailbox");
        // Archive and sent wait for discovery to name their mailbox: the old
        // [Gmail]/… guesses here failed SELECT on every non-Gmail account.
        for tag in [TAG_ARCHIVE, TAG_SENT, TAG_DRAFTS, TAG_TRASH] {
            let local = f.iter().find(|f| f.tag == tag).expect("folder present");
            assert!(!local.is_server_backed(), "{} must start local", tag);
        }
    }

    #[test]
    fn resolve_folders_takes_special_use_over_guesses() {
        // iCloud-shaped: \Archive (not Gmail's \All), sent as "Sent Messages".
        let f = resolve_folders(vec![
            ("INBOX".into(), None),
            ("Archive".into(), Some("archive")),
            ("Sent Messages".into(), Some("sent")),
            ("Deleted Messages".into(), Some("trash")),
            ("Notes".into(), None),
        ]);
        let archive = f.iter().find(|f| f.tag == TAG_ARCHIVE).unwrap();
        assert_eq!(archive.mailbox, "Archive");
        assert_eq!(archive.label, "Archive");
        let sent = f.iter().find(|f| f.tag == TAG_SENT).unwrap();
        assert_eq!(sent.mailbox, "Sent Messages");
        assert_eq!(sent.label, "Sent");
        assert!(f.iter().any(|f| f.tag == "Notes"));
        assert!(!f.iter().any(|f| f.mailbox == "Deleted Messages"));

        // Gmail-shaped: \All names the archive, and its label comes along.
        let f = resolve_folders(vec![
            ("INBOX".into(), None),
            ("[Gmail]/All Mail".into(), Some("all")),
            ("[Gmail]/Sent Mail".into(), Some("sent")),
        ]);
        let archive = f.iter().find(|f| f.tag == TAG_ARCHIVE).unwrap();
        assert_eq!(archive.mailbox, "[Gmail]/All Mail");
        assert_eq!(archive.label, "All Mail");

        // \All wins over \Archive in either LIST order.
        for entries in [
            vec![("Everything".into(), Some("all")), ("Archive".into(), Some("archive"))],
            vec![("Archive".into(), Some("archive")), ("Everything".into(), Some("all"))],
        ] {
            let f = resolve_folders(entries);
            let archive = f.iter().find(|f| f.tag == TAG_ARCHIVE).unwrap();
            assert_eq!(archive.mailbox, "Everything");
        }
    }

    #[test]
    fn resolve_folders_without_special_use_stays_local() {
        // A server that flags nothing gets no guessed mailbox: its folders
        // appear as themselves and archive/sent simply do not sync, rather
        // than SELECTing a [Gmail] name that does not exist.
        let f = resolve_folders(vec![("INBOX".into(), None), ("Sent".into(), None)]);
        assert!(!f.iter().find(|f| f.tag == TAG_ARCHIVE).unwrap().is_server_backed());
        assert!(!f.iter().find(|f| f.tag == TAG_SENT).unwrap().is_server_backed());
        assert!(f.iter().any(|f| f.tag == "Sent" && f.mailbox == "Sent"));
    }

    #[test]
    fn resolve_folders_adopts_an_archive_by_name_when_nothing_is_flagged() {
        // iCloud lists a plain-CREATEd "Archive" with no \Archive flag; the
        // slot adopts it by name rather than showing two Archive rows.
        let f = resolve_folders(vec![("INBOX".into(), None), ("Archive".into(), None)]);
        let archive = f.iter().find(|f| f.tag == TAG_ARCHIVE).unwrap();
        assert_eq!(archive.mailbox, "Archive");
        assert_eq!(f.iter().filter(|f| f.label == "Archive").count(), 1);

        // A flagged folder still wins over the name.
        let f = resolve_folders(vec![
            ("Archive".into(), None),
            ("Everything".into(), Some("all")),
        ]);
        let archive = f.iter().find(|f| f.tag == TAG_ARCHIVE).unwrap();
        assert_eq!(archive.mailbox, "Everything");
        assert!(f.iter().any(|f| f.tag == "Archive"), "the named folder stays custom");
    }

    #[test]
    fn discovered_folders_resolve_to_their_own_mailbox() {
        // A custom folder's uid only means anything in its own mailbox, so
        // the tag must never fall back to INBOX the way the old fixed map did.
        let folders = vec![
            FolderInfo { tag: TAG_INBOX.into(), label: "Inbox".into(), mailbox: "INBOX".into() },
            FolderInfo { tag: "Finance".into(), label: "Finance".into(), mailbox: "Finance".into() },
        ];
        let lookup = |tag: &str| {
            folders
                .iter()
                .find(|f| f.tag == tag)
                .filter(|f| f.is_server_backed())
                .map(|f| f.mailbox.clone())
        };
        assert_eq!(lookup("Finance").as_deref(), Some("Finance"));
        assert_eq!(lookup(TAG_INBOX).as_deref(), Some("INBOX"));
        assert_eq!(lookup("drafts"), None);
    }

    #[test]
    fn merge_rekeys_ids_that_collide() {
        // Caches written by the pre-uid scheme really do contain these: a
        // trashed message keeps the id a later arrival is handed again.
        let prior = vec![
            msg(5, Some(200), "inbox", "arrived later", true),
            msg(5, Some(90), "trash", "trashed earlier", true),
        ];
        let out = merge_sync(prior, &[]);
        assert_eq!(out.len(), 2);
        assert_ne!(out[0].id, out[1].id, "colliding ids must be re-keyed");
        assert_eq!(out[0].id, 5, "the first in display order keeps its id");
    }

    #[test]
    fn merge_orders_by_date_not_uid() {
        // A Gmail label's uids track when mail was LABELLED, so ordering by
        // uid can stand a folder on its head — this is the case that showed
        // Finance oldest-first.
        let mut old_mail = msg(1, Some(900), "Finance", "labelled last", true);
        old_mail.ts = Some(1_000);
        let mut new_mail = msg(2, Some(100), "Finance", "arrived last", true);
        new_mail.ts = Some(9_000);
        let out = merge_sync(vec![old_mail, new_mail], &[]);
        assert_eq!(out[0].subject, "arrived last", "newest date wins over highest uid");
    }

    #[test]
    fn merge_puts_undated_rows_behind_dated_ones() {
        // Rows cached before timestamps existed still have to land somewhere
        // predictable rather than jumping to the top.
        let mut dated = msg(1, Some(10), "inbox", "dated", true);
        dated.ts = Some(5_000);
        let undated = msg(2, Some(999), "inbox", "legacy", true);
        let out = merge_sync(vec![undated, dated], &[]);
        assert_eq!(out[0].subject, "dated");
        assert_eq!(out[1].subject, "legacy");
    }

    #[test]
    fn merge_orders_newest_first_with_local_mail_on_top() {
        let prior = vec![
            msg(1, Some(10), "inbox", "older", true),
            msg(2, Some(30), "inbox", "newer", true),
            msg(3, None, "drafts", "just written", true),
        ];
        let out = merge_sync(prior, &[]);
        assert_eq!(out[0].subject, "just written");
        assert_eq!(out[1].subject, "newer");
        assert_eq!(out[2].subject, "older");
    }

    #[test]
    fn a_kept_session_goes_stale_by_wall_clock_and_token() {
        use std::time::{Duration, SystemTime};
        let now = unix_now();
        let just = SystemTime::now();
        assert!(idle_session_fresh(just, None, now));
        assert!(!idle_session_fresh(just - SESSION_IDLE_MAX - Duration::from_secs(1), None, now));
        // A wall clock stepped backwards since the release: not trusted.
        assert!(!idle_session_fresh(just + Duration::from_secs(3600), None, now));
        // Logged in under an OAuth token about to lapse.
        assert!(!idle_session_fresh(just, Some(now + 30), now));
        assert!(idle_session_fresh(just, Some(now + 3600), now));
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn mail(id: usize, folder: &str, body: &str) -> Email {
        serde_json::from_value(serde_json::json!({
            "id": id, "from": format!("sender{id}@example.org"), "to": "me@example.org",
            "subject": format!("Subject {id}"), "body": body, "date": "", "read": false,
            "folder": folder,
        }))
        .unwrap()
    }

    /// The filter answer is reused only while revision, folder and search
    /// are all unchanged, and recomputed when any one moves.
    #[test]
    fn filter_cache_tracks_rev_folder_and_search() {
        let cache = std::cell::RefCell::new(None);
        let mut emails = vec![mail(1, "inbox", "alpha"), mail(2, "inbox", "beta"), mail(3, "trash", "alpha")];
        let ids = |v: Vec<&Email>| v.iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids(filtered_from(&emails, 0, "inbox", "", &cache)), [1, 2]);
        assert_eq!(ids(filtered_from(&emails, 0, "inbox", "ALPHA", &cache)), [1]);
        assert_eq!(ids(filtered_from(&emails, 0, "trash", "alpha", &cache)), [3]);
        // An in-place move to trash is only seen with the revision bumped.
        emails[0].folder = "trash".into();
        assert_eq!(ids(filtered_from(&emails, 1, "trash", "alpha", &cache)), [1, 3]);
    }

    /// A save is readable before it reaches disk, lands there whole, and
    /// `flush` waits for it.
    #[test]
    fn store_saves_off_thread_and_flushes() {
        let dir = std::env::temp_dir().join(format!("cce-mail-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("emails_x.json");
        email_store::save(path.clone(), vec![mail(7, "inbox", "hello")]);
        assert_eq!(email_store::pending(&path).map(|v| v.len()).unwrap_or(1), 1);
        email_store::flush();
        assert!(email_store::pending(&path).is_none());
        let back: Vec<Email> = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back[0].id, 7);
        assert!(!dir.join("emails_x.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not a correctness check: what the per-frame filter and the per-change
    /// save cost on a cache the size of the live Gmail one (~10k messages).
    /// `cargo test --release timings -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn timings() {
        let body = "lorem ipsum dolor sit amet ".repeat(48);
        let emails: Vec<Email> = (0..10_000).map(|i| mail(i, if i % 3 == 0 { "trash" } else { "inbox" }, &body)).collect();
        let t = std::time::Instant::now();
        let old: usize = (0..4)
            .map(|_| {
                emails.iter().filter(|e| e.folder == "inbox")
                    .filter(|e| e.from.to_lowercase().contains("zz") || e.subject.to_lowercase().contains("zz") || e.body.to_lowercase().contains("zz"))
                    .count()
            })
            .sum();
        let old_filter = t.elapsed();
        let cache = std::cell::RefCell::new(None);
        filtered_from(&emails, 0, "inbox", "zz", &cache);
        let t = std::time::Instant::now();
        let new: usize = (0..4).map(|_| filtered_from(&emails, 0, "inbox", "zz", &cache).len()).sum();
        let new_filter = t.elapsed();
        assert_eq!(old, new);
        let t = std::time::Instant::now();
        let pretty = serde_json::to_string_pretty(&emails).unwrap();
        let old_save = t.elapsed();
        let tmp = std::env::temp_dir().join(format!("cce-mail-timing-{}.json", std::process::id()));
        let t = std::time::Instant::now();
        std::fs::write(&tmp, &pretty).unwrap();
        let old_write = t.elapsed();
        let _ = std::fs::remove_file(&tmp);
        let t = std::time::Instant::now();
        let parsed: Vec<Email> = serde_json::from_str(&pretty).unwrap();
        let old_bg_load = t.elapsed();
        drop(parsed);
        println!("old UI-thread write: {old_write:?}; a background account's load+parse (also on the UI thread before): {old_bg_load:?}");
        let t = std::time::Instant::now();
        let copy = emails.clone();
        let handoff = t.elapsed();
        drop(copy);
        println!("4 filters/frame with a query: {old_filter:?} -> {new_filter:?} (cached)");
        println!("save on the UI thread: {old_save:?} pretty serialize ({} MB, before the write) -> {handoff:?} clone handed to the writer", pretty.len() / 1_000_000);
    }
}
