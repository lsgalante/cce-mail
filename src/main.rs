mod scroll_region;
use scroll_region::ScrollRegion;
use wayland_client::QueueHandle;
use cce_ui::cosmic_text::FontSystem;
use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::widget::{
    MouseButton, ElementState, MouseScrollDelta, KeyEvent, WidgetHost,
    TextBox, Button, TextLabel, Key, MenuController, MenuBar
};
use cce_ui::context::UiContext;
use native_tls::TlsConnector;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Folder {
    Inbox,
    Sent,
    Trash,
    Accounts,
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
    folder: String, // "inbox", "sent", "trash"
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
    EmailsSynced(String, Vec<Email>),
    UpdateAccountTokens(String, Option<String>, Option<u64>),
}

/// App shortcuts, resolved once at startup from input.kdl
/// (`cce-email` domain → `cce-ui` domain), defaulting to the historical keys.
struct EmailKeys {
    compose: String,
    open_search: String,
}

impl EmailKeys {
    fn load() -> Self {
        Self {
            compose: cce_ui::input::app_chord("compose", "ctrl+n"),
            open_search: cce_ui::input::app_chord("open_search", "ctrl+f"),
        }
    }
}

struct ClearEmailApp {
    keys: EmailKeys,

    // Navigation / Sidebar
    btn_compose: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    menubar: cce_ui::widget::Adapted<MenuBar>,
    btn_accounts: cce_ui::widget::Adapted<cce_ui::widget::Button>,

    // Search and List View
    search_box: cce_ui::widget::Adapted<TextBox>,
    email_list: ScrollRegion,
    email_buttons: Vec<cce_ui::widget::Adapted<cce_ui::widget::Button>>,

    // Details View
    btn_reply: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_delete: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_unread: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    detail_body: cce_ui::widget::Adapted<TextBox>,

    // Compose Dialog
    compose_to: cce_ui::widget::Adapted<TextBox>,
    compose_subject: cce_ui::widget::Adapted<TextBox>,
    compose_body: cce_ui::widget::Adapted<TextBox>,
    btn_compose_send: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_compose_cancel: cce_ui::widget::Adapted<cce_ui::widget::Button>,

    // Accounts (view/switch only — management lives in cce-system-interface)
    accounts: Vec<AccountInfo>,
    selected_account_idx: usize,
    btn_manage_accounts: cce_ui::widget::Adapted<cce_ui::widget::Button>,

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
    compose_open: bool,
    compose_title: String,
    /// When the most recent IMAP sync was spawned — folder switches re-sync
    /// through [`Self::start_sync`], throttled against tab-hopping.
    last_sync_start: Option<std::time::Instant>,
    status_message: Option<(String, f32)>, // (message, timer)
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
        let Ok(entry) = keyring::Entry::new("cce-email", &acc.email) else {
            continue;
        };
        if acc.password.is_empty() {
            if let Ok(p) = entry.get_password() {
                acc.password = p;
                acc.keyring_backed = true;
            }
        } else if entry.set_password(&acc.password).is_ok() {
            acc.keyring_backed = true;
            migrated = true;
        }
    }
    migrated
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
    cce_ui::config::cce_config_dir().join("cce-email-account.txt")
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

const FETCH_COUNT: usize = 50;

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
/// surfaces as a Status toast and yields None.
fn open_imap_session(
    account: &mut AccountInfo,
    sender: &calloop::channel::Sender<AppMessage>,
    verbose: bool,
) -> Option<imap::Session<native_tls::TlsStream<std::net::TcpStream>>> {
    macro_rules! say {
        ($msg:expr) => {
            if verbose {
                let _ = sender.send(AppMessage::Status($msg));
            }
        };
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
                say!(format!("OAuth Refresh Failed: {}", e));
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
            say!("Failed to create TLS connector".to_string());
            return None;
        }
    };

    let client = match imap::connect((domain, port), domain, &tls) {
        Ok(c) => c,
        Err(e) => {
            say!(format!("IMAP Connection failed: {}", e));
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
                say!(format!("IMAP OAuth Login failed: {}", e));
                None
            }
        }
    } else {
        match client.login(&account.email, &account.password) {
            Ok(s) => Some(s),
            Err((e, _)) => {
                say!(format!("IMAP Login failed: {}", e));
                None
            }
        }
    }
}

/// Namespaces sent-folder ids away from inbox sequence numbers (both are
/// fetch-time seq numbers; ids must stay unique across the merged list).
const SENT_ID_OFFSET: usize = 1_000_000;

/// Fetch the newest [`FETCH_COUNT`] messages of one mailbox with the two-pass
/// BODYSTRUCTURE strategy (pass 1 headers/flags/structure, pass 2 text parts
/// grouped by section, capped full-message fallback). Returns None when the
/// mailbox can't be selected or a whole-mailbox step fails; per-message body
/// fetch failures degrade to empty bodies. `verbose` gates the Status toasts
/// (the sent fetch rides quietly behind the inbox one).
fn fetch_mailbox(
    session: &mut imap::Session<native_tls::TlsStream<std::net::TcpStream>>,
    sender: &calloop::channel::Sender<AppMessage>,
    account_email: &str,
    mailbox: &str,
    folder_tag: &str,
    id_offset: usize,
    verbose: bool,
) -> Option<Vec<Email>> {
    macro_rules! say {
        ($msg:expr) => {
            if verbose {
                let _ = sender.send(AppMessage::Status($msg));
            }
        };
    }
    if let Err(e) = session.select(mailbox) {
        say!(format!("Failed to select {}: {}", mailbox, e));
        return None;
    }

    let mut search_results: Vec<u32> = match session.search("ALL") {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            say!(format!("IMAP Search failed: {}", e));
            return None;
        }
    };
    search_results.sort();

    let total = search_results.len();
    if total == 0 {
        return Some(Vec::new());
    }

    // Pass 1: flags + headers + BODYSTRUCTURE — no body bytes yet, so
    // attachments never ride along. PEEK semantics don't matter here
    // (headers/structure don't set \Seen), but flags come from the server.
    let start_idx = total.saturating_sub(FETCH_COUNT);
    let range = &search_results[start_idx..total];
    let query_seq = range.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
    say!(format!("Fetching {} messages...", range.len()));

    struct Pending {
        seq: u32,
        uid: Option<u32>,
        read: bool,
        from: String,
        to: String,
        subject: String,
        date: String,
        part: Option<TextPartSpec>,
        body: String,
    }
    let parser = mail_parser::MessageParser::default();
    let mut pending: Vec<Pending> = Vec::new();
    match session.fetch(&query_seq, "(UID FLAGS RFC822.HEADER BODYSTRUCTURE)") {
        Ok(fetches) => {
            for fetch in fetches.iter() {
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
                    seq: fetch.message,
                    uid: fetch.uid,
                    read,
                    from,
                    to,
                    subject,
                    date,
                    part: fetch.bodystructure().and_then(find_text_part),
                    body: String::new(),
                });
            }
        }
        Err(e) => {
            say!(format!("IMAP Fetch failed: {}", e));
            return None;
        }
    }

    // Pass 2: fetch just the chosen text part, grouped by section path so a
    // typical mailbox needs only 1-2 more round trips; capped so a giant
    // text part can't stall the sync either.
    let mut groups: std::collections::HashMap<Vec<u32>, Vec<u32>> = std::collections::HashMap::new();
    for p in &pending {
        if let Some(spec) = &p.part {
            groups.entry(spec.path.clone()).or_default().push(p.seq);
        }
    }
    for (path, seqs) in groups {
        let seq_set = seqs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(",");
        let section = section_str(&path);
        let query = format!("BODY.PEEK[{}]<0.{}>", section, PART_FETCH_CAP);
        let section_path = imap_proto::types::SectionPath::Part(path, None);
        match session.fetch(&seq_set, &query) {
            Ok(fetches) => {
                for fetch in fetches.iter() {
                    let Some(bytes) = fetch.section(&section_path) else { continue };
                    if let Some(p) = pending.iter_mut().find(|p| p.seq == fetch.message) {
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
                say!(format!("IMAP part fetch failed: {}", e));
            }
        }
    }

    // Fallback: no usable text part in the structure (or the walk failed) —
    // one capped full-message fetch for those stragglers.
    let no_part: Vec<u32> = pending.iter().filter(|p| p.part.is_none()).map(|p| p.seq).collect();
    if !no_part.is_empty() {
        let seq_set = no_part.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(",");
        let query = format!("BODY.PEEK[]<0.{}>", PART_FETCH_CAP * 4);
        match session.fetch(&seq_set, &query) {
            Ok(fetches) => {
                for fetch in fetches.iter() {
                    let Some(raw) = fetch.body() else { continue };
                    if let Some(p) = pending.iter_mut().find(|p| p.seq == fetch.message) {
                        p.body = parser.parse(raw).map(|m| extract_body(&m)).unwrap_or_default();
                    }
                }
            }
            Err(e) => {
                say!(format!("IMAP fallback fetch failed: {}", e));
            }
        }
    }

    let mut fetched: Vec<Email> = pending
        .into_iter()
        .map(|p| Email {
            id: p.seq as usize + id_offset,
            uid: p.uid,
            from: p.from,
            to: p.to,
            subject: p.subject,
            body: p.body,
            date: p.date,
            read: p.read,
            folder: folder_tag.to_string(),
        })
        .collect();

    fetched.reverse(); // Newest first
    Some(fetched)
}

fn sync_imap(mut account: AccountInfo, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        if is_mock_account(&account) {
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, true) else {
            return;
        };

        let _ = sender.send(AppMessage::Status("Syncing Inbox...".to_string()));

        let Some(mut fetched) = fetch_mailbox(
            &mut session,
            &sender,
            &account.email,
            "INBOX",
            "inbox",
            0,
            true,
        ) else {
            let _ = session.logout();
            return;
        };

        // Sent rides along quietly: Gmail's name first, the conventional one
        // second; a server with neither just syncs the inbox.
        for mailbox in ["[Gmail]/Sent Mail", "Sent"] {
            if let Some(sent) = fetch_mailbox(
                &mut session,
                &sender,
                &account.email,
                mailbox,
                "sent",
                SENT_ID_OFFSET,
                false,
            ) {
                fetched.extend(sent);
                break;
            }
        }

        let _ = sender.send(AppMessage::EmailsSynced(account.email.clone(), fetched));
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
            let _ = sender.send(AppMessage::Status(format!("Failed to select INBOX: {}", e)));
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
            let _ = sender.send(AppMessage::Status(format!("Server delete failed: {}", e)));
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

/// Push a message's read state to the server (INBOX, by UID). Fully silent:
/// this fires on every message open, so no Connecting/success toasts, and a
/// failed push is self-healing — the EmailsSynced merge keeps locally-read
/// mail read regardless of the server flag until a later push converges.
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

fn send_smtp(mut account: AccountInfo, to: String, subject: String, body: String, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        if account.password == "mock_password" {
            let _ = sender.send(AppMessage::Status("Mock Email Sent Successfully".to_string()));
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
                    let _ = sender.send(AppMessage::Status(format!("OAuth Refresh Failed: {}", e)));
                    return;
                }
            }
        }

        let domain = match account.smtp.split(':').next() {
            Some(d) => d,
            None => {
                let _ = sender.send(AppMessage::Status("Invalid SMTP hostname".to_string()));
                return;
            }
        };
        let port = match account.smtp.split(':').nth(1).and_then(|p| p.parse::<u16>().ok()) {
            Some(p) => p,
            None => 465,
        };

        let _ = sender.send(AppMessage::Status("Sending SMTP mail...".to_string()));

        let email = match Message::builder()
            .from(match account.email.parse() {
                Ok(f) => f,
                Err(e) => {
                    let _ = sender.send(AppMessage::Status(format!("Invalid From Address: {}", e)));
                    return;
                }
            })
            .to(match to.parse() {
                Ok(t) => t,
                Err(e) => {
                    let _ = sender.send(AppMessage::Status(format!("Invalid Recipient: {}", e)));
                    return;
                }
            })
            .subject(subject)
            .body(body) {
                Ok(m) => m,
                Err(e) => {
                    let _ = sender.send(AppMessage::Status(format!("Failed to build message: {}", e)));
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
                let _ = sender.send(AppMessage::Status("Email Sent Successfully".to_string()));
            }
            Err(e) => {
                let _ = sender.send(AppMessage::Status(format!("SMTP send failed: {}", e)));
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
        },
        Email {
            id: 3,
            from: "Codeberg CI <ci@codeberg.org>".to_string(),
            to: "lsgalante@cce-ui.org".to_string(),
            subject: "Build Success: cce-email (main)".to_string(),
            body: "Repository: lsgalante/cce-email\nBranch: main\nCommit: da8cf20fcb2c993c1c048ced4020\nStatus: SUCCESS\n\nAll unit tests passed. Binary compiled in 48.2s.\n\n---\nCodeberg Actions".to_string(),
            date: "June 3".to_string(),
            read: true,
            folder: "inbox".to_string(),
            uid: None,
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
        },
    ]
}

impl ClearEmailApp {
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
        let (id, ptr) = (self.compose_subject.id(), self.compose_subject.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.compose_body.id(), self.compose_body.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_compose.id(), self.btn_compose.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.menubar.id(), self.menubar.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_accounts.id(), self.btn_accounts.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_compose_send.id(), self.btn_compose_send.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_compose_cancel.id(), self.btn_compose_cancel.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_reply.id(), self.btn_reply.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_delete.id(), self.btn_delete.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_unread.id(), self.btn_unread.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_manage_accounts.id(), self.btn_manage_accounts.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        for btn in self.email_buttons.iter_mut() {
            let (id, ptr) = (btn.id(), btn.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
        }
    }


    /// Spawn an IMAP sync for the selected account. Unforced calls (folder
    /// switches) are throttled so tab-hopping doesn't stack connections;
    /// forced calls (explicit account selection) always run.
    fn start_sync(&mut self, force: bool) {
        const MIN_SYNC_GAP: std::time::Duration = std::time::Duration::from_secs(30);
        if !force && self.last_sync_start.is_some_and(|t| t.elapsed() < MIN_SYNC_GAP) {
            return;
        }
        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
            sync_imap(acc.clone(), self.sender.clone());
            self.last_sync_start = Some(std::time::Instant::now());
        }
    }

    fn save_emails(&self) {
        if let Some(acc) = self.accounts.get(self.selected_account_idx) {
            save_emails_for_account(&acc.email, &self.emails);
        }
    }

    /// Detail-pane body scrollbar geometry, mirroring `ScrollRegion::scrollbar_geom`:
    /// (sb_x, track_y, sb_w, track_h, thumb_y, thumb_h). None when the body fits
    /// (no scrollbar drawn). The single source for display_list and the drag path.
    fn body_scrollbar_geom(&self) -> Option<(f32, f32, f32, f32, f32, f32)> {
        let w = self.width as f32;
        let h = self.height as f32;
        let body_h = (h - 190.0 - MENUBAR_H).max(100.0);
        let max_scroll = (self.body_content_h - body_h).max(0.0);
        if max_scroll <= 0.0 {
            return None;
        }
        let sb_w = cce_ui::layout::scrollbar_width();
        let sb_x = w - sb_w - 4.0;
        let thumb_h = (body_h * body_h / self.body_content_h).clamp(20.0, body_h);
        let thumb_y = 170.0 + MENUBAR_H + (self.body_scroll / max_scroll) * (body_h - thumb_h);
        Some((sb_x, 170.0 + MENUBAR_H, sb_w, body_h, thumb_y, thumb_h))
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
        let body_h = (self.height as f32 - 190.0 - MENUBAR_H).max(100.0);
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

        let list_x = 10.0;
        let detail_x = list_x + 325.0;
        let separator_x = list_x + 310.0;

        // Widget text rides along with chrome in display_list's paint_root_into
        // walk — only app-composed labels are emitted here.

        // 3. Email List Labels / Accounts list labels.
        // Skipped while a modal is up: text always renders above geometry, and these
        // hand-emitted labels carry no bounds, so they'd bleed straight through the
        // modal panel (the popover-occlusion clamp only knows registered popovers).
        let modal_open = self.compose_open;
        if modal_open {
        } else if self.current_folder == Folder::Accounts {
            for (idx, acc) in self.accounts.iter().enumerate() {
                if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                    labels.push(TextLabel {
                        text: acc.email.clone(),
                        x: list_x + 20.0,
                        y: draw_y + 12.0,
                        font_size: 11.0,
                        color: [0xff, 0xff, 0xff],
                    });

                    labels.push(TextLabel {
                        text: if acc.is_default { "Default Account".to_string() } else { "Secondary Account".to_string() },
                        x: list_x + 20.0,
                        y: draw_y + 28.0,
                        font_size: 9.0,
                        color: if acc.is_default { [0x3a, 0xff, 0x80] } else { [0x70, 0x70, 0x75] },
                    });
                }
            }
        } else {
            let current_folder_str = match self.current_folder {
                Folder::Inbox => "inbox",
                Folder::Sent => "sent",
                Folder::Trash => "trash",
                _ => "inbox",
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
                        text: ellipsize(&row_head, 21),
                        x: list_x + 20.0,
                        y: draw_y + 6.0,
                        font_size: 11.0,
                        color: if !email.read { [0xff, 0xff, 0xff] } else { [0xb0, 0xb0, 0xb8] },
                    });

                    // Date — right-aligned inside the row, clear of the scrollbar strip
                    let date_w = TextLabel::estimate_width(&email.date, 9.0);
                    labels.push(TextLabel {
                        text: email.date.clone(),
                        x: list_x + 300.0 - 14.0 - date_w,
                        y: draw_y + 7.0,
                        font_size: 9.0,
                        color: [0x70, 0x70, 0x75],
                    });

                    // Subject
                    labels.push(TextLabel {
                        text: ellipsize(&email.subject, 29),
                        x: list_x + 20.0,
                        y: draw_y + 20.0,
                        font_size: 10.0,
                        color: if !email.read { [0x3a, 0x9a, 0xff] } else { [0x83, 0x83, 0x8a] },
                    });

                    // Snippet — collapse ALL whitespace: CRLF bodies leave bare '\r'
                    // after a plain '\n' replace, and the renderer treats it as a
                    // line break, bleeding preview lines into the next row.
                    let snippet_raw = email.body.split_whitespace().collect::<Vec<_>>().join(" ");
                    let snippet = ellipsize(&snippet_raw, 37);
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
        } else if self.current_folder == Folder::Accounts {
            if self.selected_account_idx < self.accounts.len() {
                let acc = &self.accounts[self.selected_account_idx];

                // Subject Header (Account email)
                labels.push(TextLabel {
                    text: acc.email.clone(),
                    x: detail_x,
                    y: 60.0 + MENUBAR_H,
                    font_size: 15.0,
                    color: [0xff, 0xff, 0xff],
                });

                // Settings details
                labels.push(TextLabel { text: format!("Incoming Server (IMAP): {}", acc.imap), x: detail_x, y: 95.0 + MENUBAR_H, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                labels.push(TextLabel { text: format!("Outgoing Server (SMTP): {}", acc.smtp), x: detail_x, y: 120.0 + MENUBAR_H, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                let auth_text = if acc.is_oauth {
                    "Authentication:          OAuth2 (Google)"
                } else {
                    "Authentication:          SSL/TLS, Normal Password"
                };
                labels.push(TextLabel { text: auth_text.to_string(), x: detail_x, y: 145.0 + MENUBAR_H, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
                labels.push(TextLabel {
                    text: format!("Default Account:         {}", if acc.is_default { "Yes" } else { "No" }),
                    x: detail_x,
                    y: 170.0 + MENUBAR_H,
                    font_size: 11.0,
                    color: if acc.is_default { [0x3a, 0xff, 0x80] } else { [0x83, 0x83, 0x8a] },
                });
            }
        } else if let Some(selected_id) = self.selected_email_id {
            if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                // Subject Header
                labels.push(TextLabel {
                    text: email.subject.clone(),
                    x: detail_x,
                    y: 60.0 + MENUBAR_H,
                    font_size: 15.0,
                    color: [0xff, 0xff, 0xff],
                });

                // Metadata
                labels.push(TextLabel { text: format!("From: {}", email.from), x: detail_x, y: 85.0 + MENUBAR_H, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                labels.push(TextLabel { text: format!("To:   {}", email.to), x: detail_x, y: 105.0 + MENUBAR_H, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
                labels.push(TextLabel { text: format!("Date: {}", email.date), x: detail_x, y: 125.0 + MENUBAR_H, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
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

        // 5. Status Banner Message
        if let Some((ref msg, _)) = self.status_message {
            labels.push(TextLabel {
                text: msg.clone(),
                x: 200.0,
                y: h_f32 - 25.0,
                font_size: 11.0,
                color: [0x3a, 0xff, 0x80],
            });
        }

        // 6. Compose Dialog Content
        if self.compose_open {
            let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
            let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

            labels.push(TextLabel {
                text: self.compose_title.clone(),
                x: modal_x + 15.0,
                y: modal_y + 16.0,
                font_size: 13.0,
                color: [0xff, 0xff, 0xff],
            });

            labels.push(TextLabel { text: "To:".to_string(), x: modal_x + 15.0, y: modal_y + 54.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Subject:".to_string(), x: modal_x + 15.0, y: modal_y + 94.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

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
        let btn_compose = Button::new(10.0, 15.0, 26.0, 26.0).with_label("+");

        // Folder selection is the bar's right-aligned title dropdown (the
        // designer's pane-switcher idiom); Accounts is a plain button beside it.
        let menubar = MenuBar::new(0.0, 0.0, 800.0, MENUBAR_H)
            .with_recess(true)
            .with_title("Inbox")
            .with_right_aligned_title(true)
            .with_item("Mail", &["New Message", "Sync Now", "Quit"])
            .with_item("Message", &["Reply", "Delete", "Mark Read/Unread"])
            .with_context_options(vec!["Inbox".to_string(), "Sent".to_string(), "Trash".to_string()], 0);

        let btn_accounts = Button::new(0.0, 5.0, 90.0, 26.0).with_label("Accounts");

        let mut search_box = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        search_box.font_size = 11.0;

        let email_list = ScrollRegion::new(54.0, 4.0);

        let btn_reply = Button::new(391.0, 8.0, 70.0, 26.0).with_label("Reply");
        let btn_delete = Button::new_reset(471.0, 8.0, 80.0, 26.0).with_label("Delete");
        let btn_unread = Button::new(561.0, 8.0, 110.0, 26.0).with_label("Mark Unread");

        let accounts = load_accounts();
        // Last-used account wins (sidecar file), else the configured default:
        // the on-start sync below should hit the account the user actually
        // reads, not whichever entry carries the is_default flag.
        let selected_account_idx = load_selected_account_email()
            .and_then(|e| accounts.iter().position(|a| a.email == e))
            .or_else(|| accounts.iter().position(|a| a.is_default))
            .unwrap_or(0);

        let btn_manage_accounts = Button::new(66.0, 15.0, 300.0, 26.0).with_label("Manage Accounts...");

        let mut detail_body = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(false);
        detail_body.font_size = 12.0;
        detail_body.font_family = "sans-serif".to_string();

        let mut compose_to = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_to.font_size = 12.0;
        let mut compose_subject = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_subject.font_size = 12.0;
        let mut compose_body = TextBox::new(String::new()).with_multiline(true).with_line_wrap(true).with_draw_bg_border(true);
        compose_body.font_size = 12.0;
        compose_body.font_family = "sans-serif".to_string();

        let btn_compose_send = Button::new(0.0, 0.0, 75.0, 28.0).with_label("Send");
        let btn_compose_cancel = Button::new_reset(0.0, 0.0, 75.0, 28.0).with_label("Cancel");


        let emails = if let Some(acc) = accounts.get(selected_account_idx) {
            load_emails_for_account(&acc.email)
        } else {
            Vec::new()
        };

        let last_sync_start = if let Some(acc) = accounts.get(selected_account_idx) {
            sync_imap(acc.clone(), _sender.clone());
            Some(std::time::Instant::now())
        } else {
            None
        };

        Self {
            last_sync_start,
            keys: EmailKeys::load(),
            btn_compose,
            menubar,
            btn_accounts,
            search_box,
            email_list,
            email_buttons: Vec::new(),
            btn_reply,
            btn_delete,
            btn_unread,
            detail_body,
            compose_to,
            compose_subject,
            compose_body,
            btn_compose_send,
            btn_compose_cancel,
            accounts,
            selected_account_idx,
            btn_manage_accounts,
            emails,
            current_folder: Folder::Inbox,
            selected_email_id: None,
            body_scroll: 0.0,
            body_content_h: 0.0,
            detail_hovered: false,
            body_sb_dragging: false,
            body_sb_drag_offset: 0.0,
            compose_open: false,
            compose_title: String::new(),
            status_message: None,
            sender: _sender.clone(),
            width: 1000,
            height: 600,
            scale_factor: 1.0,
            font_system: cce_ui::create_font_system(),
            needs_rebuild: true,
            ui_context: UiContext::new(),
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: "Clear Email Client".to_string(),
            app_id: "cce-email".to_string(),
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
                if f == Folder::Accounts {
                    // Accounts are managed by cce-system-interface — re-read the
                    // shared accounts.json on every entry so its changes appear
                    // without an app restart. Keep the selection by email; if
                    // that account is gone, fall to the default.
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
                    self.selected_account_idx = new_idx;
                    if changed_account {
                        if let Some(email) = self.accounts.get(new_idx).map(|a| a.email.clone()) {
                            self.emails = load_emails_for_account(&email);
                            save_selected_account_email(&email);
                            self.start_sync(true);
                        } else {
                            self.emails = Vec::new();
                        }
                    }
                } else {
                    // Mail folders refresh from the server on entry (throttled).
                    self.start_sync(false);
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectEmail(id) => {
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
                self.compose_to.text = String::new();
                self.compose_to.edit_buffer = String::new();
                self.compose_subject.text = String::new();
                self.compose_subject.edit_buffer = String::new();
                self.compose_body.text = String::new();
                self.compose_body.edit_buffer = String::new();
                self.compose_title = "New Message".to_string();
                self.compose_open = true;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeCancel => {
                self.compose_open = false;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::ComposeSend => {
                let to = if self.compose_to.editing { &self.compose_to.edit_buffer } else { &self.compose_to.text }.trim().to_string();
                let subject = if self.compose_subject.editing { &self.compose_subject.edit_buffer } else { &self.compose_subject.text }.trim().to_string();
                let body = if self.compose_body.editing { &self.compose_body.edit_buffer } else { &self.compose_body.text }.to_string();

                if !to.is_empty() {
                    let active_acc = self.accounts[self.selected_account_idx].clone();
                    let sender_email = active_acc.email.clone();
                    
                    // Trigger asynchronous SMTP send in background
                    send_smtp(active_acc, to.clone(), subject.clone(), body.clone(), self.sender.clone());

                    // Save email in sent folder locally
                    let new_id = self.emails.iter().map(|e| e.id).max().unwrap_or(0) + 1;
                    let new_email = Email {
                        id: new_id,
                        from: sender_email,
                        to: to.clone(),
                        subject: if subject.is_empty() { "(No Subject)".to_string() } else { subject },
                        body,
                        date: "Just now".to_string(),
                        read: true,
                        folder: "sent".to_string(),
                        uid: None,
                    };
                    self.emails.push(new_email);
                    self.save_emails();
                    self.compose_open = false;
                } else {
                    self.status_message = Some(("Recipient is required".to_string(), 4.0));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::Reply => {
                if let Some(id) = self.selected_email_id {
                    if let Some(email) = self.emails.iter().find(|e| e.id == id) {
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
                    self.status_message = Some((
                        if permanently_deleted { "Email Deleted Permanently" } else { "Moved to Trash" }.to_string(),
                        4.0
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
                self.status_message = Some(("Opening System Settings...".to_string(), 4.0));
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
                self.status_message = Some((msg, 4.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::EmailsSynced(email, list) => {
                // Determine if we should update memory state (only if active account is still this one)
                if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                    // Read state is pushed to the server best-effort (silent
                    // set_seen_on_server), so read-in-app still wins over the
                    // server flag here — it covers in-flight or failed pushes
                    // (server-seen still wins over unseen).
                    let keep_local_read = |prior: &[Email], fetched: Vec<Email>| -> Vec<Email> {
                        fetched
                            .into_iter()
                            .map(|mut e| {
                                if prior.iter().any(|p| p.id == e.id && p.folder == "inbox" && p.read) {
                                    e.read = true;
                                }
                                e
                            })
                            .collect()
                    };
                    // Locally-kept mail alongside a fresh fetch: everything
                    // except the server-backed folders — prior inbox rows are
                    // replaced wholesale, prior server-fetched sent (uid set)
                    // likewise, and a locally-appended sent copy (uid None)
                    // is dropped once the server fetch carries the same
                    // message (Gmail auto-saves SMTP sends to Sent Mail).
                    let local_keep = |prior: &[Email], fetched: &[Email]| -> Vec<Email> {
                        prior
                            .iter()
                            .filter(|e| e.folder != "inbox")
                            .filter(|e| !(e.folder == "sent" && e.uid.is_some()))
                            .filter(|e| {
                                !(e.folder == "sent"
                                    && fetched.iter().any(|f| {
                                        f.folder == "sent" && f.subject == e.subject
                                    }))
                            })
                            .cloned()
                            .collect()
                    };
                    if acc.email == email {
                        let mut merged = keep_local_read(&self.emails, list.clone());
                        merged.extend(local_keep(&self.emails, &list));
                        self.emails = merged;
                        save_emails_for_account(&email, &self.emails);
                    } else {
                        // Just write cache to disk
                        let acc_emails = load_emails_for_account(&email);
                        let mut merged = keep_local_read(&acc_emails, list.clone());
                        merged.extend(local_keep(&acc_emails, &list));
                        save_emails_for_account(&email, &merged);
                    }
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

        if let Some((_, ref mut timer)) = self.status_message {
            *timer -= dt;
            if *timer <= 0.0 {
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
            Folder::Trash => "trash",
            Folder::Accounts => "accounts",
        };

        let list_x = 10.0;
        let detail_x = list_x + 325.0;
        let separator_x = list_x + 310.0;
        let detail_panel_x = separator_x + 1.0;

        if self.needs_rebuild || size_changed {
            // Sidebar buttons layout
            self.menubar.set_rect(0.0, 0.0, w_f32, MENUBAR_H);
            // An open menubar dropdown is a popover: registration feeds the
            // dl-text occlusion clamp, and the render loop at the end of this
            // function draws it on top of everything.
            self.ui_context.clear_popovers();
            if self.menubar.popover_rect().is_some() {
                self.ui_context.register_popover(&mut self.menubar);
            }
            // Bar title = current folder (the context-dropdown trigger), with
            // the inbox unread count folded in where the sidebar badge lived.
            let inbox_unread = self.emails.iter().filter(|e| e.folder == "inbox" && !e.read).count();
            self.menubar.title = match self.current_folder {
                Folder::Inbox if inbox_unread > 0 => format!("Inbox ({})", inbox_unread),
                Folder::Inbox => "Inbox".to_string(),
                Folder::Sent => "Sent".to_string(),
                Folder::Trash => "Trash".to_string(),
                Folder::Accounts => "Accounts".to_string(),
            };
            if let Some(ci) = match self.current_folder {
                Folder::Inbox => Some(0),
                Folder::Sent => Some(1),
                Folder::Trash => Some(2),
                Folder::Accounts => None,
            } {
                self.menubar.set_context_selected(ci);
            }
            // Fixed offset from the right edge — anchoring to the title would
            // make the button drift as the folder name changes length.
            self.btn_accounts.set_rect(w_f32 - 250.0, 5.0, 90.0, 26.0);

            cce_ui::scale::set_scale_factor(scale as f32);
            self.btn_compose.set_rect(list_x, 15.0 + MENUBAR_H, 26.0, 26.0);

            // Search box / Add Account and Scrolling list
            let list_count = if self.current_folder == Folder::Accounts {
                self.btn_manage_accounts.set_rect(list_x + 36.0, 15.0 + MENUBAR_H, 264.0, 26.0);
                self.accounts.len()
            } else {
                self.search_box.set_rect(list_x + 36.0, 15.0 + MENUBAR_H, 264.0, 26.0);
                
                // Get filtered emails count for bounds setup
                self.emails.iter()
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
                    .count()
            };

            self.email_list.set_rect(list_x, 55.0 + MENUBAR_H, 300.0, h_f32 - 70.0 - MENUBAR_H);
            self.email_list.update_bounds(list_count, 55.0 + MENUBAR_H, h_f32 - 70.0 - MENUBAR_H);

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
                self.emails.iter().find(|e| e.id == selected_id).map(|e| (e.read, e.body.clone()))
            } else {
                None
            };

            // Get filtered email IDs and selection states
            let filtered_email_ids: Vec<(usize, bool)> = if self.current_folder == Folder::Accounts {
                Vec::new()
            } else {
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

            if self.current_folder == Folder::Accounts {
                for idx in 0..self.accounts.len() {
                    self.email_buttons[idx].selected = idx == self.selected_account_idx;
                    if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                        self.email_buttons[idx].set_rect(list_x, draw_y, 300.0, 54.0);
                    } else {
                        self.email_buttons[idx].set_rect(-9999.0, -9999.0, 0.0, 0.0);
                    }
                }

            } else {
                for (idx, &(_email_id, is_selected)) in filtered_email_ids.iter().enumerate() {
                    self.email_buttons[idx].selected = is_selected;
                    if let Some(draw_y) = self.email_list.get_item_draw_y(idx, 0.0) {
                        self.email_buttons[idx].set_rect(list_x, draw_y, 300.0, 54.0);
                    } else {
                        self.email_buttons[idx].set_rect(-9999.0, -9999.0, 0.0, 0.0);
                    }
                }

                // Detail View
                if let Some((read, body)) = selected_email_state {
                    self.btn_reply.set_rect(detail_x, 8.0 + MENUBAR_H, 70.0, 26.0);
                    self.btn_delete.set_rect(detail_x + 80.0, 8.0 + MENUBAR_H, 80.0, 26.0);
                    self.btn_unread.set_rect(detail_x + 170.0, 8.0 + MENUBAR_H, 110.0, 26.0);
                    self.btn_unread.set_label(if read { "Mark Unread" } else { "Mark Read" });

                    let detail_w = (w_f32 - (detail_x + 15.0)).max(100.0);
                    self.detail_body.set_rect(detail_x, 170.0 + MENUBAR_H, detail_w, (h_f32 - 190.0 - MENUBAR_H).max(100.0));
                    self.detail_body.text = body;
                }
            }

            // Compose inputs layout
            if self.compose_open {
                let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
                let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

                self.compose_to.set_rect(modal_x + 80.0, modal_y + 50.0, 400.0, 26.0);
                self.compose_subject.set_rect(modal_x + 80.0, modal_y + 90.0, 400.0, 26.0);
                self.compose_body.set_rect(modal_x + 15.0, modal_y + 130.0, 470.0, 230.0);

                self.btn_compose_send.set_rect(modal_x + 320.0, modal_y + 375.0, 75.0, 28.0);
                self.btn_compose_cancel.set_rect(modal_x + 410.0, modal_y + 375.0, 75.0, 28.0);
            }



            self.needs_rebuild = false;
        }

        // Now compute `filtered` only for rendering (immutable borrow of self)
        let filtered: Vec<&Email> = if self.current_folder == Folder::Accounts {
            Vec::new()
        } else {
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

        // Compose Button and Folders Graphics — full paint walk: chrome (rounded rects,
        // hover/selected states) AND text in one pass. The legacy extra_quads bridge only
        // forwarded plain Prim::Quads, so every widget's rounded chrome was dropped.
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose, &mut *quads.pc);
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_accounts, &mut *quads.pc);
        // The menubar itself paints at the END of display_list — its dropdown
        // must overlay every pane beneath.

        // 3. Email List Panel Separator
        quads.push((separator_x, MENUBAR_H, 1.0, h_f32 - MENUBAR_H, [0.18, 0.18, 0.22, 1.0]));

        // Search box / Add Account and List
        if self.current_folder == Folder::Accounts {
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_manage_accounts, &mut *quads.pc);
        } else {
            self.search_box.prepare_text(&mut self.font_system);
            cce_ui::widget::WidgetHost::prepare_text(&mut self.menubar, &mut self.font_system);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.search_box, &mut *quads.pc);
        }
        {
            let mut list_quads = Vec::new();
            self.email_list.push_quads(&mut list_quads);
            quads.extend(list_quads);
        }

        // Visible List Item Buttons
        let list_len = if self.current_folder == Folder::Accounts {
            self.accounts.len()
        } else {
            filtered.len()
        };
        for idx in 0..list_len {
            if self.email_list.get_item_draw_y(idx, 0.0).is_some() {
                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.email_buttons[idx], &mut *quads.pc);

                // Blue dot/unread indicator for this row (emails only)
                if self.current_folder != Folder::Accounts && !filtered[idx].read {
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
        if self.current_folder == Folder::Accounts {
            if self.selected_account_idx < self.accounts.len() {
                // Top action toolbar background
                quads.push((detail_panel_x, MENUBAR_H, w_f32 - detail_panel_x, 42.0, [0.08, 0.08, 0.12, 1.0]));
                quads.push((detail_panel_x, MENUBAR_H + 42.0, w_f32 - detail_panel_x, 1.0, [0.18, 0.18, 0.22, 1.0]));

            }
        } else if let Some(selected_id) = self.selected_email_id {
            if self.emails.iter().any(|e| e.id == selected_id) {
                // Top action toolbar background
                quads.push((detail_panel_x, MENUBAR_H, w_f32 - detail_panel_x, 42.0, [0.08, 0.08, 0.12, 1.0]));
                quads.push((detail_panel_x, MENUBAR_H + 42.0, w_f32 - detail_panel_x, 1.0, [0.18, 0.18, 0.22, 1.0]));

                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_reply, &mut *quads.pc);
                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_delete, &mut *quads.pc);
                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_unread, &mut *quads.pc);

                // Body as one boxed text prim: word-wrapped at the pane width and
                // clipped to the pane (the TextBox walk drew each logical line as a
                // single run, so long paragraphs truncated at the pane edge). Skipped
                // while a modal is up — boxed text still renders above the panel.
                if !self.compose_open {
                    if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                        let body_w = (w_f32 - (detail_x + 15.0)).max(100.0);
                        let body_h = (h_f32 - 190.0 - MENUBAR_H).max(100.0);
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
                            170.0 + MENUBAR_H - self.body_scroll,
                            12.0,
                            [0xc8, 0xc8, 0xd0],
                            Some("sans-serif".to_string()),
                            Some([detail_x, 170.0 + MENUBAR_H, detail_x + body_w, 170.0 + MENUBAR_H + body_h]),
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
            let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
            let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

            // Semitransparent modal backdrop
            quads.push((0.0, 0.0, w_f32, h_f32, [0.0, 0.0, 0.0, 0.6]));

            // Modal dialog container
            quads.push((modal_x, modal_y, 500.0, 420.0, [0.08, 0.08, 0.12, 1.0]));
            quads.push((modal_x, modal_y, 500.0, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y + 419.0, 500.0, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y, 1.0, 420.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x + 499.0, modal_y, 1.0, 420.0, [0.25, 0.35, 0.50, 0.40]));

            self.compose_to.prepare_text(&mut self.font_system);
            self.compose_subject.prepare_text(&mut self.font_system);
            self.compose_body.prepare_text(&mut self.font_system);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_to, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_subject, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.compose_body, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose_send, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose_cancel, &mut *quads.pc);
        }



        // Menubar last: its open dropdown must overlay every pane beneath.
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.menubar, &mut __pc);

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
        Some(__pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, needs_rebuild: &mut bool) {
        let mut changed = false;
        let px = pos.x as f32;
        let py = pos.y as f32;
        // Active scrollbar-thumb drag tracks the pointer — before the
        // ui_context borrow (the sb helper takes &mut self).
        if !self.compose_open && self.body_sb_dragging {
            if self.body_sb_drag_to(py) {
                changed = true;
            }
        }

        // Routed dispatch (6bd shrink): one Event per widget root through the router.
        let mv = cce_ui::widget::Event::PointerMove { x: px, y: py, local_x: px, local_y: py };
        let ctx = &mut self.ui_context;

        if ctx.propagate_event(&mv, self.menubar.id()) {
            changed = true;
        }

        if self.compose_open {
            if ctx.propagate_event(&mv, self.compose_to.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_subject.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_body.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_send.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_cancel.id()) { changed = true; }
        } else {
            // Sidebar buttons
            if ctx.propagate_event(&mv, self.btn_compose.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_accounts.id()) { changed = true; }

            // Search / Add account and lists
            if self.current_folder == Folder::Accounts {
                if ctx.propagate_event(&mv, self.btn_manage_accounts.id()) { changed = true; }
            } else {
                if ctx.propagate_event(&mv, self.search_box.id()) { changed = true; }
            }
            if self.email_list.cursor_moved(px, py) { changed = true; }
            // Hover scope for the detail-pane body scroll (wheel + keys).
            self.detail_hovered = px > 10.0 + 310.0;

            for btn in &mut self.email_buttons {
                if btn.rect().0 > -9000.0 {
                    if ctx.propagate_event(&mv, btn.id()) { changed = true; }
                }
            }

            // Detail view buttons
            if self.current_folder != Folder::Accounts && self.selected_email_id.is_some() {
                if ctx.propagate_event(&mv, self.btn_reply.id()) { changed = true; }
                if ctx.propagate_event(&mv, self.btn_delete.id()) { changed = true; }
                if ctx.propagate_event(&mv, self.btn_unread.id()) { changed = true; }
                // detail_body: read-only boxed-text pane, no event routing
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

        // Detail-pane body scrollbar drag — before the ui_context borrow (the
        // sb helpers take &mut self).
        if !self.compose_open
            && button == MouseButton::Left
            && self.current_folder != Folder::Accounts
        {
            match state {
                ElementState::Pressed => {
                    // A fresh press always supersedes a stale drag — a lost
                    // release must not leave the thumb glued to the pointer.
                    self.body_sb_dragging = false;
                    if self.selected_email_id.is_some() && self.body_sb_press(px, py) {
                        changed = true;
                    }
                }
                ElementState::Released => {
                    if std::mem::take(&mut self.body_sb_dragging) {
                        changed = true;
                    }
                }
            }
        }

        let ctx = &mut self.ui_context;

        // Accounts button first: it sits inside the bar band, and the menubar
        // consumes any press within its rect — routed after, it would never
        // see the click.
        if ctx.propagate_event(&ev, self.btn_accounts.id()) {
            if state == ElementState::Released && self.btn_accounts.take_click() {
                *needs_rebuild = true;
                self.needs_rebuild = true;
                return Some(AppMessage::SwitchFolder(Folder::Accounts));
            }
            *needs_rebuild = true;
            self.needs_rebuild = true;
            return None;
        }

        // Menubar next — an open dropdown overlays the panes, so a handled
        // press/release must not fall through to the content beneath it.
        if ctx.propagate_event(&ev, self.menubar.id()) {
            if let Some(idx) = self.menubar.take_context_change() {
                msg_out = Some(AppMessage::SwitchFolder(match idx {
                    0 => Folder::Inbox,
                    1 => Folder::Sent,
                    _ => Folder::Trash,
                }));
            } else if let Some((menu_idx, item_idx)) = self.menubar.menu_click() {
                msg_out = match (menu_idx, item_idx) {
                    (0, 0) => Some(AppMessage::ComposeNew),
                    (0, 1) => Some(AppMessage::SyncNow),
                    (0, 2) => Some(AppMessage::Quit),
                    (1, 0) => Some(AppMessage::Reply),
                    (1, 1) => Some(AppMessage::DeleteSelected),
                    (1, 2) => Some(AppMessage::ToggleUnread),
                    _ => None,
                };
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

            // Click outside the modal clears focus or behaves neutrally
            if !changed && state == ElementState::Pressed && button == MouseButton::Left {
                let w_f32 = self.width as f32;
                let h_f32 = self.height as f32;
                let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
                let modal_y = ((h_f32 - 420.0) / 2.0).max(0.0);

                if px < modal_x || px > modal_x + 500.0 || py < modal_y || py > modal_y + 420.0 {
                    ctx.clear_focus();
                    self.compose_to.unfocus();
                    self.compose_subject.unfocus();
                    self.compose_body.unfocus();
                    changed = true;
                }
            }
        } else {
            // Sidebar buttons
            if ctx.propagate_event(&ev, self.btn_compose.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_compose.take_click() {
                    msg_out = Some(AppMessage::ComposeNew);
                }
            }

            if self.current_folder == Folder::Accounts {
                if ctx.propagate_event(&ev, self.btn_manage_accounts.id()) {
                    changed = true;
                    if state == ElementState::Released && self.btn_manage_accounts.take_click() {
                        msg_out = Some(AppMessage::ManageAccounts);
                    }
                }
            } else {
                // Search input
                if ctx.propagate_event(&ev, self.search_box.id()) {
                    changed = true;
                    if state == ElementState::Pressed { ctx.set_focused(&mut self.search_box); }
                    if self.search_box.take_change() {
                        msg_out = Some(AppMessage::SearchChanged);
                    }
                } else if state == ElementState::Pressed && button == MouseButton::Left {
                    ctx.clear_focus();
                    self.search_box.unfocus();
                    changed = true;
                }
            }

            if button == MouseButton::Left {
                let handled = match state {
                    ElementState::Pressed => self.email_list.press(px, py),
                    ElementState::Released => self.email_list.release(),
                };
                if handled {
                    changed = true;
                }
            }


            if self.current_folder == Folder::Accounts {
                for (idx, _) in self.accounts.iter().enumerate() {
                    if idx < self.email_buttons.len() {
                        let btn = &mut self.email_buttons[idx];
                        if btn.rect().0 > -9000.0 {
                            if ctx.propagate_event(&ev, btn.id()) {
                                changed = true;
                                if state == ElementState::Released && btn.take_click() {
                                    msg_out = Some(AppMessage::SelectAccount(idx));
                                }
                            }
                        }
                    }
                }
            } else {
                let current_folder_str = match self.current_folder {
                    Folder::Inbox => "inbox",
                    Folder::Sent => "sent",
                    Folder::Trash => "trash",
                    _ => "inbox",
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

            // Detail View action buttons
            if self.current_folder != Folder::Accounts && self.selected_email_id.is_some() {
                if ctx.propagate_event(&ev, self.btn_reply.id()) {
                    changed = true;
                    if state == ElementState::Released && self.btn_reply.take_click() {
                        msg_out = Some(AppMessage::Reply);
                    }
                }
                if ctx.propagate_event(&ev, self.btn_delete.id()) {
                    changed = true;
                    if state == ElementState::Released && self.btn_delete.take_click() {
                        msg_out = Some(AppMessage::DeleteSelected);
                    }
                }
                if ctx.propagate_event(&ev, self.btn_unread.id()) {
                    changed = true;
                    if state == ElementState::Released && self.btn_unread.take_click() {
                        msg_out = Some(AppMessage::ToggleUnread);
                    }
                }
                // detail_body deliberately gets no events: the pane is a read-only
                // boxed-text render now, and focusing the TextBox only let you
                // invisibly edit the display copy.
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

        if !self.compose_open {
            if px >= 66.0 && px <= 366.0 {
                if self.email_list.wheel(delta, px, py) {
                    changed = true;
                }
            }
        }

        // Detail-pane body scroll.
        if !self.compose_open
            && self.current_folder != Folder::Accounts
            && self.selected_email_id.is_some()
        {
            let separator_x = 10.0 + 310.0;
            if px > separator_x {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * 24.0,
                    MouseScrollDelta::PixelDelta(pos) => -pos.y as f32,
                };
                let body_h = (self.height as f32 - 190.0 - MENUBAR_H).max(100.0);
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
        let ctx = &mut self.ui_context;

        if self.compose_open {
            if self.compose_to.editing {
                if ctx.propagate_event(&kev, self.compose_to.id()) { handled = true; }
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
            // General keyboard shortcuts (input.kdl `cce-email` domain)
            if event.state == ElementState::Pressed {
                if cce_ui::widget::match_key_shortcut(event, &self.keys.compose) {
                    msg_out = Some(AppMessage::ComposeNew);
                    handled = true;
                } else if cce_ui::widget::match_key_shortcut(event, &self.keys.open_search) {
                    if self.current_folder != Folder::Accounts {
                        ctx.set_focused(&mut self.search_box);
                        self.search_box.focus();
                        handled = true;
                    }
                }
            }

            // Detail-pane body scroll, hover-scoped like ScrollRegion's keyboard path.
            if !handled
                && self.detail_hovered
                && self.selected_email_id.is_some()
                && self.current_folder != Folder::Accounts
                && !self.search_box.editing
                && event.state == ElementState::Pressed
            {
                let body_h = (self.height as f32 - 190.0 - MENUBAR_H).max(100.0);
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

            if !handled && self.current_folder != Folder::Accounts && self.search_box.editing {
                if ctx.propagate_event(&kev, self.search_box.id()) {
                    handled = true;
                    if self.search_box.take_change() {
                        msg_out = Some(AppMessage::SearchChanged);
                    }
                }
            }

            // Escape unfocuses search
            if !handled && event.state == ElementState::Pressed && event.logical_key == Key::Named(cce_ui::widget::NamedKey::Escape) {
                if self.current_folder != Folder::Accounts && self.search_box.editing {
                    ctx.clear_focus();
                    self.search_box.unfocus();
                    handled = true;
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
}
