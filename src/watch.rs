//! Watch mode: debounce niri output changes, apply a profile or rules.
//!
//! niri's event stream in this tree does not carry output add/remove/mode
//! events, so watch polls [`Request::Outputs`] and uses an event-stream
//! connection only as a wake-up hint. A topology fingerprint keeps the loop
//! from re-applying the profile that it just applied.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use niri_ipc::{Output, Request, Response};
use serde::Serialize;

use crate::ipc::{ApplyReport, ApplyStatus, apply_named, apply_profile_object, query_outputs};
use crate::mirror::{self, MirrorStep};
use crate::rules::{RuleDecision, RuleStore, build_rules_profile};
use crate::schema::Store;

/// Options for one watch cycle or the continuous loop.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// Apply this profile instead of evaluating rules.
    pub profile: Option<String>,
    pub debounce: Duration,
    pub poll: Duration,
    pub cooldown: Duration,
    pub once: bool,
    pub dry_run: bool,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            profile: None,
            debounce: Duration::from_millis(500),
            poll: Duration::from_secs(1),
            cooldown: Duration::from_secs(3),
            once: false,
            dry_run: false,
        }
    }
}

/// Compact per-output information included in watch JSON output.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OutputSummary {
    pub name: String,
    pub enabled: bool,
    pub make: String,
    pub model: String,
    pub serial: Option<String>,
    pub mode: Option<(u16, u16, u32)>,
    pub scale: Option<f64>,
    pub position: Option<(i32, i32)>,
    pub vrr: bool,
    pub max_bpc: Option<u8>,
}

/// One evaluated watch cycle.
#[derive(Debug, Clone, Serialize)]
pub struct WatchCycle {
    pub once: bool,
    pub fingerprint: String,
    pub changed: bool,
    /// `applied`, `would_apply`, `no_matching_rules`, `debouncing`,
    /// `idle` or `refused`.
    pub decision: String,
    pub outputs: Vec<OutputSummary>,
    pub rules: Vec<RuleDecision>,
    pub reports: Vec<ApplyReport>,
    pub warnings: Vec<String>,
}

/// Debounce and loop-lock state for watch mode.
#[derive(Debug, Default, Clone)]
pub struct WatchState {
    pub last_fingerprint: Option<String>,
    pub pending_since: Option<Instant>,
    pub last_applied: Option<String>,
    pub cooldown_until: Option<Instant>,
}

/// What watch should do after observing a topology fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchDecision {
    Idle,
    Debouncing { remaining: Duration },
    Apply,
}

impl WatchState {
    /// Observes a topology fingerprint. A change arms the debounce timer; the
    /// cycle applies only after the fingerprint stayed stable for `debounce`
    /// and the post-apply cooldown elapsed.
    pub fn observe(
        &mut self,
        fingerprint: &str,
        now: Instant,
        debounce: Duration,
    ) -> WatchDecision {
        if self.last_fingerprint.as_deref() != Some(fingerprint) {
            self.last_fingerprint = Some(fingerprint.to_owned());
            self.pending_since = Some(now);
            return WatchDecision::Debouncing {
                remaining: debounce,
            };
        }

        let Some(since) = self.pending_since else {
            return WatchDecision::Idle;
        };
        let stable_at = since + debounce;
        if now < stable_at {
            return WatchDecision::Debouncing {
                remaining: stable_at.saturating_duration_since(now),
            };
        }
        if let Some(until) = self.cooldown_until
            && now < until
        {
            return WatchDecision::Debouncing {
                remaining: until.saturating_duration_since(now),
            };
        }
        if self.last_applied.as_deref() == Some(fingerprint) {
            self.pending_since = None;
            return WatchDecision::Idle;
        }
        WatchDecision::Apply
    }

    /// Records a completed apply. `post_fingerprint` is re-read after applying
    /// so the resulting layout becomes the new baseline and does not trigger a
    /// second apply.
    pub fn mark_applied(&mut self, post_fingerprint: String, now: Instant, cooldown: Duration) {
        self.last_fingerprint = Some(post_fingerprint.clone());
        self.last_applied = Some(post_fingerprint);
        self.pending_since = None;
        self.cooldown_until = Some(now + cooldown);
    }
}

/// Stable fingerprint of output topology and relevant output state.
pub fn topology_fingerprint(outputs: &BTreeMap<String, Output>) -> String {
    let mut parts = Vec::new();
    for (name, output) in outputs {
        let mode = output
            .current_mode
            .and_then(|index| output.modes.get(index))
            .map(|mode| format!("{}x{}@{}", mode.width, mode.height, mode.refresh_rate))
            .unwrap_or_else(|| "off".to_owned());
        let logical = output
            .logical
            .map(|logical| {
                format!(
                    "{},{},{}x{},{}",
                    logical.x, logical.y, logical.width, logical.height, logical.scale
                )
            })
            .unwrap_or_else(|| "unmapped".to_owned());
        parts.push(format!(
            "{name}|{mode}|{logical}|vrr={}|bpc={:?}|{}|{}|{}",
            output.vrr_enabled,
            output.max_bpc,
            output.make,
            output.model,
            output.serial.as_deref().unwrap_or("")
        ));
    }
    parts.join(";")
}

fn summarize_outputs(outputs: &BTreeMap<String, Output>) -> Vec<OutputSummary> {
    outputs
        .iter()
        .map(|(name, output)| {
            let mode = output
                .current_mode
                .and_then(|index| output.modes.get(index))
                .map(|mode| (mode.width, mode.height, mode.refresh_rate));
            let (scale, position) = output
                .logical
                .map(|logical| (Some(logical.scale), Some((logical.x, logical.y))))
                .unwrap_or((None, None));
            OutputSummary {
                name: name.clone(),
                enabled: output.logical.is_some(),
                make: output.make.clone(),
                model: output.model.clone(),
                serial: output.serial.clone(),
                mode,
                scale,
                position,
                vrr: output.vrr_enabled,
                max_bpc: output.max_bpc,
            }
        })
        .collect()
}

struct CycleOutcome {
    reports: Vec<ApplyReport>,
    decisions: Vec<RuleDecision>,
    warnings: Vec<String>,
    decision: &'static str,
}

fn apply_for_cycle(
    socket: &Path,
    store: &mut Store,
    rules: &RuleStore,
    profiles_path: &Path,
    options: &WatchOptions,
    current: &BTreeMap<String, Output>,
) -> Result<CycleOutcome, String> {
    if let Some(profile) = &options.profile {
        let report = apply_named(socket, store, profile, profiles_path, options.dry_run)?;
        let decision = decision_for(&report);
        return Ok(CycleOutcome {
            reports: vec![report],
            decisions: Vec::new(),
            warnings: Vec::new(),
            decision,
        });
    }

    let (profile, decisions, warnings) = build_rules_profile(&rules.rules, current, store);
    if profile.outputs.is_empty() {
        return Ok(CycleOutcome {
            reports: Vec::new(),
            decisions,
            warnings,
            decision: "no_matching_rules",
        });
    }
    let report = apply_profile_object(
        socket,
        store,
        &profile,
        "rules",
        profiles_path,
        options.dry_run,
        None,
    )?;
    let decision = decision_for(&report);
    Ok(CycleOutcome {
        reports: vec![report],
        decisions,
        warnings,
        decision,
    })
}

fn decision_for(report: &ApplyReport) -> &'static str {
    match report.status {
        ApplyStatus::Planned => "would_apply",
        ApplyStatus::Applied | ApplyStatus::Restored => "applied",
        ApplyStatus::Refused | ApplyStatus::RolledBack | ApplyStatus::RollbackFailed => "refused",
    }
}

fn build_cycle(
    once: bool,
    fingerprint: String,
    changed: bool,
    current: &BTreeMap<String, Output>,
    outcome: CycleOutcome,
) -> WatchCycle {
    WatchCycle {
        once,
        fingerprint,
        changed,
        decision: outcome.decision.to_owned(),
        outputs: summarize_outputs(current),
        rules: outcome.decisions,
        reports: outcome.reports,
        warnings: outcome.warnings,
    }
}

/// Evaluates and applies once. Used by `watch --once` and tests.
pub fn watch_once(
    socket: &Path,
    store: &mut Store,
    rules: &RuleStore,
    profiles_path: &Path,
    options: &WatchOptions,
) -> Result<WatchCycle, String> {
    let current = query_outputs(socket)?;
    let fingerprint = topology_fingerprint(&current);
    let outcome = apply_for_cycle(socket, store, rules, profiles_path, options, &current)?;
    let mut cycle = build_cycle(true, fingerprint, true, &current, outcome);
    if !options.dry_run {
        cycle.warnings.extend(
            mirror::reap(profiles_path, &current)
                .iter()
                .map(MirrorStep::describe),
        );
    }
    Ok(cycle)
}

/// Runs watch forever, invoking `on_cycle` for every evaluated cycle.
pub fn run_watch(
    socket: &Path,
    store: &mut Store,
    rules: &RuleStore,
    profiles_path: &Path,
    options: &WatchOptions,
    lock_path: &Path,
    mut on_cycle: impl FnMut(&WatchCycle),
) -> Result<(), String> {
    if options.once {
        let cycle = watch_once(socket, store, rules, profiles_path, options)?;
        on_cycle(&cycle);
        return Ok(());
    }

    let _lock = WatchLock::acquire(lock_path)?;
    let event_flag = Arc::new(AtomicBool::new(false));
    let _waker = spawn_event_waker(socket.to_path_buf(), Arc::clone(&event_flag));
    let mut state = WatchState::default();
    let mut mirror_warnings: Vec<String> = Vec::new();

    loop {
        let current = query_outputs(socket)?;
        if !options.dry_run {
            mirror_warnings.extend(
                mirror::reap(profiles_path, &current)
                    .iter()
                    .map(MirrorStep::describe),
            );
        }
        let fingerprint = topology_fingerprint(&current);
        let changed = state.last_fingerprint.as_deref() != Some(&fingerprint);
        let now = Instant::now();
        let decision = state.observe(&fingerprint, now, options.debounce);

        match decision {
            WatchDecision::Apply => {
                let outcome =
                    apply_for_cycle(socket, store, rules, profiles_path, options, &current)?;
                let post = query_outputs(socket)
                    .map(|outputs| topology_fingerprint(&outputs))
                    .unwrap_or_else(|_| fingerprint.clone());
                state.mark_applied(post, Instant::now(), options.cooldown);
                let mut cycle = build_cycle(false, fingerprint, changed, &current, outcome);
                cycle.warnings.append(&mut mirror_warnings);
                on_cycle(&cycle);
            }
            WatchDecision::Debouncing { .. } | WatchDecision::Idle => {}
        }

        wait_for_poll(options.poll, &event_flag);
    }
}

fn wait_for_poll(poll: Duration, event_flag: &AtomicBool) {
    let slice = Duration::from_millis(25);
    let start = Instant::now();
    while start.elapsed() < poll {
        if event_flag.swap(false, Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(slice.min(poll.saturating_sub(start.elapsed())));
    }
}

fn spawn_event_waker(
    socket: PathBuf,
    flag: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("loom-monitor-events".to_owned())
        .spawn(move || {
            use niri_ipc::socket::Socket;
            let Ok(mut socket) = Socket::connect_to(&socket) else {
                return;
            };
            if !matches!(socket.send(Request::EventStream), Ok(Ok(Response::Handled))) {
                return;
            }
            let mut read_event = socket.read_events();
            while read_event().is_ok() {
                flag.store(true, Ordering::Relaxed);
            }
        })
        .ok()
}

/// Advisory lock that keeps two continuous watchers from fighting.
#[derive(Debug)]
pub struct WatchLock {
    path: PathBuf,
}

impl WatchLock {
    pub fn acquire(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create lock directory: {err}"))?;
        }
        match create_lock(path) {
            Ok(()) => Ok(Self {
                path: path.to_path_buf(),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if remove_stale_lock(path) {
                    create_lock(path).map_err(|err| format!("cannot create lock: {err}"))?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(format!(
                    "another loom-monitor watch is running (lock `{}`); remove it if stale",
                    path.display()
                ))
            }
            Err(err) => Err(format!("cannot create lock: {err}")),
        }
    }
}

fn create_lock(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(())
}

fn remove_stale_lock(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    let proc_path = Path::new("/proc").join(pid.to_string());
    if proc_path.exists() {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

impl Drop for WatchLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::schema::tests_support;

    fn output(name: &str, x: i32, scale: f64) -> Output {
        let mut output = tests_support::output(name, "Test", "Test", None);
        if let Some(logical) = &mut output.logical {
            logical.x = x;
            logical.scale = scale;
        }
        output
    }

    fn map(outputs: Vec<(&str, Output)>) -> BTreeMap<String, Output> {
        outputs
            .into_iter()
            .map(|(name, output)| (name.to_owned(), output))
            .collect()
    }

    #[test]
    fn debounce_waits_for_a_stable_fingerprint() {
        let mut state = WatchState::default();
        let start = Instant::now();
        let debounce = Duration::from_millis(100);

        assert!(matches!(
            state.observe("a", start, debounce),
            WatchDecision::Debouncing { .. }
        ));
        assert!(matches!(
            state.observe("a", start + Duration::from_millis(50), debounce),
            WatchDecision::Debouncing { .. }
        ));
        assert_eq!(
            state.observe("a", start + Duration::from_millis(120), debounce),
            WatchDecision::Apply
        );
    }

    #[test]
    fn debounce_restarts_when_the_fingerprint_changes() {
        let mut state = WatchState::default();
        let start = Instant::now();
        let debounce = Duration::from_millis(100);
        state.observe("a", start, debounce);
        state.observe("b", start + Duration::from_millis(80), debounce);
        assert!(matches!(
            state.observe("b", start + Duration::from_millis(120), debounce),
            WatchDecision::Debouncing { .. }
        ));
        assert_eq!(
            state.observe("b", start + Duration::from_millis(200), debounce),
            WatchDecision::Apply
        );
    }

    #[test]
    fn applied_fingerprint_does_not_retrigger_and_cooldown_holds() {
        let mut state = WatchState::default();
        let start = Instant::now();
        let debounce = Duration::from_millis(10);
        let cooldown = Duration::from_secs(1);

        state.observe("a", start, debounce);
        assert_eq!(
            state.observe("a", start + debounce, debounce),
            WatchDecision::Apply
        );
        state.mark_applied("a".to_owned(), start + debounce, cooldown);
        assert_eq!(
            state.observe("a", start + debounce, debounce),
            WatchDecision::Idle,
            "the post-apply fingerprint must not trigger a loop"
        );

        // A new topology during cooldown waits, then applies.
        let changed_at = start + Duration::from_millis(200);
        state.observe("b", changed_at, debounce);
        assert!(matches!(
            state.observe("b", changed_at + Duration::from_millis(300), debounce),
            WatchDecision::Debouncing { .. }
        ));
        assert_eq!(
            state.observe("b", changed_at + Duration::from_millis(1200), debounce),
            WatchDecision::Apply
        );
    }

    #[test]
    fn fingerprint_tracks_mode_and_topology() {
        let one = map(vec![("eDP-1", output("eDP-1", 0, 1.))]);
        let two = map(vec![
            ("eDP-1", output("eDP-1", 0, 1.)),
            ("DP-1", output("DP-1", 1920, 1.)),
        ]);
        let moved = map(vec![("eDP-1", output("eDP-1", 40, 1.))]);
        assert_ne!(topology_fingerprint(&one), topology_fingerprint(&two));
        assert_ne!(topology_fingerprint(&one), topology_fingerprint(&moved));
        assert_eq!(
            topology_fingerprint(&one),
            topology_fingerprint(&map(vec![("eDP-1", output("eDP-1", 0, 1.))]))
        );
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "loom-monitor-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("watch.lock");
        let lock = WatchLock::acquire(&path).unwrap();
        assert!(WatchLock::acquire(&path).is_err());
        drop(lock);
        assert!(WatchLock::acquire(&path).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_lock_is_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "loom-monitor-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("watch.lock");
        std::fs::write(&path, "999999999\n").unwrap();
        let lock = WatchLock::acquire(&path).unwrap();
        drop(lock);
        let _ = std::fs::remove_dir_all(dir);
    }
}
