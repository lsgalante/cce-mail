//! The control socket: `/tmp/cce-mail-<WAYLAND_DISPLAY>.sock`.
//!
//! Line-oriented request/reply, the same shape as the compositor's control
//! socket: a client connects, writes one command line, and reads the reply
//! to EOF. `cce-mail ctl <command…>` is the CLI client ([`run_client`]);
//! `cce-mail ctl help` prints [`HELP`], the command list.
//!
//! Every command is answered by the app on its main thread, from the same
//! state the window paints — the listener thread only reads the line and
//! hands it over as [`AppMessage::Ipc`], carrying the stream so the reply
//! can be written once the app has one. Nothing here reads the on-disk
//! cache: it lags the window by a sync, and a reader would still have to
//! re-derive which account and folder the user is looking at.
//!
//! Replies are text by default — one row per line, tab-separated columns,
//! `error: …` on failure — and JSON with `--json`, for agents and scripts.
//! Account rows never carry passwords or tokens.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{AccountInfo, AppMessage, Email, FolderInfo};

/// Socket prefix; `cce_ui::ipc::socket_path` appends `-<WAYLAND_DISPLAY>`.
const PREFIX: &str = "cce-mail";

/// Bound on any one socket read or write. A client that connects and never
/// sends would otherwise park the listener thread forever, and one that
/// vanished mid-reply would park the app's main thread.
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Rows `list` and `search` return when no `--limit` is given.
pub const DEFAULT_LIMIT: usize = 50;

/// The socket path this process bound (and must unlink on exit), if any.
static OWNED_PATH: Mutex<Option<String>> = Mutex::new(None);

pub const HELP: &str = "\
cce-mail ctl <command> [--json]

  help                          this list
  status                        account, folder, counts, sync state
  accounts                      the configured accounts
  folders                       the selected account's folders, with counts
  list [folder] [--limit N] [--unread]
                                messages in a folder (default: the open one), newest first
  search <words…> [--limit N]   messages whose from/subject/body contain the words, every folder
  get <id>                      one message: headers and the cached text body (a preview, up to 1200 chars)
  open <id>                     show a message in the window (marks it read, like a click)
  mark-read <id>                set the read flag; mirrored to the server for inbox mail
  mark-unread <id>
  select-account <email>
  switch-folder <folder>        by tag (inbox, sent, drafts, trash, …) or label
  sync                          start a sync now
  compose [mailto:…]            open the compose dialog, prefilled from the URL if given
  quit

Text replies are tab-separated rows (list/search: id, read state, folder,
date, from, subject); --json returns JSON instead. Message ids are the app's
own and can change between syncs — take them from a fresh list.";

/// One command line off the socket, plus the stream to answer it on.
///
/// Clone-able (the engine's message type must be), so the stream sits behind
/// an `Arc`; the first [`respond`](Self::respond) takes it and later ones are
/// no-ops.
#[derive(Debug, Clone)]
pub struct Request {
    pub line: String,
    stream: Arc<Mutex<Option<UnixStream>>>,
}

impl Request {
    /// Write `reply` (newline-terminated) and close the write side, which is
    /// the EOF the client reads to.
    pub fn respond(&self, reply: &str) {
        let Some(mut stream) = self.stream.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let _ = stream.write_all(reply.as_bytes());
        if !reply.ends_with('\n') {
            let _ = stream.write_all(b"\n");
        }
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
}

/// Bind the control socket and serve it on a thread, pushing each received
/// line into the app's calloop channel as [`AppMessage::Ipc`].
///
/// A second instance finds the socket answering and runs without one — the
/// window still works, it just cannot be driven. A socket file nobody
/// answers on is a leftover from a crash and is replaced.
pub fn spawn_listener(sender: calloop::channel::Sender<AppMessage>) {
    let path = cce_ui::ipc::socket_path(PREFIX);
    if std::path::Path::new(&path).exists() {
        if UnixStream::connect(&path).is_ok() {
            eprintln!("cce-mail: another instance answers on {path}; this one runs without a control socket");
            return;
        }
        let _ = std::fs::remove_file(&path);
    }
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cce-mail: could not bind the control socket {path} ({e})");
            return;
        }
    };
    *OWNED_PATH.lock().unwrap() = Some(path);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let _ = conn.set_read_timeout(Some(IO_TIMEOUT));
            let mut reader = BufReader::new(conn);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let req = Request {
                line: line.trim().to_string(),
                stream: Arc::new(Mutex::new(Some(reader.into_inner()))),
            };
            if sender.send(AppMessage::Ipc(req)).is_err() {
                return; // channel gone: the app is shutting down
            }
        }
    });
}

/// Unlink the socket if this process bound it. Called after the engine loop
/// returns; a crash skips it, which is what the stale-socket replacement in
/// [`spawn_listener`] exists for.
pub fn cleanup() {
    if let Some(path) = OWNED_PATH.lock().unwrap().take() {
        let _ = std::fs::remove_file(path);
    }
}

/// `cce-mail ctl <args…>`: send the line, print the reply, return the exit
/// code — 0 on success, 1 when the app answered `error:`, 2 when no app
/// answered at all. `help` (or nothing) prints [`HELP`] without a socket.
pub fn run_client(args: &[String]) -> i32 {
    let line = args.join(" ");
    let line = line.trim();
    if line.is_empty() || line == "help" {
        println!("{HELP}");
        return 0;
    }
    match cce_ui::ipc::send_command(PREFIX, line) {
        Ok(reply) => {
            print!("{reply}");
            if reply.starts_with("error:") {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!(
                "error: cce-mail is not reachable at {} ({e})",
                cce_ui::ipc::socket_path(PREFIX)
            );
            2
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Help,
    Status,
    Accounts,
    Folders,
    List { folder: Option<String>, limit: usize, unread: bool },
    Search { query: String, limit: usize },
    Get(usize),
    Open(usize),
    MarkRead(usize, bool),
    SelectAccount(String),
    SwitchFolder(String),
    Sync,
    Compose(Option<String>),
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub command: Command,
    /// `--json` was given: render the reply as JSON.
    pub json: bool,
}

/// Parse one command line. Flags (`--json`, `--unread`, `--limit N`) may
/// sit anywhere; the remaining words are the command and its arguments.
pub fn parse(line: &str) -> Result<Parsed, String> {
    let mut json = false;
    let mut unread = false;
    let mut limit = DEFAULT_LIMIT;
    let mut words: Vec<&str> = Vec::new();
    let mut it = line.split_whitespace();
    while let Some(w) = it.next() {
        match w {
            "--json" => json = true,
            "--unread" => unread = true,
            "--limit" | "-n" => {
                let v = it.next().ok_or("--limit needs a number")?;
                limit = v.parse().map_err(|_| format!("bad limit {v:?}"))?;
            }
            _ if w.starts_with("--") => return Err(format!("unknown flag {w}")),
            _ => words.push(w),
        }
    }
    let Some((&cmd, rest)) = words.split_first() else {
        return Err("empty command (try `help`)".to_string());
    };
    let one_id = |rest: &[&str]| -> Result<usize, String> {
        match rest {
            [v] => v.parse().map_err(|_| format!("bad message id {v:?}")),
            _ => Err(format!("{cmd} takes exactly one message id")),
        }
    };
    let no_args = |rest: &[&str], c: Command| -> Result<Command, String> {
        if rest.is_empty() {
            Ok(c)
        } else {
            Err(format!("{cmd} takes no arguments"))
        }
    };
    let command = match cmd {
        "help" => no_args(rest, Command::Help)?,
        "status" => no_args(rest, Command::Status)?,
        "accounts" => no_args(rest, Command::Accounts)?,
        "folders" => no_args(rest, Command::Folders)?,
        "sync" => no_args(rest, Command::Sync)?,
        "quit" => no_args(rest, Command::Quit)?,
        // Folder labels can carry spaces ("All Mail"), so the rest is the name.
        "list" => Command::List {
            folder: (!rest.is_empty()).then(|| rest.join(" ")),
            limit,
            unread,
        },
        "search" => {
            if rest.is_empty() {
                return Err("search needs at least one word".to_string());
            }
            Command::Search { query: rest.join(" "), limit }
        }
        "get" => Command::Get(one_id(rest)?),
        "open" => Command::Open(one_id(rest)?),
        "mark-read" => Command::MarkRead(one_id(rest)?, true),
        "mark-unread" => Command::MarkRead(one_id(rest)?, false),
        "select-account" => match rest {
            [e] => Command::SelectAccount(e.to_string()),
            _ => return Err("select-account takes one email address".to_string()),
        },
        "switch-folder" => {
            if rest.is_empty() {
                return Err("switch-folder needs a folder tag or label".to_string());
            }
            Command::SwitchFolder(rest.join(" "))
        }
        "compose" => match rest {
            [] => Command::Compose(None),
            [u] => Command::Compose(Some(u.to_string())),
            _ => return Err("compose takes at most one mailto: URL".to_string()),
        },
        other => return Err(format!("unknown command {other:?} (try `help`)")),
    };
    Ok(Parsed { command, json })
}

// ---------------------------------------------------------------------------
// Rendering

pub fn error(msg: &str) -> String {
    format!("error: {msg}")
}

/// A success acknowledgement for a command that changes state.
pub fn ok(what: &str, json: bool) -> String {
    if json {
        serde_json::json!({ "ok": what }).to_string()
    } else {
        format!("ok: {what}")
    }
}

/// One text cell: tabs and newlines would break the row grammar.
fn cell(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Attachment names as the user sees them: what the server reported for
/// fetched mail, the local file names for drafts and sent copies.
fn attachment_names(e: &Email) -> Vec<String> {
    if !e.remote_attachments.is_empty() {
        e.remote_attachments.iter().map(|a| a.name.clone()).collect()
    } else {
        e.attachments
            .iter()
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.clone())
            })
            .collect()
    }
}

fn row_json(e: &Email) -> serde_json::Value {
    serde_json::json!({
        "id": e.id,
        "folder": e.folder,
        "read": e.read,
        "date": e.date,
        "ts": e.ts,
        "from": e.from,
        "to": e.to,
        "subject": e.subject,
        "attachments": attachment_names(e),
    })
}

/// `list` / `search` rows, in the order given (the app's list order: newest first).
pub fn render_rows(rows: &[&Email], json: bool) -> String {
    if json {
        return serde_json::Value::Array(rows.iter().map(|e| row_json(e)).collect()).to_string();
    }
    rows.iter()
        .map(|e| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                e.id,
                if e.read { "read" } else { "unread" },
                cell(&e.folder),
                cell(&e.date),
                cell(&e.from),
                cell(&e.subject),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `get`: the whole record. Text is headers, a blank line, then the body.
pub fn render_email(e: &Email, json: bool) -> String {
    if json {
        return serde_json::to_string(e).unwrap_or_else(|err| error(&err.to_string()));
    }
    let mut out = String::new();
    out.push_str(&format!("id: {}\n", e.id));
    out.push_str(&format!("folder: {}\n", cell(&e.folder)));
    out.push_str(&format!("from: {}\n", cell(&e.from)));
    out.push_str(&format!("to: {}\n", cell(&e.to)));
    if !e.cc.is_empty() {
        out.push_str(&format!("cc: {}\n", cell(&e.cc)));
    }
    if !e.bcc.is_empty() {
        out.push_str(&format!("bcc: {}\n", cell(&e.bcc)));
    }
    out.push_str(&format!("date: {}\n", cell(&e.date)));
    out.push_str(&format!("subject: {}\n", cell(&e.subject)));
    out.push_str(&format!("read: {}\n", if e.read { "yes" } else { "no" }));
    let names = attachment_names(e);
    if !names.is_empty() {
        out.push_str(&format!("attachments: {}\n", cell(&names.join(", "))));
    }
    out.push('\n');
    out.push_str(&e.body);
    out
}

/// `accounts`. Never the password, tokens or client secret.
pub fn render_accounts(accounts: &[AccountInfo], selected: usize, json: bool) -> String {
    if json {
        let rows: Vec<serde_json::Value> = accounts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                serde_json::json!({
                    "email": a.email,
                    "imap": a.imap,
                    "smtp": a.smtp,
                    "default": a.is_default,
                    "oauth": a.is_oauth,
                    "selected": i == selected,
                })
            })
            .collect();
        return serde_json::Value::Array(rows).to_string();
    }
    accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            format!(
                "{}\t{}\t{}\t{}",
                cell(&a.email),
                if i == selected { "selected" } else { "-" },
                if a.is_default { "default" } else { "-" },
                cell(&a.imap),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `folders`, with per-folder counts from the cached messages.
pub fn render_folders(folders: &[FolderInfo], emails: &[Email], current: &str, json: bool) -> String {
    let counts = |tag: &str| {
        let total = emails.iter().filter(|e| e.folder == tag).count();
        let unread = emails.iter().filter(|e| e.folder == tag && !e.read).count();
        (total, unread)
    };
    if json {
        let rows: Vec<serde_json::Value> = folders
            .iter()
            .map(|f| {
                let (total, unread) = counts(&f.tag);
                serde_json::json!({
                    "tag": f.tag,
                    "label": f.label,
                    "mailbox": if f.mailbox.is_empty() { serde_json::Value::Null } else { f.mailbox.clone().into() },
                    "current": f.tag == current,
                    "messages": total,
                    "unread": unread,
                })
            })
            .collect();
        return serde_json::Value::Array(rows).to_string();
    }
    folders
        .iter()
        .map(|f| {
            let (total, unread) = counts(&f.tag);
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                cell(&f.tag),
                cell(&f.label),
                if f.mailbox.is_empty() { "(local)".to_string() } else { cell(&f.mailbox) },
                if f.tag == current { "current" } else { "-" },
                total,
                unread,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What `status` reports, gathered by the app.
pub struct StatusInfo {
    pub account: Option<String>,
    pub accounts: usize,
    pub folder_tag: String,
    pub folder_label: String,
    pub cached: usize,
    pub in_folder: usize,
    pub unread_in_folder: usize,
    pub selected: Option<usize>,
    pub syncing: bool,
    pub last_sync_secs_ago: Option<u64>,
}

pub fn render_status(s: &StatusInfo, json: bool) -> String {
    let socket = cce_ui::ipc::socket_path(PREFIX);
    if json {
        return serde_json::json!({
            "account": s.account,
            "accounts": s.accounts,
            "folder": s.folder_tag,
            "folder_label": s.folder_label,
            "cached": s.cached,
            "in_folder": s.in_folder,
            "unread_in_folder": s.unread_in_folder,
            "selected": s.selected,
            "syncing": s.syncing,
            "last_sync_secs_ago": s.last_sync_secs_ago,
            "socket": socket,
        })
        .to_string();
    }
    let last = match s.last_sync_secs_ago {
        Some(secs) => format!("last started {secs}s ago"),
        None => "never".to_string(),
    };
    format!(
        "account: {} ({} configured)\nfolder: {} ({})\nmessages: {} cached, {} in folder, {} unread\nselected: {}\nsync: {}, {}\nsocket: {}",
        s.account.as_deref().unwrap_or("(none)"),
        s.accounts,
        s.folder_tag,
        s.folder_label,
        s.cached,
        s.in_folder,
        s.unread_in_folder,
        s.selected.map(|id| id.to_string()).unwrap_or_else(|| "none".to_string()),
        if s.syncing { "in flight" } else { "idle" },
        last,
        socket,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(parse("help").unwrap().command, Command::Help);
        assert_eq!(
            parse("list").unwrap().command,
            Command::List { folder: None, limit: DEFAULT_LIMIT, unread: false }
        );
        assert_eq!(
            parse("--json list All Mail --limit 5 --unread").unwrap(),
            Parsed {
                command: Command::List { folder: Some("All Mail".into()), limit: 5, unread: true },
                json: true
            }
        );
        assert_eq!(
            parse("search invoice due -n 3").unwrap().command,
            Command::Search { query: "invoice due".into(), limit: 3 }
        );
        assert_eq!(parse("get 42").unwrap().command, Command::Get(42));
        assert_eq!(parse("mark-unread 7").unwrap().command, Command::MarkRead(7, false));
        assert_eq!(
            parse("select-account a@b.c").unwrap().command,
            Command::SelectAccount("a@b.c".into())
        );
        assert_eq!(parse("switch-folder sent").unwrap().command, Command::SwitchFolder("sent".into()));
        assert_eq!(
            parse("compose mailto:x@y.z?subject=hi").unwrap().command,
            Command::Compose(Some("mailto:x@y.z?subject=hi".into()))
        );
        assert_eq!(parse("quit").unwrap().command, Command::Quit);
    }

    #[test]
    fn bad_lines_are_errors() {
        assert!(parse("").is_err());
        assert!(parse("bogus").is_err());
        assert!(parse("get").is_err());
        assert!(parse("get x").is_err());
        assert!(parse("get 1 2").is_err());
        assert!(parse("search").is_err());
        assert!(parse("list --limit").is_err());
        assert!(parse("list --limit many").is_err());
        assert!(parse("status --verbose").is_err());
        assert!(parse("sync now").is_err());
    }

    fn email(id: usize, subject: &str, read: bool) -> Email {
        Email {
            id,
            from: "a@b.c".into(),
            to: "me@x.y".into(),
            subject: subject.into(),
            body: "hello\nworld".into(),
            date: "2026-09-24".into(),
            read,
            folder: "inbox".into(),
            cc: String::new(),
            bcc: String::new(),
            attachments: vec!["/tmp/dir/report.pdf".into()],
            remote_attachments: Vec::new(),
            uid: Some(id as u32),
            ts: Some(1_000 + id as i64),
            origin_folder: None,
        }
    }

    #[test]
    fn rows_are_tab_separated_and_sanitized() {
        let e = email(3, "tabs\tand\nnewlines", false);
        let text = render_rows(&[&e], false);
        assert_eq!(text, "3\tunread\tinbox\t2026-09-24\ta@b.c\ttabs and newlines");
        let json: serde_json::Value = serde_json::from_str(&render_rows(&[&e], true)).unwrap();
        assert_eq!(json[0]["id"], 3);
        assert_eq!(json[0]["attachments"][0], "report.pdf");
        assert_eq!(render_rows(&[], false), "");
        assert_eq!(render_rows(&[], true), "[]");
    }

    #[test]
    fn email_text_has_headers_then_body() {
        let e = email(1, "Hi", true);
        let text = render_email(&e, false);
        assert!(text.starts_with("id: 1\nfolder: inbox\nfrom: a@b.c\n"));
        assert!(text.contains("\nread: yes\nattachments: report.pdf\n\nhello\nworld"));
        let json: serde_json::Value = serde_json::from_str(&render_email(&e, true)).unwrap();
        assert_eq!(json["body"], "hello\nworld");
    }

    #[test]
    fn accounts_never_leak_secrets() {
        let acc = AccountInfo {
            email: "me@x.y".into(),
            imap: "imap.x.y:993".into(),
            smtp: "smtp.x.y:587".into(),
            is_default: true,
            password: "hunter2".into(),
            is_oauth: true,
            access_token: Some("tok".into()),
            refresh_token: Some("ref".into()),
            token_expiry: None,
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            keyring_backed: false,
        };
        for json in [false, true] {
            let out = render_accounts(&[acc.clone()], 0, json);
            for secret in ["hunter2", "tok", "ref", "csec"] {
                assert!(!out.contains(secret), "{out}");
            }
            assert!(out.contains("me@x.y"));
        }
    }
}
