mod scroll_region;
use scroll_region::ScrollRegion;
use wayland_client::QueueHandle;
use glyphon::FontSystem;
use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::widget::{
    MouseButton, ElementState, MouseScrollDelta, KeyEvent, WidgetHost,
    TextBox, Button, TextLabel, Key, Paginator, PageSelector, MenuController
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
    AddAccount,
    MakeDefaultAccount,
    Status(String),
    EmailsSynced(String, Vec<Email>),
    AddAccountSave,
    AddAccountCancel,
    AddAccountOAuth,
    AddAccountSaveOAuth(AccountInfo),
    UpdateAccountTokens(String, Option<String>, Option<u64>),
    AddAccountICloudHelp,
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
    paginator: cce_ui::widget::Adapted<Paginator>,

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

    // Accounts Management
    accounts: Vec<AccountInfo>,
    selected_account_idx: usize,
    btn_add_account: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_make_default: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_login_oauth: cce_ui::widget::Adapted<cce_ui::widget::Button>,

    // Add Account Dialog
    account_dialog_open: bool,
    add_acc_email: cce_ui::widget::Adapted<TextBox>,
    add_acc_password: cce_ui::widget::Adapted<TextBox>,
    add_acc_imap: cce_ui::widget::Adapted<TextBox>,
    add_acc_smtp: cce_ui::widget::Adapted<TextBox>,
    btn_add_acc_save: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_add_acc_cancel: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_add_acc_oauth: cce_ui::widget::Adapted<cce_ui::widget::Button>,
    btn_add_acc_icloud: cce_ui::widget::Adapted<cce_ui::widget::Button>,

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
    let _ = load_google_client_config();
    let path = get_accounts_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(accounts) = serde_json::from_str(&content) {
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
        },
    ]
}

fn save_accounts(accounts: &[AccountInfo]) {
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

/// Newest-N window fetched per sync. Full messages (attachments included) come
/// down the wire, so this trades sync time against history depth.
const FETCH_COUNT: usize = 50;

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

fn generate_pkce() -> (String, String) {
    use ring::rand::SecureRandom;
    use base64::Engine;
    let rand = ring::rand::SystemRandom::new();
    let mut bytes = [0u8; 32];
    rand.fill(&mut bytes).unwrap();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    
    let hash = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash.as_ref());
    
    (verifier, challenge)
}

async fn exchange_code_for_tokens(code: String, verifier: String, sender: calloop::channel::Sender<AppMessage>) {
    let client_config = load_google_client_config();
    let client = reqwest::Client::new();
    let mut params = vec![
        ("code", code.as_str()),
        ("client_id", client_config.client_id.as_str()),
        ("redirect_uri", "http://127.0.0.1:8080"),
        ("grant_type", "authorization_code"),
        ("code_verifier", verifier.as_str()),
    ];
    if !client_config.client_secret.is_empty() && client_config.client_secret != "GOCSPX-dummysecret" {
        params.push(("client_secret", client_config.client_secret.as_str()));
    }

    
    match client.post("https://oauth2.googleapis.com/token")
        .form(&params)
        .send()
        .await 
    {
        Ok(resp) => {
            if resp.status().is_success() {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    let access_token = json.get("access_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let refresh_token = json.get("refresh_token").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let expires_in = json.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(3600);
                    
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let expiry = now + expires_in;
 
                    // Request user profile info to get the email address
                    if let Ok(email_resp) = client.get("https://www.googleapis.com/oauth2/v2/userinfo")
                        .bearer_auth(&access_token)
                        .send()
                        .await 
                    {
                        if let Ok(email_json) = email_resp.json::<serde_json::Value>().await {
                            if let Some(email) = email_json.get("email").and_then(|v| v.as_str()) {
                                let new_acc = AccountInfo {
                                    email: email.to_string(),
                                    imap: "imap.gmail.com:993".to_string(),
                                    smtp: "smtp.gmail.com:465".to_string(),
                                    is_default: false,
                                    password: String::new(),
                                    is_oauth: true,
                                    access_token: Some(access_token),
                                    refresh_token: Some(refresh_token),
                                    token_expiry: Some(expiry),
                                    client_id: Some(client_config.client_id),
                                    client_secret: Some(client_config.client_secret),
                                };
                                let _ = sender.send(AppMessage::AddAccountSaveOAuth(new_acc));
                                return;
                            }
                        }
                    }
                }
                let _ = sender.send(AppMessage::Status("Failed to parse Google profile".to_string()));
            } else {
                let err_text = resp.text().await.unwrap_or_default();
                let _ = sender.send(AppMessage::Status(format!("Token exchange failed: {}", err_text)));
            }
        }
        Err(e) => {
            let _ = sender.send(AppMessage::Status(format!("Token request failed: {}", e)));
        }
    }
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

fn sync_imap(mut account: AccountInfo, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        // Skip connecting for mock credentials
        if account.password == "mock_password" || account.email == "lsgalante@cce-ui.org" {
            return;
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
                    let _ = sender.send(AppMessage::Status(format!("OAuth Refresh Failed: {}", e)));
                    return;
                }
            }
        }

        let domain = match account.imap.split(':').next() {
            Some(d) => d,
            None => return,
        };
        let port = match account.imap.split(':').nth(1).and_then(|p| p.parse::<u16>().ok()) {
            Some(p) => p,
            None => 993,
        };

        let _ = sender.send(AppMessage::Status(format!("Connecting to {}...", account.imap)));
        
        let tls = match TlsConnector::new() {
            Ok(t) => t,
            Err(_) => {
                let _ = sender.send(AppMessage::Status("Failed to create TLS connector".to_string()));
                return;
            }
        };

        let client = match imap::connect((domain, port), domain, &tls) {
            Ok(c) => c,
            Err(e) => {
                let _ = sender.send(AppMessage::Status(format!("IMAP Connection failed: {}", e)));
                return;
            }
        };

        let mut session = if account.is_oauth {
            let auth = ImapOAuth2 {
                user: account.email.clone(),
                access_token: access_token.clone(),
            };
            match client.authenticate("XOAUTH2", &auth) {
                Ok(s) => s,
                Err((e, _)) => {
                    let _ = sender.send(AppMessage::Status(format!("IMAP OAuth Login failed: {}", e)));
                    return;
                }
            }
        } else {
            match client.login(&account.email, &account.password) {
                Ok(s) => s,
                Err((e, _)) => {
                    let _ = sender.send(AppMessage::Status(format!("IMAP Login failed: {}", e)));
                    return;
                }
            }
        };

        let _ = sender.send(AppMessage::Status("Syncing Inbox...".to_string()));

        if let Err(e) = session.select("INBOX") {
            let _ = sender.send(AppMessage::Status(format!("Failed to select INBOX: {}", e)));
            let _ = session.logout();
            return;
        }

        let mut search_results: Vec<u32> = match session.search("ALL") {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                let _ = sender.send(AppMessage::Status(format!("IMAP Search failed: {}", e)));
                let _ = session.logout();
                return;
            }
        };
        search_results.sort();

        let total = search_results.len();
        if total == 0 {
            let _ = sender.send(AppMessage::Status("Inbox is empty".to_string()));
            let _ = session.logout();
            return;
        }

        // Fetch the newest FETCH_COUNT messages in full. PEEK keeps the server's
        // \Seen flags untouched; FLAGS rides along so read-state comes from the
        // server instead of being hardcoded.
        let start_idx = total.saturating_sub(FETCH_COUNT);
        let range = &search_results[start_idx..total];
        let query_seq = range.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
        let _ = sender.send(AppMessage::Status(format!("Fetching {} messages...", range.len())));

        let mut fetched_emails = Vec::new();
        match session.fetch(&query_seq, "(FLAGS BODY.PEEK[])") {
            Ok(fetches) => {
                let parser = mail_parser::MessageParser::default();
                for fetch in fetches.iter() {
                    let id = fetch.message;
                    let read = fetch
                        .flags()
                        .iter()
                        .any(|f| matches!(f, imap::types::Flag::Seen));

                    let (from, subject, date, body) = match fetch.body().and_then(|raw| parser.parse(raw)) {
                        Some(msg) => (
                            format_from(&msg),
                            msg.subject().unwrap_or("(No Subject)").to_string(),
                            format_date(msg.date()),
                            extract_body(&msg),
                        ),
                        None => (
                            "Unknown".to_string(),
                            "(No Subject)".to_string(),
                            "Unknown".to_string(),
                            String::new(),
                        ),
                    };

                    fetched_emails.push(Email {
                        id: id as usize,
                        from,
                        to: account.email.clone(),
                        subject,
                        body,
                        date,
                        read,
                        folder: "inbox".to_string(),
                    });
                }
            }
            Err(e) => {
                let _ = sender.send(AppMessage::Status(format!("IMAP Fetch failed: {}", e)));
                let _ = session.logout();
                return;
            }
        }

        fetched_emails.reverse(); // Newest first

        let _ = sender.send(AppMessage::EmailsSynced(account.email.clone(), fetched_emails));
        let _ = sender.send(AppMessage::Status("Sync Complete".to_string()));
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
        let (id, ptr) = (self.btn_make_default.id(), self.btn_make_default.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_add_account.id(), self.btn_add_account.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_login_oauth.id(), self.btn_login_oauth.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_add_acc_oauth.id(), self.btn_add_acc_oauth.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_add_acc_icloud.id(), self.btn_add_acc_icloud.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_add_acc_save.id(), self.btn_add_acc_save.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.btn_add_acc_cancel.id(), self.btn_add_acc_cancel.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.add_acc_email.id(), self.add_acc_email.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.add_acc_password.id(), self.add_acc_password.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.add_acc_imap.id(), self.add_acc_imap.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        let (id, ptr) = (self.add_acc_smtp.id(), self.add_acc_smtp.as_ptr_mut());
        self.ui_context.register_widget(id, ptr);
        for btn in self.email_buttons.iter_mut() {
            let (id, ptr) = (btn.id(), btn.as_ptr_mut());
            self.ui_context.register_widget(id, ptr);
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
        let body_h = (h - 190.0).max(100.0);
        let max_scroll = (self.body_content_h - body_h).max(0.0);
        if max_scroll <= 0.0 {
            return None;
        }
        let sb_w = cce_ui::layout::scrollbar_width();
        let sb_x = w - sb_w - 4.0;
        let thumb_h = (body_h * body_h / self.body_content_h).clamp(20.0, body_h);
        let thumb_y = 170.0 + (self.body_scroll / max_scroll) * (body_h - thumb_h);
        Some((sb_x, 170.0, sb_w, body_h, thumb_y, thumb_h))
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
        let body_h = (self.height as f32 - 190.0).max(100.0);
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

        let sidebar_w = self.paginator.sidebar_w();
        let list_x = sidebar_w + 10.0;
        let detail_x = list_x + 325.0;
        let separator_x = list_x + 310.0;
        let tab_w = 46.0;
        let margin_x = (sidebar_w - tab_w) / 2.0;

        // Sidebar button/tab text now rides along with their chrome in display_list's
        // paint_root_into walk — only app-composed labels are emitted here.

        // Sidebar Folder Badges
        let inbox_unread = self.emails.iter().filter(|e| e.folder == "inbox" && !e.read).count();
        if inbox_unread > 0 {
            let badge_text = inbox_unread.to_string();
            let est_w = TextLabel::estimate_width(&badge_text, 10.0);
            labels.push(TextLabel {
                text: badge_text,
                x: margin_x + (tab_w - est_w) / 2.0,
                y: 106.0, // centers in the badge pill (by + 12 + ~2)
                font_size: 10.0,
                color: [0xff, 0xff, 0xff],
            });
        }

        // 3. Email List Labels / Accounts list labels.
        // Skipped while a modal is up: text always renders above geometry, and these
        // hand-emitted labels carry no bounds, so they'd bleed straight through the
        // modal panel (the popover-occlusion clamp only knows registered popovers).
        let modal_open = self.compose_open || self.account_dialog_open;
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
                    // Sender name
                    labels.push(TextLabel {
                        text: ellipsize(&email.from, 21),
                        x: list_x + 20.0,
                        y: draw_y + 6.0,
                        font_size: 11.0,
                        color: if !email.read { [0xff, 0xff, 0xff] } else { [0xb0, 0xb0, 0xb8] },
                    });

                    // Date
                    labels.push(TextLabel {
                        text: email.date.clone(),
                        x: list_x + 250.0,
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

                    // Snippet
                    let snippet_raw = email.body.replace('\n', " ");
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
                    y: 60.0,
                    font_size: 15.0,
                    color: [0xff, 0xff, 0xff],
                });

                // Settings details
                labels.push(TextLabel { text: format!("Incoming Server (IMAP): {}", acc.imap), x: detail_x, y: 95.0, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                labels.push(TextLabel { text: format!("Outgoing Server (SMTP): {}", acc.smtp), x: detail_x, y: 120.0, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                let auth_text = if acc.is_oauth {
                    "Authentication:          OAuth2 (Google)"
                } else {
                    "Authentication:          SSL/TLS, Normal Password"
                };
                labels.push(TextLabel { text: auth_text.to_string(), x: detail_x, y: 145.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
                labels.push(TextLabel {
                    text: format!("Default Account:         {}", if acc.is_default { "Yes" } else { "No" }),
                    x: detail_x,
                    y: 170.0,
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
                    y: 60.0,
                    font_size: 15.0,
                    color: [0xff, 0xff, 0xff],
                });

                // Metadata
                labels.push(TextLabel { text: format!("From: {}", email.from), x: detail_x, y: 85.0, font_size: 11.0, color: [0xb0, 0xb0, 0xb8] });
                labels.push(TextLabel { text: format!("To:   {}", email.to), x: detail_x, y: 105.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
                labels.push(TextLabel { text: format!("Date: {}", email.date), x: detail_x, y: 125.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
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
                text: "New Message".to_string(),
                x: modal_x + 15.0,
                y: modal_y + 16.0,
                font_size: 13.0,
                color: [0xff, 0xff, 0xff],
            });

            labels.push(TextLabel { text: "To:".to_string(), x: modal_x + 15.0, y: modal_y + 54.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Subject:".to_string(), x: modal_x + 15.0, y: modal_y + 94.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

        }

        // 7. Add Account Dialog Content
        if self.account_dialog_open {
            let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
            let modal_y = ((h_f32 - 360.0) / 2.0).max(0.0);

            labels.push(TextLabel {
                text: "Add Email Account".to_string(),
                x: modal_x + 15.0,
                y: modal_y + 16.0,
                font_size: 13.0,
                color: [0xff, 0xff, 0xff],
            });

            labels.push(TextLabel { text: "Email Address:".to_string(), x: modal_x + 15.0, y: modal_y + 54.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "Password / App PW:".to_string(), x: modal_x + 15.0, y: modal_y + 94.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "IMAP Host:port:".to_string(), x: modal_x + 15.0, y: modal_y + 134.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });
            labels.push(TextLabel { text: "SMTP Host:port:".to_string(), x: modal_x + 15.0, y: modal_y + 174.0, font_size: 11.0, color: [0x83, 0x83, 0x8a] });

            labels.push(TextLabel {
                text: "Note: For Gmail, use 'Click to Login (Google)' below. iCloud requires App PW.".to_string(),
                x: modal_x + 15.0,
                y: modal_y + 215.0,
                font_size: 9.5,
                color: [0x70, 0x70, 0x75],
            });
            labels.push(TextLabel {
                text: "Servers are automatically configured for popular domains.".to_string(),
                x: modal_x + 15.0,
                y: modal_y + 233.0,
                font_size: 9.5,
                color: [0x70, 0x70, 0x75],
            });

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
        let btn_compose = Button::new(10.0, 15.0, 36.0, 36.0).with_label("+");
        let paginator = Paginator::new(vec![
            "Inbox".to_string(),
            "Sent".to_string(),
            "Trash".to_string(),
            "Accounts".to_string(),
        ]);

        let mut search_box = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        search_box.font_size = 11.0;

        let email_list = ScrollRegion::new(54.0, 4.0);

        let btn_reply = Button::new(391.0, 8.0, 70.0, 26.0).with_label("Reply");
        let btn_delete = Button::new_reset(471.0, 8.0, 80.0, 26.0).with_label("Delete");
        let btn_unread = Button::new(561.0, 8.0, 110.0, 26.0).with_label("Mark Unread");

        let accounts = load_accounts();
        let selected_account_idx = accounts.iter().position(|a| a.is_default).unwrap_or(0);

        let btn_add_account = Button::new(66.0, 15.0, 300.0, 26.0).with_label("+ Add Account");
        let btn_make_default = Button::new(391.0, 8.0, 120.0, 26.0).with_label("Make Default");

        let mut detail_body = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(false);
        detail_body.font_size = 12.0;
        detail_body.font_family = "sans-serif".to_string();

        let mut compose_to = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_to.font_size = 12.0;
        let mut compose_subject = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        compose_subject.font_size = 12.0;
        let mut compose_body = TextBox::new(String::new()).with_multiline(true).with_draw_bg_border(true);
        compose_body.font_size = 12.0;
        compose_body.font_family = "sans-serif".to_string();

        let btn_compose_send = Button::new(0.0, 0.0, 75.0, 28.0).with_label("Send");
        let btn_compose_cancel = Button::new_reset(0.0, 0.0, 75.0, 28.0).with_label("Cancel");

        let mut add_acc_email = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        add_acc_email.font_size = 11.0;
        let mut add_acc_password = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        add_acc_password.font_size = 11.0;
        add_acc_password.is_password = true;
        let mut add_acc_imap = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        add_acc_imap.font_size = 11.0;
        let mut add_acc_smtp = TextBox::new(String::new()).with_multiline(false).with_draw_bg_border(true);
        add_acc_smtp.font_size = 11.0;

        let btn_add_acc_save = Button::new(0.0, 0.0, 75.0, 28.0).with_label("Save");
        let btn_add_acc_cancel = Button::new_reset(0.0, 0.0, 75.0, 28.0).with_label("Cancel");
        let btn_add_acc_oauth = Button::new(0.0, 0.0, 140.0, 28.0).with_label("Login (Google)");
        let btn_add_acc_icloud = Button::new(0.0, 0.0, 140.0, 28.0).with_label("Login (iCloud)");
        let btn_login_oauth = Button::new(0.0, 0.0, 180.0, 28.0).with_label("Click to Login (Browser)");

        let emails = if let Some(acc) = accounts.get(selected_account_idx) {
            load_emails_for_account(&acc.email)
        } else {
            Vec::new()
        };

        if let Some(acc) = accounts.get(selected_account_idx) {
            sync_imap(acc.clone(), _sender.clone());
        }

        Self {
            keys: EmailKeys::load(),
            btn_compose,
            paginator,
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
            btn_add_account,
            btn_make_default,
            btn_login_oauth,
            account_dialog_open: false,
            add_acc_email,
            add_acc_password,
            add_acc_imap,
            add_acc_smtp,
            btn_add_acc_save,
            btn_add_acc_cancel,
            btn_add_acc_oauth,
            btn_add_acc_icloud,
            emails,
            current_folder: Folder::Inbox,
            selected_email_id: None,
            body_scroll: 0.0,
            body_content_h: 0.0,
            detail_hovered: false,
            body_sb_dragging: false,
            body_sb_drag_offset: 0.0,
            compose_open: false,
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

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
        match msg {
            AppMessage::SwitchFolder(f) => {
                self.current_folder = f;
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectEmail(id) => {
                self.selected_email_id = Some(id);
                self.body_scroll = 0.0;
                if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                    if !email.read {
                        email.read = true;
                        self.save_emails();
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
                        self.compose_open = true;
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::DeleteSelected => {
                if let Some(id) = self.selected_email_id {
                    let mut permanently_deleted = false;
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        if email.folder == "trash" {
                            permanently_deleted = true;
                        } else {
                            email.folder = "trash".to_string();
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
                    if let Some(email) = self.emails.iter_mut().find(|e| e.id == id) {
                        email.read = !email.read;
                    }
                    self.save_emails();
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::SelectAccount(idx) => {
                self.selected_account_idx = idx;
                if let Some(acc) = self.accounts.get(idx) {
                    self.emails = load_emails_for_account(&acc.email);
                    sync_imap(acc.clone(), self.sender.clone());
                } else {
                    self.emails = Vec::new();
                }
                self.selected_email_id = None;
                self.email_list.set_scroll_y(0.0);
                self.body_scroll = 0.0;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AddAccount => {
                // Clear all Add Account inputs
                self.add_acc_email.text = String::new();
                self.add_acc_email.edit_buffer = String::new();
                self.add_acc_password.text = String::new();
                self.add_acc_password.edit_buffer = String::new();
                self.add_acc_imap.text = String::new();
                self.add_acc_imap.edit_buffer = String::new();
                self.add_acc_smtp.text = String::new();
                self.add_acc_smtp.edit_buffer = String::new();
                
                self.account_dialog_open = true;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::MakeDefaultAccount => {
                for (i, acc) in self.accounts.iter_mut().enumerate() {
                    acc.is_default = i == self.selected_account_idx;
                }
                save_accounts(&self.accounts);
                self.status_message = Some(("Default Account Changed".to_string(), 4.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::Status(msg) => {
                self.status_message = Some((msg, 4.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::EmailsSynced(email, list) => {
                // Determine if we should update memory state (only if active account is still this one)
                if let Some(acc) = self.accounts.get(self.selected_account_idx) {
                    // The app never STOREs \Seen back, so a message read locally
                    // would flip unread again on every sync — read-in-app wins
                    // over the server flag (server-seen still wins over unseen).
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
                    if acc.email == email {
                        let mut merged = keep_local_read(&self.emails, list.clone());
                        let mut local_other = self.emails.clone();
                        local_other.retain(|e| e.folder != "inbox");
                        merged.extend(local_other);
                        self.emails = merged;
                        save_emails_for_account(&email, &self.emails);
                    } else {
                        // Just write cache to disk
                        let mut acc_emails = load_emails_for_account(&email);
                        let mut merged = keep_local_read(&acc_emails, list.clone());
                        acc_emails.retain(|e| e.folder != "inbox");
                        merged.extend(acc_emails);
                        save_emails_for_account(&email, &merged);
                    }
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AddAccountSave => {
                let email = if self.add_acc_email.editing { &self.add_acc_email.edit_buffer } else { &self.add_acc_email.text }.trim().to_string();
                let password = if self.add_acc_password.editing { &self.add_acc_password.edit_buffer } else { &self.add_acc_password.text }.trim().to_string();
                let imap = if self.add_acc_imap.editing { &self.add_acc_imap.edit_buffer } else { &self.add_acc_imap.text }.trim().to_string();
                let smtp = if self.add_acc_smtp.editing { &self.add_acc_smtp.edit_buffer } else { &self.add_acc_smtp.text }.trim().to_string();

                if email.is_empty() || password.is_empty() || imap.is_empty() || smtp.is_empty() {
                    self.status_message = Some(("All fields are required".to_string(), 4.0));
                } else {
                    let is_default = self.accounts.is_empty();
                    let new_acc = AccountInfo {
                        email: email.clone(),
                        imap,
                        smtp,
                        is_default,
                        password,
                        is_oauth: false,
                        access_token: None,
                        refresh_token: None,
                        token_expiry: None,
                        client_id: None,
                        client_secret: None,
                    };
                    self.accounts.push(new_acc.clone());
                    save_accounts(&self.accounts);

                    self.selected_account_idx = self.accounts.len() - 1;
                    self.emails = load_emails_for_account(&email);
                    
                    // Trigger sync
                    sync_imap(new_acc, self.sender.clone());
                    
                    self.account_dialog_open = false;
                    self.status_message = Some(("Account Added Successfully".to_string(), 4.0));
                }
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AddAccountCancel => {
                self.account_dialog_open = false;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AddAccountOAuth => {
                let sender = self.sender.clone();
                let client_config = load_google_client_config();
                tokio::spawn(async move {
                    let listener = match tokio::net::TcpListener::bind("127.0.0.1:8080").await {
                        Ok(l) => l,
                        Err(e) => {
                            let _ = sender.send(AppMessage::Status(format!("Failed to bind port 8080: {}", e)));
                            return;
                        }
                    };
                    
                    let _ = sender.send(AppMessage::Status("Waiting for browser login...".to_string()));
                    
                    let (verifier, challenge) = generate_pkce();
                    
                    let auth_url = format!(
                        "https://accounts.google.com/o/oauth2/v2/auth?client_id={}&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080&response_type=code&scope=https%3A%2F%2Fmail.google.com%2F&access_type=offline&prompt=consent&code_challenge={}&code_challenge_method=S256",
                        client_config.client_id,
                        challenge
                    );
                    let _ = std::process::Command::new("xdg-open").arg(&auth_url).spawn();

                    if let Ok((mut stream, _)) = listener.accept().await {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut buffer = [0; 1024];
                        if let Ok(n) = stream.read(&mut buffer).await {
                            let req_str = String::from_utf8_lossy(&buffer[..n]);
                            if let Some(code_idx) = req_str.find("code=") {
                                let rest = &req_str[code_idx + 5..];
                                let end_idx = rest.find(|c: char| c == ' ' || c == '&' || c == '\r' || c == '\n').unwrap_or(rest.len());
                                let code = rest[..end_idx].to_string();
                                
                                let _ = sender.send(AppMessage::Status("Exchanging code for token...".to_string()));
                                exchange_code_for_tokens(code, verifier, sender.clone()).await;

                                
                                let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                                                <html><head><style>body { font-family: sans-serif; background-color: #08080c; color: #fff; text-align: center; padding-top: 50px; }</style></head><body><h2>Clear Mail Authentication Successful!</h2><p>You can close this tab and return to the application.</p></body></html>";
                                let _ = stream.write_all(response.as_bytes()).await;
                                let _ = stream.flush().await;
                            } else {
                                let _ = sender.send(AppMessage::Status("OAuth Error: No code received".to_string()));
                                let response = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n\
                                                <html><head><style>body { font-family: sans-serif; background-color: #08080c; color: #ff6060; text-align: center; padding-top: 50px; }</style></head><body><h2>Clear Mail Authentication Failed</h2><p>No authorization code was found.</p></body></html>";
                                let _ = stream.write_all(response.as_bytes()).await;
                                let _ = stream.flush().await;
                            }
                        }
                    }
                });
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AddAccountICloudHelp => {
                let _ = std::process::Command::new("xdg-open").arg("https://appleid.apple.com/").spawn();
                self.status_message = Some(("Log in & generate an App-Specific Password on appleid.apple.com".to_string(), 6.0));
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
            AppMessage::AddAccountSaveOAuth(new_acc) => {
                let mut acc = new_acc;
                if let Some(existing_idx) = self.accounts.iter().position(|a| a.email == acc.email) {
                    let is_default = self.accounts[existing_idx].is_default;
                    acc.is_default = is_default;
                    self.accounts[existing_idx] = acc.clone();
                    self.selected_account_idx = existing_idx;
                    self.status_message = Some(("Google Account Updated".to_string(), 4.0));
                } else {
                    acc.is_default = self.accounts.is_empty();
                    self.accounts.push(acc.clone());
                    self.selected_account_idx = self.accounts.len() - 1;
                    self.status_message = Some(("Google Account Added".to_string(), 4.0));
                }
                save_accounts(&self.accounts);

                self.emails = load_emails_for_account(&acc.email);
                
                sync_imap(acc, self.sender.clone());
                
                self.account_dialog_open = false;
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
        if let Some((_, ref mut timer)) = self.status_message {
            *timer -= dt;
            if *timer <= 0.0 {
                self.status_message = None;
                *needs_rebuild = true;
                self.needs_rebuild = true;
            }
        }
        if self.paginator.tick(dt, &mut self.ui_context) {
            *needs_rebuild = true;
            self.needs_rebuild = true;
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

        let sidebar_w = self.paginator.sidebar_w();
        let list_x = sidebar_w + 10.0;
        let detail_x = list_x + 325.0;
        let separator_x = list_x + 310.0;
        let detail_panel_x = separator_x + 1.0;
        let tab_w = 46.0;
        let margin_x = (sidebar_w - tab_w) / 2.0;

        if self.needs_rebuild || size_changed {
            // Sidebar buttons layout
            self.btn_compose.set_rect((sidebar_w - 36.0) / 2.0, 15.0, 36.0, 36.0);

            // Set paginator layout — tabs start below the compose button (the two used
            // to stack at y=0 and the "+" sat buried under the Inbox tab).
            cce_ui::scale::set_scale_factor(scale as f32);
            self.paginator.set_rect(0.0, 60.0, sidebar_w, h_f32 - 60.0);
            let folder_idx = match self.current_folder {
                Folder::Inbox => 0,
                Folder::Sent => 1,
                Folder::Trash => 2,
                Folder::Accounts => 3,
            };
            self.paginator.set_selected_page(folder_idx);

            // Search box / Add Account and Scrolling list
            let list_count = if self.current_folder == Folder::Accounts {
                self.btn_add_account.set_rect(list_x, 15.0, 300.0, 26.0);
                self.accounts.len()
            } else {
                self.search_box.set_rect(list_x, 15.0, 300.0, 26.0);
                
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

            self.email_list.set_rect(list_x, 55.0, 300.0, h_f32 - 70.0);
            self.email_list.update_bounds(list_count, 55.0, h_f32 - 70.0);

            if self.email_buttons.len() != list_count {
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

                // Detail View for selected account
                if self.selected_account_idx < self.accounts.len() {
                    self.btn_make_default.set_rect(detail_x, 8.0, 120.0, 26.0);
                    let acc = &self.accounts[self.selected_account_idx];
                    if acc.is_oauth {
                        self.btn_login_oauth.set_rect(detail_x, 200.0, 180.0, 28.0);
                    } else {
                        self.btn_login_oauth.set_rect(-9999.0, -9999.0, 0.0, 0.0);
                    }
                } else {
                    self.btn_make_default.set_rect(-9999.0, -9999.0, 0.0, 0.0);
                    self.btn_login_oauth.set_rect(-9999.0, -9999.0, 0.0, 0.0);
                }
            } else {
                self.btn_make_default.set_rect(-9999.0, -9999.0, 0.0, 0.0);
                self.btn_login_oauth.set_rect(-9999.0, -9999.0, 0.0, 0.0);
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
                    self.btn_reply.set_rect(detail_x, 8.0, 70.0, 26.0);
                    self.btn_delete.set_rect(detail_x + 80.0, 8.0, 80.0, 26.0);
                    self.btn_unread.set_rect(detail_x + 170.0, 8.0, 110.0, 26.0);
                    self.btn_unread.base_mut().label = Some((if read { "Mark Unread" } else { "Mark Read" }).to_string());

                    let detail_w = (w_f32 - (detail_x + 15.0)).max(100.0);
                    self.detail_body.set_rect(detail_x, 170.0, detail_w, (h_f32 - 190.0).max(100.0));
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

            // Add Account inputs layout
            if self.account_dialog_open {
                let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
                let modal_y = ((h_f32 - 360.0) / 2.0).max(0.0);

                self.add_acc_email.set_rect(modal_x + 140.0, modal_y + 50.0, 340.0, 26.0);
                self.add_acc_password.set_rect(modal_x + 140.0, modal_y + 90.0, 340.0, 26.0);
                self.add_acc_imap.set_rect(modal_x + 140.0, modal_y + 130.0, 340.0, 26.0);
                self.add_acc_smtp.set_rect(modal_x + 140.0, modal_y + 170.0, 340.0, 26.0);

                self.btn_add_acc_save.set_rect(modal_x + 320.0, modal_y + 310.0, 75.0, 28.0);
                self.btn_add_acc_cancel.set_rect(modal_x + 410.0, modal_y + 310.0, 75.0, 28.0);
                self.btn_add_acc_oauth.set_rect(modal_x + 15.0, modal_y + 310.0, 140.0, 28.0);
                self.btn_add_acc_icloud.set_rect(modal_x + 165.0, modal_y + 310.0, 140.0, 28.0);
            } else {
                self.btn_add_acc_icloud.set_rect(-9999.0, -9999.0, 0.0, 0.0);
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

        // 2. Sidebar background panel
        quads.push((0.0, 0.0, sidebar_w, h_f32, [0.08, 0.08, 0.12, 1.0]));
        quads.push((sidebar_w, 0.0, 1.0, h_f32, [0.18, 0.18, 0.22, 1.0])); // sidebar separator

        // Compose Button and Folders Graphics — full paint walk: chrome (rounded rects,
        // hover/selected states) AND text in one pass. The legacy extra_quads bridge only
        // forwarded plain Prim::Quads, so every widget's rounded chrome was dropped.
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_compose, &mut *quads.pc);
        cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.paginator, &mut *quads.pc);

        // Draw badge pill for inbox unread (centered on vertical rotated tab)
        let inbox_unread = self.emails.iter().filter(|e| e.folder == "inbox" && !e.read).count();
        if inbox_unread > 0 {
            let bx = margin_x;
            let by = 92.0; // lower edge of the Inbox tab (tabs start at the y=60 paginator offset)
            quads.push((bx + (tab_w - 22.0) / 2.0, by + 12.0, 22.0, 16.0, [0.20, 0.45, 0.85, 0.8]));
        }

        // 3. Email List Panel Separator
        quads.push((separator_x, 0.0, 1.0, h_f32, [0.18, 0.18, 0.22, 1.0]));

        // Search box / Add Account and List
        if self.current_folder == Folder::Accounts {
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_add_account, &mut *quads.pc);
        } else {
            self.search_box.prepare_text(&mut self.font_system);
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

        // 4. Detail View Area
        if self.current_folder == Folder::Accounts {
            if self.selected_account_idx < self.accounts.len() {
                // Top action toolbar background
                quads.push((detail_panel_x, 0.0, w_f32 - detail_panel_x, 42.0, [0.08, 0.08, 0.12, 1.0]));
                quads.push((detail_panel_x, 42.0, w_f32 - detail_panel_x, 1.0, [0.18, 0.18, 0.22, 1.0]));

                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_make_default, &mut *quads.pc);
                if self.accounts[self.selected_account_idx].is_oauth {
                    cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_login_oauth, &mut *quads.pc);
                }
            }
        } else if let Some(selected_id) = self.selected_email_id {
            if self.emails.iter().any(|e| e.id == selected_id) {
                // Top action toolbar background
                quads.push((detail_panel_x, 0.0, w_f32 - detail_panel_x, 42.0, [0.08, 0.08, 0.12, 1.0]));
                quads.push((detail_panel_x, 42.0, w_f32 - detail_panel_x, 1.0, [0.18, 0.18, 0.22, 1.0]));

                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_reply, &mut *quads.pc);
                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_delete, &mut *quads.pc);
                cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_unread, &mut *quads.pc);

                // Body as one boxed text prim: word-wrapped at the pane width and
                // clipped to the pane (the TextBox walk drew each logical line as a
                // single run, so long paragraphs truncated at the pane edge). Skipped
                // while a modal is up — boxed text still renders above the panel.
                if !self.compose_open && !self.account_dialog_open {
                    if let Some(email) = self.emails.iter().find(|e| e.id == selected_id) {
                        let body_w = (w_f32 - (detail_x + 15.0)).max(100.0);
                        let body_h = (h_f32 - 190.0).max(100.0);
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
                            170.0 - self.body_scroll,
                            12.0,
                            [0xc8, 0xc8, 0xd0],
                            Some("sans-serif".to_string()),
                            Some([detail_x, 170.0, detail_x + body_w, 170.0 + body_h]),
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

        // 6. Add Account Dialog Overlay
        if self.account_dialog_open {
            let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
            let modal_y = ((h_f32 - 360.0) / 2.0).max(0.0);

            // Semitransparent modal backdrop
            quads.push((0.0, 0.0, w_f32, h_f32, [0.0, 0.0, 0.0, 0.6]));

            // Modal dialog container
            quads.push((modal_x, modal_y, 500.0, 360.0, [0.08, 0.08, 0.12, 1.0]));
            quads.push((modal_x, modal_y, 500.0, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y + 359.0, 500.0, 1.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x, modal_y, 1.0, 360.0, [0.25, 0.35, 0.50, 0.40]));
            quads.push((modal_x + 499.0, modal_y, 1.0, 360.0, [0.25, 0.35, 0.50, 0.40]));

            self.add_acc_email.prepare_text(&mut self.font_system);
            self.add_acc_password.prepare_text(&mut self.font_system);
            self.add_acc_imap.prepare_text(&mut self.font_system);
            self.add_acc_smtp.prepare_text(&mut self.font_system);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.add_acc_email, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.add_acc_password, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.add_acc_imap, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.add_acc_smtp, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_add_acc_save, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_add_acc_cancel, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_add_acc_oauth, &mut *quads.pc);
            cce_ui::scene::painter::paint_root_into(&self.ui_context, &self.btn_add_acc_icloud, &mut *quads.pc);
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
        if !self.account_dialog_open && !self.compose_open && self.body_sb_dragging {
            if self.body_sb_drag_to(py) {
                changed = true;
            }
        }

        // Routed dispatch (6bd shrink): one Event per widget root through the router.
        let mv = cce_ui::widget::Event::PointerMove { x: px, y: py, local_x: px, local_y: py };
        let ctx = &mut self.ui_context;

        if self.account_dialog_open {
            if ctx.propagate_event(&mv, self.add_acc_email.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.add_acc_password.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.add_acc_imap.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.add_acc_smtp.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_add_acc_save.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_add_acc_cancel.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_add_acc_oauth.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_add_acc_icloud.id()) { changed = true; }
        } else if self.compose_open {
            if ctx.propagate_event(&mv, self.compose_to.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_subject.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.compose_body.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_send.id()) { changed = true; }
            if ctx.propagate_event(&mv, self.btn_compose_cancel.id()) { changed = true; }
        } else {
            // Sidebar buttons
            if ctx.propagate_event(&mv, self.btn_compose.id()) { changed = true; }
            let sidebar_w = self.paginator.sidebar_w();
            if px < sidebar_w {
                // Self-routing composite: handle_event, not propagate — the router's
                // children-first descent would let the embedded strip consume this.
                if self.paginator.handle_event(&mv, ctx) { changed = true; }
            }

            // Search / Add account and lists
            if self.current_folder == Folder::Accounts {
                if ctx.propagate_event(&mv, self.btn_add_account.id()) { changed = true; }
            } else {
                if ctx.propagate_event(&mv, self.search_box.id()) { changed = true; }
            }
            if self.email_list.cursor_moved(px, py) { changed = true; }
            // Hover scope for the detail-pane body scroll (wheel + keys).
            self.detail_hovered = px > self.paginator.sidebar_w() + 10.0 + 310.0;

            for btn in &mut self.email_buttons {
                if btn.rect().0 > -9000.0 {
                    if ctx.propagate_event(&mv, btn.id()) { changed = true; }
                }
            }

            // Detail view buttons
            if self.current_folder == Folder::Accounts {
                if self.selected_account_idx < self.accounts.len() {
                    if ctx.propagate_event(&mv, self.btn_make_default.id()) { changed = true; }
                    if self.accounts[self.selected_account_idx].is_oauth {
                        if ctx.propagate_event(&mv, self.btn_login_oauth.id()) { changed = true; }
                    }
                }
            } else if self.selected_email_id.is_some() {
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
        if !self.account_dialog_open
            && !self.compose_open
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

        if self.account_dialog_open {
            if ctx.propagate_event(&ev, self.add_acc_email.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.add_acc_email); }
            }
            if ctx.propagate_event(&ev, self.add_acc_password.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.add_acc_password); }
            }
            if ctx.propagate_event(&ev, self.add_acc_imap.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.add_acc_imap); }
            }
            if ctx.propagate_event(&ev, self.add_acc_smtp.id()) {
                changed = true;
                if state == ElementState::Pressed { ctx.set_focused(&mut self.add_acc_smtp); }
            }

            if ctx.propagate_event(&ev, self.btn_add_acc_save.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_add_acc_save.take_click() {
                    msg_out = Some(AppMessage::AddAccountSave);
                }
            }
            if ctx.propagate_event(&ev, self.btn_add_acc_cancel.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_add_acc_cancel.take_click() {
                    msg_out = Some(AppMessage::AddAccountCancel);
                }
            }
            if ctx.propagate_event(&ev, self.btn_add_acc_oauth.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_add_acc_oauth.take_click() {
                    msg_out = Some(AppMessage::AddAccountOAuth);
                }
            }
            if ctx.propagate_event(&ev, self.btn_add_acc_icloud.id()) {
                changed = true;
                if state == ElementState::Released && self.btn_add_acc_icloud.take_click() {
                    msg_out = Some(AppMessage::AddAccountICloudHelp);
                }
            }

            // Click outside the modal clears focus
            if !changed && state == ElementState::Pressed && button == MouseButton::Left {
                let w_f32 = self.width as f32;
                let h_f32 = self.height as f32;
                let modal_x = ((w_f32 - 500.0) / 2.0).max(0.0);
                let modal_y = ((h_f32 - 360.0) / 2.0).max(0.0);

                if px < modal_x || px > modal_x + 500.0 || py < modal_y || py > modal_y + 360.0 {
                    ctx.clear_focus();
                    self.add_acc_email.unfocus();
                    self.add_acc_password.unfocus();
                    self.add_acc_imap.unfocus();
                    self.add_acc_smtp.unfocus();
                    changed = true;
                }
            }
        } else if self.compose_open {
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
            let sidebar_w = self.paginator.sidebar_w();
            if px < sidebar_w {
                if self.paginator.handle_event(&ev, ctx) {
                    changed = true;
                    if let Some((page, _)) = self.paginator.menu_click() {
                        let folder = match page {
                            0 => Folder::Inbox,
                            1 => Folder::Sent,
                            2 => Folder::Trash,
                            3 => Folder::Accounts,
                            _ => Folder::Inbox,
                        };
                        msg_out = Some(AppMessage::SwitchFolder(folder));
                    }
                }
            }

            if self.current_folder == Folder::Accounts {
                if ctx.propagate_event(&ev, self.btn_add_account.id()) {
                    changed = true;
                    if state == ElementState::Released && self.btn_add_account.take_click() {
                        msg_out = Some(AppMessage::AddAccount);
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
            if self.current_folder == Folder::Accounts {
                if self.selected_account_idx < self.accounts.len() {
                    if ctx.propagate_event(&ev, self.btn_make_default.id()) {
                        changed = true;
                        if state == ElementState::Released && self.btn_make_default.take_click() {
                            msg_out = Some(AppMessage::MakeDefaultAccount);
                        }
                    }
                    if self.accounts[self.selected_account_idx].is_oauth {
                        if ctx.propagate_event(&ev, self.btn_login_oauth.id()) {
                            changed = true;
                            if state == ElementState::Released && self.btn_login_oauth.take_click() {
                                msg_out = Some(AppMessage::AddAccountOAuth);
                            }
                        }
                    }
                }
            } else if self.selected_email_id.is_some() {
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
            && !self.account_dialog_open
            && self.current_folder != Folder::Accounts
            && self.selected_email_id.is_some()
        {
            let separator_x = self.paginator.sidebar_w() + 10.0 + 310.0;
            if px > separator_x {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * 24.0,
                    MouseScrollDelta::PixelDelta(pos) => -pos.y as f32,
                };
                let body_h = (self.height as f32 - 190.0).max(100.0);
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

        if self.account_dialog_open {
            if self.add_acc_email.editing {
                if ctx.propagate_event(&kev, self.add_acc_email.id()) {
                    handled = true;
                    // Auto-fill configuration based on email domain
                    let email_val = self.add_acc_email.edit_buffer.trim().to_lowercase();
                    if email_val.ends_with("@gmail.com") {
                        self.add_acc_imap.text = "imap.gmail.com:993".to_string();
                        self.add_acc_imap.edit_buffer = "imap.gmail.com:993".to_string();
                        self.add_acc_smtp.text = "smtp.gmail.com:465".to_string();
                        self.add_acc_smtp.edit_buffer = "smtp.gmail.com:465".to_string();
                    } else if email_val.ends_with("@icloud.com") {
                        self.add_acc_imap.text = "imap.mail.me.com:993".to_string();
                        self.add_acc_imap.edit_buffer = "imap.mail.me.com:993".to_string();
                        self.add_acc_smtp.text = "smtp.mail.me.com:587".to_string();
                        self.add_acc_smtp.edit_buffer = "smtp.mail.me.com:587".to_string();
                    } else if email_val.ends_with("@outlook.com") || email_val.ends_with("@hotmail.com") {
                        self.add_acc_imap.text = "outlook.office365.com:993".to_string();
                        self.add_acc_imap.edit_buffer = "outlook.office365.com:993".to_string();
                        self.add_acc_smtp.text = "smtp.office365.com:587".to_string();
                        self.add_acc_smtp.edit_buffer = "smtp.office365.com:587".to_string();
                    }
                }
            } else if self.add_acc_password.editing {
                if ctx.propagate_event(&kev, self.add_acc_password.id()) { handled = true; }
            } else if self.add_acc_imap.editing {
                if ctx.propagate_event(&kev, self.add_acc_imap.id()) { handled = true; }
            } else if self.add_acc_smtp.editing {
                if ctx.propagate_event(&kev, self.add_acc_smtp.id()) { handled = true; }
            }

            // Escape closes dialog
            if !handled && event.state == ElementState::Pressed && event.logical_key == Key::Named(cce_ui::widget::NamedKey::Escape) {
                msg_out = Some(AppMessage::AddAccountCancel);
                handled = true;
            }
        } else if self.compose_open {
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
                let body_h = (self.height as f32 - 190.0).max(100.0);
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
}
