//! Layout resolution, validation and ordered action planning.

use std::collections::{BTreeMap, BTreeSet};

use niri_ipc::{
    ConfiguredMode, ConfiguredPosition, ModeToSet, Output, OutputAction, PositionToSet, ScaleToSet,
    Transform, VrrToSet,
};
use serde::Serialize;

use crate::schema::{OutputProfile, Profile};

/// Phase of an output action. Actions are sent in this order so an output is
/// enabled and configured before any output is disabled.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ActionPhase {
    On,
    Mode,
    Scale,
    Transform,
    Position,
    Vrr,
    MaxBpc,
    Off,
}

/// One ordered niri output action.
#[derive(Debug, Clone, Serialize)]
pub struct PlannedAction {
    pub output: String,
    pub phase: ActionPhase,
    pub action: OutputAction,
}

impl PlannedAction {
    pub fn describe(&self) -> String {
        action_description(&self.action)
    }
}

/// Human-readable description of an output action.
pub fn action_description(action: &OutputAction) -> String {
    match action {
        OutputAction::Off => "turn off".to_owned(),
        OutputAction::On => "turn on".to_owned(),
        OutputAction::Mode { mode } => match mode {
            ModeToSet::Automatic => "mode auto".to_owned(),
            ModeToSet::Specific(mode) => format!(
                "mode {}x{}@{}",
                mode.width,
                mode.height,
                mode.refresh.unwrap_or_default()
            ),
        },
        OutputAction::CustomMode { mode } => format!(
            "custom mode {}x{}@{}",
            mode.width,
            mode.height,
            mode.refresh.unwrap_or_default()
        ),
        OutputAction::Modeline { .. } => "custom modeline".to_owned(),
        OutputAction::Scale { scale } => match scale {
            ScaleToSet::Automatic => "scale auto".to_owned(),
            ScaleToSet::Specific(scale) => format!("scale {scale}"),
        },
        OutputAction::Transform { transform } => {
            format!("transform {}", transform_name(*transform))
        }
        OutputAction::Position { position } => match position {
            PositionToSet::Automatic => "position auto".to_owned(),
            PositionToSet::Specific(pos) => format!("position {},{}", pos.x, pos.y),
        },
        OutputAction::Vrr { vrr } => {
            if vrr.on_demand {
                format!("vrr {} (on demand)", vrr.vrr)
            } else {
                format!("vrr {}", vrr.vrr)
            }
        }
        OutputAction::MaxBpc { max_bpc } => format!("max bpc {}", max_bpc_number(*max_bpc)),
        OutputAction::Hdr { hdr } => {
            let mode = match hdr.mode {
                niri_ipc::HdrModeArg::Auto => "auto",
                niri_ipc::HdrModeArg::On => "on",
                niri_ipc::HdrModeArg::Off => "off",
            };
            match hdr.reference_luminance {
                Some(nits) => format!("hdr {mode} ({nits} nits)"),
                None => format!("hdr {mode}"),
            }
        }
    }
}

fn transform_name(transform: Transform) -> &'static str {
    match transform {
        Transform::Normal => "Normal",
        Transform::_90 => "90",
        Transform::_180 => "180",
        Transform::_270 => "270",
        Transform::Flipped => "Flipped",
        Transform::Flipped90 => "Flipped90",
        Transform::Flipped180 => "Flipped180",
        Transform::Flipped270 => "Flipped270",
    }
}

fn max_bpc_number(max_bpc: niri_ipc::MaxBpc) -> u8 {
    match max_bpc {
        niri_ipc::MaxBpc::_6 => 6,
        niri_ipc::MaxBpc::_8 => 8,
        niri_ipc::MaxBpc::_10 => 10,
        niri_ipc::MaxBpc::_12 => 12,
        niri_ipc::MaxBpc::_14 => 14,
        niri_ipc::MaxBpc::_16 => 16,
    }
}

/// A field that can only be changed in the niri config file, not over IPC.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigOnlyNote {
    pub output: String,
    pub field: String,
    pub detail: String,
}

/// The outcome of resolving and planning a profile.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// Profile output key (or original key) to the connected output name.
    pub resolved: BTreeMap<String, String>,
    /// Profile entries keyed by connected output name.
    pub desired: BTreeMap<String, OutputProfile>,
    /// Resulting layout, including outputs the profile does not touch.
    pub layout: BTreeMap<String, OutputProfile>,
    pub actions: Vec<PlannedAction>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub config_only: Vec<ConfigOnlyNote>,
}

impl Plan {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

pub(crate) struct Resolved {
    pub(crate) desired: BTreeMap<String, OutputProfile>,
    pub(crate) mapping: BTreeMap<String, String>,
    pub(crate) warnings: Vec<String>,
}

/// Captures the currently applied output settings.
pub fn snapshot(outputs: &BTreeMap<String, Output>) -> Profile {
    let mut profile = Profile::new();
    for (name, output) in outputs {
        let mut entry = OutputProfile::default();
        if let Some(logical) = output.logical {
            entry.off = false;
            entry.scale = Some(logical.scale);
            entry.transform = Some(logical.transform);
            entry.position = Some((logical.x, logical.y));
            if let Some(index) = output.current_mode
                && let Some(mode) = output.modes.get(index)
            {
                entry.mode = Some((mode.width, mode.height, mode.refresh_rate));
            }
            entry.vrr = Some(output.vrr_enabled);
        } else {
            entry.off = true;
        }
        entry.max_bpc = output
            .max_bpc
            .and_then(|bpc| niri_ipc::MaxBpc::try_from(bpc).ok());
        profile.outputs.insert(name.clone(), entry);
    }
    profile
}

/// Plans a named profile against the current outputs.
pub fn plan_profile(
    profile: &Profile,
    current: &BTreeMap<String, Output>,
    forced_target: Option<&str>,
) -> Plan {
    let mut errors = Vec::new();
    if let Err(err) = profile.check_version() {
        errors.push(err);
    }

    let resolved = match resolve_profile(profile, current, forced_target) {
        Ok(resolved) => resolved,
        Err(resolve_errors) => {
            errors.extend(resolve_errors);
            return Plan {
                resolved: BTreeMap::new(),
                desired: BTreeMap::new(),
                layout: BTreeMap::new(),
                actions: Vec::new(),
                errors,
                warnings: Vec::new(),
                config_only: Vec::new(),
            };
        }
    };

    if resolved.desired.is_empty() {
        errors.push("profile has no output settings".to_owned());
    }

    let layout = build_layout(current, &resolved.desired);
    errors.extend(validate_layout(&layout));
    let actions = actions_for_desired(&resolved.desired);
    let config_only = config_only_notes(&resolved.desired);

    Plan {
        resolved: resolved.mapping,
        desired: resolved.desired,
        layout,
        actions,
        errors,
        warnings: resolved.warnings,
        config_only,
    }
}

/// Plans a stored snapshot without name resolution or conflict validation.
///
/// Used by `restore` and rollback, where the previous layout is assumed to
/// have been valid before and recovery must not be blocked by a new conflict
/// check.
pub fn plan_snapshot(profile: &Profile) -> Vec<PlannedAction> {
    actions_for_desired(&profile.outputs)
}

pub(crate) fn resolve_profile(
    profile: &Profile,
    current: &BTreeMap<String, Output>,
    forced_target: Option<&str>,
) -> Result<Resolved, Vec<String>> {
    let mut warnings = Vec::new();
    let mut mapping = BTreeMap::new();
    let mut desired = BTreeMap::new();

    if let Some(target) = forced_target {
        let Some(output) = current.get(target) else {
            return Err(vec![format!(
                "rule target output `{target}` is not connected"
            )]);
        };
        if let Some(matcher) = &profile.match_
            && !matcher.matches(output)
        {
            return Err(vec![format!(
                "profile matcher `{}` does not match rule target `{target}`",
                matcher.describe()
            )]);
        }
        let (key, entry) = select_entry(profile, Some(target))?;
        mapping.insert(key, target.to_owned());
        desired.insert(target.to_owned(), entry);
    } else if let Some(matcher) = &profile.match_ {
        let matches: Vec<&String> = current
            .iter()
            .filter(|(_, output)| matcher.matches(output))
            .map(|(name, _)| name)
            .collect();
        if matches.is_empty() {
            return Err(vec![format!(
                "no connected output matches `{}`",
                matcher.describe()
            )]);
        }
        if matches.len() > 1 {
            warnings.push(format!(
                "`{}` matches {} outputs ({}); using `{}`",
                matcher.describe(),
                matches.len(),
                matches
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                matches[0]
            ));
        }
        let target = matches[0].clone();
        let (key, entry) = select_entry(profile, Some(&target))?;
        mapping.insert(key, target.clone());
        desired.insert(target, entry);
    } else {
        for (key, entry) in &profile.outputs {
            mapping.insert(key.clone(), key.clone());
            desired.insert(key.clone(), entry.clone());
        }
    }

    let mut seen = BTreeSet::new();
    for target in mapping.values() {
        if !seen.insert(target.clone()) {
            return Err(vec![format!(
                "profile resolves more than one entry to output `{target}`"
            )]);
        }
    }

    normalize_mirror_targets(&mut desired, &mapping);
    Ok(Resolved {
        desired,
        mapping,
        warnings,
    })
}

fn select_entry(
    profile: &Profile,
    target: Option<&str>,
) -> Result<(String, OutputProfile), Vec<String>> {
    match (profile.outputs.len(), target) {
        (1, _) => {
            let (key, entry) = profile.outputs.iter().next().expect("length checked");
            Ok((key.clone(), entry.clone()))
        }
        (0, _) => Err(vec![
            "profile has a matcher but no output settings".to_owned(),
        ]),
        (_, Some(target)) if profile.outputs.contains_key(target) => {
            let entry = profile.outputs[target].clone();
            Ok((target.to_owned(), entry))
        }
        _ => Err(vec![format!(
            "profile matcher must be used with a single output entry (found {})",
            profile.outputs.len()
        )]),
    }
}

fn normalize_mirror_targets(
    desired: &mut BTreeMap<String, OutputProfile>,
    mapping: &BTreeMap<String, String>,
) {
    for entry in desired.values_mut() {
        if let Some(target) = &entry.mirror
            && let Some(actual) = mapping.get(target)
        {
            entry.mirror = Some(actual.clone());
        }
    }
}

fn build_layout(
    current: &BTreeMap<String, Output>,
    desired: &BTreeMap<String, OutputProfile>,
) -> BTreeMap<String, OutputProfile> {
    let mut layout: BTreeMap<String, OutputProfile> = snapshot(current).outputs;
    for (name, over) in desired {
        match layout.get(name) {
            Some(base) => {
                let merged = merge_output(base, over);
                layout.insert(name.clone(), merged);
            }
            None => {
                layout.insert(name.clone(), over.clone());
            }
        }
    }
    layout
}

fn merge_output(base: &OutputProfile, over: &OutputProfile) -> OutputProfile {
    OutputProfile {
        off: over.off,
        scale: over.scale.or(base.scale),
        mode: over.mode.or(base.mode),
        transform: over.transform.or(base.transform),
        position: over.position.or(base.position),
        vrr: over.vrr.or(base.vrr),
        vrr_on_demand: over.vrr_on_demand.or(base.vrr_on_demand),
        max_bpc: over.max_bpc.or(base.max_bpc),
        hdr: over.hdr.or(base.hdr),
        focus_at_startup: over.focus_at_startup.or(base.focus_at_startup),
        isolated: over.isolated.or(base.isolated),
        mirror: over.mirror.clone().or_else(|| base.mirror.clone()),
    }
}

/// Validates a resulting layout.
pub fn validate_layout(layout: &BTreeMap<String, OutputProfile>) -> Vec<String> {
    let mut errors = Vec::new();

    for (name, entry) in layout {
        if let Some(scale) = entry.scale
            && (!scale.is_finite() || scale <= 0.0)
        {
            errors.push(format!("output `{name}` has invalid scale {scale}"));
        }
        if let Some((width, height, _)) = entry.mode
            && (width == 0 || height == 0)
        {
            errors.push(format!(
                "output `{name}` has an invalid mode {width}x{height}"
            ));
        }
    }

    let enabled: Vec<(&String, &OutputProfile)> =
        layout.iter().filter(|(_, entry)| !entry.off).collect();
    if enabled.is_empty() {
        errors.push("refusing to apply: the resulting layout has no enabled outputs".to_owned());
    }

    errors.extend(validate_mirrors(layout));

    let positioned: Vec<(&String, Rect)> = enabled
        .iter()
        .filter(|(_, entry)| entry.mirror.is_none())
        .filter_map(|(name, entry)| logical_rect(entry).map(|rect| (*name, rect)))
        .collect();
    for (index, (name_a, rect_a)) in positioned.iter().enumerate() {
        for (name_b, rect_b) in positioned.iter().skip(index + 1) {
            if rect_a.intersects(rect_b) {
                errors.push(format!(
                    "outputs `{name_a}` and `{name_b}` overlap: {} and {}",
                    rect_a.describe(),
                    rect_b.describe()
                ));
            }
        }
    }

    errors
}

fn validate_mirrors(layout: &BTreeMap<String, OutputProfile>) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, entry) in layout {
        let Some(target) = &entry.mirror else {
            continue;
        };
        if target == name {
            errors.push(format!("output `{name}` cannot mirror itself"));
            continue;
        }
        let Some(target_entry) = layout.get(target) else {
            errors.push(format!("output `{name}` mirrors unknown output `{target}`"));
            continue;
        };
        if target_entry.off {
            errors.push(format!(
                "output `{name}` mirrors `{target}`, which is disabled"
            ));
        }
        if entry.isolated == Some(true) || target_entry.isolated == Some(true) {
            errors.push(format!(
                "output `{name}` cannot mirror `{target}` while either output is isolated"
            ));
        }
    }
    errors
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && other.x < self.x + self.width
            && self.y < other.y + other.height
            && other.y < self.y + self.height
    }

    fn describe(&self) -> String {
        format!("{}x{} at {},{}", self.width, self.height, self.x, self.y)
    }
}

fn logical_rect(entry: &OutputProfile) -> Option<Rect> {
    let (width, height, _) = entry.mode?;
    let (x, y) = entry.position?;
    let scale = entry.scale.unwrap_or(1.0);
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let (width, height) = match entry.transform.unwrap_or(Transform::Normal) {
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
            (height, width)
        }
        _ => (width, height),
    };
    Some(Rect {
        x,
        y,
        width: (f64::from(width) / scale).round() as i32,
        height: (f64::from(height) / scale).round() as i32,
    })
}

fn actions_for_desired(desired: &BTreeMap<String, OutputProfile>) -> Vec<PlannedAction> {
    let mut actions = Vec::new();
    for (name, entry) in desired {
        if entry.off {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::Off,
                action: OutputAction::Off,
            });
            continue;
        }

        actions.push(PlannedAction {
            output: name.clone(),
            phase: ActionPhase::On,
            action: OutputAction::On,
        });
        if let Some((width, height, refresh_rate)) = entry.mode {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::Mode,
                action: OutputAction::Mode {
                    mode: ModeToSet::Specific(ConfiguredMode {
                        width,
                        height,
                        refresh: Some(f64::from(refresh_rate) / 1000.),
                    }),
                },
            });
        }
        if let Some(scale) = entry.scale {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::Scale,
                action: OutputAction::Scale {
                    scale: ScaleToSet::Specific(scale),
                },
            });
        }
        if let Some(transform) = entry.transform {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::Transform,
                action: OutputAction::Transform { transform },
            });
        }
        if let Some((x, y)) = entry.position {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::Position,
                action: OutputAction::Position {
                    position: PositionToSet::Specific(ConfiguredPosition { x, y }),
                },
            });
        }
        if let Some(vrr) = entry.vrr {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::Vrr,
                action: OutputAction::Vrr {
                    vrr: VrrToSet {
                        vrr,
                        on_demand: entry.vrr_on_demand.unwrap_or(false),
                    },
                },
            });
        }
        if let Some(max_bpc) = entry.max_bpc {
            actions.push(PlannedAction {
                output: name.clone(),
                phase: ActionPhase::MaxBpc,
                action: OutputAction::MaxBpc { max_bpc },
            });
        }
    }

    actions.sort_by(|a, b| a.phase.cmp(&b.phase).then_with(|| a.output.cmp(&b.output)));
    actions
}

fn config_only_notes(desired: &BTreeMap<String, OutputProfile>) -> Vec<ConfigOnlyNote> {
    let mut notes = Vec::new();
    for (output, entry) in desired {
        if entry.hdr.is_some() {
            notes.push(ConfigOnlyNote {
                output: output.clone(),
                field: "hdr".to_owned(),
                detail: format!(
                    "HDR is not exposed over niri IPC; add `output \"{output}\" {{ hdr {{ ... }} }}` to the niri config. The IPC apply skips this field."
                ),
            });
        }
        if entry.focus_at_startup.is_some() {
            notes.push(ConfigOnlyNote {
                output: output.clone(),
                field: "focus_at_startup".to_owned(),
                detail: format!(
                    "`focus_at_startup` only affects startup; set it in the niri config for `{output}`. The IPC apply skips this field."
                ),
            });
        }
        if entry.isolated.is_some() {
            notes.push(ConfigOnlyNote {
                output: output.clone(),
                field: "isolated".to_owned(),
                detail: format!(
                    "`isolated` is not exposed over niri IPC; set `output \"{output}\" {{ isolated; }}` in the niri config. The IPC apply skips this field."
                ),
            });
        }
        if let Some(source) = &entry.mirror {
            notes.push(ConfigOnlyNote {
                output: output.clone(),
                field: "mirror".to_owned(),
                detail: format!(
                    "output mirroring is not an niri IPC action; a real apply runs `wl-mirror {source} --fullscreen-output {output}`. Nothing is spawned for this note (dry run, refusal or explain)."
                ),
            });
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use niri_ipc::{LogicalOutput, MaxBpc, Mode};

    use super::*;
    use crate::schema::{OutputMatch, tests_support};

    fn map(outputs: Vec<(&str, Output)>) -> BTreeMap<String, Output> {
        outputs
            .into_iter()
            .map(|(name, output)| (name.to_owned(), output))
            .collect()
    }

    fn positioned(name: &str, x: i32, y: i32, scale: f64) -> Output {
        let mut output = tests_support::output(name, "Test", "Test", None);
        output.logical = Some(LogicalOutput {
            x,
            y,
            width: 1920,
            height: 1080,
            scale,
            transform: Transform::Normal,
        });
        output
    }

    fn at(mut output: Output, x: i32) -> Output {
        if let Some(logical) = &mut output.logical {
            logical.x = x;
        }
        output
    }

    #[test]
    fn snapshots_outputs_and_plans_actions_in_order() {
        let outputs = map(vec![("eDP-1", positioned("eDP-1", 0, 0, 1.5))]);
        let profile = snapshot(&outputs);
        let entry = &profile.outputs["eDP-1"];
        assert_eq!(entry.scale, Some(1.5));
        assert_eq!(entry.mode, Some((1920, 1080, 60000)));
        assert_eq!(entry.vrr, Some(true));

        let plan = plan_profile(&profile, &outputs, None);
        assert!(plan.ok(), "{:?}", plan.errors);
        assert_eq!(plan.actions.len(), 6);
        assert!(matches!(plan.actions[0].action, OutputAction::On));
        assert!(matches!(
            plan.actions[1].action,
            OutputAction::Mode {
                mode: ModeToSet::Specific(ConfiguredMode {
                    width: 1920,
                    height: 1080,
                    ..
                })
            }
        ));
        assert!(matches!(plan.actions[5].action, OutputAction::Vrr { .. }));
    }

    #[test]
    fn off_outputs_only_emit_off_and_off_actions_come_last() {
        let mut profile = Profile::default();
        profile.outputs.insert(
            "HDMI-1".to_owned(),
            OutputProfile {
                off: true,
                scale: Some(2.),
                ..OutputProfile::default()
            },
        );
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                off: false,
                scale: Some(1.),
                ..OutputProfile::default()
            },
        );
        let outputs = map(vec![
            ("eDP-1", positioned("eDP-1", 0, 0, 1.)),
            ("HDMI-1", positioned("HDMI-1", 1920, 0, 1.)),
        ]);
        let plan = plan_profile(&profile, &outputs, None);
        assert!(plan.ok(), "{:?}", plan.errors);
        assert_eq!(plan.actions.len(), 3);
        assert!(matches!(plan.actions[0].action, OutputAction::On));
        assert!(matches!(plan.actions[2].action, OutputAction::Off));
        assert_eq!(plan.actions[2].output, "HDMI-1");
    }

    #[test]
    fn max_bpc_is_planned_and_snapshotted() {
        let mut output = positioned("DP-1", 0, 0, 1.);
        output.max_bpc = Some(10);
        let outputs = map(vec![("DP-1", output)]);
        let profile = snapshot(&outputs);
        assert_eq!(profile.outputs["DP-1"].max_bpc, Some(MaxBpc::_10));
        let plan = plan_profile(&profile, &outputs, None);
        assert!(
            plan.actions
                .iter()
                .any(|action| matches!(action.action, OutputAction::MaxBpc { .. }))
        );
    }

    #[test]
    fn profile_match_resolves_by_make_model_serial() {
        let outputs = map(vec![
            (
                "eDP-1",
                at(tests_support::output("eDP-1", "BOE", "0x095F", None), 0),
            ),
            (
                "DP-1",
                at(
                    tests_support::output("DP-1", "Dell Inc.", "U2720Q", Some("ABC123")),
                    1920,
                ),
            ),
        ]);
        let mut profile = Profile::new();
        profile.match_ = Some(OutputMatch {
            make: Some("Dell Inc.".to_owned()),
            model: Some("U2720Q".to_owned()),
            serial: Some("ABC123".to_owned()),
            name: None,
        });
        profile.outputs.insert(
            "desk".to_owned(),
            OutputProfile {
                scale: Some(2.),
                ..OutputProfile::default()
            },
        );

        let plan = plan_profile(&profile, &outputs, None);
        assert!(plan.ok(), "{:?}", plan.errors);
        assert_eq!(plan.resolved.get("desk").map(String::as_str), Some("DP-1"));
        assert!(plan.desired.contains_key("DP-1"));
        assert_eq!(plan.actions[0].output, "DP-1");
    }

    #[test]
    fn profile_match_without_a_match_is_an_error() {
        let outputs = map(vec![(
            "eDP-1",
            tests_support::output("eDP-1", "BOE", "X", None),
        )]);
        let mut profile = Profile::new();
        profile.match_ = Some(OutputMatch {
            make: Some("Dell".to_owned()),
            ..OutputMatch::default()
        });
        profile
            .outputs
            .insert("x".to_owned(), OutputProfile::default());
        let plan = plan_profile(&profile, &outputs, None);
        assert!(!plan.ok());
        assert!(plan.errors[0].contains("no connected output matches"));
    }

    #[test]
    fn forced_target_binds_single_output_profile() {
        let outputs = map(vec![
            (
                "eDP-1",
                at(tests_support::output("eDP-1", "BOE", "X", None), 0),
            ),
            (
                "DP-1",
                at(tests_support::output("DP-1", "Dell", "Y", None), 1920),
            ),
        ]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "DP-1".to_owned(),
            OutputProfile {
                scale: Some(2.),
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, Some("eDP-1"));
        assert!(plan.ok(), "{:?}", plan.errors);
        assert!(plan.desired.contains_key("eDP-1"));
        assert!(!plan.desired.contains_key("DP-1"));
        assert_eq!(plan.resolved["DP-1"], "eDP-1");
    }

    #[test]
    fn overlapping_positions_are_refused() {
        let outputs = map(vec![
            ("eDP-1", positioned("eDP-1", 0, 0, 1.)),
            ("DP-1", positioned("DP-1", 100, 100, 1.)),
        ]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "DP-1".to_owned(),
            OutputProfile {
                position: Some((100, 100)),
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        assert!(!plan.ok());
        assert!(
            plan.errors.iter().any(|err| err.contains("overlap")),
            "{:?}",
            plan.errors
        );
    }

    #[test]
    fn side_by_side_layout_is_valid() {
        let outputs = map(vec![
            ("eDP-1", positioned("eDP-1", 0, 0, 1.)),
            ("DP-1", positioned("DP-1", 1920, 0, 1.)),
        ]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "DP-1".to_owned(),
            OutputProfile {
                position: Some((1920, 0)),
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        assert!(plan.ok(), "{:?}", plan.errors);
    }

    #[test]
    fn disabling_every_output_is_refused() {
        let outputs = map(vec![("eDP-1", positioned("eDP-1", 0, 0, 1.))]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                off: true,
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        assert!(!plan.ok());
        assert!(
            plan.errors
                .iter()
                .any(|err| err.contains("no enabled outputs"))
        );
    }

    #[test]
    fn mirrored_isolated_output_is_refused() {
        let outputs = map(vec![("eDP-1", positioned("eDP-1", 0, 0, 1.))]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                isolated: Some(true),
                mirror: Some("eDP-1".to_owned()),
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        assert!(!plan.ok());
        // Both self-mirroring and mirrored+isolated are caught.
        assert!(plan.errors.iter().any(|err| err.contains("mirror")));
    }

    #[test]
    fn mirroring_an_isolated_output_is_refused() {
        let outputs = map(vec![
            ("eDP-1", positioned("eDP-1", 0, 0, 1.)),
            ("DP-1", positioned("DP-1", 1920, 0, 1.)),
        ]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "DP-1".to_owned(),
            OutputProfile {
                mirror: Some("eDP-1".to_owned()),
                ..OutputProfile::default()
            },
        );
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                isolated: Some(true),
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        assert!(!plan.ok());
        assert!(
            plan.errors
                .iter()
                .any(|err| err.contains("while either output is isolated")),
            "{:?}",
            plan.errors
        );
    }

    #[test]
    fn mirror_of_disabled_output_is_refused() {
        let outputs = map(vec![
            ("eDP-1", positioned("eDP-1", 0, 0, 1.)),
            ("DP-1", positioned("DP-1", 1920, 0, 1.)),
        ]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "DP-1".to_owned(),
            OutputProfile {
                mirror: Some("eDP-1".to_owned()),
                position: Some((0, 0)),
                ..OutputProfile::default()
            },
        );
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                off: true,
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        assert!(!plan.ok());
        assert!(
            plan.errors
                .iter()
                .any(|err| err.contains("which is disabled"))
        );
    }

    #[test]
    fn config_only_fields_are_reported() {
        let outputs = map(vec![("eDP-1", positioned("eDP-1", 0, 0, 1.))]);
        let mut profile = Profile::default();
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                hdr: Some(crate::schema::HdrProfile {
                    mode: crate::schema::HdrMode::On,
                    reference_luminance: Some(200.),
                }),
                focus_at_startup: Some(true),
                isolated: Some(false),
                ..OutputProfile::default()
            },
        );
        let plan = plan_profile(&profile, &outputs, None);
        let fields: Vec<_> = plan
            .config_only
            .iter()
            .map(|note| note.field.as_str())
            .collect();
        assert_eq!(fields, ["hdr", "focus_at_startup", "isolated"]);
    }

    #[test]
    fn snapshot_of_disabled_output_is_off() {
        let mut out = positioned("DP-1", 0, 0, 1.);
        out.logical = None;
        let outputs = map(vec![("DP-1", out)]);
        let profile = snapshot(&outputs);
        assert!(profile.outputs["DP-1"].off);
    }

    #[test]
    fn public_ipc_types_still_construct() {
        let logical = LogicalOutput {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
            scale: 1.,
            transform: Transform::Normal,
        };
        assert_eq!(logical.x, 1);
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_rate: 60000,
            is_preferred: true,
        };
        assert_eq!(mode.height, 1080);
    }
}
