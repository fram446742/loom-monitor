mod common;

use common::{FakeNiri, TempState, output_at};
use loom_monitor::{
    ApplyStatus, OutputMatch, OutputProfile, Profile, Rule, RuleStore, Store, WatchOptions,
    apply_named, restore, watch_once,
};
use niri_ipc::Transform;

fn profile_for(name: &str, scale: f64) -> Profile {
    let mut profile = Profile::new();
    profile.outputs.insert(
        name.to_owned(),
        OutputProfile {
            off: false,
            scale: Some(scale),
            mode: Some((1920, 1080, 60000)),
            transform: Some(Transform::Normal),
            position: Some((0, 0)),
            vrr: Some(true),
            ..OutputProfile::default()
        },
    );
    profile
}

fn store_with(temp: &TempState, name: &str, profile: Profile) -> Store {
    let mut store = Store::default();
    store.insert(name, profile);
    store.save(&temp.profiles).unwrap();
    store
}

#[test]
fn apply_sends_ordered_actions_and_saves_previous() {
    let fake = FakeNiri::start(
        "apply",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("apply");
    let mut store = store_with(&temp, "desk", profile_for("eDP-1", 1.5));

    let report = apply_named(&fake.path, &mut store, "desk", &temp.profiles, false).unwrap();

    assert_eq!(report.status, ApplyStatus::Applied, "{:?}", report.errors);
    assert!(report.previous_saved);
    assert_eq!(report.applied.len(), 6);
    assert_eq!(
        fake.action_labels(),
        [
            "turn on",
            "mode 1920x1080@60",
            "scale 1.5",
            "transform Normal",
            "position 0,0",
            "vrr true"
        ]
    );
    assert_eq!(fake.output("eDP-1").logical.unwrap().scale, 1.5);
    assert!(
        Store::load(&temp.profiles)
            .unwrap()
            .get("__previous")
            .is_some()
    );
}

#[test]
fn dry_run_sends_nothing_and_lists_actions() {
    let fake = FakeNiri::start(
        "dry",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("dry");
    let mut store = store_with(&temp, "desk", profile_for("eDP-1", 1.5));

    let report = apply_named(&fake.path, &mut store, "desk", &temp.profiles, true).unwrap();

    assert_eq!(report.status, ApplyStatus::Planned);
    assert_eq!(report.actions.len(), 6);
    assert!(!report.previous_saved);
    assert!(fake.recorded().is_empty(), "dry run must not touch niri");
    assert!(
        Store::load(&temp.profiles)
            .unwrap()
            .get("__previous")
            .is_none()
    );
}

#[test]
fn overlapping_layout_is_refused_before_any_action() {
    let fake = FakeNiri::start(
        "conflict",
        vec![
            output_at("eDP-1", "BOE", "Panel", None, 0, 1.0),
            output_at("DP-1", "Dell Inc.", "U2720Q", Some("ABC"), 1920, 1.0),
        ],
    );
    let temp = TempState::new("conflict");
    let mut profile = Profile::new();
    profile.outputs.insert(
        "DP-1".to_owned(),
        OutputProfile {
            position: Some((100, 100)),
            ..OutputProfile::default()
        },
    );
    let mut store = store_with(&temp, "bad", profile);

    let report = apply_named(&fake.path, &mut store, "bad", &temp.profiles, false).unwrap();

    assert_eq!(report.status, ApplyStatus::Refused);
    assert!(
        report.errors.iter().any(|error| error.contains("overlap")),
        "{:?}",
        report.errors
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
fn mirroring_an_isolated_output_is_refused_over_ipc() {
    let fake = FakeNiri::start(
        "mirror",
        vec![
            output_at("eDP-1", "BOE", "Panel", None, 0, 1.0),
            output_at("DP-1", "Dell Inc.", "U2720Q", Some("ABC"), 1920, 1.0),
        ],
    );
    let temp = TempState::new("mirror");
    let mut profile = Profile::new();
    profile.outputs.insert(
        "eDP-1".to_owned(),
        OutputProfile {
            isolated: Some(true),
            ..OutputProfile::default()
        },
    );
    profile.outputs.insert(
        "DP-1".to_owned(),
        OutputProfile {
            mirror: Some("eDP-1".to_owned()),
            ..OutputProfile::default()
        },
    );
    let mut store = store_with(&temp, "mirror", profile);

    let report = apply_named(&fake.path, &mut store, "mirror", &temp.profiles, false).unwrap();
    assert_eq!(report.status, ApplyStatus::Refused);
    assert!(fake.recorded().is_empty());
    assert!(report.config_only.iter().any(|note| note.field == "mirror"));
}

#[test]
fn disabling_the_last_output_is_refused() {
    let fake = FakeNiri::start(
        "all-off",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("all-off");
    let mut profile = Profile::new();
    profile.outputs.insert(
        "eDP-1".to_owned(),
        OutputProfile {
            off: true,
            ..OutputProfile::default()
        },
    );
    let mut store = store_with(&temp, "off", profile);

    let report = apply_named(&fake.path, &mut store, "off", &temp.profiles, false).unwrap();
    assert_eq!(report.status, ApplyStatus::Refused);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("no enabled outputs"))
    );
    assert!(fake.recorded().is_empty());
}

#[test]
fn partial_failure_rolls_back_to_the_previous_layout() {
    let fake = FakeNiri::start(
        "rollback",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("rollback");
    let mut store = store_with(&temp, "desk", profile_for("eDP-1", 1.5));
    fake.fail_on_attempt(2);

    let report = apply_named(&fake.path, &mut store, "desk", &temp.profiles, false).unwrap();

    assert_eq!(
        report.status,
        ApplyStatus::RolledBack,
        "{:?}",
        report.errors
    );
    assert_eq!(report.applied.len(), 2);
    assert!(report.failed_action.is_some());
    assert_eq!(report.rollback_actions.len(), 6);
    assert!(report.rollback_errors.is_empty());
    assert!(fake.recorded().len() > report.actions.len());
    assert_eq!(
        fake.output("eDP-1").logical.unwrap().scale,
        1.0,
        "rollback must restore the pre-apply scale"
    );
    let stored = Store::load(&temp.profiles).unwrap();
    assert!(stored.get("__previous").is_some());
}

#[test]
fn restore_reapplies_previous_without_overwriting_it() {
    let fake = FakeNiri::start(
        "restore",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("restore");
    let mut store = store_with(&temp, "desk", profile_for("eDP-1", 1.5));
    apply_named(&fake.path, &mut store, "desk", &temp.profiles, false).unwrap();
    assert_eq!(fake.output("eDP-1").logical.unwrap().scale, 1.5);
    fake.clear_recorded();

    let report = restore(&fake.path, &store, &temp.profiles, false).unwrap();

    assert_eq!(report.status, ApplyStatus::Restored, "{:?}", report.errors);
    assert_eq!(fake.output("eDP-1").logical.unwrap().scale, 1.0);
    assert!(
        Store::load(&temp.profiles)
            .unwrap()
            .get("__previous")
            .is_some()
    );
    assert_eq!(fake.recorded().len(), 6);
}

#[test]
fn profile_match_applies_to_the_matching_output() {
    let fake = FakeNiri::start(
        "match",
        vec![
            output_at("eDP-1", "BOE", "Panel", None, 0, 1.0),
            output_at("DP-1", "Dell Inc.", "U2720Q", Some("ABC123"), 1920, 1.0),
        ],
    );
    let temp = TempState::new("match");
    let mut profile = Profile::new();
    profile.match_ = Some(OutputMatch {
        make: Some("Dell Inc.".to_owned()),
        model: Some("U2720Q".to_owned()),
        serial: Some("ABC123".to_owned()),
        name: None,
    });
    profile.outputs.insert(
        "monitor".to_owned(),
        OutputProfile {
            scale: Some(2.),
            ..OutputProfile::default()
        },
    );
    let mut store = store_with(&temp, "dock", profile);

    let report = apply_named(&fake.path, &mut store, "dock", &temp.profiles, false).unwrap();

    assert_eq!(report.status, ApplyStatus::Applied, "{:?}", report.errors);
    assert_eq!(report.resolved["monitor"], "DP-1");
    assert!(fake.recorded().iter().all(|record| record.output == "DP-1"));
    assert_eq!(fake.output("DP-1").logical.unwrap().scale, 2.0);
    assert_eq!(fake.output("eDP-1").logical.unwrap().scale, 1.0);
}

#[test]
fn watch_once_applies_the_highest_priority_rule() {
    let fake = FakeNiri::start(
        "watch",
        vec![output_at(
            "DP-1",
            "Dell Inc.",
            "U2720Q",
            Some("ABC"),
            0,
            1.0,
        )],
    );
    let temp = TempState::new("watch");
    let mut profile = Profile::new();
    profile.outputs.insert(
        "DP-1".to_owned(),
        OutputProfile {
            scale: Some(2.),
            ..OutputProfile::default()
        },
    );
    let mut store = store_with(&temp, "desk", profile);

    let mut rules = RuleStore::default();
    rules.insert(Rule {
        name: "fallback".to_owned(),
        profile: "desk".to_owned(),
        priority: 1,
        match_: OutputMatch::default(),
    });
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

    let options = WatchOptions {
        once: true,
        ..WatchOptions::default()
    };
    let cycle = watch_once(&fake.path, &mut store, &rules, &temp.profiles, &options).unwrap();

    assert_eq!(cycle.decision, "applied");
    assert_eq!(cycle.rules[0].rule.as_deref(), Some("dock"));
    assert_eq!(cycle.reports.len(), 1);
    assert_eq!(cycle.reports[0].status, ApplyStatus::Applied);
    assert_eq!(fake.output("DP-1").logical.unwrap().scale, 2.0);
}

#[test]
fn watch_once_reports_when_no_rule_matches() {
    let fake = FakeNiri::start(
        "watch-none",
        vec![output_at("eDP-1", "BOE", "Panel", None, 0, 1.0)],
    );
    let temp = TempState::new("watch-none");
    let mut store = Store::default();
    store.save(&temp.profiles).unwrap();
    let rules = RuleStore::default();

    let options = WatchOptions {
        once: true,
        ..WatchOptions::default()
    };
    let cycle = watch_once(&fake.path, &mut store, &rules, &temp.profiles, &options).unwrap();

    assert_eq!(cycle.decision, "no_matching_rules");
    assert!(cycle.reports.is_empty());
    assert!(fake.recorded().is_empty());
}
