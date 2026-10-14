//! Talking IMAP: OAuth token refresh and the XOAUTH2 authenticator, pooled
//! sessions, folder listing and sync, flags, deletes and moves, attachment and HTML-part
//! fetches, and the fetch caps they share.
//! Split out of main.rs on 2026-10-10.

use crate::*;
use imap_proto::NameAttribute;

/// Newest-N window fetched per sync. Only headers, structure, and the chosen
/// text part come down the wire, so attachments never inflate a sync.
pub(crate) const FETCH_COUNT: usize = 50;

/// How much of a folder's missing mail one report carries. A pass syncs
/// every folder's newest this-many first, so a mailbox with years of history
/// does not hold up the others, then works through the rest in batches of
/// it; each batch is merged and saved as it lands, so progress survives a
/// restart.
pub(crate) const MAX_FETCH_PER_SYNC: usize = 500;

/// How many already-cached messages get their flags refreshed per pass, so
/// mail read on another client stops showing unread here. Newest-first; the
/// whole mailbox would be a needless round trip on every sync.
pub(crate) const FLAG_REFRESH_WINDOW: usize = 200;

/// Gap before the next pass when one left history behind (a failed batch,
/// or one cut short for a Sync Now). Short enough to feel continuous, long
/// enough not to hammer the server.
pub(crate) const BACKFILL_DELAY_SECS: u64 = 3;

/// Byte cap on a fetched text part (pre-decode); the display model caps at
/// 1200 chars, so 64 KiB of qp/base64 is plenty.
pub(crate) const PART_FETCH_CAP: u32 = 65536;

/// Byte cap on an on-demand HTML part fetch (pre-decode). Marketing mail
/// runs tens of KB; 1 MiB covers pathological newsletters without letting
/// one message stall the connection.
#[cfg(feature = "wpe")]
pub(crate) const HTML_FETCH_CAP: u32 = 1_048_576;

/// Caps on the cid: inline images fetched alongside the HTML: per part
/// (pre-decode) and summed per message. A signature logo is kilobytes; a
/// message inlining more than this renders what fits and the rest break.
#[cfg(feature = "wpe")]
pub(crate) const INLINE_PART_CAP: u32 = 2_097_152;
#[cfg(feature = "wpe")]
pub(crate) const INLINE_TOTAL_CAP: usize = 8 * 1024 * 1024;

/// In-memory HTML bodies kept for re-opening without a refetch. Bodies can
/// be large, so the cache is small and simply dumped when full.
#[cfg(feature = "wpe")]
pub(crate) const HTML_CACHE_CAP: usize = 16;


#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct GoogleClientConfig {
    pub(crate) client_id: String,
    pub(crate) client_secret: String,
}

/// The Google OAuth client, from google_client.json, which the System
/// Interface's Accounts page writes when the user pastes their own client's
/// ID and secret. There is no built-in default any more: a placeholder ID
/// and secret used to be compiled in here and written to that file when it
/// was missing, which is one hardcoded credential away from the mistake
/// cce-system-interface made. Missing or unreadable means "not set up", and
/// the token refresh says so instead of sending empty strings to Google.
pub(crate) fn load_google_client_config() -> GoogleClientConfig {
    let p = cce_ui::config::cce_config_dir().join("google_client.json");
    if let Ok(content) = std::fs::read_to_string(&p) {
        if let Ok(config) = serde_json::from_str::<GoogleClientConfig>(&content) {
            return config;
        }
    }
    GoogleClientConfig { client_id: String::new(), client_secret: String::new() }
}

pub(crate) async fn refresh_access_token(account: &mut AccountInfo) -> Result<String, String> {
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
    if client_id.is_empty() || client_secret.is_empty() {
        return Err("Google client not configured: set the client ID and secret in System Interface > Accounts".to_string());
    }

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

pub(crate) struct ImapOAuth2 {
    pub(crate) user: String,
    pub(crate) access_token: String,
}

impl imap::Authenticator for ImapOAuth2 {
    type Response = String;
    #[inline]
    fn process(&self, _data: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.access_token)
    }
}

/// The runtime `main()` builds, for worker threads to block on.
pub(crate) static RUNTIME: std::sync::OnceLock<tokio::runtime::Handle> = std::sync::OnceLock::new();

/// Block on a future from a plain worker thread. `pollster::block_on` is NOT
/// enough for reqwest/hyper futures — they need a live tokio reactor, and the
/// `rt.enter()` guard in main() only covers the main thread. The workers
/// share main's runtime through its handle; one was built and torn down per
/// call before. Only a run without that runtime (a test) builds its own.
pub(crate) fn block_on_worker<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    if let Some(handle) = RUNTIME.get() {
        return Ok(handle.block_on(fut));
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {}", e))
        .map(|rt| rt.block_on(fut))
}

/// True for the built-in demo account — workers skip the network entirely.
pub(crate) fn is_mock_account(account: &AccountInfo) -> bool {
    account.password == "mock_password" || account.email == "lsgalante@cce-ui.org"
}

pub(crate) type ImapSession = imap::Session<native_tls::TlsStream<std::net::TcpStream>>;

/// A logged-in session, borrowed from [`IDLE_SESSIONS`] or freshly opened.
/// Hand it back with [`Imap::release`] when the job is done; dropping it
/// instead just closes the socket.
pub(crate) struct Imap {
    pub(crate) session: ImapSession,
    /// The session's own socket (a `try_clone`), kept so the liveness probe
    /// can bound its wait — the session itself does not expose its stream.
    pub(crate) socket: std::net::TcpStream,
    pub(crate) key: String,
    /// The OAuth token expiry this session logged in under (unix seconds).
    pub(crate) expires: Option<u64>,
}

impl std::ops::Deref for Imap {
    type Target = ImapSession;
    fn deref(&self) -> &ImapSession {
        &self.session
    }
}

impl std::ops::DerefMut for Imap {
    fn deref_mut(&mut self) -> &mut ImapSession {
        &mut self.session
    }
}

/// A released session waiting for the next job on the same account.
pub(crate) struct IdleSession {
    pub(crate) imap: Imap,
    /// Wall clock, not `Instant`: Linux's monotonic clock stops during
    /// suspend, and a session kept across one is exactly the dead kind.
    pub(crate) since: std::time::SystemTime,
}

/// Sessions kept logged in between jobs. Opening one is TCP + TLS + the
/// greeting + LOGIN (and on Gmail LOGIN alone is the slow part); every
/// message open used to pay that once for the seen-flag push and again for
/// its HTML, and every backfill pass paid it again. A kept session costs
/// one NOOP to vouch for.
pub(crate) static IDLE_SESSIONS: std::sync::Mutex<Vec<IdleSession>> = std::sync::Mutex::new(Vec::new());

/// How long a released session is worth keeping. Long enough to cover a
/// run of message opens; short of the point where the server, a NAT or a
/// suspend has likely dropped it — past this it is discarded unprobed.
pub(crate) const SESSION_IDLE_MAX: std::time::Duration = std::time::Duration::from_secs(120);

/// Kept per account: the sync worker and one interactive job can each hold
/// a session at once, and anything beyond that logs out on release.
pub(crate) const SESSIONS_KEPT_PER_ACCOUNT: usize = 2;

/// The NOOP that vouches for a kept session. A socket the network dropped
/// accepts the write and never answers, so the read is bounded. A NOOP is
/// one round trip; a probe that misses this only costs a fresh login.
pub(crate) const SESSION_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Whether a kept session may still be used: not idle too long, and not
/// logged in under an OAuth token that is about to lapse.
pub(crate) fn idle_session_fresh(since: std::time::SystemTime, expires: Option<u64>, now: u64) -> bool {
    // A clock stepped backwards reads as an error here: not fresh.
    since.elapsed().is_ok_and(|idle| idle < SESSION_IDLE_MAX)
        && expires.is_none_or(|e| e > now + 60)
}

/// Pool key: the account and the server it is for.
pub(crate) fn session_key(account: &AccountInfo) -> String {
    format!("{}\u{0}{}", account.email, account.imap)
}

impl Imap {
    /// Done with this session: keep it for the next job on the account,
    /// unless enough are kept already.
    fn release(mut self) {
        // Untagged responses the jobs never read (EXISTS, FETCH flag
        // updates) queue in the session for its life.
        drop(self.session.take_all_unsolicited());
        let mut pool = IDLE_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let now = unix_now();
        let stale: Vec<IdleSession> = {
            let (keep, stale): (Vec<_>, Vec<_>) = std::mem::take(&mut *pool)
                .into_iter()
                .partition(|s| idle_session_fresh(s.since, s.imap.expires, now));
            *pool = keep;
            stale
        };
        let kept = pool.iter().filter(|s| s.imap.key == self.key).count();
        let extra = if kept < SESSIONS_KEPT_PER_ACCOUNT {
            pool.push(IdleSession { imap: self, since: std::time::SystemTime::now() });
            None
        } else {
            Some(self)
        };
        drop(pool);
        // Dropping closes the stale ones' sockets; no LOGOUT, which could
        // block on exactly the dead connection that made them stale.
        drop(stale);
        if let Some(mut extra) = extra {
            let _ = extra.session.logout();
        }
    }
}

/// A kept session for this account that still answers, if there is one.
pub(crate) fn take_idle_session(key: &str) -> Option<Imap> {
    loop {
        let candidate = {
            let mut pool = IDLE_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            let idx = pool.iter().rposition(|s| s.imap.key == key)?;
            pool.swap_remove(idx)
        };
        if !idle_session_fresh(candidate.since, candidate.imap.expires, unix_now()) {
            continue;
        }
        let mut imap = candidate.imap;
        let _ = imap.socket.set_read_timeout(Some(SESSION_PROBE_TIMEOUT));
        let _ = imap.socket.set_write_timeout(Some(SESSION_PROBE_TIMEOUT));
        let alive = imap.session.noop().is_ok();
        // Back to blocking as every job has always run: a long UID SEARCH
        // must not be cut off by the probe's bound.
        let _ = imap.socket.set_read_timeout(None);
        let _ = imap.socket.set_write_timeout(None);
        if alive {
            return Some(imap);
        }
        eprintln!("cce-mail: a kept IMAP session stopped answering; opening another");
    }
}

/// Connect + authenticate an IMAP session; shared by every worker (call
/// from a worker thread — it blocks). A session released by an earlier job
/// on the same account is reused when it still answers. Refreshed OAuth
/// tokens are reported back via UpdateAccountTokens; every failure lands on
/// stderr and (when `verbose`) as a sticky error toast, then yields None.
pub(crate) fn open_imap_session(
    account: &mut AccountInfo,
    sender: &calloop::channel::Sender<AppMessage>,
    verbose: bool,
) -> Option<Imap> {
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
    let key = session_key(account);
    if let Some(imap) = take_idle_session(&key) {
        return Some(imap);
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

    // `imap::connect`, opened by hand to keep a handle on the socket.
    let connected = std::net::TcpStream::connect((domain, port))
        .map_err(|e| e.to_string())
        .and_then(|tcp| {
            let socket = tcp.try_clone().map_err(|e| e.to_string())?;
            let tls_stream = tls.connect(domain, tcp).map_err(|e| e.to_string())?;
            let mut client = imap::Client::new(tls_stream);
            client.read_greeting().map_err(|e| e.to_string())?;
            Ok((client, socket))
        });
    let (client, socket) = match connected {
        Ok(c) => c,
        Err(e) => {
            say!(err: format!("IMAP Connection failed: {}", e));
            return None;
        }
    };

    let session = if account.is_oauth {
        let auth = ImapOAuth2 {
            user: account.email.clone(),
            access_token,
        };
        match client.authenticate("XOAUTH2", &auth) {
            Ok(s) => s,
            Err((e, _)) => {
                say!(err: format!("IMAP OAuth Login failed: {}", e));
                return None;
            }
        }
    } else {
        match client.login(&account.email, &account.password) {
            Ok(s) => s,
            Err((e, _)) => {
                say!(err: format!("IMAP Login failed: {}", e));
                return None;
            }
        }
    };
    let expires = if account.is_oauth { account.token_expiry } else { None };
    Some(Imap { session, socket, key, expires })
}

/// What the pass on the wire covers: whose mail, and which folders. A second
/// sync request is only worth queueing when it falls outside this.
#[derive(Debug, Clone)]
pub(crate) struct SyncScope {
    /// Which pass this is. Only the worker holding the current generation
    /// may declare the wire free.
    pub(crate) generation: u64,
    pub(crate) account: String,
    pub(crate) tags: Vec<String>,
    /// When the worker left, or last reported a folder. The IMAP session
    /// carries no read timeout, so a pass that stalls on a dead socket would
    /// otherwise hold the wire — and the user's Sync Now — for as long as
    /// the kernel takes to give up on the connection. Past
    /// [`SYNC_STUCK_AFTER`] without news the pass is presumed lost and a new
    /// one may go out over it. Refreshed per report because a pass now
    /// carries on into history for as long as there is some.
    pub(crate) started: std::time::Instant,
    pub(crate) progress: SyncProgress,
}

/// Shared with the worker of one pass.
#[derive(Debug, Clone, Default)]
pub(crate) struct SyncProgress {
    /// Set by the worker once every folder has been synced and what is left
    /// is history: from then on a request the pass would otherwise cover is
    /// better served by a fresh one.
    pub(crate) backfilling: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Set by the app: stop after the batch in hand.
    pub(crate) cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How long a pass may hold the wire before a forced sync overrides it. Well
/// past a slow real pass (a full `[Gmail]/All Mail` walk runs half a minute),
/// short enough that a wedged one costs a wait rather than a restart. A
/// straggler that wakes up afterwards cannot damage what the new pass fetched
/// — [`merge_sync`] refuses to delete mail newer than the straggler's own
/// search.
pub(crate) const SYNC_STUCK_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

/// One mailbox's outcome for a single sync pass.
///
/// `server_uids` is the whole mailbox, not just what was fetched: the merge
/// needs it to notice messages deleted from another client. Without it a
/// local cache that only ever grows would keep showing mail that no longer
/// exists.
#[derive(Debug, Clone)]
pub(crate) struct FolderSync {
    pub(crate) folder: String,
    /// Newly downloaded messages. `id` is left 0 — identity is assigned by
    /// the merge, which is the only place that can see the whole cache.
    pub(crate) fetched: Vec<Email>,
    pub(crate) server_uids: Vec<u32>,
    /// Of the recent window, the uids the server marks \Seen. Read state set
    /// on another client reaches already-cached mail through this.
    pub(crate) seen_uids: Vec<u32>,
    /// Still missing after this pass — drives the backfill continuation.
    pub(crate) remaining: usize,
}

/// Download one batch of messages by UID, with the two-pass BODYSTRUCTURE
/// strategy (pass 1 headers/flags/structure, pass 2 text parts grouped by
/// section, capped full-message fallback). Keyed on UID throughout: sequence
/// numbers shift whenever anything is expunged, so they cannot identify a
/// message across syncs.
pub(crate) fn fetch_uid_batch(
    session: &mut ImapSession,
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
        ts: Option<i64>,
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
                let (from, to, subject, date, ts) = match fetch.header().and_then(|h| parser.parse(h)) {
                    Some(msg) => (
                        format_from(&msg),
                        format_to(&msg, account_email),
                        msg.subject().unwrap_or("(No Subject)").to_string(),
                        format_date(msg.date()),
                        msg.date().map(|d| d.to_timestamp()),
                    ),
                    None => (
                        "Unknown".to_string(),
                        account_email.to_string(),
                        "(No Subject)".to_string(),
                        "Unknown".to_string(),
                        None,
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
                    ts,
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
            origin_folder: None,
            from: p.from,
            to: p.to,
            subject: p.subject,
            body: p.body,
            date: p.date,
            ts: p.ts,
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
/// however often it ran. Now it takes up to [`MAX_FETCH_PER_SYNC`] of the
/// missing ones and hands back the rest, ascending, for [`sync_imap`] to
/// work through once every folder has had its turn.
pub(crate) fn sync_folder(
    session: &mut ImapSession,
    sender: &calloop::channel::Sender<AppMessage>,
    account_email: &str,
    mailbox: &str,
    folder_tag: &str,
    known: &std::collections::HashSet<u32>,
    verbose: bool,
) -> Option<(FolderSync, Vec<u32>)> {
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
    let mut backlog = missing;
    let batch_uids = backlog.split_off(backlog.len() - take); // newest first come first
    let remaining = backlog.len();

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

    let fetched = fetch_newest_first(session, account_email, folder_tag, &batch_uids, &say_err);

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

    Some((
        FolderSync {
            folder: folder_tag.to_string(),
            fetched,
            server_uids,
            seen_uids,
            remaining,
        },
        backlog,
    ))
}

/// Download `uids` (ascending) in [`FETCH_COUNT`] chunks, newest chunk
/// first, so a long backfill still surfaces recent mail early. Each chunk is
/// a separate round trip but one session.
pub(crate) fn fetch_newest_first(
    session: &mut ImapSession,
    account_email: &str,
    folder_tag: &str,
    uids: &[u32],
    say_err: &dyn Fn(String),
) -> Vec<Email> {
    let mut fetched = Vec::new();
    for chunk in uids.rchunks(FETCH_COUNT) {
        let mut ordered: Vec<u32> = chunk.to_vec();
        ordered.sort_unstable_by(|a, b| b.cmp(a));
        fetched.extend(fetch_uid_batch(session, account_email, folder_tag, &ordered, say_err));
    }
    fetched
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
/// A folder as the switcher and the sync see it.
///
/// The five the app has always had keep their historical lowercase tags so
/// existing caches keep loading; a folder discovered on the server is tagged
/// with its raw mailbox name, which is unique within an account.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FolderInfo {
    pub(crate) tag: String,
    /// Switcher text — decoded out of modified UTF-7.
    pub(crate) label: String,
    /// The IMAP mailbox to SELECT. Empty for the folders this app keeps
    /// locally (Drafts, Trash), which is what makes them local.
    #[serde(default)]
    pub(crate) mailbox: String,
}

impl FolderInfo {
    pub(crate) fn is_server_backed(&self) -> bool {
        !self.mailbox.is_empty()
    }
}

pub(crate) const TAG_INBOX: &str = "inbox";
pub(crate) const TAG_ARCHIVE: &str = "archive";
pub(crate) const TAG_SENT: &str = "sent";
pub(crate) const TAG_DRAFTS: &str = "drafts";
pub(crate) const TAG_TRASH: &str = "trash";

/// The folders that exist before the server has been asked — and the order the
/// switcher always leads with. Discovered folders are appended after these.
///
/// Only INBOX carries a mailbox here: it is the one name RFC 3501 guarantees.
/// Archive and sent start local and get their mailbox from special-use
/// discovery (`resolve_folders`) — guessing a provider's names instead (this
/// used to say `[Gmail]/All Mail`) means every non-Gmail account SELECTs a
/// mailbox that does not exist, on every sync.
pub(crate) fn default_folders() -> Vec<FolderInfo> {
    vec![
        FolderInfo { tag: TAG_INBOX.into(), label: "Inbox".into(), mailbox: "INBOX".into() },
        FolderInfo { tag: TAG_ARCHIVE.into(), label: "Archive".into(), mailbox: String::new() },
        FolderInfo { tag: TAG_SENT.into(), label: "Sent".into(), mailbox: String::new() },
        // Local-only: this app owns Drafts and Trash, so they have no mailbox
        // and the server's own Drafts/Trash are deliberately not adopted.
        FolderInfo { tag: TAG_DRAFTS.into(), label: "Drafts".into(), mailbox: String::new() },
        FolderInfo { tag: TAG_TRASH.into(), label: "Trash".into(), mailbox: String::new() },
    ]
}

/// Decode an IMAP mailbox name from modified UTF-7 (RFC 3501 §5.1.3).
///
/// `&` opens a base64 run — with `,` standing in for `/` — terminated by `-`,
/// and `&-` is a literal ampersand. Server folder names really arrive this
/// way: "Events &- Tickets" is "Events & Tickets", and anything non-ASCII is
/// unreadable without this.
pub(crate) fn decode_imap_utf7(raw: &str) -> String {
    fn sextet(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b',' => Some(63), // the modification: ',' where base64 has '/'
            _ => None,
        }
    }

    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            // Names are ASCII outside the escapes; anything else is passed
            // through as-is rather than mangled.
            let start = i;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&raw[start..i]);
            continue;
        }
        i += 1;
        if i < bytes.len() && bytes[i] == b'-' {
            out.push('&');
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'-' {
            i += 1;
        }
        let chunk = &raw[start..i];
        if i < bytes.len() {
            i += 1; // consume the terminating '-'
        }
        let mut bits: u32 = 0;
        let mut nbits = 0;
        let mut units: Vec<u16> = Vec::new();
        let mut ok = true;
        for &c in chunk.as_bytes() {
            let Some(v) = sextet(c) else {
                ok = false;
                break;
            };
            bits = (bits << 6) | v;
            nbits += 6;
            if nbits >= 16 {
                nbits -= 16;
                units.push(((bits >> nbits) & 0xFFFF) as u16);
            }
        }
        if ok {
            out.push_str(&String::from_utf16_lossy(&units));
        } else {
            // Not a run we understand — show it rather than dropping it.
            out.push('&');
            out.push_str(chunk);
        }
    }
    out
}

/// Ask the server which folders exist, folded into the default set.
///
/// `None` means LIST itself failed — the caller falls back to the last saved
/// discovery rather than the defaults, whose archive/sent have no mailbox yet.
pub(crate) fn list_folders(
    session: &mut ImapSession,
) -> Option<Vec<FolderInfo>> {
    let names = session.list(Some(""), Some("*")).ok()?;

    let mut entries: Vec<(String, Option<&'static str>)> = Vec::new();
    for n in names.iter() {
        let mut noselect = false;
        let mut special: Option<&'static str> = None;
        for a in n.attributes() {
            if matches!(a, NameAttribute::NoSelect) {
                noselect = true;
            } else if let Some(kind) = special_use(a) {
                special = Some(kind);
            }
        }
        if !noselect {
            entries.push((n.name().to_string(), special));
        }
    }
    Some(resolve_folders(entries))
}

/// A LIST attribute's RFC 6154 special use, as `resolve_folders` names it.
/// imap-proto parses these into their own variants (case-insensitively);
/// before 0.16 they arrived as `Custom("\\Sent")` and were matched by name.
pub(crate) fn special_use(a: &NameAttribute) -> Option<&'static str> {
    match a {
        NameAttribute::All => Some("all"),
        NameAttribute::Archive => Some("archive"),
        NameAttribute::Sent => Some("sent"),
        NameAttribute::Drafts => Some("drafts"),
        NameAttribute::Trash => Some("trash"),
        _ => None,
    }
}

/// Fold the LIST reply into the default folder set: each entry is a selectable
/// mailbox name plus its special-use verdict (RFC 6154).
///
/// Mailboxes the server flags `\Drafts` or `\Trash` are deliberately skipped:
/// this app keeps Drafts and Trash locally, and adopting the server's would
/// give one tag two meanings. `\Sent` fills in the sent mailbox — which is how
/// an account that calls its sent mail "Sent Messages" (iCloud) syncs at all.
/// The archive slot takes `\All` (Gmail's All Mail) or, failing that,
/// `\Archive` (everyone else's), `\All` winning in either LIST order; it also
/// adopts the server's own label, because what the folder *is* differs — All
/// Mail holds everything, an Archive folder only what was archived — and a
/// slot labelled for one provider reads as a lie on another.
pub(crate) fn resolve_folders(entries: Vec<(String, Option<&str>)>) -> Vec<FolderInfo> {
    let mut folders = default_folders();

    let adopt = |folders: &mut Vec<FolderInfo>, tag: &str, mailbox: &str, relabel: bool| {
        if let Some(f) = folders.iter_mut().find(|f| f.tag == tag) {
            f.mailbox = mailbox.to_string();
            if relabel {
                f.label = folder_label(mailbox);
            }
        }
    };

    let mut archive_from_all = false;
    let mut custom: Vec<FolderInfo> = Vec::new();
    for (raw, special) in entries {
        match special {
            Some("drafts") | Some("trash") => continue,
            Some("all") => {
                adopt(&mut folders, TAG_ARCHIVE, &raw, true);
                archive_from_all = true;
            }
            Some("archive") if !archive_from_all => adopt(&mut folders, TAG_ARCHIVE, &raw, true),
            Some("archive") => continue,
            Some("sent") => adopt(&mut folders, TAG_SENT, &raw, false),
            _ if raw.eq_ignore_ascii_case("INBOX") => adopt(&mut folders, TAG_INBOX, &raw, false),
            _ => custom.push(FolderInfo {
                tag: raw.clone(),
                label: folder_label(&raw),
                mailbox: raw,
            }),
        }
    }

    // A folder literally named "Archive" fills the slot when nothing was
    // flagged: iCloud leaves a plain-CREATEd Archive without \Archive (the
    // flag rides on CREATE (USE (\Archive)), which only Apple's own clients
    // send), and without this the switcher shows the local archive beside the
    // server's — two rows named Archive claiming one meaning.
    if !folders.iter().any(|f| f.tag == TAG_ARCHIVE && f.is_server_backed()) {
        if let Some(i) = custom.iter().position(|f| f.mailbox.eq_ignore_ascii_case("archive")) {
            let named = custom.remove(i);
            adopt(&mut folders, TAG_ARCHIVE, &named.mailbox, true);
        }
    }

    custom.sort_by_key(|a| a.label.to_lowercase());
    folders.extend(custom);
    folders
}

/// Switcher text for a raw mailbox name: decoded, and without the provider
/// namespace prefix that would otherwise read as "[Gmail]/Spam".
pub(crate) fn folder_label(raw: &str) -> String {
    let decoded = decode_imap_utf7(raw);
    decoded
        .strip_prefix("[Gmail]/")
        .or_else(|| decoded.strip_prefix("[Google Mail]/"))
        .unwrap_or(&decoded)
        .to_string()
}

/// What the cache already holds per server-backed folder, which is what a
/// sync fetches the complement of.
///
/// Trashed mail counts under the folder it came from: the server still has
/// it (deletion only propagates for INBOX), so leaving it out would make the
/// next pass treat it as new and download it straight back.
pub(crate) fn known_uids(
    emails: &[Email],
    folders: &[FolderInfo],
) -> std::collections::HashMap<String, std::collections::HashSet<u32>> {
    let mut known: std::collections::HashMap<String, std::collections::HashSet<u32>> =
        std::collections::HashMap::new();
    for e in emails {
        let Some(uid) = e.uid else { continue };
        let tag = if e.folder == "trash" {
            match &e.origin_folder {
                Some(o) => o.as_str(),
                None => continue,
            }
        } else {
            e.folder.as_str()
        };
        if folders.iter().any(|f| f.is_server_backed() && f.tag == tag) {
            known.entry(tag.to_string()).or_default().insert(uid);
        }
    }
    known
}

pub(crate) fn merge_sync(prior: Vec<Email>, folders: &[FolderSync]) -> Vec<Email> {
    use std::collections::HashSet;

    let mut out = prior;

    for f in folders {
        let server: HashSet<u32> = f.server_uids.iter().copied().collect();
        let seen: HashSet<u32> = f.seen_uids.iter().copied().collect();

        // Deleted from another client. Scoped to this folder and to mail that
        // came from the server: locally-created rows (uid None) and anything
        // the user moved elsewhere (trash, drafts) are not this pass's business.
        //
        // And scoped in time, by `server_max`. A pass reports the mailbox as
        // its own `UID SEARCH` saw it, which is a snapshot from before the
        // fetch that followed; mail that arrived after that search is absent
        // from `server` for the innocent reason that it did not exist yet.
        // Treating that as "deleted elsewhere" is how a slow pass used to
        // wipe the message a quick one had just brought in — the mail was
        // fetched, merged, saved, and then thrown out again by a straggler,
        // so it took another Sync Now to reappear. UIDs only ever increase
        // within a mailbox (RFC 3501 §2.3.1.1), so anything above everything
        // this pass saw postdates the pass and is not its business either.
        let server_max = f.server_uids.iter().copied().max().unwrap_or(0);
        out.retain(|e| {
            e.folder != f.folder
                || e.uid.is_none_or(|u| u > server_max || server.contains(&u))
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

    // Newest first, which is the order every list pass paints in. By the
    // message's own date: uid order is only a proxy for arrival within one
    // mailbox, and for a Gmail label it tracks when mail was labelled — which
    // is how a folder ends up showing its oldest message at the top. Rows
    // cached before timestamps existed have none, and fall back to uid order
    // behind everything dated. Locally-created mail has neither and belongs
    // at the top, being the most recent thing the user did.
    out.sort_by(|a, b| {
        let key = |e: &Email| match (e.uid, e.ts) {
            (None, _) => (2i8, i64::MAX, u32::MAX),
            (Some(u), Some(t)) => (1, t, u),
            (Some(u), None) => (0, i64::MIN, u),
        };
        key(b).cmp(&key(a))
    });

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

/// Put right the row a move took ahead of the server (`move_email`) once
/// the server has answered. `true` when the rows changed.
///
/// - Given its new uid, the row takes it — unless the folder already holds
///   that uid: a sync got there first, or the message was already in it
///   (Gmail's All Mail holds the inbox too). Then the stand-in goes.
/// - Moved with no uid reported, the stand-in goes, and the folder's next
///   sync brings the message in under its own.
/// - Failed, the row goes back where it was, with its old uid — or goes,
///   if a sync has meanwhile downloaded it there again. A failed restore
///   goes back to Trash, where it came from still noted.
pub(crate) fn settle_move(
    emails: &mut Vec<Email>,
    job: &MoveJob,
    result: &Result<Option<u32>, String>,
) -> bool {
    if job.copy_only {
        return false;
    }
    let Some(pos) = emails
        .iter()
        .position(|e| e.id == job.id && e.folder == job.to && e.uid.is_none())
    else {
        return false;
    };
    let held = |folder: &str, uid: u32| emails.iter().any(|e| e.folder == folder && e.uid == Some(uid));
    match *result {
        Ok(Some(uid)) if !held(&job.to, uid) => emails[pos].uid = Some(uid),
        Err(_) if !held(&job.from, job.uid) => {
            emails[pos].folder = job.from.clone();
            emails[pos].uid = Some(job.uid);
            if job.restore {
                emails[pos].origin_folder = Some(job.to.clone());
            }
        }
        _ => {
            emails.remove(pos);
        }
    }
    true
}

pub(crate) fn sync_imap(
    mut account: AccountInfo,
    known: std::collections::HashMap<String, std::collections::HashSet<u32>>,
    want_tags: Vec<String>,
    generation: u64,
    progress: SyncProgress,
    sender: calloop::channel::Sender<AppMessage>,
) {
    /// Hands `sync_in_flight` back however the worker leaves — the early
    /// returns below, or a panic unwinding through them. A flag cleared only
    /// on the success path is a flag that eventually sticks, and a stuck one
    /// here means the app never syncs again until it is restarted.
    struct Finish(u64, calloop::channel::Sender<AppMessage>);
    impl Drop for Finish {
        fn drop(&mut self) {
            let _ = self.1.send(AppMessage::SyncFinished(self.0));
        }
    }

    std::thread::spawn(move || {
        let _finish = Finish(generation, sender.clone());
        if is_mock_account(&account) {
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, true) else {
            return;
        };

        let _ = sender.send(AppMessage::Status("Syncing Inbox...".to_string()));

        // Ask what exists before fetching anything: discovery is what supplies
        // the real mailbox names, so an account that calls its sent mail
        // "Sent Messages" is handled without guessing, and it is what puts the
        // account's own folders in the switcher. If LIST itself fails, sync
        // from the last discovery instead — the defaults know no archive/sent
        // mailbox — and leave the saved set alone.
        let discovered = match list_folders(&mut session) {
            Some(d) => {
                let _ = sender.send(AppMessage::FoldersDiscovered(
                    account.email.clone(),
                    d.clone(),
                ));
                d
            }
            None => load_folders(&account.email),
        };

        let empty = std::collections::HashSet::new();
        // Folders with history still to download: (tag, the pass's
        // `UID SEARCH` snapshot, the uids it is missing, ascending).
        let mut backlogs: Vec<(String, Vec<u32>, Vec<u32>)> = Vec::new();

        for tag in &want_tags {
            let Some(info) = discovered.iter().find(|f| &f.tag == tag) else {
                continue;
            };
            if !info.is_server_backed() {
                continue;
            }
            // The inbox is the one whose progress is worth narrating; the rest
            // ride along quietly. A mailbox that cannot be selected reports
            // nothing, which the merge reads as "no news about that folder"
            // rather than "that folder is empty".
            let verbose = info.tag == TAG_INBOX;
            if let Some((synced, backlog)) = sync_folder(
                &mut session,
                &sender,
                &account.email,
                &info.mailbox,
                &info.tag,
                known.get(&info.tag).unwrap_or(&empty),
                verbose,
            ) {
                // Straight out, not into a batch collected for the end of the
                // pass. `want_tags` puts the inbox first and it answers in
                // about a second, while `[Gmail]/All Mail` behind it searches
                // the entire account; batching meant new mail sat finished in
                // this thread for the half-minute that took, which reads as a
                // sync that did nothing.
                if !backlog.is_empty() {
                    backlogs.push((info.tag.clone(), synced.server_uids.clone(), backlog));
                }
                let _ = sender.send(AppMessage::EmailsSynced(
                    account.email.clone(),
                    vec![synced],
                ));
            }
        }

        // History, on the same session and against the same search. This
        // used to be a pass of its own per MAX_FETCH_PER_SYNC messages, each
        // a fresh login, LIST, and `UID SEARCH ALL` of every folder — All
        // Mail's searching the whole account again for each 500 it brought
        // down. Mail deleted elsewhere meanwhile is the next pass's to
        // notice; mail arriving meanwhile is above the snapshot, which the
        // merge leaves alone. Stops between batches when the app wants the
        // wire for something the user asked for (`SyncProgress::cancel`).
        progress.backfilling.store(true, std::sync::atomic::Ordering::Relaxed);
        'history: for (tag, server_uids, mut backlog) in backlogs {
            while !backlog.is_empty() {
                if progress.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break 'history;
                }
                let take = backlog.len().min(MAX_FETCH_PER_SYNC);
                let batch = backlog.split_off(backlog.len() - take);
                let say_err = |m: String| eprintln!("cce-mail: {}", m);
                let fetched =
                    fetch_newest_first(&mut session, &account.email, &tag, &batch, &say_err);
                if fetched.is_empty() {
                    // A failed fetch, or a batch deleted since the search.
                    // Either way the last report for this folder still says
                    // what is outstanding, so the app schedules the next
                    // pass, which searches afresh.
                    break;
                }
                let _ = sender.send(AppMessage::EmailsSynced(
                    account.email.clone(),
                    vec![FolderSync {
                        folder: tag.clone(),
                        fetched,
                        server_uids: server_uids.clone(),
                        seen_uids: Vec::new(),
                        remaining: backlog.len(),
                    }],
                ));
            }
        }

        let _ = sender.send(AppMessage::Status("Sync Complete".to_string()));
        session.release();
    });
}

/// Delete an INBOX message on the server, by UID. Best-effort trash-copy
/// first: on Gmail a copy to [Gmail]/Trash removes the message everywhere,
/// elsewhere a copy in the server's Trash (`trash_mailboxes`) keeps the safety net; the
/// STORE+expunge below is what actually removes it from INBOX either way.
/// UIDPLUS uid_expunge targets just our message; plain EXPUNGE fallback also
/// flushes any other \Deleted-flagged mail, which matches client convention.
/// Remove a message from `folder_tag`'s mailbox on the server.
///
/// `permanent` decides whether the safety net runs. Normally the message is
/// copied into the server's own Trash first, so expunging it here only drops
/// it from this mailbox and it stays recoverable for as long as the provider
/// keeps its Trash. A permanent delete skips that copy, which for
/// `[Gmail]/All Mail` means the message is gone from the account outright —
/// All Mail is the last label holding it.
pub(crate) fn delete_on_server(
    mut account: AccountInfo,
    mailbox: String,
    uid: u32,
    permanent: bool,
    sender: calloop::channel::Sender<AppMessage>,
) {
    std::thread::spawn(move || {
        if is_mock_account(&account) {
            return;
        }
        // Same guard as start_sync: dialing out with an empty password gets
        // the server's baffling "Empty username or password" instead of the
        // actual problem — the keyring never yielded the credential.
        if !account.is_oauth && account.password.is_empty() {
            eprintln!("cce-mail: server delete: no password (keyring locked or entry missing); not connecting");
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, true) else {
            return;
        };
        // The uid only means anything inside its own mailbox, so this must be
        // the one the message was synced from.
        if session.select(&mailbox).is_err() {
            let msg = format!("Failed to select {}", mailbox);
            eprintln!("cce-mail: {}", msg);
            let _ = sender.send(AppMessage::StatusError(msg));
            session.release();
            return;
        }
        let uid_set = uid.to_string();
        if !permanent {
            for trash in trash_mailboxes(&mut session) {
                let Some(quoted) = quote_mailbox(&trash) else { continue };
                if session.uid_copy(&uid_set, &quoted).is_ok() {
                    break;
                }
            }
        }
        if let Err(e) = session.uid_store(&uid_set, "+FLAGS (\\Deleted)") {
            eprintln!("cce-mail: Server delete failed: {}", e);
            let _ = sender.send(AppMessage::StatusError(format!("Server delete failed: {}", e)));
            session.release();
            return;
        }
        if session.uid_expunge(&uid_set).is_err() {
            let _ = session.expunge();
        }
        let _ = sender.send(AppMessage::Status(
            if permanent { "Deleted permanently on server" } else { "Deleted on server" }.to_string(),
        ));
        session.release();
    });
}

/// One message on its way between two of the account's server folders, as
/// [`move_on_server`] hands it back: what [`settle_move`] needs to find the
/// row the move took ahead of the server and put it right.
#[derive(Debug, Clone)]
pub(crate) struct MoveJob {
    /// The account it belongs to: an answer landing after an account switch
    /// settles that account's cache, not the rows on screen.
    pub(crate) account: String,
    /// The row's id. Until the answer it sits in `to` with no uid.
    pub(crate) id: usize,
    pub(crate) from: String,
    pub(crate) to: String,
    /// Its uid in `from` — it numbers nothing in `to`.
    pub(crate) uid: u32,
    /// Only file a copy in `to` ([`move_keeps_source`]): the row never left.
    pub(crate) copy_only: bool,
    /// A restore out of Trash ([`restore_on_server`]): `from` is the local
    /// Trash and `uid` the dead one the message had in `to` before it was
    /// deleted, so a failed one goes back to Trash still remembering `to`.
    pub(crate) restore: bool,
}

/// Whether a move out of folder `tag` (mailbox `mailbox`) must leave the
/// message where it is. Gmail's All Mail and Sent Mail are not folders but
/// views over every message and every sent one, and expunging from All Mail
/// deletes — it is the last label a message has (see `DeleteSelected`). Out
/// of those a move can only add the destination's label: a copy.
pub(crate) fn move_keeps_source(tag: &str, mailbox: &str) -> bool {
    (tag == TAG_ARCHIVE || tag == TAG_SENT)
        && (mailbox.starts_with("[Gmail]/") || mailbox.starts_with("[Google Mail]/"))
}

/// `name` as an IMAP quoted string. `uid_copy` sends its mailbox argument
/// as given, which breaks on the first space ("Events &- Tickets"). `None`
/// for a name carrying a line break, which no quoting can send.
pub(crate) fn quote_mailbox(name: &str) -> Option<String> {
    if name.contains(['\r', '\n']) {
        return None;
    }
    Some(format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// The destination uid out of a `COPYUID` response code (RFC 4315 §3),
/// `[COPYUID <uidvalidity> <source uids> <destination uids>]`, for a move or
/// copy of one message.
pub(crate) fn copyuid_dest(response: &[u8]) -> Option<u32> {
    let text = String::from_utf8_lossy(response).to_ascii_uppercase();
    let rest = &text[text.find("[COPYUID ")? + "[COPYUID ".len()..];
    let mut fields = rest[..rest.find(']')?].split_ascii_whitespace();
    let (_validity, _source) = (fields.next()?, fields.next()?);
    fields.next()?.parse().ok()
}

/// Move a message between two of the account's server mailboxes, by UID,
/// and report the uid it was given in the destination as
/// [`AppMessage::MoveFinished`].
///
/// `UID MOVE` (RFC 6851) where the server has it — every major provider
/// does — and with UIDPLUS it names the new uid in an untagged `COPYUID`.
/// Without MOVE it is COPY, STORE \Deleted and EXPUNGE, as for a delete;
/// that COPYUID rides the tagged OK, which the imap crate drops, so the uid
/// comes back `None` and the destination's next sync brings the message in.
/// A `copy_only` job stops at the COPY.
pub(crate) fn move_on_server(
    mut account: AccountInfo,
    job: MoveJob,
    from_mailbox: String,
    to_mailbox: String,
    sender: calloop::channel::Sender<AppMessage>,
) {
    std::thread::spawn(move || {
        let report = |r: Result<Option<u32>, String>| {
            if let Err(e) = &r {
                eprintln!("cce-mail: move to {}: {}", to_mailbox, e);
            }
            let _ = sender.send(AppMessage::MoveFinished(job.clone(), r));
        };
        if is_mock_account(&account) {
            // No server: the uid is the mock's own and stays good.
            report(Ok(Some(job.uid)));
            return;
        }
        // Same guard as start_sync: dialing out with an empty password gets
        // the server's baffling "Empty username or password" instead of the
        // actual problem — the keyring never yielded the credential.
        if !account.is_oauth && account.password.is_empty() {
            report(Err("No password — is the keyring unlocked?".to_string()));
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            report(Err("IMAP connection failed".to_string()));
            return;
        };
        let result = move_in_session(&mut session, &job, &from_mailbox, &to_mailbox);
        session.release();
        report(result);
    });
}

fn move_in_session(
    session: &mut ImapSession,
    job: &MoveJob,
    from: &str,
    to: &str,
) -> Result<Option<u32>, String> {
    let to_quoted = quote_mailbox(to).ok_or_else(|| format!("Bad mailbox name {to:?}"))?;
    // The uid only means anything inside its own mailbox.
    session.select(from).map_err(|e| format!("Failed to select {from}: {e}"))?;
    move_selected(session, job.uid, from, &to_quoted, job.copy_only)
}

/// Move message `uid` of the selected mailbox `from` to `to_quoted` (an
/// IMAP quoted string) — or only copy it there — and report its uid there
/// when the server says.
fn move_selected(
    session: &mut ImapSession,
    uid: u32,
    from: &str,
    to_quoted: &str,
    copy_only: bool,
) -> Result<Option<u32>, String> {
    let uid = uid.to_string();
    if copy_only {
        session.uid_copy(&uid, to_quoted).map_err(|e| format!("Copy failed: {e}"))?;
        return Ok(None);
    }
    if session.capabilities().is_ok_and(|c| c.has_str("MOVE")) {
        let response = session
            .run_command_and_read_response(format!("UID MOVE {uid} {to_quoted}"))
            .map_err(|e| format!("Move failed: {e}"))?;
        return Ok(copyuid_dest(&response));
    }
    session.uid_copy(&uid, to_quoted).map_err(|e| format!("Copy failed: {e}"))?;
    // Copied but still here: an error, so the row goes back where the
    // message still is. The copy the destination now holds comes in with
    // its next sync.
    session
        .uid_store(&uid, "+FLAGS (\\Deleted)")
        .map_err(|e| format!("Copied, but could not remove it from {from}: {e}"))?;
    if session.uid_expunge(&uid).is_err() {
        let _ = session.expunge();
    }
    Ok(None)
}

/// The mailboxes that may be the account's own Trash, likeliest first: the
/// one LIST flags `\Trash` (RFC 6154), then the names guessed before that
/// was asked. Discovery leaves it out of the switcher on purpose — Trash
/// here is local — so it is asked for where it is needed. The name differs
/// by provider (`[Gmail]/Trash`, `[Gmail]/Bin`, iCloud's `Deleted
/// Messages`): with the guesses alone, an iCloud delete copied to a `Trash`
/// that does not exist and then expunged the message outright.
fn trash_mailboxes(session: &mut ImapSession) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(names) = session.list(Some(""), Some("*")) {
        for n in names.iter() {
            let flagged = n.attributes().iter().any(|a| matches!(a, NameAttribute::Trash));
            if flagged {
                out.push(n.name().to_string());
            }
        }
    }
    for guess in ["[Gmail]/Trash", "Trash"] {
        if !out.iter().any(|m| m == guess) {
            out.push(guess.to_string());
        }
    }
    out
}

/// A message deleted from INBOX, as the server's Trash should hold it.
/// Its uid there was never learned (the delete's COPYUID rides the tagged
/// OK, which the imap crate drops), so [`restore_on_server`] finds it by
/// what the sync parsed out of its header: the Date to the second, the
/// subject and the sender, each made by the same code from the same bytes.
#[derive(Debug, Clone)]
pub(crate) struct TrashedMail {
    pub(crate) ts: i64,
    pub(crate) subject: String,
    pub(crate) from: String,
}

/// An IMAP `date` (RFC 3501 §9: `13-Oct-2026`) for the UTC day `ts` falls on.
pub(crate) fn imap_date(ts: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let d = mail_parser::DateTime::from_timestamp(ts);
    format!("{}-{}-{}", d.day, MONTHS[(d.month.clamp(1, 12) - 1) as usize], d.year)
}

/// Whether a header the server holds is `mail`'s.
pub(crate) fn is_trashed_mail(msg: &mail_parser::Message, mail: &TrashedMail) -> bool {
    msg.date().map(|d| d.to_timestamp()) == Some(mail.ts)
        && msg.subject().unwrap_or("(No Subject)") == mail.subject
        && format_from(msg) == mail.from
}

/// The uid of `mail` in the selected mailbox, if it is there; the newest,
/// should it be there twice.
fn find_trashed(session: &mut ImapSession, mail: &TrashedMail) -> Result<Option<u32>, String> {
    // SENTSINCE / SENTBEFORE read the Date header's own calendar day, in its
    // own zone, which can be a day either side of the UTC one.
    const DAY: i64 = 86_400;
    let query = format!("SENTSINCE {} SENTBEFORE {}", imap_date(mail.ts - DAY), imap_date(mail.ts + 2 * DAY));
    let uids = session.uid_search(&query).map_err(|e| format!("Search failed: {e}"))?;
    if uids.is_empty() {
        return Ok(None);
    }
    let set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
    let fetches = session.uid_fetch(&set, "(UID RFC822.HEADER)").map_err(|e| format!("Fetch failed: {e}"))?;
    let parser = mail_parser::MessageParser::default();
    Ok(fetches
        .iter()
        .filter_map(|f| {
            let msg = parser.parse(f.header()?)?;
            is_trashed_mail(&msg, mail).then_some(f.uid?)
        })
        .max())
}

/// Find `mail` in the server's Trash and move it back to `to_mailbox`,
/// reporting as a move does ([`AppMessage::MoveFinished`]): the uid it was
/// given there, or why not — the commonest why being a Trash emptied since
/// (Gmail empties its own after 30 days).
pub(crate) fn restore_on_server(
    mut account: AccountInfo,
    job: MoveJob,
    mail: TrashedMail,
    to_mailbox: String,
    sender: calloop::channel::Sender<AppMessage>,
) {
    std::thread::spawn(move || {
        let report = |r: Result<Option<u32>, String>| {
            if let Err(e) = &r {
                eprintln!("cce-mail: restore to {}: {}", to_mailbox, e);
            }
            let _ = sender.send(AppMessage::MoveFinished(job.clone(), r));
        };
        if is_mock_account(&account) {
            // No server: the uid is the mock's own and stays good.
            report(Ok(Some(job.uid)));
            return;
        }
        // Same guard as start_sync: dialing out with an empty password gets
        // the server's baffling "Empty username or password" instead of the
        // actual problem — the keyring never yielded the credential.
        if !account.is_oauth && account.password.is_empty() {
            report(Err("No password — is the keyring unlocked?".to_string()));
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            report(Err("IMAP connection failed".to_string()));
            return;
        };
        let result = restore_in_session(&mut session, &mail, &to_mailbox);
        session.release();
        report(result);
    });
}

fn restore_in_session(session: &mut ImapSession, mail: &TrashedMail, to: &str) -> Result<Option<u32>, String> {
    let to_quoted = quote_mailbox(to).ok_or_else(|| format!("Bad mailbox name {to:?}"))?;
    for trash in trash_mailboxes(session) {
        if session.select(&trash).is_err() {
            continue;
        }
        if let Some(uid) = find_trashed(session, mail)? {
            return move_selected(session, uid, &trash, &to_quoted, false);
        }
    }
    Err("Not found in the server's Trash — emptied since?".to_string())
}

/// Write attachment bytes into ~/Downloads under a collision-safe name.
/// The server-supplied filename is reduced to its final path component —
/// a hostile "../.ssh/authorized_keys" must not escape the directory.
pub(crate) fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<String, String> {
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
pub(crate) fn fetch_attachment(
    mut account: AccountInfo,
    mailbox: String,
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
        // Same guard as start_sync: dialing out with an empty password gets
        // the server's baffling "Empty username or password" instead of the
        // actual problem — the keyring never yielded the credential.
        if !account.is_oauth && account.password.is_empty() {
            eprintln!("cce-mail: attachment fetch: no password (keyring locked or entry missing); not connecting");
            report(&sender, Err("No password — is the keyring unlocked?".to_string()));
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            report(&sender, Err("IMAP connection failed".to_string()));
            return;
        };
        // The same mailbox the sync pulled this message from. A uid only
        // means anything inside its own mailbox: opening an attachment on an
        // archived message after selecting INBOX would address whatever
        // happened to hold that uid there, which is a different message.
        let mailboxes: &[&str] = &[mailbox.as_str()];
        if !mailboxes.iter().any(|mb| session.select(mb).is_ok()) {
            report(&sender, Err("Cannot select mailbox".to_string()));
            session.release();
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
        session.release();
    });
}

/// Fetch a message's full text/html part by UID for the HTML view — the
/// sync caches only a 1200-char text preview. Same thread shape as
/// [`fetch_attachment`]; the outcome comes back as
/// [`AppMessage::HtmlFetched`]. The BODYSTRUCTURE is fetched here rather
/// than reusing anything from sync time, so it works for mail cached
/// before this feature existed and can never go stale.
#[cfg(feature = "wpe")]
pub(crate) fn fetch_html_part(
    mut account: AccountInfo,
    mailbox: String,
    uid: u32,
    email_id: usize,
    sender: calloop::channel::Sender<AppMessage>,
) {
    std::thread::spawn(move || {
        // Falling back to text is silent in the UI by design, but the WHY
        // must not vanish with it — stderr, like every other failure here.
        let report = |r: Option<HtmlMail>, why: &str| {
            if r.is_none() {
                eprintln!("cce-mail: html fetch uid {uid} in {mailbox:?}: {why}");
            }
            let _ = sender.send(AppMessage::HtmlFetched(email_id, r));
        };
        if is_mock_account(&account) {
            report(None, "mock account has no server copy");
            return;
        }
        // Same guard as start_sync: dialing out with an empty password gets
        // the server's baffling "Empty username or password" instead of the
        // actual problem — the keyring never yielded the credential.
        if !account.is_oauth && account.password.is_empty() {
            report(None, "no password (keyring locked or entry missing); not connecting");
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            report(None, "IMAP connection failed");
            return;
        };
        if let Err(e) = session.select(&mailbox) {
            report(None, &format!("cannot select mailbox: {e}"));
            session.release();
            return;
        }
        // One structure fetch feeds both walks: the html part to render and
        // the Content-ID parts its `cid:` references may name.
        let parsed = match session.uid_fetch(uid.to_string(), "(BODYSTRUCTURE)") {
            Ok(fetches) => match fetches.iter().next() {
                Some(f) => match f.bodystructure() {
                    Some(bs) => match find_html_part(bs) {
                        Some(spec) => Ok((spec, find_inline_parts(bs))),
                        None => Err("no text/html part in the structure".to_string()),
                    },
                    None => Err("fetch reply carried no BODYSTRUCTURE".to_string()),
                },
                None => Err("server returned no message for the uid".to_string()),
            },
            Err(e) => Err(format!("BODYSTRUCTURE fetch failed: {e}")),
        };
        let (spec, inline_specs) = match parsed {
            Ok(parts) => parts,
            Err(why) => {
                report(None, &why);
                session.release();
                return;
            }
        };
        let query = format!("(UID BODY.PEEK[{}]<0.{}>)", section_str(&spec.path), HTML_FETCH_CAP);
        let section_path = imap_proto::types::SectionPath::Part(spec.path.clone(), None);
        let html = match session.uid_fetch(uid.to_string(), &query) {
            Ok(fetches) => match fetches.iter().next().and_then(|f| f.section(&section_path)) {
                Some(bytes) => match mail_parser::MessageParser::default()
                    .parse(&spec.synthesize(bytes))
                    .and_then(|m| m.body_html(0).map(|c| c.into_owned()))
                {
                    Some(html) => Ok(html),
                    None => Err("part bytes did not decode as HTML".to_string()),
                },
                None => Err(format!("no data for section {}", section_str(&spec.path))),
            },
            Err(e) => Err(format!("part fetch failed: {e}")),
        };
        match html {
            Ok(html) => {
                // Only the inline parts the HTML actually names are worth a
                // round trip; the rest are ordinary attachments to the chips.
                let mut inline = Vec::new();
                let mut total = 0usize;
                for p in inline_specs {
                    if !html.contains(&p.cid) {
                        continue;
                    }
                    if total >= INLINE_TOTAL_CAP {
                        eprintln!(
                            "cce-mail: html fetch uid {uid}: inline images over {INLINE_TOTAL_CAP} bytes; the rest will show broken"
                        );
                        break;
                    }
                    let query = format!(
                        "(UID BODY.PEEK[{}]<0.{}>)",
                        section_str(&p.section),
                        INLINE_PART_CAP
                    );
                    let section_path =
                        imap_proto::types::SectionPath::Part(p.section.clone(), None);
                    match session.uid_fetch(uid.to_string(), &query) {
                        Ok(fetches) => {
                            if let Some(bytes) =
                                fetches.iter().next().and_then(|f| f.section(&section_path))
                            {
                                let decoded = decode_part_bytes(&p.encoding, bytes);
                                total += decoded.len();
                                inline.push((p.cid, p.mime, decoded));
                            }
                        }
                        Err(e) => {
                            eprintln!("cce-mail: html fetch uid {uid}: inline part {} failed: {e}", p.cid)
                        }
                    }
                }
                report(Some(HtmlMail { html, inline }), "");
            }
            Err(why) => report(None, &why),
        }
        session.release();
    });
}

/// Push a message's read state to the server (INBOX, by UID). UI-silent
/// (stderr still logs failures): this fires on every message open, so no
/// Connecting/success toasts, and a failed push is self-healing — the
/// EmailsSynced merge keeps locally-read mail read regardless of the
/// server flag until a later push converges.
pub(crate) fn set_seen_on_server(mut account: AccountInfo, uid: u32, seen: bool, sender: calloop::channel::Sender<AppMessage>) {
    std::thread::spawn(move || {
        if is_mock_account(&account) {
            return;
        }
        // Same guard as start_sync: dialing out with an empty password gets
        // the server's baffling "Empty username or password" instead of the
        // actual problem — the keyring never yielded the credential.
        if !account.is_oauth && account.password.is_empty() {
            eprintln!("cce-mail: seen-flag push: no password (keyring locked or entry missing); not connecting");
            return;
        }
        let Some(mut session) = open_imap_session(&mut account, &sender, false) else {
            return;
        };
        if session.select("INBOX").is_ok() {
            let query = if seen { "+FLAGS (\\Seen)" } else { "-FLAGS (\\Seen)" };
            let _ = session.uid_store(uid.to_string(), query);
        }
        session.release();
    });
}
