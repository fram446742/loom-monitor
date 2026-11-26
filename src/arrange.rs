//! Runtime output arrangement: read logical positions and move outputs over
//! niri IPC.
//!
//! Unlike profiles, an arrangement is a small set of logical coordinates. It is
//! validated against the current layout (connected outputs, sane coordinates,
//! no overlaps) and applied as ordered `OutputAction::Position` requests. Apply
//! keeps the existing contract: the pre-change layout is stored as
//! `__previous`, `--dry-run` sends nothing and a failure mid-apply triggers a
//! compensating rollback.

use std::collections::BTreeMap;
use std::path::Path;

use niri_ipc::{ConfiguredPosition, Output, OutputAction, PositionToSet, Transform};
use serde::Serialize;

use crate::ipc::{ApplyReport, ApplyStatus, apply_actions, query_outputs};
use crate::plan::{ActionPhase, PlannedAction};
use crate::schema::Store;

/// Largest absolute logical coordinate accepted in a plan. This only rejects
/// obviously bogus values while leaving room for large multi-monitor walls.
pub const MAX_COORDINATE: i64 = 1_000_000;

/// One output's logical geometry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputPosition {
    pub output: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub transform: Transform,
    pub enabled: bool,
}

/// Current logical positions, one entry per connected output.
pub fn current_positions(outputs: &BTreeMap<String, Output>) -> Vec<OutputPosition> {
    outputs
        .iter()
        .map(|(name, output)| match &output.logical {
            Some(logical) => OutputPosition {
                output: name.clone(),
                x: logical.x,
                y: logical.y,
                width: logical.width,
                height: logical.height,
                scale: logical.scale,
                transform: logical.transform,
                enabled: true,
            },
            None => OutputPosition {
                output: name.clone(),
                x: 0,
                y: 0,
                width: 0,
                height: 0,
                scale: 1.0,
                transform: Transform::Normal,
                enabled: false,
            },
        })
        .collect()
}

/// A resolved arrangement plan.
#[derive(Debug, Clone, Serialize)]
pub struct PositionPlan {
    /// Effective geometry after the plan (all connected outputs).
    pub positions: Vec<OutputPosition>,
    pub actions: Vec<PlannedAction>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl PositionPlan {
    pub fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// The result of applying an arrangement, including the geometry it targeted.
#[derive(Debug, Clone, Serialize)]
pub struct ArrangeReport {
    #[serde(flatten)]
    pub report: ApplyReport,
    pub positions: Vec<OutputPosition>,
}

/// Parses a position plan.
///
/// Accepted shapes:
///
/// ```json
/// {"eDP-1": [0, 0], "DP-1": [1920, 0]}
/// {"eDP-1": {"x": 0, "y": 0}}
/// {"outputs": {"eDP-1": [0, 0]}}
/// ```
pub fn parse_plan(text: &str) -> Result<BTreeMap<String, (i32, i32)>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("invalid plan JSON: {err}"))?;
    let object = value
        .get("outputs")
        .unwrap_or(&value)
        .as_object()
        .ok_or_else(|| "plan JSON must be an object of output -> position".to_owned())?;
    if object.is_empty() {
        return Err("plan has no outputs".to_owned());
    }

    let mut plan = BTreeMap::new();
    for (output, position) in object {
        let (x, y) = parse_position(position).map_err(|err| format!("output `{output}`: {err}"))?;
        plan.insert(output.clone(), (x, y));
    }
    Ok(plan)
}

fn parse_position(value: &serde_json::Value) -> Result<(i32, i32), String> {
    let pair = if let Some(array) = value.as_array() {
        if array.len() != 2 {
            return Err("position array must be [x, y]".to_owned());
        }
        (array[0].as_i64(), array[1].as_i64())
    } else if let Some(object) = value.as_object() {
        if let Some(array) = object.get("position") {
            let array = array
                .as_array()
                .ok_or_else(|| "`position` must be [x, y]".to_owned())?;
            if array.len() != 2 {
                return Err("`position` must be [x, y]".to_owned());
            }
            (array[0].as_i64(), array[1].as_i64())
        } else {
            (
                object.get("x").and_then(|v| v.as_i64()),
                object.get("y").and_then(|v| v.as_i64()),
            )
        }
    } else {
        return Err("position must be [x, y] or {x, y}".to_owned());
    };
    let (Some(x), Some(y)) = pair else {
        return Err("position needs integer x and y".to_owned());
    };
    let x = i32::try_from(x).map_err(|_| format!("x {x} is out of range"))?;
    let y = i32::try_from(y).map_err(|_| format!("y {y} is out of range"))?;
    Ok((x, y))
}

/// Builds and validates a plan against the current outputs.
pub fn plan_positions(
    outputs: &BTreeMap<String, Output>,
    wanted: &BTreeMap<String, (i32, i32)>,
) -> PositionPlan {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if wanted.is_empty() {
        errors.push("position plan has no outputs".to_owned());
    }
    if !outputs.values().any(|output| output.logical.is_some()) {
        errors.push("refusing to apply: the current layout has no enabled outputs".to_owned());
    }

    for (name, (x, y)) in wanted {
        if i64::from(*x).abs() > MAX_COORDINATE || i64::from(*y).abs() > MAX_COORDINATE {
            errors.push(format!(
                "output `{name}` has an implausible position {x},{y} (limit ±{MAX_COORDINATE})"
            ));
        }
        match outputs.get(name) {
            None => errors.push(format!("output `{name}` is not connected")),
            Some(output) if output.logical.is_none() => errors.push(format!(
                "output `{name}` is disabled; enable it before moving it"
            )),
            Some(_) => {}
        }
    }

    let effective = effective_positions(outputs, wanted);
    let mut enabled: Vec<&OutputPosition> =
        effective.iter().filter(|entry| entry.enabled).collect();
    enabled.sort_by(|a, b| a.output.cmp(&b.output));
    for (index, first) in enabled.iter().enumerate() {
        for second in enabled.iter().skip(index + 1) {
            if overlaps(first, second) {
                errors.push(format!(
                    "outputs `{}` and `{}` overlap: {}x{} at {},{} and {}x{} at {},{}",
                    first.output,
                    second.output,
                    first.width,
                    first.height,
                    first.x,
                    first.y,
                    second.width,
                    second.height,
                    second.x,
                    second.y
                ));
            }
        }
    }

    let actions = wanted
        .iter()
        .map(|(output, (x, y))| PlannedAction {
            output: output.clone(),
            phase: ActionPhase::Position,
            action: OutputAction::Position {
                position: PositionToSet::Specific(ConfiguredPosition { x: *x, y: *y }),
            },
        })
        .collect();

    if wanted.is_empty() {
        warnings.push("nothing to move".to_owned());
    }

    PositionPlan {
        positions: effective,
        actions,
        errors,
        warnings,
    }
}

fn effective_positions(
    outputs: &BTreeMap<String, Output>,
    wanted: &BTreeMap<String, (i32, i32)>,
) -> Vec<OutputPosition> {
    current_positions(outputs)
        .into_iter()
        .map(|mut entry| {
            if entry.enabled
                && let Some((x, y)) = wanted.get(&entry.output)
            {
                entry.x = *x;
                entry.y = *y;
            }
            entry
        })
        .collect()
}

fn overlaps(first: &OutputPosition, second: &OutputPosition) -> bool {
    let first_right = i64::from(first.x) + i64::from(first.width);
    let second_right = i64::from(second.x) + i64::from(second.width);
    let first_bottom = i64::from(first.y) + i64::from(first.height);
    let second_bottom = i64::from(second.y) + i64::from(second.height);
    i64::from(first.x) < second_right
        && i64::from(second.x) < first_right
        && i64::from(first.y) < second_bottom
        && i64::from(second.y) < first_bottom
}

/// Applies an arrangement, storing `__previous` first unless this is a dry run.
pub fn arrange(
    socket: &Path,
    store: &mut Store,
    state_path: &Path,
    wanted: &BTreeMap<String, (i32, i32)>,
    dry_run: bool,
) -> Result<ArrangeReport, String> {
    let current = query_outputs(socket)?;
    let plan = plan_positions(&current, wanted);
    if !plan.ok() {
        let report = ApplyReport::refused("arrange", dry_run, plan.errors.clone(), plan.actions);
        return Ok(ArrangeReport {
            report,
            positions: plan.positions,
        });
    }

    let report = apply_actions(socket, store, state_path, "arrange", &plan.actions, dry_run)?;
    let positions = match report.status {
        ApplyStatus::RolledBack | ApplyStatus::RollbackFailed => {
            let after = query_outputs(socket)?;
            current_positions(&after)
        }
        _ => plan.positions,
    };
    Ok(ArrangeReport { report, positions })
}

#[cfg(test)]
mod tests {
    use niri_ipc::LogicalOutput;

    use super::*;

    fn output(name: &str, x: i32, y: i32, scale: f64) -> Output {
        let mut output = crate::schema::tests_support::output(name, "Test", "Test", None);
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

    fn layout(entries: Vec<(&str, i32, i32, f64)>) -> BTreeMap<String, Output> {
        entries
            .into_iter()
            .map(|(name, x, y, scale)| (name.to_owned(), output(name, x, y, scale)))
            .collect()
    }

    #[test]
    fn reads_positions_and_sizes() {
        let outputs = layout(vec![("eDP-1", 0, 0, 1.5), ("DP-1", 1280, 0, 1.0)]);
        let positions = current_positions(&outputs);
        assert_eq!(positions.len(), 2);
        let dp = positions.iter().find(|p| p.output == "DP-1").unwrap();
        assert_eq!((dp.x, dp.y, dp.width, dp.height), (1280, 0, 1920, 1080));
        assert!(dp.enabled);
    }

    #[test]
    fn parses_all_documented_plan_shapes() {
        let plan = parse_plan(r#"{"eDP-1":[0,0],"DP-1":[1920,0]}"#).unwrap();
        assert_eq!(plan["DP-1"], (1920, 0));
        let plan = parse_plan(r#"{"outputs":{"eDP-1":{"x":-1,"y":2}}}"#).unwrap();
        assert_eq!(plan["eDP-1"], (-1, 2));
        let plan = parse_plan(r#"{"DP-1":{"position":[10,20]}}"#).unwrap();
        assert_eq!(plan["DP-1"], (10, 20));
        assert!(parse_plan(r#"{"DP-1":[1]}"#).is_err());
        assert!(parse_plan("[]").is_err());
        assert!(parse_plan("{}").unwrap_err().contains("no outputs"));
    }

    #[test]
    fn accepts_non_overlapping_positions() {
        let outputs = layout(vec![("eDP-1", 0, 0, 1.0), ("DP-1", 1920, 0, 1.0)]);
        let wanted = BTreeMap::from([("DP-1".to_owned(), (1920, 0))]);
        let plan = plan_positions(&outputs, &wanted);
        assert!(plan.ok(), "{:?}", plan.errors);
        assert_eq!(plan.actions.len(), 1);
        assert!(matches!(
            plan.actions[0].action,
            OutputAction::Position {
                position: PositionToSet::Specific(ConfiguredPosition { x: 1920, y: 0 })
            }
        ));
    }

    #[test]
    fn refuses_overlaps() {
        let outputs = layout(vec![("eDP-1", 0, 0, 1.0), ("DP-1", 1920, 0, 1.0)]);
        let wanted = BTreeMap::from([("DP-1".to_owned(), (100, 100))]);
        let plan = plan_positions(&outputs, &wanted);
        assert!(!plan.ok());
        assert!(plan.errors.iter().any(|error| error.contains("overlap")));
    }

    #[test]
    fn touching_edges_do_not_overlap() {
        let outputs = layout(vec![("eDP-1", 0, 0, 1.0), ("DP-1", 1920, 0, 1.0)]);
        let wanted = BTreeMap::from([("DP-1".to_owned(), (1920, 0))]);
        assert!(plan_positions(&outputs, &wanted).ok());
    }

    #[test]
    fn refuses_unknown_disabled_and_implausible_outputs() {
        let mut outputs = layout(vec![("eDP-1", 0, 0, 1.0)]);
        let mut disabled = crate::schema::tests_support::output("HDMI-1", "Test", "Test", None);
        disabled.logical = None;
        outputs.insert("HDMI-1".to_owned(), disabled);
        let wanted = BTreeMap::from([
            ("DP-9".to_owned(), (0, 0)),
            ("HDMI-1".to_owned(), (1920, 0)),
            ("eDP-1".to_owned(), (MAX_COORDINATE as i32 + 1, 0)),
        ]);
        let plan = plan_positions(&outputs, &wanted);
        assert!(!plan.ok());
        assert!(
            plan.errors
                .iter()
                .any(|error| error.contains("not connected"))
        );
        assert!(plan.errors.iter().any(|error| error.contains("disabled")));
        assert!(
            plan.errors
                .iter()
                .any(|error| error.contains("implausible"))
        );
    }

    #[test]
    fn refuses_a_layout_with_every_output_disabled() {
        let mut outputs = layout(vec![("eDP-1", 0, 0, 1.0)]);
        outputs.get_mut("eDP-1").unwrap().logical = None;
        let wanted = BTreeMap::from([("eDP-1".to_owned(), (0, 0))]);
        let plan = plan_positions(&outputs, &wanted);
        assert!(!plan.ok());
        assert!(
            plan.errors
                .iter()
                .any(|error| error.contains("no enabled outputs"))
        );
    }

    #[test]
    fn planned_geometry_applies_each_override() {
        let outputs = layout(vec![("eDP-1", 0, 0, 1.0), ("DP-1", 1920, 0, 1.0)]);
        let wanted = BTreeMap::from([("eDP-1".to_owned(), (-1920, 0))]);
        let plan = plan_positions(&outputs, &wanted);
        let edp = plan
            .positions
            .iter()
            .find(|entry| entry.output == "eDP-1")
            .unwrap();
        let dp = plan
            .positions
            .iter()
            .find(|entry| entry.output == "DP-1")
            .unwrap();
        assert_eq!((edp.x, edp.y), (-1920, 0));
        assert_eq!((dp.x, dp.y), (1920, 0));
    }
}
