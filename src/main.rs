mod scroll_region;
use scroll_region::ScrollRegion;
use wayland_client::QueueHandle;
use cce_ui::cosmic_text::FontSystem;
use cce_ui::engine::{Application, CursorIcon, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::widget::{
    MouseButton, ElementState, MouseScrollDelta, KeyEvent, WidgetHost,
    TextBox, Button, TextLabel, Key, Dropdown
};
use cce_ui::context::UiContext;
use native_tls::TlsConnector;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Folder {
    Inbox,
    Sent,
    Drafts,
    Trash,
}

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
}

#[derive(Debug, Clone)]
enum AppMessage {
    SwitchFolder(Folder),
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
    Quit,
    Status(String),
    /// A failure a user must not miss: shown as a sticky red toast (no
    /// timer) where [`AppMessage::Status`] is a green 4-second one.
    StatusError(String),
    EmailsSynced(String, Vec<FolderSync>),
    UpdateAccountTokens(String, Option<String>, Option<u64>),
}

/// The single status slot at the bottom of the window. Info toasts count
/// down in `tick` and expire; errors carry no timer — they stay until
/// clicked away or replaced, so a failed sync can't vanish unseen.
#[derive(Debug, Clone)]
enum StatusToast {
    Info { text: String, ttl: f32 },
    Error { text: String },
}

impl StatusToast {
    fn info(text: impl Into<String>, ttl: f32) -> Self {
        StatusToast::Info { text: text.into(), ttl }
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
}

impl EmailKeys {
    fn load() -> Self {
        Self {
            compose: cce_ui::input::app_chord("compose", "ctrl+n"),
            open_search: cce_ui::input::app_chord("open_search", "/"),
        }
    }
}

struct ClearEmailApp {
    keys: EmailKeys,

    // Navigation / Sidebar — the whole bar is Dropdowns (MenuBar retired):
    // two menu-button dropdowns (custom_display_text = fixed trigger label,
    // rows are commands that re-fire on repeat) and two selection dropdowns.
    mail_menu: cce_ui::widget::Adapted<Dropdown>,
    /// Folder switcher: options[0] carries the live inbox unread count
    /// ("Inbox (6)"), refreshed each rebuild, so rows and trigger agree.
    folder_dropdown: cce_ui::widget::Adapted<Dropdown>,
    /// Account switcher beside the folder dropdown: options are the account
    /// emails plus a trailing "Manage Accounts…" pseudo-entry (management
    /// lives in cce-system-interface). Options refresh from accounts.json on
    /// every open — the job the retired Accounts page did on entry.
    account_dropdown: cce_ui::widget::Adapted<Dropdown>,

    // Search and List View
    search_box: cce_ui::widget::Adapted<TextBox>,
    email_list: ScrollRegion,
    email_buttons: Vec<cce_ui::widget::Adapted<cce_ui::widget::Button>>,

    // Details View (Reply/Delete/Mark Read/Unread live in the Message menu)
    detail_body: cce_ui::widget::Adapted<TextBox>,

    // Compose Dialog
    compose_to: cce_ui::widget::Adapted<TextBox>,
    compose_cc: cce_ui::widget::Adapted<TextBox>,
    compose_bcc: cce_ui::widget::Adapted<TextBox>,
    compose_subject: cce_ui::widget::Adapted<TextBox>,
    compose_body: cce_ui::widget::Adapted<TextBox>,
    /// Absolute paths queued for the next send; drawn as removable chips.
    compose_attachments: Vec<String>,
    btn_compose_send: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_compose_cancel: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_compose_attach: cce_ui::widget::Adapted<cce_ui::widget::Button>,

    // Accounts (switch via the bar dropdown — management lives in cce-system-interface)
    accounts: Vec<AccountInfo>,
    selected_account_idx: usize,

    // Application state
    emails: Vec<Email>,
    current_folder: Folder,
    selected_email_id: Option<usize>,
    /// Detail-pane body scroll offset (logical px) and the measured height of
    /// the wrapped body text, refreshed each display_list; hover scopes the
    /// wheel/keyboard scrolling to the pane. The sb_* pair is the scrollbar
    /// thumb drag (grab offset within the thumb), mirroring ScrollRegion.
    body_scroll: f32,
    body_content_h: f32,
    detail_hovered: bool,
    body_sb_dragging: bool,
    body_sb_drag_offset: f32,
    /// Width of the email-list band (the rows), user-draggable via the
    /// list/detail separator. The stored preference survives narrow windows
    /// un-clobbered — [`Self::split_geom`] clamps at use, not here.
    list_w: f32,
    split_dragging: bool,
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

fn get_accounts_path() -> std::path::PathBuf {
    let p = cce_ui::config::cce_config_dir();
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&p) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(&p, perms);
            }
        }
    }
    p.join("accounts.json")
}

/// The bar account-switcher's option list: one row per account email, plus the
/// trailing management pseudo-entry (index == accounts.len()), which opens
/// cce-system-interface instead of switching.
const MANAGE_ACCOUNTS_OPTION: &str = "Manage Accounts…";

fn account_dropdown_options(accounts: &[AccountInfo]) -> Vec<String> {
    accounts
        .iter()
        .map(|a| a.email.clone())
        .chain(std::iter::once(MANAGE_ACCOUNTS_OPTION.to_string()))
        .collect()
}

fn load_accounts() -> Vec<AccountInfo> {
    let path = get_accounts_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut accounts) = serde_json::from_str::<Vec<AccountInfo>>(&content) {
                if resolve_account_secrets(&mut accounts) {
                    // A plaintext password just moved into the keyring —
                    // rewrite the file now so it stops living on disk.
                    save_accounts(&accounts);
                }
                return accounts;
            }
        }
    }
    vec![
        AccountInfo {
            email: "lsgalante@cce-ui.org".to_string(),
            imap: "imap.cce-ui.org:993".to_string(),
            smtp: "smtp.cce-ui.org:465".to_string(),
            is_default: true,
            password: "mock_password".to_string(),
            is_oauth: false,
            access_token: None,
            refresh_token: None,
            token_expiry: None,
            client_id: None,
            client_secret: None,
            keyring_backed: false,
        },
    ]
}

/// Secret Service entries are keyed by (service, address). The app was renamed
/// from `cce-email`, so entries created before the rename still live under the
/// old service name; `resolve_account_secrets` adopts them on first run.
/// cce-system-interface writes the same pair when it saves an account.
const KEYRING_SERVICE: &str = "cce-mail";
const KEYRING_SERVICE_LEGACY: &str = "cce-email";

/// Resolve account passwords through the Secret Service (KeePassXC here).
/// An empty on-disk password field is filled from the keyring; a plaintext
/// one is migrated INTO the keyring (returns true so the caller rewrites the
/// redacted file). In-memory passwords stay resolved for the IMAP/SMTP
/// workers. The mock account never touches the keyring. If the keyring is
/// locked, the provider pops its unlock dialog and this blocks until the
/// user answers — the standard desktop flow.
fn resolve_account_secrets(accounts: &mut [AccountInfo]) -> bool {
    let mut migrated = false;
    for acc in accounts.iter_mut() {
        if is_mock_account(acc) {
            continue;
        }
        let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &acc.email) else {
            continue;
        };
        if acc.password.is_empty() {
            if let Ok(p) = entry.get_password() {
                acc.password = p;
                acc.keyring_backed = true;
            } else if let Some(p) = legacy_keyring_password(&acc.email) {
                // Pre-rename entry. Copy it forward FIRST and drop the old one
                // only once the new one holds it: deleting first would destroy
                // the sole copy whenever the write failed, and `keyring_backed`
                // would then blank the on-disk field on top of that. A failed
                // write just leaves the legacy entry for the next start.
                acc.password = p.clone();
                if entry.set_password(&p).is_ok() {
                    acc.keyring_backed = true;
                    if let Ok(legacy) = keyring::Entry::new(KEYRING_SERVICE_LEGACY, &acc.email) {
                        let _ = legacy.delete_credential();
                    }
                }
            }
        } else if entry.set_password(&acc.password).is_ok() {
            acc.keyring_backed = true;
            migrated = true;
        }
    }
    migrated
}

/// This address's password as left by the pre-rename `cce-email` service.
/// None when there is nothing to adopt — including when the keyring is locked,
/// in which case the entry is simply picked up on a later start.
fn legacy_keyring_password(email: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE_LEGACY, email)
        .ok()?
        .get_password()
        .ok()
}

fn save_accounts(accounts: &[AccountInfo]) {
    // Keyring-backed passwords never go back to disk.
    let redacted: Vec<AccountInfo> = accounts
        .iter()
        .map(|a| {
            let mut a = a.clone();
            if a.keyring_backed {
                a.password = String::new();
            }
            a
        })
        .collect();
    let accounts = &redacted;
    let path = get_accounts_path();
    if let Ok(content) = serde_json::to_string_pretty(accounts) {
        let _ = std::fs::write(&path, content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&path) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&path, perms);
            }
        }
    }
}

/// Sidecar remembering which account was last selected, so a restart returns
/// to (and on-start syncs) the account the user actually reads. Deliberately
/// not in accounts.json — that file is owned by cce-system-interface.
fn selected_account_path() -> std::path::PathBuf {
    cce_ui::config::cce_config_dir().join("cce-mail-account.txt")
}

fn load_selected_account_email() -> Option<String> {
    std::fs::read_to_string(selected_account_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn save_selected_account_email(email: &str) {
    let _ = std::fs::write(selected_account_path(), email);
}

/// Sidecar remembering the dragged list/detail split, same pattern as the
/// account sidecar above (config.kdl stays the user's file).
fn split_path() -> std::path::PathBuf {
    cce_ui::config::cce_config_dir().join("cce-mail-split.txt")
}

fn load_list_w() -> Option<f32> {
    std::fs::read_to_string(split_path())
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|w| (LIST_W_MIN..=4000.0).contains(w))
}

fn save_list_w(w: f32) {
    let _ = std::fs::write(split_path(), format!("{:.0}", w));
}

fn get_account_emails_path(email: &str) -> std::path::PathBuf {
    let p = cce_ui::config::cce_config_dir();
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
    }
    let safe_email = email.replace('@', "_").replace('.', "_");
    p.join(format!("emails_{}.json", safe_email))
}

fn load_emails_for_account(email: &str) -> Vec<Email> {
    let path = get_account_emails_path(email);
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(emails) = serde_json::from_str(&content) {
                return emails;
            }
        }
    }
    if email == "lsgalante@cce-ui.org" {
        return get_default_mock_emails();
    }
    Vec::new()
}

fn save_emails_for_account(email: &str, emails: &[Email]) {
    let path = get_account_emails_path(email);
    if let Ok(content) = serde_json::to_string_pretty(emails) {
        let _ = std::fs::write(&path, content);
    }
}

/// Newest-N window fetched per sync. Only headers, structure, and the chosen
/// text part come down the wire, so attachments never inflate a sync.
/// Menubar height; all chrome below the bar offsets by this.
const MENUBAR_H: f32 = 36.0;

// List/detail split geometry. The list band starts at LIST_X; the separator
// line sits LIST_SEP_GAP after the band and the detail pane LIST_DETAIL_GAP
// after the separator. `list_w` (the band width) is the one draggable value —
// everything else derives from it through `split_geom`.
const LIST_X: f32 = 10.0;
const LIST_SEP_GAP: f32 = 10.0;
const LIST_DETAIL_GAP: f32 = 15.0;
const LIST_W_DEFAULT: f32 = 300.0;
const LIST_W_MIN: f32 = 180.0;
/// The detail pane never gets squeezed below this by a drag or a narrow window.
const DETAIL_W_MIN: f32 = 220.0;
/// Half-width of the separator's grab band (±, matching ScrollRegion's slop).
const SPLIT_GRAB_SLOP: f32 = 4.0;

/// How long to wait before re-reading the keyring after a sync found the
/// account had no password. Short enough that unlocking the vault feels
/// self-healing, long enough that a vault left locked costs nothing.
const SECRET_RETRY_SECS: u64 = 5;

/// Row pitch of the toolkit context menu — its own layout constant
/// (`ContextMenuState`), mirrored here because hit-testing is done app-side.
const CONTEXT_ROW_H: f32 = 24.0;

// The search band. It is not permanent chrome: the `open_search` chord
// ("/" by default) reveals and focuses it, and closing hands its strip back
// to the list, so `list_geom` is the single source for where the rows start.
const SEARCH_ROW_Y: f32 = 15.0;
const SEARCH_ROW_H: f32 = 26.0;
const LIST_TOP_WITH_SEARCH: f32 = 55.0;
const LIST_BOTTOM_PAD: f32 = 15.0;

// Detail-pane vertical layout, every offset measured from MENUBAR_H. These
// were literals scattered across paint, layout, the scrollbar geometry and
// three input handlers; the 170/190 pair in particular had to move in
// lockstep or the scrollbar detached from the text it scrolls, so the body
// pair lives behind `detail_body_geom` as one source.
const DETAIL_SUBJECT_Y: f32 = 18.0;
const DETAIL_FROM_Y: f32 = 43.0;
const DETAIL_TO_Y: f32 = 63.0;
const DETAIL_DATE_Y: f32 = 83.0;
const DETAIL_CHIPS_Y: f32 = 98.0;
const DETAIL_BODY_Y: f32 = 128.0;
const DETAIL_BODY_BOTTOM_PAD: f32 = 20.0;

// Compose modal geometry. One source of truth: the background quads, the
// input rects, the labels, the chip row and the outside-click test all
// derive from these — the old duplicated 500.0/420.0 literals meant growing
// the dialog required finding every site by hand.
const COMPOSE_W: f32 = 500.0;
const COMPOSE_H: f32 = 520.0;

fn compose_modal_origin(w: f32, h: f32) -> (f32, f32) {
    (((w - COMPOSE_W) / 2.0).max(0.0), ((h - COMPOSE_H) / 2.0).max(0.0))
}

/// Rects of the attachment chips (one per queued file), in the row between
/// the body and the buttons. Paint and hit-test both call this, so a click
/// lands exactly on what was drawn.
fn compose_chip_rects(attachments: &[String], modal_x: f32, modal_y: f32) -> Vec<(f32, f32, f32, f32)> {
    let mut rects = Vec::with_capacity(attachments.len());
    let mut x = modal_x + 15.0;
    let y = modal_y + 413.0;
    for path in attachments {
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment");
        let shown = ellipsize(name, 22);
        // Estimated glyph advance at font_size 10 — the chip is a painted
        // quad, not a widget, so an estimate only has to be consistent
        // between paint and hit-test (it is: both use this fn).
        let w = shown.chars().count() as f32 * 6.0 + 26.0;
        rects.push((x, y, w, 24.0));
        x += w + 8.0;
    }
    rects
}

/// Chip label for a server attachment: name plus a humanized size.
fn detail_chip_label(att: &RemoteAttachment) -> String {
    let size = if att.size >= 1_048_576 {
        format!("{:.1} MB", att.size as f32 / 1_048_576.0)
    } else if att.size >= 1024 {
        format!("{} KB", att.size / 1024)
    } else {
        format!("{} B", att.size)
    };
    format!("{} \u{00b7} {}", ellipsize(&att.name, 26), size)
}

/// Rects of the detail pane's attachment chips, one per server attachment,
/// in the fixed header band between Date and the body (the body top never
/// moves). Paint and hit-test both call this — the compose-chip convention.
fn detail_chip_rects(atts: &[RemoteAttachment], detail_x: f32) -> Vec<(f32, f32, f32, f32)> {
    let mut rects = Vec::with_capacity(atts.len());
    let mut x = detail_x;
    let y = DETAIL_CHIPS_Y + MENUBAR_H;
    for att in atts {
        let w = detail_chip_label(att).chars().count() as f32 * 6.0 + 16.0;
        rects.push((x, y, w, 22.0));
        x += w + 8.0;
    }
    rects
}

const FETCH_COUNT: usize = 50;

/// Ceiling on how much one sync pass downloads. A mailbox with years of
/// history backfills across passes instead of stalling the first one, and
/// each pass is committed to disk, so progress survives a restart.
const MAX_FETCH_PER_SYNC: usize = 500;

/// How many already-cached messages get their flags refreshed per pass, so
/// mail read on another client stops showing unread here. Newest-first; the
/// whole mailbox would be a needless round trip on every sync.
const FLAG_REFRESH_WINDOW: usize = 200;

/// Gap between backfill passes while history is still coming down. Short
/// enough to feel continuous, long enough not to hammer the server.
const BACKFILL_DELAY_SECS: u64 = 3;

/// Byte cap on a fetched text part (pre-decode); the display model caps at
/// 1200 chars, so 64 KiB of qp/base64 is plenty.
const PART_FETCH_CAP: u32 = 65536;

/// The text part chosen from a BODYSTRUCTURE walk: its IMAP section path plus
/// the metadata needed to rebuild a decodable single-part MIME message.
struct TextPartSpec {
    path: Vec<u32>,
    subtype: String,
    charset: Option<String>,
    encoding: String,
}

/// IMAP dotted section string for a part path ("1.2").
fn section_str(path: &[u32]) -> String {
    path.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

impl TextPartSpec {
    /// Wrap raw (still transfer-encoded) part bytes in a minimal single-part
    /// message so mail-parser handles the decode (CTE + charset + HTML→text).
    fn synthesize(&self, part_bytes: &[u8]) -> Vec<u8> {
        let charset = self.charset.as_deref().unwrap_or("utf-8");
        let mut raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: text/{}; charset=\"{}\"\r\nContent-Transfer-Encoding: {}\r\n\r\n",
            self.subtype, charset, self.encoding
        )
        .into_bytes();
        raw.extend_from_slice(part_bytes);
        raw
    }
}

fn encoding_str(enc: &imap_proto::types::ContentEncoding) -> String {
    use imap_proto::types::ContentEncoding as E;
    match enc {
        E::SevenBit => "7bit".to_string(),
        E::EightBit => "8bit".to_string(),
        E::Binary => "binary".to_string(),
        E::Base64 => "base64".to_string(),
        E::QuotedPrintable => "quoted-printable".to_string(),
        E::Other(s) => s.to_string(),
    }
}

/// DFS over a BODYSTRUCTURE for the best displayable part: the first
/// text/plain anywhere, else the first text/html. Multipart children are
/// numbered 1.. and nest dotted (RFC 3501); a non-multipart top level is
/// section 1. None (e.g. embedded message/rfc822 only) → caller falls back
/// to a capped full-message fetch.
fn find_text_part(bs: &imap_proto::types::BodyStructure<'_>) -> Option<TextPartSpec> {
    use imap_proto::types::BodyStructure as B;
    fn walk(bs: &B<'_>, path: &mut Vec<u32>, best: &mut Option<(u8, TextPartSpec)>) {
        match bs {
            B::Text { common, other, .. } => {
                let sub = common.ty.subtype.to_ascii_lowercase();
                let rank = match sub.as_str() {
                    "plain" => 0u8,
                    "html" => 1u8,
                    _ => return,
                };
                if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                    let charset = common.ty.params.as_ref().and_then(|ps| {
                        ps.iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("charset"))
                            .map(|(_, v)| v.to_string())
                    });
                    *best = Some((
                        rank,
                        TextPartSpec {
                            path: if path.is_empty() { vec![1] } else { path.clone() },
                            subtype: sub,
                            charset,
                            encoding: encoding_str(&other.transfer_encoding),
                        },
                    ));
                }
            }
            B::Multipart { bodies, .. } => {
                for (i, b) in bodies.iter().enumerate() {
                    path.push(i as u32 + 1);
                    walk(b, path, best);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut best = None;
    walk(bs, &mut Vec::new(), &mut best);
    best.map(|(_, spec)| spec)
}

/// One attachment as the server describes it: everything needed to list it
/// in the detail pane and to fetch exactly that part on demand.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct RemoteAttachment {
    name: String,
    /// BODYSTRUCTURE part path (dotted section = section_str(&section)).
    section: Vec<u32>,
    /// Content-Transfer-Encoding, for the decode after the fetch.
    encoding: String,
    mime: String,
    /// Transfer-encoded size in octets, as reported by the server.
    size: u32,
}

/// DFS over a BODYSTRUCTURE for the parts that are attachments: anything
/// with an `attachment` disposition, or any part carrying a filename (many
/// senders attach with only a Content-Type `name` param). The filename falls
/// back through disposition `filename` → type `name` → a synthesized
/// `attachment.<subtype>`. Same path numbering as [`find_text_part`].
fn find_attachment_parts(bs: &imap_proto::types::BodyStructure<'_>) -> Vec<RemoteAttachment> {
    use imap_proto::types::BodyStructure as B;

    fn param<'a>(params: &'a imap_proto::types::BodyParams<'_>, key: &str) -> Option<String> {
        params.as_ref().and_then(|ps| {
            ps.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.to_string())
        })
    }

    fn single(
        common: &imap_proto::types::BodyContentCommon<'_>,
        other: &imap_proto::types::BodyContentSinglePart<'_>,
        path: &[u32],
        out: &mut Vec<RemoteAttachment>,
    ) {
        let disp_attachment = common
            .disposition
            .as_ref()
            .is_some_and(|d| d.ty.eq_ignore_ascii_case("attachment"));
        let filename = common
            .disposition
            .as_ref()
            .and_then(|d| param(&d.params, "filename"))
            .or_else(|| param(&common.ty.params, "name"));
        if !disp_attachment && filename.is_none() {
            return;
        }
        let mime = format!(
            "{}/{}",
            common.ty.ty.to_ascii_lowercase(),
            common.ty.subtype.to_ascii_lowercase()
        );
        out.push(RemoteAttachment {
            name: filename
                .unwrap_or_else(|| format!("attachment.{}", common.ty.subtype.to_ascii_lowercase())),
            section: if path.is_empty() { vec![1] } else { path.to_vec() },
            encoding: encoding_str(&other.transfer_encoding),
            mime,
            size: other.octets,
        });
    }

    fn walk(bs: &B<'_>, path: &mut Vec<u32>, out: &mut Vec<RemoteAttachment>) {
        match bs {
            B::Basic { common, other, .. }
            | B::Text { common, other, .. }
            | B::Message { common, other, .. } => single(common, other, path, out),
            B::Multipart { bodies, .. } => {
                for (i, b) in bodies.iter().enumerate() {
                    path.push(i as u32 + 1);
                    walk(b, path, out);
                    path.pop();
                }
            }
        }
    }

    let mut out = Vec::new();
    walk(bs, &mut Vec::new(), &mut out);
    out
}

/// Decode fetched part bytes per their Content-Transfer-Encoding, by the
/// same trick [`TextPartSpec::synthesize`] uses for bodies: wrap the raw
/// bytes as a minimal single-part message and let mail-parser do the decode
/// (it already handles base64/quoted-printable and their whitespace forms).
/// Content-Type is forced to application/octet-stream so the part parses as
/// an opaque attachment — the real MIME type only matters to the opener.
fn decode_part_bytes(encoding: &str, bytes: &[u8]) -> Vec<u8> {
    match encoding.to_ascii_lowercase().as_str() {
        "7bit" | "8bit" | "binary" | "" => bytes.to_vec(),
        enc => {
            let mut raw = format!(
                "MIME-Version: 1.0\r\nContent-Type: application/octet-stream\r\nContent-Transfer-Encoding: {}\r\nContent-Disposition: attachment\r\n\r\n",
                enc
            )
            .into_bytes();
            raw.extend_from_slice(bytes);
            mail_parser::MessageParser::default()
                .parse(&raw)
                .and_then(|m| m.attachments().next().map(|a| a.contents().to_vec()))
                .unwrap_or_else(|| bytes.to_vec())
        }
    }
}

/// Char-boundary-safe ellipsized truncation. Byte slicing (`&s[..n]`) panics
/// mid-UTF-8, and real-world mail headers/bodies are full of multi-byte chars.
fn ellipsize(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...", &s[..idx]),
        None => s.to_string(),
    }
}

/// Display form of the From address: "Name <addr>", falling back through the
/// parts. mail-parser has already decoded any RFC 2047 encoded-words.
fn format_from(msg: &mail_parser::Message) -> String {
    if let Some(addr) = msg.from().and_then(|a| a.first()) {
        match (addr.name(), addr.address()) {
            (Some(n), Some(a)) => format!("{} <{}>", n, a),
            (None, Some(a)) => a.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => "Unknown".to_string(),
        }
    } else {
        "Unknown".to_string()
    }
}

/// Display form of the To address; sent-folder rows lead with this. Falls
/// back to the account's own address (inbox mail is addressed to us anyway).
fn format_to(msg: &mail_parser::Message, fallback: &str) -> String {
    if let Some(addr) = msg.to().and_then(|a| a.first()) {
        match (addr.name(), addr.address()) {
            (Some(n), Some(a)) => format!("{} <{}>", n, a),
            (None, Some(a)) => a.to_string(),
            (Some(n), None) => n.to_string(),
            (None, None) => fallback.to_string(),
        }
    } else {
        fallback.to_string()
    }
}

/// "Jul 23 10:50" from the parsed Date header (raw header strings previously
/// showed as e.g. "Wed, 23 Jul 2026 10:50:12 +0200 (CEST)").
fn format_date(dt: Option<&mail_parser::DateTime>) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    match dt {
        Some(d) => {
            let month = MONTHS
                .get((d.month as usize).wrapping_sub(1))
                .copied()
                .unwrap_or("?");
            format!("{} {} {:02}:{:02}", month, d.day, d.hour, d.minute)
        }
        None => "Unknown".to_string(),
    }
}

/// Readable body text: the first text part (mail-parser decodes transfer
/// encoding + charset, and converts an HTML-only message to text), trimmed
/// and capped for the 1-message display model.
fn extract_body(msg: &mail_parser::Message) -> String {
    let mut body = msg
        .body_text(0)
        .map(|t| t.to_string())
        .or_else(|| msg.body_html(0).map(|h| strip_html(&h)))
        .unwrap_or_default();
    body = body.trim_end().to_string();
    if let Some((idx, _)) = body.char_indices().nth(1200) {
        body.truncate(idx);
        body.push_str("...");
    }
    body
}

/// Last-resort HTML-to-text: drop tags, decode the common entities, collapse
/// blank-line runs. Only reached if mail-parser yields no text conversion.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut collapsed = String::with_capacity(out.len());
    let mut blank_run = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        collapsed.push_str(line.trim_end());
        collapsed.push('\n');
    }
    collapsed
}

/// What a `mailto:` argv prefills into the compose dialog.
#[derive(Debug, Clone, PartialEq, Default)]
struct MailtoPrefill {
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
}

/// RFC 6068 percent-decoding. Unlike form encoding, `+` is a literal plus in
/// a mailto URL — spaces arrive as `%20` — so only `%XX` sequences decode.
/// Decoding happens on bytes and re-validates as UTF-8, since an encoded
/// subject/body may hold multi-byte sequences split across several `%XX`.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // get() covers a trailing "%" or "%X"; a non-hex pair falls
            // through and the '%' stays literal, matching lenient browsers.
            let hex = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a `mailto:` URL (RFC 6068) into compose-dialog prefills.
///
/// Recipients come from the path *and* any `to` query key; `cc` and `bcc`
/// keep their own header fields — the compose dialog has all three, and bcc
/// folded anywhere visible would defeat what bcc is for. Header names are
/// case-insensitive; unknown ones are ignored per spec.
fn parse_mailto(arg: &str) -> Option<MailtoPrefill> {
    let rest = arg.strip_prefix("mailto:").or_else(|| arg.strip_prefix("MAILTO:"))?;
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    fn addr_list(raw: &str) -> Vec<String> {
        percent_decode(raw)
            .split(',')
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty())
            .collect()
    }
    let mut to = addr_list(path);
    let mut cc = Vec::new();
    let mut bcc = Vec::new();
    let mut prefill = MailtoPrefill::default();
    if let Some(q) = query {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k.to_ascii_lowercase().as_str() {
                "to" => to.extend(addr_list(v)),
                "cc" => cc.extend(addr_list(v)),
                "bcc" => bcc.extend(addr_list(v)),
                "subject" => prefill.subject = percent_decode(v),
                "body" => prefill.body = percent_decode(v),
                _ => {}
            }
        }
    }
    prefill.to = to.join(", ");
    prefill.cc = cc.join(", ");
    prefill.bcc = bcc.join(", ");
    Some(prefill)
}

const GOOGLE_CLIENT_ID: &str = "946029775684-m4u4mme60a6a0qj3p5m5jvea8d2987o9.apps.googleusercontent.com";
const GOOGLE_CLIENT_SECRET: &str = "GOCSPX-dummysecret";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GoogleClientConfig {
    client_id: String,
    client_secret: String,
}

fn load_google_client_config() -> GoogleClientConfig {
    let p = cce_ui::config::cce_config_dir().join("google_client.json");
    if p.exists() {
        if let Ok(content) = std::fs::read_to_string(&p) {
            if let Ok(config) = serde_json::from_str::<GoogleClientConfig>(&content) {
                return config;
            }
        }
    }
    let default_config = GoogleClientConfig {
        client_id: GOOGLE_CLIENT_ID.to_string(),
        client_secret: GOOGLE_CLIENT_SECRET.to_string(),
    };
    if let Ok(content) = serde_json::to_string_pretty(&default_config) {
        let _ = std::fs::write(&p, content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(&p) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&p, perms);
            }
        }
    }
    default_config
}

async fn refresh_access_token(account: &mut AccountInfo) -> Result<String, String> {
    if !account.is_oauth {
        return Err("Not an OAuth account".to_string());
    }
    
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Check if token is still valid (with a 5 minute safety buffer)
    if let (Some(token), Some(expiry)) = (&account.access_token, account.token_expiry) {
        if expiry > now + 300 {
            return Ok(token.clone());
        }
    }

    let refresh_token = match &account.refresh_token {
        Some(t) => t,
        None => return Err("No refresh token".to_string()),
    };

    let client_config = load_google_client_config();
    let client_id = account.client_id.as_deref().unwrap_or(&client_config.client_id);
    let client_secret = account.client_secret.as_deref().unwrap_or(&client_config.client_secret);

    let client = reqwest::Client::new();
    let params = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("refresh_token", refresh_token.as_str()),
        ("grant_type", "refresh_token"),
    ];

    match client.post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await 
    {
        Ok(resp) => {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    let access_token = json.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let expires_in = json.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
                    let expiry = now + expires_in;

                    account.access_token = Some(access_token.clone());
                    account.token_expiry = Some(expiry);
                    return Ok(access_token);
                }
                Err("Failed to parse refresh token JSON response".to_string())
            } else {
                let status = resp.status();
                let err_text = resp.text().await.unwrap_or_default();
                Err(format!("Refresh request failed status: {}, error: {}", status, err_text))
            }
        }
        Err(e) => Err(format!("Refresh request request failed: {}", e)),
    }
}

struct ImapOAuth2 {
    user: String,
    access_token: String,
}

impl imap::Authenticator for ImapOAuth2 {
    type Response = String;
    #[inline]
    fn process(&self, _data: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.access_token)
    }
}

/// Block on a future from a plain worker thread. `pollster::block_on` is NOT
/// enough for reqwest/hyper futures — they need a live tokio reactor, and the
/// `rt.enter()` guard in main() only covers the main thread.
fn block_on_worker<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {}", e))
        .map(|rt| rt.block_on(fut))
}

/// True for the built-in demo account — workers skip the network entirely.
fn is_mock_account(account: &AccountInfo) -> bool {
    account.password == "mock_password" || account.email == "lsgalante@cce-ui.org"
}

/// Connect + authenticate an IMAP session; shared by the sync and
/// server-delete workers (call from a worker thread — it blocks). Refreshed
/// OAuth tokens are reported back via UpdateAccountTokens; every failure
/// lands on stderr and (when `verbose`) as a sticky error toast, then
/// yields None.
fn open_imap_session(
    account: &mut AccountInfo,
    sender: &calloop::channel::Sender<AppMessage>,
    verbose: bool,
) -> Option<imap::Session<native_tls::TlsStream<std::net::TcpStream>>> {
    // Everything mirrors to stderr regardless of `verbose` — the quiet
    // paths (seen-push, sent fetch) stay UI-silent but must not be
    // undebuggable. `err:` = sticky red toast, plain = timed green one.
    macro_rules! say {
        (err: $msg:expr) => {{
            let msg: String = $msg;
            eprintln!("cce-mail: {}", msg);
            if verbose {
                let _ = sender.send(AppMessage::StatusError(msg));
            }
        }};
        ($msg:expr) => {{
            let msg: String = $msg;
            eprintln!("cce-mail: {}", msg);
            if verbose {
                let _ = sender.send(AppMessage::Status(msg));
            }
        }};
    }
    let mut access_token = account.password.clone();
    if account.is_oauth {
        let mut acc = account.clone();
        match block_on_worker(refresh_access_token(&mut acc)).and_then(|r| r) {
            Ok(token) => {
                access_token = token;
                // Send refreshed tokens back to main thread to save them
                let _ = sender.send(AppMessage::UpdateAccountTokens(
                    acc.email.clone(),
                    acc.access_token.clone(),
                    acc.token_expiry,
                ));
                account.access_token = acc.access_token;
                account.token_expiry = acc.token_expiry;
            }
            Err(e) => {
                say!(err: format!("OAuth Refresh Failed: {}", e));
                return None;
            }
        }
    }

    let domain = account.imap.split(':').next()?;
    let port = account
        .imap
        .split(':')
        .nth(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(993);

    say!(format!("Connecting to {}...", account.imap));

    let tls = match TlsConnector::new() {
        Ok(t) => t,
        Err(_) => {
            say!(err: "Failed to create TLS connector".to_string());
            return None;
        }
    };

    let client = match imap::connect((domain, port), domain, &tls) {
        Ok(c) => c,
        Err(e) => {
            say!(err: format!("IMAP Connection failed: {}", e));
            return None;
        }
    };

    if account.is_oauth {
        let auth = ImapOAuth2 {
            user: account.email.clone(),
            access_token,
        };
        match client.authenticate("XOAUTH2", &auth) {
            Ok(s) => Some(s),
            Err((e, _)) => {
                say!(err: format!("IMAP OAuth Login failed: {}", e));
                None
            }
        }
    } else {
        match client.login(&account.email, &account.password) {
            Ok(s) => Some(s),
            Err((e, _)) => {
                say!(err: format!("IMAP Login failed: {}", e));
                None
            }
        }
    }
}

/// Namespaces sent-folder ids away from inbox sequence numbers (both are
/// fetch-time seq numbers; ids must stay unique across the merged list).

/// One mailbox's outcome for a single sync pass.
///
/// `server_uids` is the whole mailbox, not just what was fetched: the merge
/// needs it to notice messages deleted from another client. Without it a
/// local cache that only ever grows would keep showing mail that no longer
/// exists.
#[derive(Debug, Clone)]
struct FolderSync {
    folder: String,
    /// Newly downloaded messages. `id` is left 0 — identity is assigned by
    /// the merge, which is the only place that can see the whole cache.
    fetched: Vec<Email>,
    server_uids: Vec<u32>,
    /// Of the recent window, the uids the server marks \Seen. Read state set
    /// on another client reaches already-cached mail through this.
    seen_uids: Vec<u32>,
    /// Still missing after this pass — drives the backfill continuation.
    remaining: usize,
}

/// Download one batch of messages by UID, with the two-pass BODYSTRUCTURE
/// strategy (pass 1 headers/flags/structure, pass 2 text parts grouped by
/// section, capped full-message fallback). Keyed on UID throughout: sequence
/// numbers shift whenever anything is expunged, so they cannot identify a
/// message across syncs.
fn fetch_uid_batch(
    session: &mut imap::Session<native_tls::TlsStream<std::net::TcpStream>>,
    account_email: &str,
    folder_tag: &str,
    uids: &[u32],
    say_err: &dyn Fn(String),
) -> Vec<Email> {
    if uids.is_empty() {
        return Vec::new();
    }
    let uid_set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");

    struct Pending {
        uid: u32,
        read: bool,
        from: String,
        to: String,
        subject: String,
        date: String,
        part: Option<TextPartSpec>,
        body: String,
        remote: Vec<RemoteAttachment>,
    }
    let parser = mail_parser::MessageParser::default();
    let mut pending: Vec<Pending> = Vec::new();

    // Pass 1: flags + headers + BODYSTRUCTURE — no body bytes yet, so
    // attachments never ride along. PEEK semantics don't matter here
    // (headers/structure don't set \Seen), but flags come from the server.
    match session.uid_fetch(&uid_set, "(UID FLAGS RFC822.HEADER BODYSTRUCTURE)") {
        Ok(fetches) => {
            for fetch in fetches.iter() {
                let Some(uid) = fetch.uid else { continue };
                let read = fetch
                    .flags()
                    .iter()
                    .any(|f| matches!(f, imap::types::Flag::Seen));
                let (from, to, subject, date) = match fetch.header().and_then(|h| parser.parse(h)) {
                    Some(msg) => (
                        format_from(&msg),
                        format_to(&msg, account_email),
                        msg.subject().unwrap_or("(No Subject)").to_string(),
                        format_date(msg.date()),
                    ),
                    None => (
                        "Unknown".to_string(),
                        account_email.to_string(),
                        "(No Subject)".to_string(),
                        "Unknown".to_string(),
                    ),
                };
                pending.push(Pending {
                    uid,
                    read,
                    from,
                    to,
                    subject,
                    date,
                    part: fetch.bodystructure().and_then(find_text_part),
                    body: String::new(),
                    remote: fetch.bodystructure().map(find_attachment_parts).unwrap_or_default(),
                });
            }
        }
        Err(e) => {
            say_err(format!("IMAP Fetch failed: {}", e));
            return Vec::new();
        }
    }

    // Pass 2: fetch just the chosen text part, grouped by section path so a
    // typical batch needs only 1-2 more round trips; capped so a giant text
    // part can't stall the sync either.
    let mut groups: std::collections::HashMap<Vec<u32>, Vec<u32>> = std::collections::HashMap::new();
    for p in &pending {
        if let Some(spec) = &p.part {
            groups.entry(spec.path.clone()).or_default().push(p.uid);
        }
    }
    for (path, group_uids) in groups {
        let set = group_uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let section = section_str(&path);
        let query = format!("(UID BODY.PEEK[{}]<0.{}>)", section, PART_FETCH_CAP);
        let section_path = imap_proto::types::SectionPath::Part(path, None);
        match session.uid_fetch(&set, &query) {
            Ok(fetches) => {
                for fetch in fetches.iter() {
                    let Some(uid) = fetch.uid else { continue };
                    let Some(bytes) = fetch.section(&section_path) else { continue };
                    if let Some(p) = pending.iter_mut().find(|p| p.uid == uid) {
                        if let Some(spec) = &p.part {
                            p.body = parser
                                .parse(&spec.synthesize(bytes))
                                .map(|m| extract_body(&m))
                                .unwrap_or_default();
                        }
                    }
                }
            }
            Err(e) => {
                say_err(format!("IMAP part fetch failed: {}", e));
            }
        }
    }

    // Fallback: no usable text part in the structure (or the walk failed) —
    // one capped full-message fetch for those stragglers.
    let no_part: Vec<u32> = pending.iter().filter(|p| p.part.is_none()).map(|p| p.uid).collect();
    if !no_part.is_empty() {
        let set = no_part.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let query = format!("(UID BODY.PEEK[]<0.{}>)", PART_FETCH_CAP * 4);
        match session.uid_fetch(&set, &query) {
            Ok(fetches) => {
                for fetch in fetches.iter() {
                    let Some(uid) = fetch.uid else { continue };
                    let Some(raw) = fetch.body() else { continue };
                    if let Some(p) = pending.iter_mut().find(|p| p.uid == uid) {
                        p.body = parser.parse(raw).map(|m| extract_body(&m)).unwrap_or_default();
                    }
                }
            }
            Err(e) => {
                say_err(format!("IMAP fallback fetch failed: {}", e));
            }
        }
    }

    pending
        .into_iter()
        .map(|p| Email {
            id: 0, // assigned by the merge
            uid: Some(p.uid),
            from: p.from,
            to: p.to,
            subject: p.subject,
            body: p.body,
            date: p.date,
            read: p.read,
            folder: folder_tag.to_string(),
            cc: String::new(),
            bcc: String::new(),
            attachments: Vec::new(),
            remote_attachments: p.remote,
        })
        .collect()
}

/// Sync one mailbox incrementally: download only what the cache is missing,
/// newest first, and report what the server holds so the merge can drop mail
/// deleted elsewhere.
///
/// This used to refetch the newest [`FETCH_COUNT`] every time and the merge
/// replaced the folder wholesale, which pinned the cache at 50 messages
/// however often it ran. Now a pass takes up to [`MAX_FETCH_PER_SYNC`] of the
/// missing ones and reports the rest as `remaining`, so history backfills
/// across passes instead of never arriving.
fn sync_folder(
    session: &mut imap::Session<native_tls::TlsStream<std::net::TcpStream>>,
    sender: &calloop::channel::Sender<AppMessage>,
    account_email: &str,
    mailbox: &str,
    folder_tag: &str,
    known: &std::collections::HashSet<u32>,
    verbose: bool,
) -> Option<FolderSync> {
    macro_rules! say {
        (err: $msg:expr) => {{
            let msg: String = $msg;
            eprintln!("cce-mail: {}", msg);
            if verbose {
                let _ = sender.send(AppMessage::StatusError(msg));
            }
        }};
        ($msg:expr) => {{
            let msg: String = $msg;
            eprintln!("cce-mail: {}", msg);
            if verbose {
                let _ = sender.send(AppMessage::Status(msg));
            }
        }};
    }
    if let Err(e) = session.select(mailbox) {
        say!(err: format!("Failed to select {}: {}", mailbox, e));
        return None;
    }

    // UID SEARCH, not SEARCH: sequence numbers are meaningless across syncs.
    let mut server_uids: Vec<u32> = match session.uid_search("ALL") {
        Ok(uids) => uids.into_iter().collect(),
        Err(e) => {
            say!(err: format!("IMAP Search failed: {}", e));
            return None;
        }
    };
    server_uids.sort_unstable();

    let missing: Vec<u32> = server_uids.iter().copied().filter(|u| !known.contains(u)).collect();
    let take = missing.len().min(MAX_FETCH_PER_SYNC);
    let batch_uids = &missing[missing.len() - take..]; // newest first come first
    let remaining = missing.len() - take;

    if take > 0 {
        if remaining > 0 {
            say!(format!("Fetching {} messages ({} older still to come)...", take, remaining));
        } else {
            say!(format!("Fetching {} messages...", take));
        }
    }

    let say_err = |m: String| {
        eprintln!("cce-mail: {}", m);
        if verbose {
            let _ = sender.send(AppMessage::StatusError(m));
        }
    };

    let mut fetched = Vec::new();
    for chunk in batch_uids.chunks(FETCH_COUNT) {
        // Newest chunk first, so a long backfill still surfaces recent mail
        // early. Each chunk is a separate round trip but one session.
        let mut ordered: Vec<u32> = chunk.to_vec();
        ordered.sort_unstable_by(|a, b| b.cmp(a));
        fetched.extend(fetch_uid_batch(session, account_email, folder_tag, &ordered, &say_err));
    }

    // Flags for the recent window of already-cached mail, so a message read
    // on another client stops showing as unread here. Cheap: no bodies.
    let mut seen_uids = Vec::new();
    let recent_known: Vec<u32> = {
        let mut v: Vec<u32> = server_uids.iter().copied().filter(|u| known.contains(u)).collect();
        let keep = v.len().min(FLAG_REFRESH_WINDOW);
        v.split_off(v.len() - keep)
    };
    if !recent_known.is_empty() {
        let set = recent_known.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        match session.uid_fetch(&set, "(UID FLAGS)") {
            Ok(fetches) => {
                for fetch in fetches.iter() {
                    if let Some(uid) = fetch.uid {
                        if fetch.flags().iter().any(|f| matches!(f, imap::types::Flag::Seen)) {
                            seen_uids.push(uid);
                        }
                    }
                }
            }
            Err(e) => {
                // Not fatal: read state simply stays as cached this pass.
                eprintln!("cce-mail: IMAP flag refresh failed: {}", e);
            }
        }
    }

    Some(FolderSync {
        folder: folder_tag.to_string(),
        fetched,
        server_uids,
        seen_uids,
        remaining,
    })
}

/// Fold one sync pass into the cached mail.
///
/// Additive by design: the previous merge replaced each server-backed folder
/// wholesale, so the cache could never hold more than one pass fetched and
/// syncing twice threw away what the first pass downloaded. Here prior mail
/// survives, and identity is `(folder, uid)` — never `id`, which is local, and
/// never the IMAP sequence number, which shifts on any expunge.
///
/// Only folders present in `folders` are reconciled: a pass that could not
/// select Sent must not be read as "the server has no sent mail".
fn merge_sync(prior: Vec<Email>, folders: &[FolderSync]) -> Vec<Email> {
    use std::collections::HashSet;

    let mut out = prior;

    for f in folders {
        let server: HashSet<u32> = f.server_uids.iter().copied().collect();
        let seen: HashSet<u32> = f.seen_uids.iter().copied().collect();

        // Deleted from another client. Scoped to this folder and to mail that
        // came from the server: locally-created rows (uid None) and anything
        // the user moved elsewhere (trash, drafts) are not this pass's business.
        out.retain(|e| {
            e.folder != f.folder || e.uid.is_none_or(|u| server.contains(&u))
        });

        // Read elsewhere. Local "read" still wins over server "unread": the
        // seen-push is best-effort, so an in-flight or failed push must not
        // flip a message the user has already opened back to unread.
        for e in out.iter_mut() {
            if e.folder == f.folder && e.uid.is_some_and(|u| seen.contains(&u)) {
                e.read = true;
            }
        }

        let mut next_id = out.iter().map(|e| e.id).max().unwrap_or(0) + 1;
        for mut fresh in f.fetched.iter().cloned() {
            let Some(uid) = fresh.uid else { continue };
            match out
                .iter_mut()
                .find(|e| e.folder == f.folder && e.uid == Some(uid))
            {
                // Already held: refresh the content but keep the local identity
                // and a local read that the server has not caught up with.
                Some(existing) => {
                    let was_read = existing.read;
                    fresh.id = existing.id;
                    fresh.read = fresh.read || was_read;
                    *existing = fresh;
                }
                None => {
                    fresh.id = next_id;
                    next_id += 1;
                    out.push(fresh);
                }
            }
        }

        // A locally-appended sent copy (uid None) is dropped once the server
        // fetch carries the same message — Gmail auto-files SMTP sends into
        // Sent Mail, so both would otherwise show.
        if f.folder == "sent" {
            let fetched_subjects: HashSet<&str> =
                f.fetched.iter().map(|e| e.subject.as_str()).collect();
            out.retain(|e| {
                !(e.folder == "sent"
                    && e.uid.is_none()
                    && fetched_subjects.contains(e.subject.as_str()))
            });
        }
    }

    // Newest first, which is the order every list pass paints in. Server mail
    // orders by uid (monotonic per mailbox); locally-created mail has no uid
    // and belongs at the top, being the most recent thing the user did.
    out.sort_by_key(|e| std::cmp::Reverse(e.uid.unwrap_or(u32::MAX)));

    // Identity must be unique or selection and the per-message actions act on
    // the wrong row. The pre-uid scheme derived `id` from the IMAP sequence
    // number, so a message moved to trash kept an id that a later arrival was
    // handed again — caches written by it carry real collisions. Re-key the
    // later duplicate; the first in display order keeps its id, so whatever
    // the user currently has selected stays put.
    let mut used: HashSet<usize> = HashSet::new();
    let mut next_free = out.iter().map(|e| e.id).max().unwrap_or(0) + 1;
    for e in out.iter_mut() {
        if e.id == 0 || !used.insert(e.id) {
            e.id = next_free;
            next_free += 1;
            used.insert(e.id);
        }
    }

    out
}

fn sync_imap(
    mut account: AccountInfo,
    known: std::collections::HashMap<String, std::collections::HashSet<u32>>,
    sender: calloop::channel::Sender<AppMessage>,
) {
    std::thread::spawn(move || {
        if is_mock_account(&account) {
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, true) else {
            return;
        };

        let _ = sender.send(AppMessage::Status("Syncing Inbox...".to_string()));

        let empty = std::collections::HashSet::new();
        let mut folders = Vec::new();

        let Some(inbox) = sync_folder(
            &mut session,
            &sender,
            &account.email,
            "INBOX",
            "inbox",
            known.get("inbox").unwrap_or(&empty),
            true,
        ) else {
            let _ = session.logout();
            return;
        };
        folders.push(inbox);

        // Sent rides along quietly: Gmail's name first, the conventional one
        // second; a server with neither just syncs the inbox.
        for mailbox in ["[Gmail]/Sent Mail", "Sent"] {
            if let Some(sent) = sync_folder(
                &mut session,
                &sender,
                &account.email,
                mailbox,
                "sent",
                known.get("sent").unwrap_or(&empty),
                false,
            ) {
                folders.push(sent);
                break;
            }
        }

        let _ = sender.send(AppMessage::EmailsSynced(account.email.clone(), folders));
        let _ = sender.send(AppMessage::Status("Sync Complete".to_string()));
        let _ = session.logout();
    });
}

/// Delete an INBOX message on the server, by UID. Best-effort trash-copy
/// first: on Gmail a copy to [Gmail]/Trash removes the message everywhere,
/// on generic servers a "Trash" copy keeps the conventional safety net; the
/// STORE+expunge below is what actually removes it from INBOX either way.
/// UIDPLUS uid_expunge targets just our message; plain EXPUNGE fallback also
/// flushes any other \Deleted-flagged mail, which matches client convention.
fn delete_on_server(mut account: AccountInfo, uid: u32, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        if is_mock_account(&account) {
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, true) else {
            return;
        };
        if let Err(e) = session.select("INBOX") {
            eprintln!("cce-mail: Failed to select INBOX: {}", e);
            let _ = sender.send(AppMessage::StatusError(format!("Failed to select INBOX: {}", e)));
            let _ = session.logout();
            return;
        }
        let uid_set = uid.to_string();
        for trash in ["[Gmail]/Trash", "Trash"] {
            if session.uid_copy(&uid_set, trash).is_ok() {
                break;
            }
        }
        if let Err(e) = session.uid_store(&uid_set, "+FLAGS (\\Deleted)") {
            eprintln!("cce-mail: Server delete failed: {}", e);
            let _ = sender.send(AppMessage::StatusError(format!("Server delete failed: {}", e)));
            let _ = session.logout();
            return;
        }
        if session.uid_expunge(&uid_set).is_err() {
            let _ = session.expunge();
        }
        let _ = sender.send(AppMessage::Status("Deleted on server".to_string()));
        let _ = session.logout();
    });
}

/// Write attachment bytes into ~/Downloads under a collision-safe name.
/// The server-supplied filename is reduced to its final path component —
/// a hostile "../.ssh/authorized_keys" must not escape the directory.
fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<String, String> {
    let dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join("Downloads"))
        .map_err(|_| "No HOME".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {}", dir.display(), e))?;

    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("attachment");
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{}", e)),
        _ => (base.to_string(), String::new()),
    };
    let mut path = dir.join(base);
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{} ({}){}", stem, n, ext));
        n += 1;
    }
    std::fs::write(&path, bytes).map_err(|e| format!("Cannot write {}: {}", path.display(), e))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Fetch one attachment part by UID, decode it, and save it to ~/Downloads.
/// Same thread shape as [`set_seen_on_server`]; the outcome comes back as
/// [`AppMessage::AttachmentFetched`].
fn fetch_attachment(
    mut account: AccountInfo,
    folder: String,
    uid: u32,
    att: RemoteAttachment,
    sender: calloop::channel::Sender<AppMessage>,
) {
    std::thread::spawn(move || {
        let report = |sender: &calloop::channel::Sender<AppMessage>, r: Result<String, String>| {
            let _ = sender.send(AppMessage::AttachmentFetched(r));
        };
        if is_mock_account(&account) {
            report(&sender, Err("This account has no server copy".to_string()));
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            report(&sender, Err("IMAP connection failed".to_string()));
            return;
        };
        // Same mailbox names the sync uses; sent-folder mail lives under
        // Gmail's name first, the conventional one second.
        let mailboxes: &[&str] =
            if folder == "sent" { &["[Gmail]/Sent Mail", "Sent"] } else { &["INBOX"] };
        if !mailboxes.iter().any(|mb| session.select(mb).is_ok()) {
            report(&sender, Err("Cannot select mailbox".to_string()));
            let _ = session.logout();
            return;
        }
        let query = format!("(BODY.PEEK[{}])", section_str(&att.section));
        let section_path = imap_proto::types::SectionPath::Part(att.section.clone(), None);
        let outcome = match session.uid_fetch(uid.to_string(), &query) {
            Ok(fetches) => match fetches.iter().next().and_then(|f| f.section(&section_path)) {
                Some(bytes) => save_to_downloads(&att.name, &decode_part_bytes(&att.encoding, bytes)),
                None => Err("Server returned no data for the part".to_string()),
            },
            Err(e) => Err(format!("Fetch failed: {}", e)),
        };
        report(&sender, outcome);
        let _ = session.logout();
    });
}

/// Push a message's read state to the server (INBOX, by UID). UI-silent
/// (stderr still logs failures): this fires on every message open, so no
/// Connecting/success toasts, and a failed push is self-healing — the
/// EmailsSynced merge keeps locally-read mail read regardless of the
/// server flag until a later push converges.
fn set_seen_on_server(mut account: AccountInfo, uid: u32, seen: bool, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        if is_mock_account(&account) {
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            return;
        };
        if session.select("INBOX").is_ok() {
            let query = if seen { "+FLAGS (\\Seen)" } else { "-FLAGS (\\Seen)" };
            let _ = session.uid_store(uid.to_string(), query);
        }
        let _ = session.logout();
    });
}

/// One outgoing message, exactly as composed. This travels to the send
/// thread and comes back in [`AppMessage::SendResult`] so the outcome
/// handler still holds the full content — a success records it in Sent, a
/// failure lands it in Drafts instead of losing it.
#[derive(Debug, Clone)]
struct OutgoingMail {
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
    /// Absolute paths; read at build time on the send thread.
    attachments: Vec<String>,
}

/// Content type for an attachment, by extension. A tiny map beats a mime
/// dependency: anything unlisted is application/octet-stream, which every
/// receiver treats as "download it".
fn attachment_content_type(path: &str) -> &'static str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") | Some("log") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        Some("gz") | Some("tgz") => "application/gzip",
        _ => "application/octet-stream",
    }
}

/// Build the lettre message for one [`OutgoingMail`].
///
/// To and Cc become headers; **Bcc deliberately never becomes a header** —
/// bcc recipients ride only in the SMTP envelope, so no copy of the message
/// can name them regardless of how the library formats headers. Attachments
/// are read here (the send thread), each as one part of a multipart/mixed.
fn build_outgoing(from: &str, mail: &OutgoingMail) -> Result<lettre::Message, String> {
    use lettre::message::{Attachment, MultiPart, SinglePart, header::ContentType};

    fn mailboxes(field: &str, label: &str) -> Result<Vec<lettre::message::Mailbox>, String> {
        field
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| a.parse().map_err(|e| format!("Invalid {} address {}: {}", label, a, e)))
            .collect()
    }

    let from_mb: lettre::message::Mailbox =
        from.parse().map_err(|e| format!("Invalid From address: {}", e))?;
    let to = mailboxes(&mail.to, "To")?;
    let cc = mailboxes(&mail.cc, "Cc")?;
    let bcc = mailboxes(&mail.bcc, "Bcc")?;
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err("No recipients".to_string());
    }

    // Envelope = actual delivery list: To + Cc + Bcc.
    let rcpts: Vec<lettre::Address> =
        to.iter().chain(cc.iter()).chain(bcc.iter()).map(|m| m.email.clone()).collect();
    let envelope = lettre::address::Envelope::new(Some(from_mb.email.clone()), rcpts)
        .map_err(|e| format!("Invalid envelope: {}", e))?;

    let mut builder = Message::builder().from(from_mb).envelope(envelope).subject(&mail.subject);
    for mb in to {
        builder = builder.to(mb);
    }
    for mb in cc {
        builder = builder.cc(mb);
    }

    let msg = if mail.attachments.is_empty() {
        builder.body(mail.body.clone())
    } else {
        let mut mp = MultiPart::mixed().singlepart(SinglePart::plain(mail.body.clone()));
        for path in &mail.attachments {
            let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {}", path, e))?;
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("attachment")
                .to_string();
            let ct = ContentType::parse(attachment_content_type(path))
                .map_err(|e| format!("Bad content type: {}", e))?;
            mp = mp.singlepart(Attachment::new(name).body(bytes, ct));
        }
        builder.multipart(mp)
    };
    msg.map_err(|e| format!("Failed to build message: {}", e))
}

fn send_smtp(mut account: AccountInfo, mail: OutgoingMail, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        // Every exit reports through SendResult so the app can file the
        // message (Sent on success, Drafts on failure) — a bare Status would
        // discard the composed content.
        let fail = |sender: &calloop::channel::Sender<AppMessage>, mail: OutgoingMail, err: String| {
            let _ = sender.send(AppMessage::SendResult(mail, Some(err)));
        };

        if account.password == "mock_password" {
            let _ = sender.send(AppMessage::SendResult(mail, None));
            return;
        }

        let mut access_token = account.password.clone();
        if account.is_oauth {
            let mut acc = account.clone();
            match block_on_worker(refresh_access_token(&mut acc)).and_then(|r| r) {
                Ok(token) => {
                    access_token = token;
                    let _ = sender.send(AppMessage::UpdateAccountTokens(
                        acc.email.clone(),
                        acc.access_token.clone(),
                        acc.token_expiry,
                    ));
                    account.access_token = acc.access_token;
                    account.token_expiry = acc.token_expiry;
                }
                Err(e) => {
                    fail(&sender, mail, format!("OAuth Refresh Failed: {}", e));
                    return;
                }
            }
        }

        let domain = match account.smtp.split(':').next() {
            Some(d) => d,
            None => {
                fail(&sender, mail, "Invalid SMTP hostname".to_string());
                return;
            }
        };
        let port = match account.smtp.split(':').nth(1).and_then(|p| p.parse::<u16>().ok()) {
            Some(p) => p,
            None => 465,
        };

        let _ = sender.send(AppMessage::Status("Sending SMTP mail...".to_string()));

        let email = match build_outgoing(&account.email, &mail) {
            Ok(m) => m,
            Err(e) => {
                fail(&sender, mail, e);
                return;
            }
        };

        let creds = Credentials::new(account.email.clone(), access_token);

        let mut mailer_builder = if port == 465 {
            SmtpTransport::relay(domain).unwrap().port(port)
        } else {
            SmtpTransport::starttls_relay(domain).unwrap().port(port)
        };

        if account.is_oauth {
            mailer_builder = mailer_builder.authentication(vec![lettre::transport::smtp::authentication::Mechanism::Xoauth2]);
        }

        let mailer = mailer_builder.credentials(creds).build();

        match mailer.send(&email) {
            Ok(_) => {
                let _ = sender.send(AppMessage::SendResult(mail, None));
            }
            Err(e) => {
                fail(&sender, mail, format!("SMTP send failed: {}", e));
            }
        }
    });
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
            to: val(&self.compose_to),
            cc: val(&self.compose_cc),
            bcc: val(&self.compose_bcc),
            subject: val(&self.compose_subject),
            body: if self.compose_body.editing {
                self.compose_body.edit_buffer.clone()
            } else {
                self.compose_body.text.clone()
            },
            attachments: self.compose_attachments.clone(),
        }
    }

    /// Reset every compose field (text and edit_buffer both — the reader
    /// picks a side by the editing flag, so they must always agree).
    fn clear_compose(&mut self) {
        for tb in [
            &mut self.compose_to,
            &mut self.compose_cc,
            &mut self.compose_bcc,
            &mut self.compose_subject,
            &mut self.compose_body,
        ] {
            tb.text = String::new();
            tb.edit_buffer = String::new();
        }
        self.compose_attachments.clear();
    }

    /// File content in the local Drafts folder (never synced — drafts have
    /// no uid and a folder the retention filters don't touch).
    fn file_as_draft(&mut self, mail: &OutgoingMail) {
        let new_id = self.emails.iter().map(|e| e.id).max().unwrap_or(0) + 1;
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
            cc: mail.cc.clone(),
            bcc: mail.bcc.clone(),
            attachments: mail.attachments.clone(),
            remote_attachments: Vec::new(),
        });
        self.save_emails();
    }

    /// Register every dispatch root in the ui_context (idempotent, runs each frame).
    /// The id-rooted router (`propagate_event(event, WidgetId)`) resolves roots through
    /// the registry; email assembles its frame by hand and never goes through
    /// `render_widget`'s registration side effect. `email_buttons` is rebuilt on list
    /// refresh — per-frame registration follows the fresh allocations.
    fn register_dispatch_roots(&mut self) {
        let (id, ptr) = (self.search_box.id(), self.search_box.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.detail_body.id(), self.detail_body.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.compose_to.id(), self.compose_to.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.compose_cc.id(), self.compose_cc.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.compose_bcc.id(), self.compose_bcc.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.compose_subject.id(), self.compose_subject.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.compose_body.id(), self.compose_body.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.mail_menu.id(), self.mail_menu.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.folder_dropdown.id(), self.folder_dropdown.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.account_dropdown.id(), self.account_dropdown.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_compose_send.id(), self.btn_compose_send.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_compose_cancel.id(), self.btn_compose_cancel.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_compose_attach.id(), self.btn_compose_attach.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        for btn in self.email_buttons.iter_mut() {
            let (id, ptr) = (btn.id(), btn.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
        }
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
            save_accounts(&self.accounts);
        }
    }

    fn start_sync(&mut self, force: bool) {
        const MIN_SYNC_GAP: std::time::Duration = std::time::Duration::from_secs(30);
        if !force && self.last_sync_start.is_some_and(|t| t.elapsed() < MIN_SYNC_GAP) {
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
        let mut known: std::collections::HashMap<String, std::collections::HashSet<u32>> =
            std::collections::HashMap::new();
        for e in &self.emails {
            if let Some(uid) = e.uid {
                if e.folder == "inbox" || e.folder == "sent" {
                    known.entry(e.folder.clone()).or_default().insert(uid);
                }
            }
        }

        let account = self.accounts[self.selected_account_idx].clone();
        sync_imap(account, known, self.sender.clone());
        self.last_sync_start = Some(std::time::Instant::now());
    }

    fn save_emails(&self) {
        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
            save_emails_for_account(&acc.email, &self.emails);
        }
    }

    /// The list/detail split, clamped to the current window: (list band
    /// width, separator x, detail pane x). The single source for paint,
    /// layout, and every hit-test — the stored `list_w` preference is never
    /// mutated by a window resize, only re-clamped here.
    fn split_geom(&self) -> (f32, f32, f32) {
        let max_w = (self.width as f32 - LIST_X - LIST_SEP_GAP - LIST_DETAIL_GAP - DETAIL_W_MIN)
            .max(LIST_W_MIN);
        let lw = self.list_w.clamp(LIST_W_MIN, max_w);
        let sep = LIST_X + lw + LIST_SEP_GAP;
        (lw, sep, sep + LIST_DETAIL_GAP)
    }

    /// The email list's band: (top y, height). The search row above it only
    /// exists while the band is open, and the list takes that strip back when
    /// it closes — so nothing hardcodes the top.
    fn list_geom(&self) -> (f32, f32) {
        let top = if self.search_open { LIST_TOP_WITH_SEARCH } else { SEARCH_ROW_Y } + MENUBAR_H;
        (top, (self.height as f32 - top - LIST_BOTTOM_PAD).max(50.0))
    }

    /// The rows the list is currently showing, in paint order — the folder
    /// filter plus the search box. A row index means nothing without this:
    /// it is what maps the card under the pointer to its message.
    fn filtered_emails(&self) -> Vec<&Email> {
        let current_folder_str = match self.current_folder {
            Folder::Inbox => "inbox",
            Folder::Sent => "sent",
            Folder::Drafts => "drafts",
            Folder::Trash => "trash",
        };
        let search_text = if self.search_box.editing {
            &self.search_box.edit_buffer
        } else {
            &self.search_box.text
        };
        let search_lower = search_text.to_lowercase();
        self.emails
            .iter()
            .filter(|e| e.folder == current_folder_str)
            .filter(|e| {
                search_lower.is_empty()
                    || e.from.to_lowercase().contains(&search_lower)
                    || e.subject.to_lowercase().contains(&search_lower)
                    || e.body.to_lowercase().contains(&search_lower)
            })
            .collect()
    }

    /// Right-press on a message card: open its context menu at the pointer.
    /// Returns false when the press missed every card.
    fn open_card_context_menu(&mut self, px: f32, py: f32) -> bool {
        let hit = self.email_buttons.iter().position(|b| {
            let (bx, by, bw, bh) = b.rect();
            bx > -9000.0 && px >= bx && px <= bx + bw && py >= by && py <= by + bh
        });
        let Some(idx) = hit else { return false };
        let Some((id, read, subject, is_draft)) = self
            .filtered_emails()
            .get(idx)
            .map(|e| (e.id, e.read, e.subject.clone(), e.folder == "drafts"))
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
        let (labels, actions): (Vec<&str>, Vec<Option<AppMessage>>) = if is_draft {
            (
                vec!["Edit", "Delete"],
                vec![Some(AppMessage::SelectEmail(id)), Some(AppMessage::DeleteSelected)],
            )
        } else {
            (
                vec!["Reply", "Delete", if read { "Mark Unread" } else { "Mark Read" }],
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
    fn context_menu_press(&mut self, px: f32, py: f32) -> Option<AppMessage> {
        let mx = cce_ui::widget::context_menu::x();
        let my = cce_ui::widget::context_menu::y();
        let mw = cce_ui::widget::context_menu::w();
        let mh = cce_ui::widget::context_menu::h();
        let mut msg = None;
        if px >= mx && px <= mx + mw && py >= my && py <= my + mh {
            let row = ((py - my) / CONTEXT_ROW_H) as usize;
            msg = self.context_menu_actions.get(row).cloned().flatten();
        }
        cce_ui::widget::context_menu::hide();
        self.context_menu_actions.clear();
        msg
    }

    /// The detail-pane body box: (top y, height). Paint, layout, the
    /// scrollbar and the scroll clamps all derive from this one pair.
    fn detail_body_geom(&self) -> (f32, f32) {
        let top = DETAIL_BODY_Y + MENUBAR_H;
        let h = (self.height as f32 - top - DETAIL_BODY_BOTTOM_PAD).max(100.0);
        (top, h)
    }

    /// Detail-pane body scrollbar geometry, mirroring `ScrollRegion::scrollbar_geom`:
    /// (sb_x, track_y, sb_w, track_h, thumb_y, thumb_h). None when the body fits
    /// (no scrollbar drawn). The single source for display_list and the drag path.
    fn body_scrollbar_geom(&self) -> Option<(f32, f32, f32, f32, f32, f32)> {
        let w = self.width as f32;
        let (body_y, body_h) = self.detail_body_geom();
        let max_scroll = (self.body_content_h - body_h).max(0.0);
        if max_scroll <= 0.0 {
            return None;
        }
        let sb_w = cce_ui::layout::scrollbar_width();
        let sb_x = w - sb_w - 4.0;
        let thumb_h = (body_h * body_h / self.body_content_h).clamp(20.0, body_h);
        let thumb_y = body_y + (self.body_scroll / max_scroll) * (body_h - thumb_h);
        Some((sb_x, body_y, sb_w, body_h, thumb_y, thumb_h))
    }

    /// Left press on the scrollbar strip (±4px slop like ScrollRegion): grab the
    /// thumb where it was clicked, or jump the track and drag from the thumb center.
    fn body_sb_press(&mut self, px: f32, py: f32) -> bool {
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

    fn emit_text_prims(&mut self, pc: &mut cce_ui::scene::paint::PaintCtx) {
        let mut labels = Vec::new();

        let w_f32 = self.width as f32;
        let h_f32 = self.height as f32;

        let list_x = LIST_X;
        let (list_w, separator_x, detail_x) = self.split_geom();
        // Row-label char budgets scale with the band; the bases are the
        // hand-tuned counts at the 300px default.
        let fit = |base: f32| (base * list_w / LIST_W_DEFAULT) as usize;

        // Widget text rides along with chrome in display_list's paint_root_into
        // walk — only app-composed labels are emitted here.

        // 3. Email List Labels.
        // Skipped while a modal is up: text always renders above geometry, and these
        // hand-emitted labels carry no bounds, so they'd bleed straight through the
        // modal panel (the popover-occlusion clamp only knows registered popovers).
        let modal_open = self.compose_open;
        if modal_open {
        } else {
            let current_folder_str = match self.current_folder {
                Folder::Inbox => "inbox",
                Folder::Sent => "sent",
                Folder::Drafts => "drafts",
                Folder::Trash => "trash",
            };
            let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
            let search_lower = search_text.to_lowercase();
            let filtered: Vec<&Email> = self.emails.iter()
                .filter(|e| e.folder == current_folder_str)
                .filter(|e| {
                    if search_lower.is_empty() {
                        true
                    } else {
                        e.from.to_lowercase().contains(&search_lower) ||
                        e.subject.to_lowercase().contains(&search_lower) ||
                        e.body.to_lowercase().contains(&search_lower)
                    }
                })
                .collect();

            for (idx, email) in filtered.iter().enumerate() {
                if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                    // Sender — recipient on sent rows (every sent mail is
                    // "from" ourselves; the interesting party is the other end)
                    let row_head = if email.folder == "sent" {
                        format!("To: {}", email.to)
                    } else {
                        email.from.clone()
                    };
                    labels.push(TextLabel {
                        text: ellipsize(&row_head, fit(21.0)),
                        x: list_x + 20.0,
                        y: draw_y + 6.0,
                        font_size: 11.0,
                        color: if !email.read { [0xff, 0xff, 0xff] } else { [0xb0, 0xb0, 0xb8] },
                    });

                    // Date — right-aligned inside the row, clear of the scrollbar strip
                    let date_w = TextLabel::estimate_width(&email.date, 9.0);
                    labels.push(TextLabel {
                        text: email.date.clone(),
                        x: list_x + list_w - 14.0 - date_w,
                        y: draw_y + 7.0,
                        font_size: 9.0,
                        color: [0x70, 0x70, 0x75],
                    });

                    // Subject
                    labels.push(TextLabel {
                        text: ellipsize(&email.subject, fit(29.0)),
                        x: list_x + 20.0,
                        y: draw_y + 20.0,
                        font_size: 10.0,
                        color: if !email.read { [0x3a, 0x9a, 0xff] } else { [0x83, 0x83, 0x8a] },
                    });

                    // Snippet — collapse ALL whitespace: CRLF bodies leave bare '\r'
                    // after a plain '\n' replace, and the renderer treats it as a
                    // line break, bleeding preview lines into the next row.
                    let snippet_raw = email.body.split_whitespace().collect::<Vec<_>>().join(" ");
                    let snippet = ellipsize(&snippet_raw, fit(37.0));
                    labels.push(TextLabel {
                        text: snippet,
                        x: list_x + 20.0,
                        y: draw_y + 34.0,
                        font_size: 9.0,
                        color: [0x60, 0x60, 0x65],
                    });
                }
            }
        }

        // 4. Detail View Content (same modal gate as the list labels above)
        if modal_open {
        } else if let Some(selected_id) = self.selected_email_id {
            if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                // Subject Header
                labels.push(TextLabel {
                    text: email.subject.clone(),
                    x: detail_x,
                    y: DETAIL_SUBJECT_Y + MENUBAR_H,
                    font_size: 15.0,
                    color: [0xff, 0xff, 0xff],
                });

                // Metadata
                labels.push(TextLabel { text: format!("From: {}", email.from), x: detail_x, y: DETAIL_FROM_Y + MENUBAR_H, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                labels.push(TextLabel { text: format!("To:   {}", email.to), x: detail_x, y: DETAIL_TO_Y + MENUBAR_H, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
                labels.push(TextLabel { text: format!("Date: {}", email.date), x: detail_x, y: DETAIL_DATE_Y + MENUBAR_H, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

                // Server-attachment chip labels (quads paint in display_list;
                // both sides lay out via detail_chip_rects).
                for (att, (cx, cy, _, _)) in email
                    .remote_attachments
                    .iter()
                    .zip(detail_chip_rects(&email.remote_attachments, detail_x))
                {
                    labels.push(TextLabel {
                        text: detail_chip_label(att),
                        x: cx + 8.0,
                        y: cy + 5.0,
                        font_size: 10.0,
                        color: [0xc8, 0xc8, 0xd2],
                    });
                }
            }
        } else {
            let placeholder = "Select an email to view its content".to_string();
            let est_w = TextLabel::estimate_width(&placeholder, 13.0);
            let px = separator_x + ((w_f32 - separator_x) - est_w) / 2.0;
            labels.push(TextLabel {
                text: placeholder,
                x: px.max(separator_x + 4.0),
                y: h_f32 / 2.0 - 10.0,
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

            labels.push(TextLabel {
                text: self.compose_title.clone(),
                x: modal_x + 15.0,
                y: modal_y + 16.0,
                font_size: 13.0,
                color: [0xff, 0xff, 0xff],
            });

            labels.push(TextLabel { text: "To:".to_string(), x: modal_x + 15.0, y: modal_y + 54.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Cc:".to_string(), x: modal_x + 15.0, y: modal_y + 94.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Bcc:".to_string(), x: modal_x + 15.0, y: modal_y + 134.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Subject:".to_string(), x: modal_x + 15.0, y: modal_y + 174.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

            // Attachment chips: name + "×", clickable to remove (hit-test in
            // handle_mouse_input via the same compose_chip_rects).
            for (path, (cx, cy, _cw, _ch)) in self
                .compose_attachments
                .iter()
                .zip(compose_chip_rects(&self.compose_attachments, modal_x, modal_y))
            {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("attachment");
                labels.push(TextLabel {
                    text: format!("{} \u{00d7}", ellipsize(name, 22)),
                    x: cx + 8.0,
                    y: cy + 6.0,
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
    fn extend<I: IntoIterator<Item = (f32, f32, f32, f32, [f32; 4])>>(&mut self, it: I) {
        for q in it {
            self.push(q);
        }
    }
}

impl Application for ClearEmailApp {
    type Message = AppMessage;

    fn ui_context(&self) -> Option<&cce_ui::context::UiContext> {
        Some(&self.ui_context)
    }

    fn new(_qh: &QueueHandle<EngineState<Self>>, _sender: calloop::channel::Sender<Self::Message>) -> Self {
        cce_ui::scale::set_scale_factor(1.0);

        // The bar: three Dropdowns. Mail is a menu-button dropdown (fixed
        // trigger label, command rows that re-fire on repeat — the
        // custom_display_text mode); folder + account are selection dropdowns.
        // Per-message actions are not here: they live on each card's
        // right-click context menu, where the target is unambiguous.
        // The recessed bar chrome itself is carved in display_list.
        let bar_font = cce_ui::layout::parse_font_string(&cce_ui::layout::menubar_font()).0;
        let mail_menu = Dropdown::new(
            vec!["New Message".to_string(), "Sync Now".to_string(), "Quit".to_string()],
            0,
        )
        .with_custom_display_text("Mail")
        .with_font_family(&bar_font);
        let folder_dropdown = Dropdown::new(
            vec!["Inbox".to_string(), "Sent".to_string(), "Drafts".to_string(), "Trash".to_string()],
            0,
        )
        .with_font_family(&bar_font);

        let mut search_box = TextBox::new(String::new())
            .with_multiline(false)
            .with_draw_bg_border(true)
            .with_placeholder("Search mail — sender, subject or body");
        search_box.font_size = 11.0;

        let email_list = ScrollRegion::new(54.0, 4.0);

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
        .with_font_family(&bar_font);

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

        let btn_compose_send = Button::new(0.0, 0.0, 75.0, 28.0).with_label("Send");
        let btn_compose_cancel = Button::new_reset(0.0, 0.0, 75.0, 28.0).with_label("Cancel");
        let btn_compose_attach = Button::new(0.0, 0.0, 80.0, 28.0).with_label("Attach...");

        // A mailto: argv (this is the x-scheme-handler/mailto handler) opens
        // the compose dialog prefilled. Both text and edit_buffer are set,
        // same as ComposeNew/Reply: ComposeSend reads whichever side the
        // editing flag selects, so a box left untouched must agree with one
        // the user clicked into.
        let mailto = std::env::args().nth(1).and_then(|a| parse_mailto(&a));
        let (compose_open, compose_title) = match &mailto {
            Some(m) => {
                compose_to.text = m.to.clone();
                compose_to.edit_buffer = m.to.clone();
                compose_cc.text = m.cc.clone();
                compose_cc.edit_buffer = m.cc.clone();
                compose_bcc.text = m.bcc.clone();
                compose_bcc.edit_buffer = m.bcc.clone();
                compose_subject.text = m.subject.clone();
                compose_subject.edit_buffer = m.subject.clone();
                compose_body.text = m.body.clone();
                compose_body.edit_buffer = m.body.clone();
                (true, "New Message".to_string())
            }
            None => (false, String::new()),
        };


        let emails = if let Some(acc) = accounts.get(selected_account_idx) {
            load_emails_for_account(&acc.email)
        } else {
            Vec::new()
        };

        let mut app = Self {
            last_sync_start: None,
            secret_retry_at: None,
            secret_missing_reported: false,
            backfill_at: None,
            keys: EmailKeys::load(),
            mail_menu,
            folder_dropdown,
            account_dropdown,
            search_box,
            email_list,
            email_buttons: Vec::new(),
            detail_body,
            compose_to,
            compose_cc,
            compose_bcc,
            compose_subject,
            compose_body,
            compose_attachments: Vec::new(),
            btn_compose_send,
            btn_compose_cancel,
            btn_compose_attach,
            accounts,
            selected_account_idx,
            emails,
            current_folder: Folder::Inbox,
            selected_email_id: None,
            body_scroll: 0.0,
            body_content_h: 0.0,
            detail_hovered: false,
            body_sb_dragging: false,
            body_sb_drag_offset: 0.0,
            list_w: load_list_w().unwrap_or(LIST_W_DEFAULT),
            split_dragging: false,
            context_menu_actions: Vec::new(),
            search_open: false,
            compose_open,
            compose_title,
            status_message: None,
            sender: _sender.clone(),
            width: 1000,
            height: 600,
            scale_factor: 1.0,
            font_system: cce_ui::create_font_system(),
            needs_rebuild: true,
            ui_context: UiContext::new(),
        };

        // The first sync goes through start_sync like every other one, so it
        // gets the same keyring refresh and missing-password guard. It used to
        // call sync_imap inline here — the one sync that bypassed both, and
        // precisely the one that fails when the vault is not up yet.
        app.start_sync(true);
        app
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
                self.current_folder = f;
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                // Folders refresh from the server on entry (throttled).
                self.start_sync(false);
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectEmail(id) => {
                // A draft doesn't open in the read pane — it resumes in the
                // compose dialog and leaves the folder (Cancel re-files it,
                // Send delivers it, a send failure re-files it too).
                if let Some(pos) = self.emails.iter().position(|e| e.id == id && e.folder == "drafts") {
                    let draft = self.emails.remove(pos);
                    self.save_emails();
                    self.clear_compose();
                    for (tb, v) in [
                        (&mut self.compose_to, &draft.to),
                        (&mut self.compose_cc, &draft.cc),
                        (&mut self.compose_bcc, &draft.bcc),
                        (&mut self.compose_subject, &draft.subject),
                        (&mut self.compose_body, &draft.body),
                    ] {
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
                let mut push_seen_uid = None;
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
                        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                            self.status_message = Some(StatusToast::info(format!("Fetching {}...", att.name), 4.0));
                            fetch_attachment(acc.clone(), folder, uid, att, self.sender.clone());
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
                        let mut cmd = std::process::Command::new("xdg-open");
                        cmd.arg(&path);
                        let _ = cce_ui::process::spawn_detached(cmd);
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
                        self.compose_to.text = email.from.clone();
                        self.compose_to.edit_buffer = email.from.clone();
                        self.compose_subject.text = if email.subject.starts_with("Re:") {
                            email.subject.clone()
                        } else {
                            format!("Re: {}", email.subject)
                        };
                        self.compose_subject.edit_buffer = self.compose_subject.text.clone();

                        let reply_intro = format!("\n\nOn {}, {} wrote:\n> {}", email.date, email.from, email.body.replace('\n', "\n> "));
                        self.compose_body.text = reply_intro.clone();
                        self.compose_body.edit_buffer = reply_intro;
                        self.compose_body.cursor_idx = 0;
                        self.compose_title = "Reply".to_string();
                        self.compose_open = true;
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::DeleteSelected => {
                if let Some(id) = self.selected_email_id {
                    let mut permanently_deleted = false;
                    let mut server_uid = None;
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        if email.folder == "trash" {
                            permanently_deleted = true;
                        } else {
                            if email.folder == "inbox" {
                                server_uid = email.uid;
                            }
                            email.folder = "trash".to_string();
                        }
                    }
                    if let Some(uid) = server_uid {
                        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                            delete_on_server(acc.clone(), uid, self.sender.clone());
                        }
                    }
                    if permanently_deleted {
                        self.emails.retain(|e| e.id != id);
                    }
                    self.save_emails();
                    self.selected_email_id = None;
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
                    let mut push = None;
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        email.read = !email.read;
                        if email.folder == "inbox" {
                            push = email.uid.map(|u| (u, email.read));
                        }
                    }
                    if let Some((uid, seen)) = push {
                        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                            set_seen_on_server(acc.clone(), uid, seen, self.sender.clone());
                        }
                    }
                    self.save_emails();
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectAccount(idx) => {
                self.selected_account_idx = idx;
                self.account_dropdown.selected = idx.min(self.accounts.len().saturating_sub(1));
                if let Some(email) = self.accounts.get(idx).map(|a| a.email.clone()) {
                    self.emails = load_emails_for_account(&email);
                    save_selected_account_email(&email);
                    self.start_sync(true);
                } else {
                    self.emails = Vec::new();
                }
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ManageAccounts => {
                let mut cmd = std::process::Command::new("cce-system-interface");
                cmd.arg("accounts");
                let _ = cce_ui::process::spawn_detached(cmd);
                self.status_message = Some(StatusToast::info("Opening System Settings...", 4.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SyncNow => {
                self.start_sync(true);
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
            AppMessage::EmailsSynced(email, folders) => {
                let remaining: usize = folders.iter().map(|f| f.remaining).sum();
                let fetched: usize = folders.iter().map(|f| f.fetched.len()).sum();
                let active = self
                    .accounts
                    .get(self.selected_account_idx)
                    .map(|a| a.email.clone());

                if active.as_deref() == Some(email.as_str()) {
                    let prior = std::mem::take(&mut self.emails);
                    self.emails = merge_sync(prior, &folders);
                    save_emails_for_account(&email, &self.emails);
                } else if active.is_some() {
                    // A background account: fold into its cache on disk only.
                    let prior = load_emails_for_account(&email);
                    save_emails_for_account(&email, &merge_sync(prior, &folders));
                }

                // Keep pulling while history is still coming down. Each pass
                // is already saved, so this can stop at any point without
                // losing what arrived.
                if remaining > 0 {
                    self.backfill_at = Some(
                        std::time::Instant::now()
                            + std::time::Duration::from_secs(BACKFILL_DELAY_SECS),
                    );
                    self.status_message = Some(StatusToast::info(
                        format!("Fetched {} — {} older messages still to come...", fetched, remaining),
                        4.0,
                    ));
                } else {
                    self.backfill_at = None;
                }

                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::UpdateAccountTokens(email, access_token, expiry) => {
                if let Some(acc) = self.accounts.iter_mut().find(|a| a.email == email) {
                    acc.access_token = access_token;
                    acc.token_expiry = expiry;
                    save_accounts(&self.accounts);
                }
            }
        }
    }

    fn tick(&mut self, dt: f32, needs_rebuild: &mut bool) {
        // Pump the widget tick walk: animating widgets (dropdown menus'
        // expand/contract) register as tick receivers and report changed
        // until their transition lands — without this a closing menu freezes
        // fully open.
        if self.ui_context.tick(dt) {
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
        // when its results land, so nothing is requested here.
        if self.backfill_at.is_some_and(|t| std::time::Instant::now() >= t) {
            self.backfill_at = None;
            self.start_sync(true);
        }

        // Only info toasts expire; an error stays until clicked or replaced.
        if let Some(StatusToast::Info { ref mut ttl, .. }) = self.status_message {
            *ttl -= dt;
            if *ttl <= 0.0 {
                self.status_message = None;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
        }
    }

    fn display_list(&mut self, size: LogicalSize, scale: f64) -> Option<cce_ui::scene::paint::DisplayList> {
        self.register_dispatch_roots();
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

        let current_folder_str = match self.current_folder {
            Folder::Inbox => "inbox",
            Folder::Sent => "sent",
            Folder::Drafts => "drafts",
            Folder::Trash => "trash",
        };

        let list_x = LIST_X;
        let (list_w, separator_x, detail_x) = self.split_geom();

        if self.needs_rebuild || size_changed {
            // Bar layout — every open dropdown is a popover: registration
            // feeds the dl-text occlusion clamp, and the popover pass at the
            // end of this function draws it on top of everything.
            self.ui_context.clear_popovers();
            if self.mail_menu.popover_rect().is_some() {
                self.ui_context.register_popover(&mut self.mail_menu);
            }
            if self.folder_dropdown.popover_rect().is_some() {
                self.ui_context.register_popover(&mut self.folder_dropdown);
            }
            if self.account_dropdown.popover_rect().is_some() {
                self.ui_context.register_popover(&mut self.account_dropdown);
            }
            // Folder rows and trigger both carry the live inbox unread count
            // (options[0]), where the old bar title folded the badge in.
            let inbox_unread = self.emails.iter().filter(|e| e.folder == "inbox" && !e.read).count();
            self.folder_dropdown.options[0] = if inbox_unread > 0 {
                format!("Inbox ({})", inbox_unread)
            } else {
                "Inbox".to_string()
            };
            self.folder_dropdown.selected = match self.current_folder {
                Folder::Inbox => 0,
                Folder::Sent => 1,
                Folder::Drafts => 2,
                Folder::Trash => 3,
            };
            // Menus left; selectors right at fixed offsets from the edge —
            // anchoring to the folder label would make the account switcher
            // drift as the folder name changes length.
            self.mail_menu.set_rect(8.0, 5.0, 80.0, 26.0);
            self.folder_dropdown.set_rect(w_f32 - 142.0, 5.0, 134.0, 26.0);
            self.account_dropdown.set_rect(w_f32 - 360.0, 5.0, 210.0, 26.0);

            cce_ui::scale::set_scale_factor(scale as f32);

            // The search box spans the band when open, and parks off-screen
            // when closed so a stale rect can't be hit by anything that still
            // routes to it.
            if self.search_open {
                self.search_box.set_rect(list_x, SEARCH_ROW_Y + MENUBAR_H, list_w, SEARCH_ROW_H);
            } else {
                self.search_box.set_rect(-9999.0, -9999.0, 0.0, 0.0);
            }

            // Get filtered emails count for bounds setup
            let list_count = self.emails.iter()
                .filter(|e| e.folder == current_folder_str)
                .filter(|e| {
                    let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
                    let search_lower = search_text.to_lowercase();
                    if search_lower.is_empty() {
                        true
                    } else {
                        e.from.to_lowercase().contains(&search_lower) ||
                        e.subject.to_lowercase().contains(&search_lower) ||
                        e.body.to_lowercase().contains(&search_lower)
                    }
                })
                .count();

            let (list_top, list_h) = self.list_geom();
            self.email_list.set_rect(list_x, list_top, list_w, list_h);
            self.email_list.update_bounds(list_count, list_top, list_h);

            if self.email_buttons.len() != list_count {
                // Widget ids are globally monotonic and never reused, so the fresh buttons
                // register under NEW ids — the outgoing ones would linger in the registry
                // pointing into this Vec's freed buffer, and the engine walks the whole
                // registry and derefs it on every left press
                // (`close_popovers_missed_by_press`). Drop them before the reallocation.
                let stale: Vec<_> = self.email_buttons.iter().map(|b| b.id()).collect();
                for id in stale {
                    self.ui_context.unregister_widget(id);
                }
                self.email_buttons = (0..list_count)
                    .map(|_| Button::new_list_row(0.0, 0.0, 0.0, 0.0))
                    .collect();
                // register_dispatch_roots() already ran this frame with the OLD
                // buttons; the fresh ids must be registered now or every click on
                // the list is dropped as a stale root (and nothing re-triggers a
                // rebuild, so the list stays dead).
                for btn in self.email_buttons.iter_mut() {
                    let (id, ptr) = (btn.id(), btn.as_ptr_mut());
                    self.ui_context.register_widget(id, ptr);
                }
            }

            // Get selected email state to avoid borrowing self while mutating
            let selected_email_state = if let Some(selected_id) = self.selected_email_id {
                self.emails.iter().find(|e| e.id == selected_id).map(|e| e.body.clone())
            } else {
                None
            };

            // Get filtered email IDs and selection states
            let filtered_email_ids: Vec<(usize, bool)> = {
                let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
                let search_lower = search_text.to_lowercase();
                self.emails.iter()
                    .filter(|e| e.folder == current_folder_str)
                    .filter(|e| {
                        if search_lower.is_empty() {
                            true
                        } else {
                            e.from.to_lowercase().contains(&search_lower) ||
                            e.subject.to_lowercase().contains(&search_lower) ||
                            e.body.to_lowercase().contains(&search_lower)
                        }
                    })
                    .map(|e| (e.id, Some(e.id) == self.selected_email_id))
                    .collect()
            };

            for (idx, &(_email_id, is_selected)) in filtered_email_ids.iter().enumerate() {
                self.email_buttons[idx].selected = is_selected;
                if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                    self.email_buttons[idx].set_rect(list_x, draw_y, list_w, 54.0);
                } else {
                    self.email_buttons[idx].set_rect(-9999.0, -9999.0, 0.0, 0.0);
                }
            }

            // Detail View
            if let Some(body) = selected_email_state {
                let detail_w = (w_f32 - (detail_x + 15.0)).max(100.0);
                let (body_y, body_h) = self.detail_body_geom();
                self.detail_body.set_rect(detail_x, body_y, detail_w, body_h);
                self.detail_body.text = body;
            }

            // Compose inputs layout
            if self.compose_open {
                let (modal_x, modal_y) = compose_modal_origin(w_f32, h_f32);

                self.compose_to.set_rect(modal_x + 80.0, modal_y + 50.0, 400.0, 26.0);
                self.compose_cc.set_rect(modal_x + 80.0, modal_y + 90.0, 400.0, 26.0);
                self.compose_bcc.set_rect(modal_x + 80.0, modal_y + 130.0, 400.0, 26.0);
                self.compose_subject.set_rect(modal_x + 80.0, modal_y + 170.0, 400.0, 26.0);
                self.compose_body.set_rect(modal_x + 15.0, modal_y + 210.0, 470.0, 195.0);

                self.btn_compose_attach.set_rect(modal_x + 15.0, modal_y + 452.0, 80.0, 28.0);
                self.btn_compose_send.set_rect(modal_x + 320.0, modal_y + 452.0, 75.0, 28.0);
                self.btn_compose_cancel.set_rect(modal_x + 410.0, modal_y + 452.0, 75.0, 28.0);
            }



            self.needs_rebuild = false;
        }

        // Now compute `filtered` only for rendering (immutable borrow of self)
        let filtered: Vec<&Email> = {
            let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
            let search_lower = search_text.to_lowercase();
            self.emails.iter()
                .filter(|e| e.folder == current_folder_str)
                .filter(|e| {
                    if search_lower.is_empty() {
                        true
                    } else {
                        e.from.to_lowercase().contains(&search_lower) ||
                        e.subject.to_lowercase().contains(&search_lower) ||
                        e.body.to_lowercase().contains(&search_lower)
                    }
                })
                .collect()
        };

        // 1. General window background (deep slate blue)
        quads.push((0.0, 0.0, w_f32, h_f32, [0.05, 0.05, 0.07, 1.0]));

        // Bar chrome (was MenuBar's recessed paint): the bar is a plateau one
        // step down from the plate, flush with its top-left, so its only wall
        // is the bottom one facing the content — the other three sides are the
        // plate's own rolled edge.
        {
            let depth = cce_ui::layout::bar_wall_width().min(MENUBAR_H * 0.6);
            quads.pc.recess_edges(
                cce_ui::scene::layout::Rect { x: 0.0, y: 0.0, width: w_f32, height: MENUBAR_H },
                (0.0, 0.0, 0.0, 0.0),
                depth,
                (false, false, true, false),
            );
        }
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.mail_menu, &mut *quads.pc);
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.folder_dropdown, &mut *quads.pc);
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.account_dropdown, &mut *quads.pc);

        // 3. Email List Panel Separator
        quads.push((separator_x, MENUBAR_H, 1.0, h_f32 - MENUBAR_H, [0.18, 0.18, 0.22, 1.0]));

        // Search band (only while open) and the list
        if self.search_open {
            self.search_box.prepare_text(&mut self.font_system);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.search_box, &mut *quads.pc);
        }
        {
            let mut list_quads = Vec::new();
            self.email_list.push_quads(&mut list_quads);
            quads.extend(list_quads);
        }

        // Visible List Item Buttons
        for idx in 0..filtered.len() {
            if self.email_list.get_item_draw_y(idx, 0.0).is_some() {
                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.email_buttons[idx], &mut *quads.pc);

                // Blue dot/unread indicator for this row
                if !filtered[idx].read {
                    if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                        quads.push((list_x + 8.0, draw_y + 12.0, 6.0, 6.0, [0.20, 0.45, 0.85, 1.0]));
                    }
                }
            }
        }
        // Scrollbar after the rows so the thumb rides on top of them instead of
        // peeking through the inter-row gaps.
        {
            let mut sb_quads = Vec::new();
            self.email_list.push_scrollbar_quads(&mut sb_quads);
            quads.extend(sb_quads);
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
                        for (cx, cy, cw, ch) in detail_chip_rects(&email.remote_attachments, detail_x) {
                            quads.push((cx, cy, cw, ch, [0.14, 0.14, 0.20, 1.0]));
                            quads.push((cx, cy, cw, 1.0, [0.25, 0.35, 0.50, 0.40]));
                            quads.push((cx, cy + ch - 1.0, cw, 1.0, [0.25, 0.35, 0.50, 0.40]));
                            quads.push((cx, cy, 1.0, ch, [0.25, 0.35, 0.50, 0.40]));
                            quads.push((cx + cw - 1.0, cy, 1.0, ch, [0.25, 0.35, 0.50, 0.40]));
                        }

                        let body_w = (w_f32 - (detail_x + 15.0)).max(100.0);
                        let line_h = 12.0 * 1.4; // get_text_buffer_laid_out's placed-text metric

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

                        // Scrollbar (ScrollRegion's colors) when the body overflows.
                        if let Some((sb_x, track_y, sb_w, track_h, thumb_y, thumb_h)) = self.body_scrollbar_geom() {
                            quads.push((sb_x, track_y, sb_w, track_h, cce_ui::color::scrollbar_track_color()));
                            quads.push((sb_x, thumb_y, sb_w, thumb_h, cce_ui::color::scrollbar_thumb_color()));
                        }

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

        // 5. Compose Dialog Overlay
        if self.compose_open {
            let (modal_x, modal_y) = compose_modal_origin(w_f32, h_f32);

            // Semitransparent modal backdrop
            quads.push((0.0, 0.0, w_f32, h_f32, [0.0, 0.0, 0.0, 0.6]));

            // Modal dialog container
            quads.push((modal_x, modal_y, COMPOSE_W, COMPOSE_H, [0.08, 0.08, 0.12, 1.0]));
            quads.push((modal_x, modal_y, COMPOSE_W, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y + COMPOSE_H - 1.0, COMPOSE_W, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y, 1.0, COMPOSE_H, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x + COMPOSE_W - 1.0, modal_y, 1.0, COMPOSE_H, [0.25, 0.35, 0.50, 0.40]));

            // Attachment chips: quads here, labels in the labels pass — both
            // laid out by compose_chip_rects.
            for (cx, cy, cw, ch) in compose_chip_rects(&self.compose_attachments, modal_x, modal_y) {
                quads.push((cx, cy, cw, ch, [0.14, 0.14, 0.20, 1.0]));
                quads.push((cx, cy, cw, 1.0, [0.25, 0.35, 0.50, 0.40]));
                quads.push((cx, cy + ch - 1.0, cw, 1.0, [0.25, 0.35, 0.50, 0.40]));
                quads.push((cx, cy, 1.0, ch, [0.25, 0.35, 0.50, 0.40]));
                quads.push((cx + cw - 1.0, cy, 1.0, ch, [0.25, 0.35, 0.50, 0.40]));
            }

            self.compose_to.prepare_text(&mut self.font_system);
            self.compose_cc.prepare_text(&mut self.font_system);
            self.compose_bcc.prepare_text(&mut self.font_system);
            self.compose_subject.prepare_text(&mut self.font_system);
            self.compose_body.prepare_text(&mut self.font_system);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_to, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_cc, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_bcc, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_subject, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_body, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose_send, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose_cancel, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose_attach, &mut *quads.pc);
        }



        // Popover pass (the data-editor pattern): geometry and labels on top of
        // everything, exactly where they hit-test; labels carry bounds equal to
        // the overlay rect (the is-overlay-text convention).
        {
            use cce_ui::scene::layout::Rect;
            for &pop_id in &self.ui_context.active_popovers {
                let Some(pop_ptr) = self.ui_context.tree.get_ptr(pop_id) else { continue };
                let popover = unsafe { &*pop_ptr };
                let Some((px2, py2, pw2, ph2)) = popover.popover_rect() else { continue };
                let mut coll = cce_ui::layout::PopoverCollector::new();
                popover.render_popover(&mut coll);
                for &(c, x, y, qw, qh) in &coll.rects {
                    __pc.quad(Rect { x, y, width: qw, height: qh }, c);
                }
                let pop_bounds = Some([px2, py2, px2 + pw2, py2 + ph2]);
                for (content, size, tx, ty, color, font, _bounds) in coll.texts {
                    let color_u8 = [
                        (color[0] * 255.0).clamp(0.0, 255.0) as u8,
                        (color[1] * 255.0).clamp(0.0, 255.0) as u8,
                        (color[2] * 255.0).clamp(0.0, 255.0) as u8,
                    ];
                    __pc.text_with(content, tx, ty, size, color_u8, font, pop_bounds);
                }
            }
        }

        self.emit_text_prims(&mut __pc);

        // The card context menu draws last, over everything. Its labels carry
        // bounds EXACTLY equal to the menu rect: the engine's popover
        // occlusion clamp exempts only an exact match, and pushes this same
        // rect as an overlay so the row text underneath is clamped away.
        if cce_ui::widget::context_menu::is_visible() {
            let (mx, my) = (
                cce_ui::widget::context_menu::x(),
                cce_ui::widget::context_menu::y(),
            );
            let (mw, mh) = (
                cce_ui::widget::context_menu::w(),
                cce_ui::widget::context_menu::h(),
            );
            let menu_bounds = Some([mx, my, mx + mw, my + mh]);
            for (qx, qy, qw, qh, qc) in cce_ui::widget::context_menu::extra_quads() {
                __pc.quad(
                    cce_ui::scene::layout::Rect { x: qx, y: qy, width: qw, height: qh },
                    qc,
                );
            }
            for label in cce_ui::widget::context_menu::text_labels() {
                __pc.text_with(
                    label.text.clone(),
                    label.x,
                    label.y,
                    label.font_size,
                    label.color,
                    None,
                    menu_bounds,
                );
            }
        }

        Some(__pc.finish())
    }

    fn cursor_icon(&self, x: f32, y: f32) -> Option<CursorIcon> {
        // Resize arrows over the split separator's grab band, and for the
        // whole drag — mid-drag the pointer legally outruns the clamped
        // line, and the cursor must not flicker back to the arrow there.
        if !self.compose_open
            && !cce_ui::widget::context_menu::is_visible()
            && (self.split_dragging
                || ((x - self.split_geom().1).abs() <= SPLIT_GRAB_SLOP && y > MENUBAR_H))
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

        // Active split drag: the separator follows the pointer, clamped so
        // neither pane collapses. Clamp into the stored value (not just at
        // paint) so the release persists what the user actually sees.
        if !self.compose_open && self.split_dragging {
            let max_w = (self.width as f32 - LIST_X - LIST_SEP_GAP - LIST_DETAIL_GAP - DETAIL_W_MIN)
                .max(LIST_W_MIN);
            let new_w = (px - LIST_X - LIST_SEP_GAP).clamp(LIST_W_MIN, max_w);
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

        if ctx.propagate_event(&mv, self.mail_menu.id()) { changed = true; }
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

            for btn in &mut self.email_buttons {
                if btn.rect().0 > -9000.0 {
                    if ctx.propagate_event(&mv, btn.id()) { changed = true; }
                }
            }

            // Detail pane: no hover routing — no widgets there any more.
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
                    if (px - separator_x).abs() <= SPLIT_GRAB_SLOP && py > MENUBAR_H {
                        self.split_dragging = true;
                        *needs_rebuild = true;
                        self.needs_rebuild = true;
                        return None;
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
                            let hit = detail_chip_rects(&email.remote_attachments, detail_x)
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
            let was_open = self.account_dropdown.open;
            if ctx.propagate_event(&ev, self.account_dropdown.id()) {
                if !was_open && self.account_dropdown.open {
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
                    self.account_dropdown.options = account_dropdown_options(&self.accounts);
                    self.account_dropdown.selected = new_idx;
                    if changed_account {
                        // The account under the selection vanished — switch to
                        // what the trigger now shows.
                        *needs_rebuild = true;
                        self.needs_rebuild = true;
                        return Some(AppMessage::SelectAccount(new_idx));
                    }
                }
                let mut msg_out = None;
                if self.account_dropdown.take_change() {
                    let idx = self.account_dropdown.selected;
                    if idx < self.accounts.len() {
                        if idx != self.selected_account_idx {
                            msg_out = Some(AppMessage::SelectAccount(idx));
                        }
                    } else {
                        // The trailing "Manage Accounts…" pseudo-entry: not an
                        // account — restore the trigger to the active one.
                        self.account_dropdown.selected = self.selected_account_idx;
                        msg_out = Some(AppMessage::ManageAccounts);
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return msg_out;
            }
        }

        // The menu-button dropdowns — command rows; take_change re-fires on
        // the same row (custom_display_text mode), so Sync Now works twice.
        if ctx.propagate_event(&ev, self.mail_menu.id()) {
            if self.mail_menu.take_change() {
                msg_out = match self.mail_menu.selected {
                    0 => Some(AppMessage::ComposeNew),
                    1 => Some(AppMessage::SyncNow),
                    _ => Some(AppMessage::Quit),
                };
            }
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return msg_out;
        }
        // Folder switcher.
        if ctx.propagate_event(&ev, self.folder_dropdown.id()) {
            if self.folder_dropdown.take_change() {
                msg_out = Some(AppMessage::SwitchFolder(match self.folder_dropdown.selected {
                    0 => Folder::Inbox,
                    1 => Folder::Sent,
                    2 => Folder::Drafts,
                    _ => Folder::Trash,
                }));
            }
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return msg_out;
        }

        if self.compose_open {
            if ctx.propagate_event(&ev, self.compose_to.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.compose_to); }
            }
            if ctx.propagate_event(&ev, self.compose_cc.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.compose_cc); }
            }
            if ctx.propagate_event(&ev, self.compose_bcc.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.compose_bcc); }
            }
            if ctx.propagate_event(&ev, self.compose_subject.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.compose_subject); }
            }
            if ctx.propagate_event(&ev, self.compose_body.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.compose_body); }
            }

            if ctx.propagate_event(&ev, self.btn_compose_send.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_compose_send.take_click() {
                    msg_out = Some(AppMessage::ComposeSend);
                }
            }
            if ctx.propagate_event(&ev, self.btn_compose_cancel.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_compose_cancel.take_click() {
                    msg_out = Some(AppMessage::ComposeCancel);
                }
            }
            if ctx.propagate_event(&ev, self.btn_compose_attach.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_compose_attach.take_click() {
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

                if px < modal_x || px > modal_x + COMPOSE_W || py < modal_y || py > modal_y + COMPOSE_H {
                    ctx.clear_focus();
                    self.compose_to.unfocus();
                    self.compose_cc.unfocus();
                    self.compose_bcc.unfocus();
                    self.compose_subject.unfocus();
                    self.compose_body.unfocus();
                    changed = true;
                }
            }
        } else {
            // Search input — only reachable while the band is open.
            if self.search_open && ctx.propagate_event(&ev, self.search_box.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.search_box); }
                if self.search_box.take_change() {
                    msg_out = Some(AppMessage::SearchChanged);
                }
            } else if state == ElementState::Pressed && button == MouseButton::Left {
                ctx.clear_focus();
                self.search_box.unfocus();
                // Clicking away from an EMPTY search collapses the band —
                // an unfocused box with no query is dead chrome. One holding
                // a query stays: it is the visible reason the list is short.
                if self.search_open && self.search_box.text.is_empty() && self.search_box.edit_buffer.is_empty() {
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
                let current_folder_str = match self.current_folder {
                    Folder::Inbox => "inbox",
                    Folder::Sent => "sent",
                    Folder::Drafts => "drafts",
                    Folder::Trash => "trash",
                };
                let search_text = if self.search_box.editing { &self.search_box.edit_buffer } else { &self.search_box.text };
                let search_lower = search_text.to_lowercase();
                let filtered: Vec<&Email> = self.emails.iter()
                    .filter(|e| e.folder == current_folder_str)
                    .filter(|e| {
                        if search_lower.is_empty() {
                            true
                        } else {
                            e.from.to_lowercase().contains(&search_lower) ||
                            e.subject.to_lowercase().contains(&search_lower) ||
                            e.body.to_lowercase().contains(&search_lower)
                        }
                    })
                    .collect();

                for (idx, email) in filtered.iter().enumerate() {
                    if idx < self.email_buttons.len() {
                        let btn = &mut self.email_buttons[idx];
                        if btn.rect().0 > -9000.0 {
                            if ctx.propagate_event(&ev, btn.id()) {
                                changed = true;
                                if state == ElementState::Released && btn.take_click() {
                                    msg_out = Some(AppMessage::SelectEmail(email.id));
                                }
                            }
                        }
                    }
                }
            }

            // The detail pane takes no events at all now: Reply/Delete/Mark
            // Read/Unread moved to the Message menu, and detail_body is a
            // read-only boxed-text render — focusing the TextBox only let you
            // invisibly edit the display copy.
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

        // Detail-pane body scroll.
        if !self.compose_open && self.selected_email_id.is_some() {
            if px > separator_x {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * 24.0,
                    MouseScrollDelta::PixelDelta(pos) => -pos.y as f32,
                };
                let (_, body_h) = self.detail_body_geom();
                let max = (self.body_content_h - body_h).max(0.0);
                let old = self.body_scroll;
                self.body_scroll = (self.body_scroll + dy).clamp(0.0, max);
                if (self.body_scroll - old).abs() > 0.01 {
                    changed = true;
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
        let ctx = &mut self.ui_context;

        if self.compose_open {
            if self.compose_to.editing {
                if ctx.propagate_event(&kev, self.compose_to.id()) { handled = true; }
            } else if self.compose_cc.editing {
                if ctx.propagate_event(&kev, self.compose_cc.id()) { handled = true; }
            } else if self.compose_bcc.editing {
                if ctx.propagate_event(&kev, self.compose_bcc.id()) { handled = true; }
            } else if self.compose_subject.editing {
                if ctx.propagate_event(&kev, self.compose_subject.id()) { handled = true; }
            } else if self.compose_body.editing {
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
                self.search_box.unfocus();
                self.search_box.text.clear();
                self.search_box.edit_buffer.clear();
                self.search_open = false;
                msg_out = Some(AppMessage::SearchChanged);
                handled = true;
            }

            // General keyboard shortcuts (input.kdl `cce-mail` domain)
            if !handled && event.state == ElementState::Pressed {
                if cce_ui::widget::match_key_shortcut(event, &self.keys.compose) {
                    msg_out = Some(AppMessage::ComposeNew);
                    handled = true;
                } else if !self.search_box.editing
                    && cce_ui::widget::match_key_shortcut(event, &self.keys.open_search)
                {
                    // The !editing guard is load-bearing now the chord is a
                    // bare "/": without it, typing a slash into the open box
                    // would re-match here and never reach the text.
                    self.search_open = true;
                    ctx.set_focused(&mut self.search_box);
                    self.search_box.focus();
                    handled = true;
                }
            }

            // Detail-pane body scroll, hover-scoped like ScrollRegion's keyboard path.
            if !handled
                && self.detail_hovered
                && self.selected_email_id.is_some()
                && !self.search_box.editing
                && event.state == ElementState::Pressed
            {
                let max = (self.body_content_h - body_h).max(0.0);
                let old = self.body_scroll;
                match &event.logical_key {
                    Key::Named(cce_ui::widget::NamedKey::ArrowDown) => self.body_scroll = (self.body_scroll + 24.0).min(max),
                    Key::Named(cce_ui::widget::NamedKey::ArrowUp) => self.body_scroll = (self.body_scroll - 24.0).max(0.0),
                    Key::Named(cce_ui::widget::NamedKey::PageDown) => self.body_scroll = (self.body_scroll + body_h).min(max),
                    Key::Named(cce_ui::widget::NamedKey::PageUp) => self.body_scroll = (self.body_scroll - body_h).max(0.0),
                    Key::Named(cce_ui::widget::NamedKey::Home) => self.body_scroll = 0.0,
                    Key::Named(cce_ui::widget::NamedKey::End) => self.body_scroll = max,
                    _ => {}
                }
                if (self.body_scroll - old).abs() > 0.01 {
                    handled = true;
                }
            }

            // Email/accounts list keyboard scrolling. ScrollRegion scopes itself to
            // hover-or-focus (a row click focuses the region, a press elsewhere
            // unfocuses); the search box owns the keys while editing, and a
            // body-scroll above wins when the detail pane is hovered.
            if !handled && !self.search_box.editing && self.email_list.keyboard(event) {
                handled = true;
            }

            if !handled && self.search_box.editing {
                if ctx.propagate_event(&kev, self.search_box.id()) {
                    handled = true;
                    if self.search_box.take_change() {
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
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter();

    cce_ui::engine::run::<ClearEmailApp>();
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
}
