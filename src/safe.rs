//! Safe apply: a confirmation window that automatically restores the previous
//! profile when the user does not confirm.
//!
//! `apply --confirm-timeout N` writes a pending record and spawns
//! `__confirm-watch`; if `confirm` does not remove the record within N seconds,
//! the watcher restores `__previous`. The record carries a unique id so a newer
//! apply supersedes an older watcher, and the state/socket paths it needs.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ipc::{ApplyReport, restore};
use crate::schema::Store;

/// A pending safe-apply confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingApply {
    /// Unique id; a watcher only acts while the file still carries its id.
    pub id: String,
    /// Profile that was applied, for diagnostics.
    pub profile: String,
    /// Profile store to restore from.
    pub state: PathBuf,
    /// niri IPC socket used for the restore.
    pub socket: PathBuf,
    /// Unix milliseconds after which the watcher rolls back.
    pub expires_at_ms: u64,
}

/// The public summary of a pending confirmation attached to an apply report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingConfirmation {
    pub id: String,
    pub pending_path: PathBuf,
    pub expires_at_ms: u64,
    pub timeout_secs: u64,
}

/// Default pending record next to the profile store.
pub fn pending_path(state: &Path) -> PathBuf {
    state.with_file_name("pending.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_id() -> String {
    format!("{}-{}", std::process::id(), now_ms())
}

/// Creates and persists a pending confirmation, returning its summary.
pub fn write_pending(
    state: &Path,
    socket: &Path,
    profile: &str,
    timeout: Duration,
) -> Result<PendingConfirmation, String> {
    let path = pending_path(state);
    let pending = PendingApply {
        id: new_id(),
        profile: profile.to_owned(),
        state: state.to_path_buf(),
        socket: socket.to_path_buf(),
        expires_at_ms: now_ms() + timeout.as_millis() as u64,
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    }
    let body = serde_json::to_vec_pretty(&pending)
        .map_err(|err| format!("cannot encode pending confirmation: {err}"))?;
    fs::write(&path, body).map_err(|err| format!("cannot write {}: {err}", path.display()))?;
    Ok(PendingConfirmation {
        id: pending.id,
        pending_path: path,
        expires_at_ms: pending.expires_at_ms,
        timeout_secs: timeout.as_secs(),
    })
}

/// Loads the current pending record, if any.
pub fn load_pending(path: &Path) -> Result<Option<PendingApply>, String> {
    match fs::read(path) {
        Ok(body) => serde_json::from_slice(&body)
            .map(Some)
            .map_err(|err| format!("invalid pending confirmation {}: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// Removes the pending record. Returns whether one existed.
pub fn clear_pending(path: &Path) -> Result<bool, String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(format!("cannot remove {}: {err}", path.display())),
    }
}

/// Confirms the pending apply, keeping it. Returns whether a record existed.
pub fn confirm(path: &Path) -> Result<bool, String> {
    clear_pending(path)
}

/// Cancels a pending apply by restoring `__previous` immediately.
pub fn cancel(pending_path: &Path) -> Result<bool, String> {
    let Some(pending) = load_pending(pending_path)? else {
        return Ok(false);
    };
    let store = Store::load(&pending.state).map_err(|err| format!("cannot load state: {err}"))?;
    restore(&pending.socket, &store, &pending.state, false)?;
    clear_pending(pending_path)?;
    Ok(true)
}

/// Rolls back one pending record when its deadline has passed.
///
/// `expected_id` binds a watcher to the record it was spawned for: a newer
/// apply replaces the file and the old watcher exits without touching anything.
/// Returns the restore report when it rolled back.
pub fn expire_pending(
    pending_path: &Path,
    expected_id: &str,
    now_ms: u64,
) -> Result<Option<ApplyReport>, String> {
    let Some(pending) = load_pending(pending_path)? else {
        return Ok(None);
    };
    if pending.id != expected_id {
        return Ok(None);
    }
    if now_ms < pending.expires_at_ms {
        return Ok(None);
    }
    let store = Store::load(&pending.state).map_err(|err| format!("cannot load state: {err}"))?;
    let report = restore(&pending.socket, &store, &pending.state, false)?;
    clear_pending(pending_path)?;
    Ok(Some(report))
}

/// Watches the pending record until it is confirmed, superseded or expires.
pub fn watch_pending(
    pending_path: &Path,
    expected_id: &str,
    poll: Duration,
) -> Result<Option<ApplyReport>, String> {
    loop {
        match expire_pending(pending_path, expected_id, now_ms())? {
            Some(report) => return Ok(Some(report)),
            None => {
                let Some(pending) = load_pending(pending_path)? else {
                    return Ok(None);
                };
                if pending.id != expected_id {
                    return Ok(None);
                }
                std::thread::sleep(poll.max(Duration::from_millis(20)));
            }
        }
    }
}

/// Milliseconds since the Unix epoch, exposed for tests and CLI messages.
pub fn now_unix_ms() -> u64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    use serde_json::json;

    use super::*;
    use crate::schema::{OutputProfile, Profile};

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-monitor-safe-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_niri(path: &Path, requests: usize) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..requests {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                seen.push(line.trim().to_owned());
                let response = if line.contains("Outputs") {
                    json!({ "Ok": { "Outputs": {} } })
                } else {
                    json!({ "Ok": "Handled" })
                };
                let mut stream = stream;
                stream
                    .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                    .unwrap();
                stream.write_all(b"\n").unwrap();
                stream.flush().unwrap();
            }
            seen
        })
    }

    fn store_with_previous(path: &Path) {
        let mut store = Store::default();
        let mut profile = Profile::default();
        profile
            .outputs
            .insert("DP-1".to_owned(), OutputProfile::default());
        store.insert("__previous", profile);
        store.save(path).unwrap();
    }

    #[test]
    fn confirm_removes_a_pending_record() {
        let dir = temp_dir("confirm");
        let state = dir.join("profiles.json");
        let pending = write_pending(
            &state,
            Path::new("/tmp/niri.sock"),
            "desk",
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(load_pending(&pending.pending_path).unwrap().is_some());
        assert!(confirm(&pending.pending_path).unwrap());
        assert!(load_pending(&pending.pending_path).unwrap().is_none());
        assert!(!confirm(&pending.pending_path).unwrap());
    }

    #[test]
    fn not_yet_due_keeps_the_record_and_newer_ids_are_ignored() {
        let dir = temp_dir("due");
        let state = dir.join("profiles.json");
        let pending = write_pending(
            &state,
            Path::new("/tmp/niri.sock"),
            "desk",
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(
            expire_pending(&pending.pending_path, &pending.id, now_unix_ms())
                .unwrap()
                .is_none()
        );
        assert!(load_pending(&pending.pending_path).unwrap().is_some());
        // A different watcher id must not touch the newer record.
        assert!(
            expire_pending(&pending.pending_path, "other", now_unix_ms() + 120_000)
                .unwrap()
                .is_none()
        );
        assert!(load_pending(&pending.pending_path).unwrap().is_some());
    }

    #[test]
    fn expired_pending_restores_the_previous_profile_and_clears() {
        let dir = temp_dir("expire");
        let state = dir.join("profiles.json");
        store_with_previous(&state);
        let socket = dir.join("niri.sock");
        // restore() queries outputs once; an empty previous profile sends no actions.
        let server = fake_niri(&socket, 1);
        let pending = write_pending(&state, &socket, "desk", Duration::from_millis(1)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let report = expire_pending(&pending.pending_path, &pending.id, now_unix_ms() + 10_000)
            .unwrap()
            .expect("expired record restores");
        assert_eq!(report.profile, "__previous");
        assert!(load_pending(&pending.pending_path).unwrap().is_none());
        let requests = server.join().unwrap();
        assert!(requests[0].contains("Outputs"));
    }

    #[test]
    fn cancel_restores_immediately_and_clears_the_record() {
        let dir = temp_dir("cancel");
        let state = dir.join("profiles.json");
        store_with_previous(&state);
        let socket = dir.join("niri.sock");
        let server = fake_niri(&socket, 1);
        let pending = write_pending(&state, &socket, "desk", Duration::from_secs(60)).unwrap();
        assert!(cancel(&pending.pending_path).unwrap());
        assert!(load_pending(&pending.pending_path).unwrap().is_none());
        server.join().unwrap();
    }
}
