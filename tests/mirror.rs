//! End-to-end `mirror` backend tests: a fake `wl-mirror` script stands in for
//! the real client, so no compositor or wl-mirror install is needed.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{FakeNiri, TempState, output_at};
use loom_monitor::{OutputProfile, Profile, Store};
use serde_json::Value;

/// Spawning and killing are process-global; serializing also avoids the
/// fork/exec ETXTBSY race with a script that another test is writing.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_loom-monitor")
}

fn run(temp: &TempState, socket: &Path, mirror_bin: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(binary());
    command
        .arg("--state")
        .arg(&temp.profiles)
        .args(args)
        .env("NIRI_SOCKET", socket)
        .env("LOOM_MONITOR_WL_MIRROR", mirror_bin)
        .env_remove("LOOM_MONITOR_STATE")
        .env_remove("LOOM_MONITOR_RULES");
    command.output().unwrap()
}

fn envelope(output: &std::process::Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|err| {
        panic!(
            "stdout is not JSON: {err}\nstdout:\n{text}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn data(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    envelope(output)["data"].clone()
}

fn children(state: &TempState) -> Vec<Value> {
    let path = state.profiles.with_file_name("mirrors.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    let value: Value = serde_json::from_str(&text).unwrap();
    value["children"].as_array().cloned().unwrap_or_default()
}

fn alive(pid: u64) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn kill(pid: u64) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

/// Fake `wl-mirror`: records its arguments, then stays alive like the real
/// client until it is stopped.
fn fake_wl_mirror(temp: &TempState) -> (PathBuf, PathBuf) {
    let script = temp.dir.join("wl-mirror");
    let log = temp.dir.join("wl-mirror.log");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nparent={}\nwhile kill -0 \"$parent\" 2>/dev/null; do sleep 1; done\n",
            log.display(),
            std::process::id()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
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

fn mirror_profile() -> Profile {
    let mut profile = Profile::new();
    profile.outputs.insert(
        "eDP-1".to_owned(),
        OutputProfile {
            mirror: Some("DP-1".to_owned()),
            ..OutputProfile::default()
        },
    );
    profile
        .outputs
        .insert("DP-1".to_owned(), OutputProfile::default());
    profile
}

fn plain_profile() -> Profile {
    let mut profile = Profile::new();
    profile
        .outputs
        .insert("eDP-1".to_owned(), OutputProfile::default());
    profile
        .outputs
        .insert("DP-1".to_owned(), OutputProfile::default());
    profile
}

fn store_profile(temp: &TempState, name: &str, profile: Profile) {
    let mut store = Store::default();
    store.insert(name, profile);
    store.save(&temp.profiles).unwrap();
}

fn outputs() -> Vec<niri_ipc::Output> {
    vec![
        output_at("eDP-1", "BOE", "Panel", None, 0, 1.0),
        output_at("DP-1", "Dell Inc.", "U2720Q", Some("ABC"), 1920, 1.0),
    ]
}

#[test]
fn apply_spawns_wl_mirror_and_tracks_it() {
    let _guard = lock();
    let fake = FakeNiri::start("mirror-apply", outputs());
    let temp = TempState::new("mirror-apply");
    let (mirror_bin, log) = fake_wl_mirror(&temp);
    store_profile(&temp, "desk", mirror_profile());

    let data = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "desk"],
    ));

    assert_eq!(data["status"], "applied");
    let step = &data["mirrors"][0];
    assert_eq!(step["status"], "started");
    assert_eq!(step["output"], "eDP-1");
    assert_eq!(step["source"], "DP-1");
    assert_eq!(step["target"], "eDP-1");
    let pid = step["pid"].as_u64().expect("a started mirror has a pid");
    assert!(alive(pid), "the fake wl-mirror is running");

    let tracked = children(&temp);
    assert_eq!(tracked.len(), 1);
    assert_eq!(tracked[0]["output"], "eDP-1");
    assert_eq!(tracked[0]["source"], "DP-1");
    assert_eq!(tracked[0]["target"], "eDP-1");
    assert_eq!(tracked[0]["pid"].as_u64(), Some(pid));

    assert_eq!(
        wait_for_log(&log, 1).trim(),
        "DP-1 --fullscreen-output eDP-1",
        "exact wl-mirror arguments"
    );
    assert!(
        data["config_only"]
            .as_array()
            .unwrap()
            .iter()
            .all(|note| note["field"] != "mirror"),
        "a spawned mirror is an applied step, not config-only"
    );

    kill(pid);
}

#[test]
fn second_apply_keeps_a_healthy_mirror_and_stops_a_removed_one() {
    let _guard = lock();
    let fake = FakeNiri::start("mirror-second", outputs());
    let temp = TempState::new("mirror-second");
    let (mirror_bin, log) = fake_wl_mirror(&temp);
    let mut store = Store::default();
    store.insert("desk", mirror_profile());
    store.insert("plain", plain_profile());
    store.save(&temp.profiles).unwrap();

    let first = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "desk"],
    ));
    let pid = first["mirrors"][0]["pid"].as_u64().unwrap();
    wait_for_log(&log, 1);

    // Re-applying the same profile keeps the healthy child.
    let again = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "desk"],
    ));
    assert_eq!(again["mirrors"][0]["status"], "running");
    assert_eq!(again["mirrors"][0]["pid"].as_u64(), Some(pid));
    assert_eq!(children(&temp).len(), 1);
    assert_eq!(
        wait_for_log(&log, 1).lines().count(),
        1,
        "no duplicate wl-mirror"
    );

    // A second apply whose profile removes the mirror stops the first child.
    let removed = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "plain"],
    ));
    assert_eq!(removed["mirrors"][0]["status"], "stopped");
    assert_eq!(removed["mirrors"][0]["pid"].as_u64(), Some(pid));
    assert!(!alive(pid), "the removed mirror was stopped");
    assert!(children(&temp).is_empty(), "no stale pid");
}

#[test]
fn restore_stops_the_mirror_child() {
    let _guard = lock();
    let fake = FakeNiri::start("mirror-restore", outputs());
    let temp = TempState::new("mirror-restore");
    let (mirror_bin, log) = fake_wl_mirror(&temp);
    store_profile(&temp, "desk", mirror_profile());

    let applied = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "desk"],
    ));
    let pid = applied["mirrors"][0]["pid"].as_u64().unwrap();
    wait_for_log(&log, 1);

    let restored = data(&run(&temp, &fake.path, &mirror_bin, &["--json", "restore"]));
    assert_eq!(restored["status"], "restored");
    assert_eq!(restored["mirrors"][0]["status"], "stopped");
    assert!(!alive(pid), "restore rolled the mirror back");
    assert!(children(&temp).is_empty());
}

#[test]
fn missing_wl_mirror_is_reported_failed_without_a_stale_pid() {
    let _guard = lock();
    let fake = FakeNiri::start("mirror-missing", outputs());
    let temp = TempState::new("mirror-missing");
    let missing = temp.dir.join("absent-wl-mirror");
    store_profile(&temp, "desk", mirror_profile());

    let data = data(&run(
        &temp,
        &fake.path,
        &missing,
        &["--json", "apply", "desk"],
    ));

    assert_eq!(data["status"], "applied", "niri actions still applied");
    let step = &data["mirrors"][0];
    assert_eq!(step["status"], "failed");
    assert!(
        step["detail"].as_str().unwrap().contains("not found"),
        "reason: {}",
        step["detail"]
    );
    assert_eq!(step["pid"], Value::Null);
    assert!(children(&temp).is_empty(), "no stale pid");
    assert!(
        data["config_only"]
            .as_array()
            .unwrap()
            .iter()
            .all(|note| note["field"] != "mirror")
    );
}

#[test]
fn dry_run_keeps_the_config_only_note_and_spawns_nothing() {
    let _guard = lock();
    let fake = FakeNiri::start("mirror-dry", outputs());
    let temp = TempState::new("mirror-dry");
    let (mirror_bin, log) = fake_wl_mirror(&temp);
    store_profile(&temp, "desk", mirror_profile());

    let data = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "desk", "--dry-run"],
    ));

    assert_eq!(data["status"], "planned");
    assert_eq!(data["mirrors"].as_array().unwrap().len(), 0);
    assert!(
        data["config_only"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note["field"] == "mirror")
    );
    assert!(children(&temp).is_empty());
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "dry run must not spawn wl-mirror"
    );
}

#[test]
fn watch_once_cleans_up_when_the_source_output_disappears() {
    let _guard = lock();
    let fake = FakeNiri::start("mirror-watch", outputs());
    let temp = TempState::new("mirror-watch");
    let (mirror_bin, log) = fake_wl_mirror(&temp);
    store_profile(&temp, "desk", mirror_profile());

    let applied = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "apply", "desk"],
    ));
    let pid = applied["mirrors"][0]["pid"].as_u64().unwrap();
    wait_for_log(&log, 1);

    fake.set_outputs(vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)]);
    let cycle = data(&run(
        &temp,
        &fake.path,
        &mirror_bin,
        &["--json", "watch", "--once", "--profile", "desk"],
    ));

    assert!(!alive(pid), "watch stopped the orphaned mirror");
    assert!(children(&temp).is_empty());
    assert!(
        cycle["reports"][0]["mirrors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step["status"] == "stopped" && step["output"] == "eDP-1"),
        "watch reports the cleanup: {}",
        cycle["reports"][0]["mirrors"]
    );
}
