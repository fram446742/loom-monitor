mod common;

use std::process::Command;

use common::{FakeNiri, TempState, output_at};
use loom_monitor::Store;
use niri_ipc::OutputAction;
use serde_json::Value;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_loom-monitor")
}

fn run(args: &[String], socket: &std::path::Path) -> std::process::Output {
    let mut command = Command::new(binary());
    command.args(args);
    command.env_remove("NIRI_SOCKET");
    command.env_remove("LOOM_MONITOR_STATE");
    command.env_remove("LOOM_MONITOR_RULES");
    command.env("NIRI_SOCKET", socket);
    command.output().unwrap()
}

fn args(values: &[&str], state: &TempState) -> Vec<String> {
    let mut args: Vec<String> = vec!["--state".to_owned(), state.profiles.display().to_string()];
    args.extend(values.iter().map(|value| (*value).to_owned()));
    args
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

fn two_outputs() -> Vec<niri_ipc::Output> {
    vec![
        output_at("eDP-1", "BOE", "Panel", None, 0, 1.0),
        output_at("DP-1", "Dell Inc.", "U2720Q", Some("ABC"), 1920, 1.0),
    ]
}

#[test]
fn position_json_reports_logical_geometry() {
    let fake = FakeNiri::start("arrange-read", two_outputs());
    let temp = TempState::new("arrange-read");

    let output = run(&args(&["--json", "position"], &temp), &fake.path);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "position");
    assert_eq!(json["ok"], true);
    let positions = json["data"]["positions"].as_array().unwrap();
    assert_eq!(positions.len(), 2);
    let dp = positions
        .iter()
        .find(|entry| entry["output"] == "DP-1")
        .unwrap();
    assert_eq!(dp["x"], 1920);
    assert_eq!(dp["y"], 0);
    assert_eq!(dp["width"], 1920);
    assert_eq!(dp["height"], 1080);
    assert_eq!(dp["scale"], 1.0);
    assert_eq!(dp["enabled"], true);
}

#[test]
fn position_moves_one_output_and_saves_previous() {
    let fake = FakeNiri::start("arrange-move", two_outputs());
    let temp = TempState::new("arrange-move");

    let output = run(
        &args(&["--json", "position", "DP-1", "0", "1080"], &temp),
        &fake.path,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["data"]["status"], "applied");
    assert_eq!(json["data"]["previous_saved"], true);
    let logical = fake.output("DP-1").logical.unwrap();
    assert_eq!((logical.x, logical.y), (0, 1080));

    let recorded = fake.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].output, "DP-1");
    assert!(matches!(
        recorded[0].action,
        OutputAction::Position {
            position: niri_ipc::PositionToSet::Specific(_)
        }
    ));
    assert!(
        Store::load(&temp.profiles)
            .unwrap()
            .get("__previous")
            .is_some()
    );
}

#[test]
fn arrange_applies_a_plan_and_reports_json() {
    let fake = FakeNiri::start("arrange-plan", two_outputs());
    let temp = TempState::new("arrange-plan");
    let plan = r#"{"outputs":{"eDP-1":[-1920,0],"DP-1":[0,0]}}"#;

    let output = run(
        &args(&["--json", "arrange", "--plan", plan], &temp),
        &fake.path,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["command"], "arrange");
    assert_eq!(json["data"]["status"], "applied");
    assert_eq!(json["data"]["actions"].as_array().unwrap().len(), 2);
    assert_eq!(json["data"]["positions"].as_array().unwrap().len(), 2);
    assert_eq!(fake.output("eDP-1").logical.unwrap().x, -1920);
    assert_eq!(fake.output("DP-1").logical.unwrap().x, 0);
}

#[test]
fn arrange_refuses_overlap_without_talking_to_niri() {
    let fake = FakeNiri::start("arrange-overlap", two_outputs());
    let temp = TempState::new("arrange-overlap");
    let plan = r#"{"eDP-1":[0,0],"DP-1":[100,100]}"#;

    let output = run(
        &args(&["--json", "arrange", "--plan", plan], &temp),
        &fake.path,
    );
    assert!(!output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["ok"], false);
    assert_eq!(json["data"]["status"], "refused");
    assert!(
        json["data"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("overlap"))
    );
    assert!(fake.recorded().is_empty());
    assert!(
        Store::load(&temp.profiles)
            .unwrap()
            .get("__previous")
            .is_none()
    );
}

#[test]
fn arrange_dry_run_is_isolated() {
    let fake = FakeNiri::start("arrange-dry", two_outputs());
    let temp = TempState::new("arrange-dry");
    let plan = r#"{"DP-1":[0,1080]}"#;

    let output = run(
        &args(&["--json", "arrange", "--plan", plan, "--dry-run"], &temp),
        &fake.path,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["data"]["status"], "planned");
    assert_eq!(json["data"]["dry_run"], true);
    assert!(fake.recorded().is_empty());
    assert!(
        Store::load(&temp.profiles)
            .unwrap()
            .get("__previous")
            .is_none()
    );
    assert_eq!(fake.output("DP-1").logical.unwrap().x, 1920);
}

#[test]
fn arrange_rejects_unknown_outputs_and_bad_json() {
    let fake = FakeNiri::start("arrange-bad", two_outputs());
    let temp = TempState::new("arrange-bad");

    let output = run(
        &args(&["--json", "arrange", "--plan", r#"{"DP-9":[0,0]}"#], &temp),
        &fake.path,
    );
    assert!(!output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["data"]["status"], "refused");
    assert!(
        json["data"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("not connected"))
    );

    let output = run(
        &args(&["--json", "arrange", "--plan", "{not json"], &temp),
        &fake.path,
    );
    assert!(!output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["ok"], false);
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("invalid plan JSON")
    );
    assert!(fake.recorded().is_empty());
}

#[test]
fn arrange_failure_rolls_back_to_the_previous_layout() {
    let fake = FakeNiri::start("arrange-rollback", two_outputs());
    let temp = TempState::new("arrange-rollback");
    let plan = r#"{"eDP-1":[-1920,0],"DP-1":[0,0]}"#;
    fake.fail_on_attempt(1);

    let output = run(
        &args(&["--json", "arrange", "--plan", plan], &temp),
        &fake.path,
    );
    assert!(!output.status.success());
    let json = stdout_json(&output);
    assert_eq!(json["data"]["status"], "rolled_back");
    assert!(
        !json["data"]["rollback_actions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(fake.output("eDP-1").logical.unwrap().x, 0);
    assert_eq!(fake.output("DP-1").logical.unwrap().x, 1920);
    let positions = json["data"]["positions"].as_array().unwrap();
    let edp = positions
        .iter()
        .find(|entry| entry["output"] == "eDP-1")
        .unwrap();
    assert_eq!(edp["x"], 0);
}

#[test]
fn arrange_reads_a_plan_file() {
    let fake = FakeNiri::start("arrange-file", two_outputs());
    let temp = TempState::new("arrange-file");
    let plan_path = temp.dir.join("plan.json");
    std::fs::write(&plan_path, r#"{"DP-1":[0,1080]}"#).unwrap();

    let output = run(
        &args(
            &[
                "--json",
                "arrange",
                "--plan",
                &format!("@{}", plan_path.display()),
            ],
            &temp,
        ),
        &fake.path,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fake.output("DP-1").logical.unwrap().y, 1080);
}
