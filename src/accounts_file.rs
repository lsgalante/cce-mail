//! Safe writes to the shared `accounts.json`.
//!
//! Two processes write that file — cce-system-interface (it owns the account
//! list) and cce-mail (it refreshes OAuth tokens and moves plaintext passwords
//! into the keyring) — and cce-calendar's sync reads it. Until 2026-10-01 each
//! writer saved its whole in-memory list with a plain `fs::write`, which lost
//! updates both ways: cce-mail, refreshing a token an hour after it started,
//! wrote back the list it had loaded then, deleting an account added in
//! Settings since and resurrecting one removed there, refresh token included.
//! And a plain write truncates before it writes, so a reader in between saw
//! an empty file, fell back to the mock account, and its next save replaced
//! the real accounts with it.
//!
//! So a writer never saves a list it holds. It hands [`update`] the change
//! it means to make, and `update` re-reads the file, applies it, and writes
//! the result back — holding `accounts.json.lock` throughout, so the two
//! writers take turns, and replacing the file by rename, so a reader sees the
//! old file or the new one and never half of either. A file that does not
//! parse is never overwritten: that is somebody's accounts, and an error is
//! cheaper than guessing.
//!
//! The same helper lives in cce-system-interface as `src/accounts_file.rs`; the
//! two must agree on the lock file's name, so change both together.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// Apply `change` to the accounts in `path` as they are NOW, write them back
/// atomically under the writers' lock, and return what was written. A missing
/// file is an empty list.
pub fn update<T, F>(path: &Path, change: F) -> io::Result<Vec<T>>
where
    T: serde::Serialize + serde::de::DeserializeOwned,
    F: FnOnce(&mut Vec<T>),
{
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(lock_path(path))?;
    // Released when `lock` drops, after the rename.
    lock.lock()?;
    let mut accounts: Vec<T> = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} does not parse ({e}); left as it is", path.display()),
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    change(&mut accounts);
    let text = serde_json::to_string_pretty(&accounts).map_err(io::Error::other)?;
    write_atomic(path, text.as_bytes())?;
    Ok(accounts)
}

/// Write a sibling temp file (0600 from creation: it holds tokens), flush it
/// to disk, then rename it over `path`.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_name().unwrap_or_default());
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let _ = std::fs::remove_file(&tmp);
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("accounts-file-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("accounts.json")
    }

    fn emails(path: &Path) -> Vec<String> {
        let v: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        v.iter().map(|a| a["email"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_missing_file_is_created_private() {
        let path = scratch("create");
        update::<Value, _>(&path, |a| a.push(json!({ "email": "a@x" }))).unwrap();
        assert_eq!(emails(&path), ["a@x"]);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No temp file left beside it.
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().flatten()
            .map(|e| e.file_name().into_string().unwrap()).collect();
        assert!(left.iter().all(|n| n == "accounts.json" || n == "accounts.json.lock"), "{left:?}");
    }

    #[test]
    fn a_file_that_does_not_parse_is_never_overwritten() {
        let path = scratch("corrupt");
        std::fs::write(&path, "[{\"email\": \"a@x\"").unwrap();
        let err = update::<Value, _>(&path, |a| a.clear()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[{\"email\": \"a@x\"");
    }

    #[test]
    fn a_stale_writer_keeps_what_the_other_one_added() {
        // The bug: mail loaded [a], Settings then added b, mail refreshed a's
        // token and wrote its [a] back. A targeted change keeps b.
        let path = scratch("stale");
        std::fs::write(&path, r#"[{"email":"a@x","access_token":"old"}]"#).unwrap();
        update::<Value, _>(&path, |a| a.push(json!({ "email": "b@x" }))).unwrap();
        update::<Value, _>(&path, |accs| {
            for a in accs.iter_mut().filter(|a| a["email"] == "a@x") {
                a["access_token"] = json!("new");
            }
        })
        .unwrap();
        assert_eq!(emails(&path), ["a@x", "b@x"]);
        let v: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v[0]["access_token"], "new");
    }

    #[test]
    fn concurrent_writers_take_turns() {
        // flock is per open file, so threads opening the lock themselves
        // exclude each other exactly as two processes do.
        let path = scratch("concurrent");
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    update::<Value, _>(&path, |a| a.push(json!({ "email": format!("{i}@x") }))).unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut got = emails(&path);
        got.sort();
        let mut want: Vec<String> = (0..16).map(|i| format!("{i}@x")).collect();
        want.sort();
        assert_eq!(got, want, "an update was lost");
    }
}
