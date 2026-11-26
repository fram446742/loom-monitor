//! External `wl-mirror` backend for profile `mirror` relationships.
//!
//! niri in this tree has no output-mirror primitive, so a profile entry whose
//! `mirror` names another output is realised with
//! `wl-mirror <source> --fullscreen-output <target>`. The children this tool
//! starts are tracked in `mirrors.json` beside the profile store, so a later
//! apply, restore or watch cycle stops exactly those processes and never an
//! unrelated one.
//!
//! `mirror` on output `eDP-1` naming `DP-1` means "show DP-1's content on
//! eDP-1", hence source `DP-1` and target `eDP-1`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use niri_ipc::Output;
use serde::{Deserialize, Serialize};

use crate::schema::OutputProfile;

/// Overrides the `wl-mirror` binary; used by tests and for odd installs.
pub const WL_MIRROR_ENV: &str = "LOOM_MONITOR_WL_MIRROR";
const DEFAULT_BINARY: &str = "wl-mirror";

/// Children spawned by this process. Reaping through the handles here keeps a
/// long-running `watch` from accumulating zombies; other processes only have
/// the PIDs in the state file.
static SPAWNED: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// The mirror state file, beside the profile store.
pub fn state_path(profiles: &Path) -> PathBuf {
    profiles.with_file_name("mirrors.json")
}

/// One requested mirror relationship.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MirrorRelation {
    /// Output that shows the copy (the profile entry carrying `mirror`).
    pub output: String,
    /// Output whose content is shown.
    pub source: String,
}

impl MirrorRelation {
    fn failed(&self, detail: String) -> MirrorStep {
        MirrorStep {
            output: self.output.clone(),
            source: self.source.clone(),
            target: self.output.clone(),
            status: MirrorStatus::Failed,
            pid: None,
            detail: Some(detail),
        }
    }
}

/// Outcome of one mirror step in an apply report.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MirrorStatus {
    /// A new `wl-mirror` child was spawned.
    Started,
    /// An identical child was already running and was kept.
    Running,
    /// A tracked child was stopped.
    Stopped,
    /// The step could not be performed.
    Failed,
}

impl MirrorStatus {
    pub fn name(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// One reported mirror action.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MirrorStep {
    /// Output that shows the copy.
    pub output: String,
    /// Output whose content is shown.
    pub source: String,
    /// The `--fullscreen-output` argument (same as `output`).
    pub target: String,
    pub status: MirrorStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl MirrorStep {
    fn from_child(child: &MirrorChild, status: MirrorStatus, detail: Option<String>) -> Self {
        Self {
            output: child.output.clone(),
            source: child.source.clone(),
            target: child.target.clone(),
            status,
            pid: Some(child.pid),
            detail,
        }
    }

    fn failed(what: &str, detail: String) -> Self {
        Self {
            output: what.to_owned(),
            source: String::new(),
            target: String::new(),
            status: MirrorStatus::Failed,
            pid: None,
            detail: Some(detail),
        }
    }

    /// Human-readable line for the default CLI output.
    pub fn describe(&self) -> String {
        let mut text = format!("mirror {} <- {}", self.output, self.source);
        if let Some(pid) = self.pid {
            text.push_str(&format!(" (pid {pid})"));
        }
        text.push_str(": ");
        text.push_str(self.status.name());
        if let Some(detail) = &self.detail {
            text.push_str(": ");
            text.push_str(detail);
        }
        text
    }
}

/// A tracked `wl-mirror` child.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MirrorChild {
    pub output: String,
    pub source: String,
    pub target: String,
    pub pid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MirrorFile {
    #[serde(default)]
    children: Vec<MirrorChild>,
}

/// Collects the mirror relations a set of output settings asks for.
pub fn desired_from(desired: &BTreeMap<String, OutputProfile>) -> Vec<MirrorRelation> {
    desired
        .iter()
        .filter(|(_, entry)| !entry.off)
        .filter_map(|(output, entry)| {
            entry.mirror.as_ref().map(|source| MirrorRelation {
                output: output.clone(),
                source: source.clone(),
            })
        })
        .collect()
}

fn load(path: &Path) -> Result<Vec<MirrorChild>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<MirrorFile>(&bytes)
            .map(|file| file.children)
            .map_err(|err| format!("invalid {}: {err}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

fn save(path: &Path, children: &[MirrorChild]) -> Result<(), String> {
    let file = MirrorFile {
        children: children.to_vec(),
    };
    crate::schema::write_json_atomic(path, &file)
        .map_err(|err| format!("cannot write {}: {err}", path.display()))
}

fn resolve_binary() -> PathBuf {
    std::env::var_os(WL_MIRROR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY))
}

/// Brings the running children in line with `desired`: keep identical
/// children, replace changed ones, stop removed or disconnected ones, spawn
/// the rest. A spawn failure is reported in the returned steps, not as an
/// error, so the caller's apply still counts as applied.
pub fn sync(
    profiles: &Path,
    desired: &[MirrorRelation],
    connected: &BTreeMap<String, Output>,
) -> Vec<MirrorStep> {
    sync_with(&resolve_binary(), profiles, desired, connected)
}

fn sync_with(
    binary: &Path,
    profiles: &Path,
    desired: &[MirrorRelation],
    connected: &BTreeMap<String, Output>,
) -> Vec<MirrorStep> {
    reap_local();
    let path = state_path(profiles);
    let mut steps = Vec::new();
    let prior = match load(&path) {
        Ok(children) => children,
        Err(err) => {
            steps.push(MirrorStep::failed("mirror state", err));
            Vec::new()
        }
    };

    let mut tracked = Vec::new();
    for child in prior {
        if !alive(child.pid) {
            steps.push(MirrorStep::from_child(
                &child,
                MirrorStatus::Stopped,
                Some("process had exited".to_owned()),
            ));
        } else if !owns(&child) {
            steps.push(MirrorStep::from_child(
                &child,
                MirrorStatus::Failed,
                Some("pid no longer belongs to this mirror; leaving it alone".to_owned()),
            ));
        } else {
            tracked.push(child);
        }
    }

    let mut next = Vec::new();
    for relation in desired {
        if let Some(index) = tracked
            .iter()
            .position(|child| child.output == relation.output && child.source == relation.source)
        {
            let child = tracked.remove(index);
            match disconnected(relation, connected) {
                None => {
                    steps.push(MirrorStep::from_child(&child, MirrorStatus::Running, None));
                    next.push(child);
                }
                Some(detail) => {
                    let step = stop_child(&child);
                    let stopped = step.status != MirrorStatus::Failed;
                    steps.push(step);
                    steps.push(relation.failed(detail));
                    if !stopped {
                        next.push(child);
                    }
                }
            }
            continue;
        }

        if let Some(index) = tracked
            .iter()
            .position(|child| child.output == relation.output)
        {
            let child = tracked.remove(index);
            let step = stop_child(&child);
            let stopped = step.status != MirrorStatus::Failed;
            steps.push(step);
            if !stopped {
                steps.push(relation.failed(format!(
                    "could not stop the previous wl-mirror (pid {})",
                    child.pid
                )));
                next.push(child);
                continue;
            }
        }

        if let Some(detail) = disconnected(relation, connected) {
            steps.push(relation.failed(detail));
            continue;
        }

        match spawn(binary, relation) {
            Ok(pid) => {
                let child = MirrorChild {
                    output: relation.output.clone(),
                    source: relation.source.clone(),
                    target: relation.output.clone(),
                    pid,
                };
                steps.push(MirrorStep::from_child(&child, MirrorStatus::Started, None));
                next.push(child);
            }
            Err(err) => steps.push(relation.failed(err)),
        }
    }

    for child in tracked {
        let step = stop_child(&child);
        let stopped = step.status != MirrorStatus::Failed;
        steps.push(step);
        if !stopped {
            next.push(child);
        }
    }

    if let Err(err) = save(&path, &next) {
        // Without a state file the children cannot be stopped later; leave
        // none running.
        for child in &next {
            let _ = stop_child(child);
        }
        steps.push(MirrorStep::failed("mirror state", err));
    }
    steps
}

/// Drops completed children from the state file and stops tracked children
/// whose output or source disappeared. Used by watch so a disconnected output
/// or an exited process does not leave a stale PID behind.
pub fn reap(profiles: &Path, connected: &BTreeMap<String, Output>) -> Vec<MirrorStep> {
    reap_local();
    let path = state_path(profiles);
    let mut steps = Vec::new();
    let prior = match load(&path) {
        Ok(children) => children,
        Err(err) => {
            steps.push(MirrorStep::failed("mirror state", err));
            return steps;
        }
    };

    let mut next = Vec::new();
    let mut changed = false;
    for child in prior {
        if !alive(child.pid) {
            changed = true;
            steps.push(MirrorStep::from_child(
                &child,
                MirrorStatus::Stopped,
                Some("process had exited".to_owned()),
            ));
        } else if !owns(&child) {
            changed = true;
            steps.push(MirrorStep::from_child(
                &child,
                MirrorStatus::Failed,
                Some("pid no longer belongs to this mirror; leaving it alone".to_owned()),
            ));
        } else if !connected.contains_key(&child.output) || !connected.contains_key(&child.source) {
            changed = true;
            let step = stop_child(&child);
            let stopped = step.status != MirrorStatus::Failed;
            steps.push(step);
            if !stopped {
                next.push(child);
            }
        } else {
            next.push(child);
        }
    }

    if changed && let Err(err) = save(&path, &next) {
        steps.push(MirrorStep::failed("mirror state", err));
    }
    steps
}

fn disconnected(relation: &MirrorRelation, connected: &BTreeMap<String, Output>) -> Option<String> {
    if !connected.contains_key(&relation.output) {
        return Some(format!("output `{}` is not connected", relation.output));
    }
    if !connected.contains_key(&relation.source) {
        return Some(format!(
            "mirror source `{}` is not connected",
            relation.source
        ));
    }
    None
}

fn spawn(binary: &Path, relation: &MirrorRelation) -> Result<u32, String> {
    let child = Command::new(binary)
        .arg(&relation.source)
        .arg("--fullscreen-output")
        .arg(&relation.output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => format!(
                "`{}` was not found; install wl-mirror or set {WL_MIRROR_ENV}",
                binary.display()
            ),
            _ => format!("cannot start `{}`: {err}", binary.display()),
        })?;
    let pid = child.id();
    if let Ok(mut spawned) = SPAWNED.lock() {
        spawned.push(child);
    }
    Ok(pid)
}

fn stop_child(child: &MirrorChild) -> MirrorStep {
    // Re-check right before signalling: a PID recycled since the last check
    // must never receive our signal.
    if alive(child.pid) && !owns(child) {
        reap_local();
        if !alive(child.pid) {
            return MirrorStep::from_child(
                child,
                MirrorStatus::Stopped,
                Some("process had exited".to_owned()),
            );
        }
        return MirrorStep::from_child(
            child,
            MirrorStatus::Failed,
            Some("pid no longer belongs to this mirror; leaving it alone".to_owned()),
        );
    }
    match stop_pid(child.pid) {
        Ok(()) => MirrorStep::from_child(child, MirrorStatus::Stopped, None),
        Err(err) => MirrorStep::from_child(child, MirrorStatus::Failed, Some(err)),
    }
}

fn stop_pid(pid: u32) -> Result<(), String> {
    if !alive(pid) {
        reap_local();
        return Ok(());
    }
    signal(pid, "-TERM")?;
    if wait_gone(pid, Duration::from_millis(500)) {
        return Ok(());
    }
    signal(pid, "-KILL")?;
    if wait_gone(pid, Duration::from_millis(200)) {
        Ok(())
    } else {
        Err(format!("pid {pid} is still running after SIGKILL"))
    }
}

fn signal(pid: u32, signal: &str) -> Result<(), String> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .map_err(|err| format!("cannot run kill: {err}"))?;
    // The process may have exited between the liveness check and the signal.
    if status.success() || !alive(pid) {
        Ok(())
    } else {
        Err(format!("kill {signal} {pid} failed"))
    }
}

fn wait_gone(pid: u32, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        reap_local();
        if !alive(pid) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a PID exists. niri runs on Linux, where `/proc` is available.
fn alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Whether the PID still runs the command this tool started. Guards against a
/// recycled PID: the command line must still carry both output names.
fn owns(child: &MirrorChild) -> bool {
    let path = Path::new("/proc")
        .join(child.pid.to_string())
        .join("cmdline");
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let args: Vec<String> = bytes
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    args.iter().any(|arg| arg == &child.source) && args.iter().any(|arg| arg == &child.target)
}

fn reap_local() {
    if let Ok(mut spawned) = SPAWNED.lock() {
        spawned.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use super::*;

    /// Spawning and killing are process-global; serializing also avoids the
    /// fork/exec ETXTBSY race with a script that another test is writing.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn temp_state(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-monitor-mirror-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("profiles.json")
    }

    fn fake_binary(dir: &Path, log: &Path) -> PathBuf {
        let path = dir.join("wl-mirror");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "printf '%s\\n' \"$*\" >> '{}'", log.display()).unwrap();
        writeln!(file, "parent={}", std::process::id()).unwrap();
        writeln!(
            file,
            "while kill -0 \"$parent\" 2>/dev/null; do sleep 1; done"
        )
        .unwrap();
        drop(file);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn outputs(names: &[&str]) -> BTreeMap<String, Output> {
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_owned(),
                    crate::schema::tests_support::output(name, "Test", "Test", None),
                )
            })
            .collect()
    }

    fn relation(output: &str, source: &str) -> MirrorRelation {
        MirrorRelation {
            output: output.to_owned(),
            source: source.to_owned(),
        }
    }

    fn wait_for_log(path: &Path, lines: usize) -> String {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.lines().count() >= lines
            {
                return text;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn sync_spawns_keeps_and_stops_tracked_children() {
        let _guard = lock();
        let profiles = temp_state("sync");
        let dir = profiles.parent().unwrap();
        let log = dir.join("calls.log");
        let binary = fake_binary(dir, &log);
        let connected = outputs(&["eDP-1", "DP-1"]);

        let steps = sync_with(&binary, &profiles, &[relation("eDP-1", "DP-1")], &connected);
        assert_eq!(steps[0].status, MirrorStatus::Started, "{steps:?}");
        let pid = steps[0].pid.unwrap();
        let tracked = load(&state_path(&profiles)).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].target, "eDP-1");
        assert_eq!(
            wait_for_log(&log, 1).trim(),
            "DP-1 --fullscreen-output eDP-1"
        );

        // A second sync with the same relation keeps the child.
        let steps = sync_with(&binary, &profiles, &[relation("eDP-1", "DP-1")], &connected);
        assert_eq!(steps[0].status, MirrorStatus::Running);
        assert_eq!(steps[0].pid, Some(pid));
        assert_eq!(
            wait_for_log(&log, 1).lines().count(),
            1,
            "the healthy child must not be spawned twice"
        );

        // Removing the mirror stops it and clears the state.
        let steps = sync_with(&binary, &profiles, &[], &connected);
        assert_eq!(steps[0].status, MirrorStatus::Stopped);
        assert!(!alive(pid));
        assert!(load(&state_path(&profiles)).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sync_reports_a_missing_binary_without_tracking_a_pid() {
        let _guard = lock();
        let profiles = temp_state("missing");
        let dir = profiles.parent().unwrap();
        let binary = dir.join("absent-wl-mirror");
        let connected = outputs(&["eDP-1", "DP-1"]);

        let steps = sync_with(&binary, &profiles, &[relation("eDP-1", "DP-1")], &connected);
        assert_eq!(steps[0].status, MirrorStatus::Failed);
        assert!(steps[0].detail.as_deref().unwrap().contains("not found"));
        assert!(load(&state_path(&profiles)).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reap_drops_a_disconnected_child() {
        let _guard = lock();
        let profiles = temp_state("reap");
        let dir = profiles.parent().unwrap();
        let log = dir.join("calls.log");
        let binary = fake_binary(dir, &log);
        let connected = outputs(&["eDP-1", "DP-1"]);
        let pid = sync_with(&binary, &profiles, &[relation("eDP-1", "DP-1")], &connected)[0]
            .pid
            .unwrap();

        let steps = reap(&profiles, &outputs(&["eDP-1"]));
        assert_eq!(steps[0].status, MirrorStatus::Stopped, "{steps:?}");
        assert!(!alive(pid));
        assert!(load(&state_path(&profiles)).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_recycled_pid_is_not_killed() {
        let _guard = lock();
        let profiles = temp_state("recycled");
        let path = state_path(&profiles);
        save(
            &path,
            &[MirrorChild {
                output: "eDP-1".to_owned(),
                source: "DP-1".to_owned(),
                target: "eDP-1".to_owned(),
                pid: std::process::id(),
            }],
        )
        .unwrap();

        let steps = sync(&profiles, &[], &outputs(&["eDP-1", "DP-1"]));
        assert_eq!(steps[0].status, MirrorStatus::Failed);
        assert!(
            steps[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("leaving it alone")
        );
        assert!(load(&path).unwrap().is_empty());
        assert!(alive(std::process::id()));

        let _ = std::fs::remove_dir_all(profiles.parent().unwrap());
    }
}
