//! Accounts and what mail keeps on disk: the accounts file, secrets in the keyring
//! (and their migration), the selected account, the list split, folders and the
//! per-account message cache.
//! Split out of main.rs on 2026-10-10.

use crate::*;

pub(crate) fn get_accounts_path() -> std::path::PathBuf {
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
pub(crate) const MANAGE_ACCOUNTS_OPTION: &str = "Manage Accounts…";

pub(crate) fn account_dropdown_options(accounts: &[AccountInfo]) -> Vec<String> {
    accounts
        .iter()
        .map(|a| a.email.clone())
        .chain(std::iter::once(MANAGE_ACCOUNTS_OPTION.to_string()))
        .collect()
}

pub(crate) fn load_accounts() -> Vec<AccountInfo> {
    let path = get_accounts_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut accounts) = serde_json::from_str::<Vec<AccountInfo>>(&content) {
                if resolve_account_secrets(&mut accounts) {
                    // A plaintext password just moved into the keyring —
                    // clear it from the file now so it stops living on disk.
                    persist_keyring_migration(&accounts);
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
pub(crate) const KEYRING_SERVICE: &str = "cce-mail";
pub(crate) const KEYRING_SERVICE_LEGACY: &str = "cce-email";

/// Resolve account passwords through the Secret Service (KeePassXC here).
/// An empty on-disk password field is filled from the keyring; a plaintext
/// one is migrated INTO the keyring (returns true so the caller rewrites the
/// redacted file). In-memory passwords stay resolved for the IMAP/SMTP
/// workers. The mock account never touches the keyring. If the keyring is
/// locked, the provider pops its unlock dialog and this blocks until the
/// user answers — the standard desktop flow.
pub(crate) fn resolve_account_secrets(accounts: &mut [AccountInfo]) -> bool {
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
pub(crate) fn legacy_keyring_password(email: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE_LEGACY, email)
        .ok()?
        .get_password()
        .ok()
}

/// accounts.json belongs to cce-system-interface, so this app never writes
/// its own list back — only the one change it means, re-read under the lock
/// (`accounts_file`). Writing the list it loaded at startup is what deleted
/// accounts added in Settings since, and revived ones removed there.
///
/// Passwords now in the keyring: blank the on-disk copy, but only where the
/// file still holds the very password that was moved, so a password Settings
/// has changed in the meantime is not touched.
pub(crate) fn persist_keyring_migration(accounts: &[AccountInfo]) {
    let moved: Vec<(&str, &str)> = accounts
        .iter()
        .filter(|a| a.keyring_backed && !a.password.is_empty())
        .map(|a| (a.email.as_str(), a.password.as_str()))
        .collect();
    if moved.is_empty() {
        return;
    }
    let result = accounts_file::update::<serde_json::Value, _>(&get_accounts_path(), |on_disk| {
        for acc in on_disk.iter_mut() {
            let (Some(email), Some(password)) = (acc["email"].as_str(), acc["password"].as_str()) else {
                continue;
            };
            if moved.contains(&(email, password)) {
                acc["password"] = serde_json::Value::String(String::new());
            }
        }
    });
    if let Err(e) = result {
        eprintln!("cce-mail: could not clear migrated passwords from accounts.json: {e}");
    }
}

/// A refreshed OAuth token, written into that one account. An account no
/// longer in the file was removed in Settings, and stays removed.
pub(crate) fn persist_account_tokens(email: &str, access_token: Option<&str>, expiry: Option<u64>) {
    let result = accounts_file::update::<serde_json::Value, _>(&get_accounts_path(), |on_disk| {
        for acc in on_disk.iter_mut().filter(|a| a["email"] == email) {
            if let Some(obj) = acc.as_object_mut() {
                obj.insert("access_token".into(), serde_json::json!(access_token));
                obj.insert("token_expiry".into(), serde_json::json!(expiry));
            }
        }
    });
    if let Err(e) = result {
        eprintln!("cce-mail: could not save the refreshed token for {email}: {e}");
    }
}

/// Sidecar remembering which account was last selected, so a restart returns
/// to (and on-start syncs) the account the user actually reads. Deliberately
/// not in accounts.json — that file is owned by cce-system-interface.
pub(crate) fn selected_account_path() -> std::path::PathBuf {
    cce_ui::config::cce_config_dir().join("cce-mail-account.txt")
}

pub(crate) fn load_selected_account_email() -> Option<String> {
    std::fs::read_to_string(selected_account_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) fn save_selected_account_email(email: &str) {
    let _ = std::fs::write(selected_account_path(), email);
}

/// Sidecar remembering the dragged list/detail split, same pattern as the
/// account sidecar above (config.kdl stays the user's file).
pub(crate) fn split_path() -> std::path::PathBuf {
    cce_ui::config::cce_config_dir().join("cce-mail-split.txt")
}

pub(crate) fn load_list_w() -> Option<f32> {
    std::fs::read_to_string(split_path())
        .ok()?
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|w| (LIST_W_MIN..=4000.0).contains(w))
}

pub(crate) fn save_list_w(w: f32) {
    let _ = std::fs::write(split_path(), format!("{:.0}", w));
}

/// Sidecar holding the folders last reported by an account's server, so the
/// switcher is populated before the first sync answers.
pub(crate) fn get_folders_path(email: &str) -> std::path::PathBuf {
    let p = cce_ui::config::cce_config_dir();
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
    }
    let safe = email.replace('@', "_").replace('.', "_");
    p.join(format!("folders_{}.json", safe))
}

pub(crate) fn load_folders(email: &str) -> Vec<FolderInfo> {
    std::fs::read_to_string(get_folders_path(email))
        .ok()
        .and_then(|c| serde_json::from_str::<Vec<FolderInfo>>(&c).ok())
        .filter(|f| !f.is_empty())
        .unwrap_or_else(default_folders)
}

pub(crate) fn save_folders(email: &str, folders: &[FolderInfo]) {
    if let Ok(json) = serde_json::to_string_pretty(folders) {
        let _ = std::fs::write(get_folders_path(email), json);
    }
}

/// The body of `ClearEmailApp::filtered_emails`, over the fields it reads, so
/// a caller that also borrows other fields mutably can call it. The answer
/// is cached under (`rev`, folder, search) — see `filter_cache`.
pub(crate) fn filtered_from<'a>(
    emails: &'a [Email],
    rev: u64,
    folder: &str,
    search: &str,
    cache: &std::cell::RefCell<Option<(u64, String, String, Vec<usize>)>>,
) -> Vec<&'a Email> {
    let mut cache = cache.borrow_mut();
    let fresh = cache.as_ref().is_some_and(|(r, f, q, _)| *r == rev && f == folder && q == search);
    if !fresh {
        let search_lower = search.to_lowercase();
        let idx = emails
            .iter()
            .enumerate()
            .filter(|(_, e)| e.folder == folder)
            .filter(|(_, e)| {
                search_lower.is_empty()
                    || e.from.to_lowercase().contains(&search_lower)
                    || e.subject.to_lowercase().contains(&search_lower)
                    || e.body.to_lowercase().contains(&search_lower)
            })
            .map(|(i, _)| i)
            .collect();
        *cache = Some((rev, folder.to_string(), search.to_string(), idx));
    }
    let (_, _, _, idx) = cache.as_ref().unwrap();
    idx.iter().filter_map(|&i| emails.get(i)).collect()
}

/// How long after a change the active account's cache is saved.
pub(crate) const EMAILS_SAVE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) fn get_account_emails_path(email: &str) -> std::path::PathBuf {
    let p = cce_ui::config::cce_config_dir();
    if !p.exists() {
        let _ = std::fs::create_dir_all(&p);
    }
    let safe_email = email.replace('@', "_").replace('.', "_");
    p.join(format!("emails_{}.json", safe_email))
}

pub(crate) fn load_emails_for_account(email: &str) -> Vec<Email> {
    let path = get_account_emails_path(email);
    // A save still on its way to disk is the newest copy.
    if let Some(pending) = email_store::pending(&path) {
        return pending;
    }
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

/// Hand `emails` to the background writer (`email_store`). Until
/// 2026-10-05 this pretty-printed and wrote the whole cache on the UI thread
/// — 13.5 MB for the Gmail account — on every unread open, flag toggle,
/// delete and synced folder.
pub(crate) fn save_emails_for_account(email: &str, emails: Vec<Email>) {
    email_store::save(get_account_emails_path(email), emails);
}

/// The per-account mail caches, written off the UI thread.
///
/// A save lands in `PENDING` and a writer thread takes it from there:
/// compact JSON (the reader takes either form) to `<file>.tmp`, renamed
/// over the cache so a crash cannot truncate it. A later save of the same
/// file replaces a pending one, so a burst costs one write. A load consults
/// `PENDING` first ([`pending`]), so nothing reads a cache older than the
/// newest save; [`flush`] waits for the writer to finish, for the exit.
pub(crate) mod email_store {
    use super::Email;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    struct Store {
        pending: Mutex<HashMap<PathBuf, Arc<Vec<Email>>>>,
        /// Signalled when something is pending, and when a write finishes.
        changed: Condvar,
    }

    fn store() -> &'static Store {
        static STORE: OnceLock<&'static Store> = OnceLock::new();
        STORE.get_or_init(|| {
            let store: &'static Store =
                Box::leak(Box::new(Store { pending: Mutex::new(HashMap::new()), changed: Condvar::new() }));
            std::thread::Builder::new()
                .name("mail-cache-writer".into())
                .spawn(move || writer(store))
                .expect("mail cache writer thread");
            store
        })
    }

    pub fn save(path: PathBuf, emails: Vec<Email>) {
        let s = store();
        s.pending.lock().unwrap().insert(path, Arc::new(emails));
        s.changed.notify_all();
    }

    pub fn pending(path: &PathBuf) -> Option<Vec<Email>> {
        let s = store();
        let pending = s.pending.lock().unwrap();
        pending.get(path).map(|e| (**e).clone())
    }

    /// Block until every pending save is on disk.
    pub fn flush() {
        let s = store();
        let mut pending = s.pending.lock().unwrap();
        while !pending.is_empty() {
            pending = s.changed.wait(pending).unwrap();
        }
    }

    fn writer(s: &'static Store) {
        loop {
            let batch: Vec<(PathBuf, Arc<Vec<Email>>)> = {
                let mut pending = s.pending.lock().unwrap();
                while pending.is_empty() {
                    pending = s.changed.wait(pending).unwrap();
                }
                pending.iter().map(|(p, e)| (p.clone(), Arc::clone(e))).collect()
            };
            for (path, emails) in batch {
                write(&path, &emails);
                // Done with it unless a newer save replaced it meanwhile.
                let mut pending = s.pending.lock().unwrap();
                if pending.get(&path).is_some_and(|cur| Arc::ptr_eq(cur, &emails)) {
                    pending.remove(&path);
                }
            }
            s.changed.notify_all();
        }
    }

    fn write(path: &PathBuf, emails: &[Email]) {
        let Ok(bytes) = serde_json::to_vec(emails) else { return };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, path)) {
            eprintln!("[cce-mail] mail cache {}: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }
}
