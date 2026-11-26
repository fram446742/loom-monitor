mod common;

use std::process::Command;

use common::{FakeNiri, TempState, output_at};
use loom_monitor::{OutputMatch, OutputProfile, Profile, Rule, RuleStore, Store};
use serde_json::Value;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_loom-monitor")
}

fn run(args: &[String], socket: Option<&std::path::Path>) -> std::process::Output {
    let mut command = Command::new(binary());
    command.args(args);
    command.env_remove("NIRI_SOCKET");
    command.env_remove("LOOM_MONITOR_STATE");
    command.env_remove("LOOM_MONITOR_RULES");
    if let Some(socket) = socket {
        command.env("NIRI_SOCKET", socket);
    }
    command.output().unwrap()
}

fn args(values: &[&str], state: &TempState) -> Vec<String> {
    let mut args: Vec<String> = vec!["--state".to_owned(), state.profiles.display().to_string()];
    args.extend(values.iter().map(|value| (*value).to_owned()));
    args
}

fn profile_for(name: &str, scale: f64) -> Profile {
    let mut profile = Profile::new();
    profile.outputs.insert(
        name.to_owned(),
        OutputProfile {
            scale: Some(scale),
            mode: Some((1920, 1080, 60000)),
            position: Some((0, 0)),
            ..OutputProfile::default()
        },
    );
    profile
}

fn save_store(temp: &TempState, name: &str, profile: Profile) {
    let mut store = Store::default();
    store.insert(name, profile);
    store.save(&temp.profiles).unwrap();
}

fn stdout_json(output: &std::process::Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).unwrap_or_else(|err| {
        panic!(
            "stdout is not JSON: {err}\nstdout:\n{text}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn apply_confirm_timeout_writes_pending_and_confirm_clears_it() {
    let fake = FakeNiri::start(
        "cli-confirm",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("cli-confirm");
    save_store(&temp, "desk", profile_for("eDP-1", 1.5));
    let pending = temp.profiles.with_file_name("pending.json");

    let output = run(
        &args(
            &["--json", "apply", "desk", "--confirm-timeout", "2"],
            &temp,
        ),
        Some(&fake.path),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["data"]["confirmation"]["timeout_secs"], 2);
    assert!(pending.is_file(), "pending record was written");
    let first: Value = serde_json::from_slice(&std::fs::read(&pending).unwrap()).unwrap();
    let id = first["id"].as_str().unwrap().to_owned();

    let output = run(&args(&["--json", "confirm"], &temp), None);
    assert!(output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["data"]["confirmed"], true);
    assert!(!pending.is_file(), "confirm removed the pending record");

    // The watcher that was spawned for this record must see it gone (or a newer id)
    // and leave the applied profile alone. A short wait covers its poll interval.
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert!(!pending.is_file());
    let _ = id;
}

#[test]
fn cancel_restores_the_previous_profile_and_clears_pending() {
    let fake = FakeNiri::start(
        "cli-cancel",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("cli-cancel");
    let mut store = Store::default();
    let mut previous = Profile::new();
    previous
        .outputs
        .insert("eDP-1".to_owned(), OutputProfile::default());
    store.insert("__previous", previous);
    store.save(&temp.profiles).unwrap();

    let pending = temp.profiles.with_file_name("pending.json");
    loom_monitor::write_pending(
        &temp.profiles,
        &fake.path,
        "desk",
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    assert!(pending.is_file());

    let output = run(&args(&["--json", "cancel"], &temp), Some(&fake.path));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["data"]["cancelled"], true);
    assert!(!pending.is_file());
}

#[test]
fn list_json_uses_an_envelope() {
    let temp = TempState::new("cli-list");
    save_store(&temp, "desk", profile_for("eDP-1", 1.5));

    let output = run(&args(&["--json", "list"], &temp), None);
    assert!(output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["command"], "list");
    assert_eq!(json["ok"], true);
    assert_eq!(json["data"]["profiles"][0], "desk");
}

#[test]
fn apply_json_reports_actions_and_status() {
    let fake = FakeNiri::start(
        "cli-apply",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("cli-apply");
    save_store(&temp, "desk", profile_for("eDP-1", 1.5));

    let output = run(&args(&["--json", "apply", "desk"], &temp), Some(&fake.path));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "apply");
    assert_eq!(json["ok"], true);
    assert_eq!(json["data"]["status"], "applied");
    assert_eq!(json["data"]["actions"].as_array().unwrap().len(), 4);
    assert_eq!(json["data"]["resolved"]["eDP-1"], "eDP-1");
}

#[test]
fn default_apply_dry_run_keeps_the_action_array_shape() {
    let fake = FakeNiri::start(
        "cli-dry",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("cli-dry");
    save_store(&temp, "desk", profile_for("eDP-1", 1.5));

    let output = run(
        &args(&["apply", "desk", "--dry-run"], &temp),
        Some(&fake.path),
    );
    assert!(output.status.success());
    let json = stdout_json(&output);
    let actions = json.as_array().unwrap();
    assert_eq!(actions.len(), 4);
    assert_eq!(actions[0][0], "eDP-1");
    assert_eq!(actions[0][1], "On");
}

#[test]
fn apply_json_refusal_is_machine_readable_and_fails() {
    let fake = FakeNiri::start(
        "cli-refuse",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("cli-refuse");
    let mut profile = Profile::new();
    profile.outputs.insert(
        "eDP-1".to_owned(),
        OutputProfile {
            off: true,
            ..OutputProfile::default()
        },
    );
    save_store(&temp, "off", profile);

    let output = run(&args(&["--json", "apply", "off"], &temp), Some(&fake.path));
    assert!(!output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["command"], "apply");
    assert_eq!(json["ok"], false);
    assert_eq!(json["data"]["status"], "refused");
    assert!(!json["data"]["errors"].as_array().unwrap().is_empty());
    assert!(fake.recorded().is_empty());
}

#[test]
fn rules_add_explain_and_apply_json() {
    let fake = FakeNiri::start(
        "cli-rules",
        vec![output_at(
            "DP-1",
            "Dell Inc.",
            "U2720Q",
            Some("ABC"),
            0,
            1.0,
        )],
    );
    let temp = TempState::new("cli-rules");
    save_store(&temp, "desk", profile_for("DP-1", 2.0));

    let output = run(
        &args(
            &[
                "--json",
                "rules",
                "add",
                "dock",
                "--profile",
                "desk",
                "--priority",
                "10",
                "--make",
                "Dell Inc.",
            ],
            &temp,
        ),
        None,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "rules add");
    assert_eq!(json["data"]["rule"]["priority"], 10);

    let output = run(
        &args(&["--json", "rules", "explain", "--output", "DP-1"], &temp),
        Some(&fake.path),
    );
    assert!(output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["command"], "rules explain");
    assert_eq!(json["data"]["decisions"][0]["rule"], "dock");
    assert!(json["data"]["decisions"][0]["reason"].is_string());
    assert!(
        !json["data"]["plan"]["actions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let output = run(
        &args(&["--json", "rules", "apply", "--dry-run"], &temp),
        Some(&fake.path),
    );
    assert!(output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["data"]["report"]["status"], "planned");
    assert!(fake.recorded().is_empty());
}

#[test]
fn migrate_json_upgrades_old_profiles() {
    let temp = TempState::new("cli-migrate");
    std::fs::write(
        &temp.profiles,
        r###"{"profiles":{"old":{"outputs":{"eDP-1":{"scale":1.5,"vrr":true}}}}}"###,
    )
    .unwrap();

    let output = run(&args(&["--json", "migrate"], &temp), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "migrate");
    assert_eq!(json["data"]["migrated_profiles"][0], "old");
    assert_eq!(json["data"]["version"], 2);

    let stored = Store::load(&temp.profiles).unwrap();
    assert_eq!(stored.get("old").unwrap().version, 2);
    assert_eq!(stored.get("old").unwrap().outputs["eDP-1"].scale, Some(1.5));
}

#[test]
fn save_json_snapshots_the_fake_compositor() {
    let fake = FakeNiri::start(
        "cli-save",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.5)],
    );
    let temp = TempState::new("cli-save");

    let output = run(&args(&["--json", "save", "snap"], &temp), Some(&fake.path));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "save");
    assert_eq!(json["data"]["profile"]["outputs"]["eDP-1"]["scale"], 1.5);
    assert_eq!(json["data"]["profile"]["version"], 2);
}

#[test]
fn watch_once_json_applies_a_rule() {
    let fake = FakeNiri::start(
        "cli-watch",
        vec![output_at(
            "DP-1",
            "Dell Inc.",
            "U2720Q",
            Some("ABC"),
            0,
            1.0,
        )],
    );
    let temp = TempState::new("cli-watch");
    save_store(&temp, "desk", profile_for("DP-1", 2.0));
    let mut rules = RuleStore::default();
    rules.insert(Rule {
        name: "dock".to_owned(),
        profile: "desk".to_owned(),
        priority: 10,
        match_: OutputMatch {
            make: Some("Dell Inc.".to_owned()),
            ..OutputMatch::default()
        },
    });
    rules.save(&temp.rules).unwrap();

    let output = run(
        &args(&["--json", "watch", "--once"], &temp),
        Some(&fake.path),
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "watch");
    assert_eq!(json["data"]["decision"], "applied");
    assert_eq!(json["data"]["rules"][0]["rule"], "dock");
    assert_eq!(json["data"]["reports"][0]["status"], "applied");
    assert_eq!(fake.output("DP-1").logical.unwrap().scale, 2.0);
}

#[test]
fn default_dry_run_prints_actions_and_validation_errors() {
    let fake = FakeNiri::start(
        "cli-invalid",
        vec![
            output_at("eDP-1", "BOE", "Panel", None, 0, 1.0),
            output_at("DP-1", "Dell Inc.", "U2720Q", Some("ABC"), 1920, 1.0),
        ],
    );
    let temp = TempState::new("cli-invalid");
    let mut profile = Profile::new();
    profile.outputs.insert(
        "DP-1".to_owned(),
        OutputProfile {
            position: Some((100, 100)),
            ..OutputProfile::default()
        },
    );
    save_store(&temp, "bad", profile);

    let output = run(
        &args(&["apply", "bad", "--dry-run"], &temp),
        Some(&fake.path),
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let actions: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(!actions.as_array().unwrap().is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("overlap"), "stderr: {stderr}");
    assert!(stderr.contains("refused"), "stderr: {stderr}");
    assert!(fake.recorded().is_empty());
}
