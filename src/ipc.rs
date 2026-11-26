//! niri IPC transport, safe apply, rollback and restore.

use std::collections::BTreeMap;
use std::path::Path;

use niri_ipc::{Output, OutputAction};
use serde::Serialize;

use crate::mirror::{self, MirrorStep};
use crate::plan::{ConfigOnlyNote, Plan, PlannedAction, plan_profile, plan_snapshot, snapshot};
use crate::schema::{Profile, Store};

/// Result of an apply/restore attempt.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    /// Dry run; nothing was sent.
    Planned,
    /// Every planned action was accepted by niri.
    Applied,
    /// Validation refused the profile; nothing was sent.
    Refused,
    /// An action failed and the previous layout was restored.
    RolledBack,
    /// An action failed and restoring the previous layout also failed.
    RollbackFailed,
    /// Restore completed.
    Restored,
}

impl ApplyStatus {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Planned | Self::Applied | Self::Restored)
    }
}

/// Full report for apply, restore, rules apply and watch cycles.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    /// Profile name or label (`rules`, `watch`, `__previous`).
    pub profile: String,
    pub status: ApplyStatus,
    pub dry_run: bool,
    /// Profile key to connected output name.
    pub resolved: BTreeMap<String, String>,
    /// Planned actions in send order.
    pub actions: Vec<PlannedAction>,
    /// Actions niri accepted.
    pub applied: Vec<PlannedAction>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub config_only: Vec<ConfigOnlyNote>,
    /// `wl-mirror` children started, kept or stopped for this apply.
    pub mirrors: Vec<MirrorStep>,
    /// Whether the pre-apply state was written to `__previous`.
    pub previous_saved: bool,
    /// The action that failed, if any.
    pub failed_action: Option<PlannedAction>,
    /// Recovery actions sent after a failure.
    pub rollback_actions: Vec<PlannedAction>,
    pub rollback_errors: Vec<String>,
    /// Confirmation window for `apply --confirm-timeout`; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<crate::safe::PendingConfirmation>,
}

impl ApplyReport {
    /// Report for a plan that failed validation; nothing was sent.
    pub fn refused(
        profile: &str,
        dry_run: bool,
        errors: Vec<String>,
        actions: Vec<PlannedAction>,
    ) -> Self {
        Self {
            profile: profile.to_owned(),
            status: ApplyStatus::Refused,
            dry_run,
            resolved: BTreeMap::new(),
            actions,
            applied: Vec::new(),
            errors,
            warnings: Vec::new(),
            config_only: Vec::new(),
            mirrors: Vec::new(),
            previous_saved: false,
            failed_action: None,
            rollback_actions: Vec::new(),
            rollback_errors: Vec::new(),
            confirmation: None,
        }
    }

    fn from_plan(profile: &str, dry_run: bool, plan: Plan) -> Self {
        let status = if plan.ok() {
            if dry_run {
                ApplyStatus::Planned
            } else {
                ApplyStatus::Applied
            }
        } else {
            ApplyStatus::Refused
        };
        Self {
            profile: profile.to_owned(),
            status,
            dry_run,
            resolved: plan.resolved,
            actions: plan.actions,
            applied: Vec::new(),
            errors: plan.errors,
            warnings: plan.warnings,
            config_only: plan.config_only,
            mirrors: Vec::new(),
            previous_saved: false,
            failed_action: None,
            rollback_actions: Vec::new(),
            rollback_errors: Vec::new(),
            confirmation: None,
        }
    }
}

/// Sends one output action to niri.
pub fn send_action(socket: &Path, output: &str, action: OutputAction) -> Result<(), String> {
    use niri_ipc::socket::Socket;
    use niri_ipc::{Reply, Request, Response};

    let mut socket =
        Socket::connect_to(socket).map_err(|err| format!("cannot connect to niri IPC: {err}"))?;
    let reply: Reply = socket
        .send(Request::Output {
            output: output.to_owned(),
            action,
        })
        .map_err(|err| format!("niri output request: {err}"))?;
    match reply.map_err(|err| format!("niri returned an error: {err}"))? {
        Response::OutputConfigChanged(_) => Ok(()),
        other => Err(format!("unexpected niri response: {other:?}")),
    }
}

/// Queries the current outputs.
pub fn query_outputs(socket: &Path) -> Result<BTreeMap<String, Output>, String> {
    use niri_ipc::socket::Socket;
    use niri_ipc::{Reply, Request, Response};

    let mut socket =
        Socket::connect_to(socket).map_err(|err| format!("cannot connect to niri IPC: {err}"))?;
    let reply: Reply = socket
        .send(Request::Outputs)
        .map_err(|err| format!("niri outputs request: {err}"))?;
    match reply.map_err(|err| format!("niri returned an error: {err}"))? {
        Response::Outputs(outputs) => Ok(outputs.into_iter().collect()),
        other => Err(format!("unexpected niri response: {other:?}")),
    }
}

/// Applies a named profile, storing the current state under `__previous`
/// first.
pub fn apply_named(
    socket: &Path,
    store: &mut Store,
    name: &str,
    path: &Path,
    dry_run: bool,
) -> Result<ApplyReport, String> {
    if name == "__previous" {
        return restore(socket, store, path, dry_run);
    }
    let profile = store
        .get(name)
        .cloned()
        .ok_or_else(|| format!("unknown profile `{name}`"))?;
    apply_profile_object(socket, store, &profile, name, path, dry_run, None)
}

/// Applies a profile object. `forced_target` binds single-output profiles to a
/// specific connected output (used by rules).
#[allow(clippy::too_many_arguments)]
pub fn apply_profile_object(
    socket: &Path,
    store: &mut Store,
    profile: &Profile,
    label: &str,
    path: &Path,
    dry_run: bool,
    forced_target: Option<&str>,
) -> Result<ApplyReport, String> {
    let current = query_outputs(socket)?;
    let plan = plan_profile(profile, &current, forced_target);
    let desired_mirrors = mirror::desired_from(&plan.desired);
    let mut report = ApplyReport::from_plan(label, dry_run, plan);

    if !report.errors.is_empty() || dry_run {
        return Ok(report);
    }

    let previous = snapshot(&current);
    store.insert("__previous", previous.clone());
    store.version = crate::schema::SCHEMA_VERSION;
    store
        .save(path)
        .map_err(|err| format!("cannot save profiles: {err}"))?;
    report.previous_saved = true;

    match execute_actions(socket, &report.actions) {
        Ok(applied) => {
            report.applied = applied;
            report.status = ApplyStatus::Applied;
            // The mirror field is applied by the external wl-mirror backend,
            // so it is no longer a config-only note.
            report.config_only.retain(|note| note.field != "mirror");
            report.mirrors = mirror::sync(path, &desired_mirrors, &current);
        }
        Err((applied, failed, message)) => {
            report.applied = applied;
            report.errors.push(message);
            report.failed_action = Some(failed);

            let fallback = plan_snapshot(&previous);
            match execute_actions(socket, &fallback) {
                Ok(rollback_actions) => {
                    report.rollback_actions = rollback_actions;
                    report.status = ApplyStatus::RolledBack;
                }
                Err((_, failed_rollback, rollback_message)) => {
                    report.rollback_actions = fallback;
                    report.rollback_errors.push(rollback_message);
                    report.rollback_errors.push(format!(
                        "rollback stopped at `{}` {}",
                        failed_rollback.output,
                        failed_rollback.describe()
                    ));
                    report.status = ApplyStatus::RollbackFailed;
                }
            }
        }
    }

    Ok(report)
}

/// Applies an ordered action list with the same save/rollback contract as
/// profile apply. Used by the runtime arrangement command, which builds its
/// actions directly instead of going through a profile.
pub fn apply_actions(
    socket: &Path,
    store: &mut Store,
    path: &Path,
    label: &str,
    actions: &[PlannedAction],
    dry_run: bool,
) -> Result<ApplyReport, String> {
    let current = query_outputs(socket)?;
    let mut report = ApplyReport {
        profile: label.to_owned(),
        status: if dry_run {
            ApplyStatus::Planned
        } else {
            ApplyStatus::Applied
        },
        dry_run,
        resolved: BTreeMap::new(),
        actions: actions.to_vec(),
        applied: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        config_only: Vec::new(),
        mirrors: Vec::new(),
        previous_saved: false,
        failed_action: None,
        rollback_actions: Vec::new(),
        rollback_errors: Vec::new(),
        confirmation: None,
    };

    if dry_run {
        return Ok(report);
    }

    let previous = snapshot(&current);
    store.insert("__previous", previous.clone());
    store.version = crate::schema::SCHEMA_VERSION;
    store
        .save(path)
        .map_err(|err| format!("cannot save profiles: {err}"))?;
    report.previous_saved = true;

    match execute_actions(socket, actions) {
        Ok(applied) => {
            report.applied = applied;
            report.status = ApplyStatus::Applied;
        }
        Err((applied, failed, message)) => {
            report.applied = applied;
            report.errors.push(message);
            report.failed_action = Some(failed);

            let fallback = plan_snapshot(&previous);
            match execute_actions(socket, &fallback) {
                Ok(rollback_actions) => {
                    report.rollback_actions = rollback_actions;
                    report.status = ApplyStatus::RolledBack;
                }
                Err((_, failed_rollback, rollback_message)) => {
                    report.rollback_actions = fallback;
                    report.rollback_errors.push(rollback_message);
                    report.rollback_errors.push(format!(
                        "rollback stopped at `{}` {}",
                        failed_rollback.output,
                        failed_rollback.describe()
                    ));
                    report.status = ApplyStatus::RollbackFailed;
                }
            }
        }
    }

    Ok(report)
}

/// Restores the `__previous` profile without overwriting it.
///
/// `profiles` is the profile store path, used to find the mirror state file:
/// restoring stops the `wl-mirror` children started by the profile being
/// rolled back, because the `__previous` snapshot has no mirror relations.
pub fn restore(
    socket: &Path,
    store: &Store,
    profiles: &Path,
    dry_run: bool,
) -> Result<ApplyReport, String> {
    let previous = store
        .get("__previous")
        .cloned()
        .ok_or_else(|| "no previous profile; apply one first".to_owned())?;
    let current = query_outputs(socket)?;
    let desired_mirrors = mirror::desired_from(&previous.outputs);
    let actions = plan_snapshot(&previous);
    let mut report = ApplyReport {
        profile: "__previous".to_owned(),
        status: if dry_run {
            ApplyStatus::Planned
        } else {
            ApplyStatus::Restored
        },
        dry_run,
        resolved: previous
            .outputs
            .keys()
            .map(|name| (name.clone(), name.clone()))
            .collect(),
        actions: actions.clone(),
        applied: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        config_only: Vec::new(),
        mirrors: Vec::new(),
        previous_saved: false,
        failed_action: None,
        rollback_actions: Vec::new(),
        rollback_errors: Vec::new(),
        confirmation: None,
    };

    if dry_run {
        return Ok(report);
    }

    let fallback = snapshot(&current);
    match execute_actions(socket, &actions) {
        Ok(applied) => {
            report.applied = applied;
            report.status = ApplyStatus::Restored;
            report.mirrors = mirror::sync(profiles, &desired_mirrors, &current);
        }
        Err((applied, failed, message)) => {
            report.applied = applied;
            report.errors.push(message);
            report.failed_action = Some(failed);
            let fallback_actions = plan_snapshot(&fallback);
            match execute_actions(socket, &fallback_actions) {
                Ok(rollback) => {
                    report.rollback_actions = rollback;
                    report.status = ApplyStatus::RolledBack;
                }
                Err((_, failed_rollback, rollback_message)) => {
                    report.rollback_actions = fallback_actions;
                    report.rollback_errors.push(rollback_message);
                    report.rollback_errors.push(format!(
                        "rollback stopped at `{}` {}",
                        failed_rollback.output,
                        failed_rollback.describe()
                    ));
                    report.status = ApplyStatus::RollbackFailed;
                }
            }
        }
    }

    Ok(report)
}

fn execute_actions(
    socket: &Path,
    actions: &[PlannedAction],
) -> Result<Vec<PlannedAction>, (Vec<PlannedAction>, PlannedAction, String)> {
    let mut applied = Vec::new();
    for action in actions {
        match send_action(socket, &action.output, action.action.clone()) {
            Ok(()) => applied.push(action.clone()),
            Err(message) => {
                let message = format!(
                    "output `{}`: {} failed: {message}",
                    action.output,
                    action.describe()
                );
                return Err((applied, action.clone(), message));
            }
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::ActionPhase;
    use crate::schema::OutputProfile;

    #[test]
    fn status_ok_mapping() {
        assert!(ApplyStatus::Planned.is_ok());
        assert!(ApplyStatus::Applied.is_ok());
        assert!(ApplyStatus::Restored.is_ok());
        assert!(!ApplyStatus::Refused.is_ok());
        assert!(!ApplyStatus::RolledBack.is_ok());
        assert!(!ApplyStatus::RollbackFailed.is_ok());
    }

    #[test]
    fn refused_report_keeps_planned_actions_for_dry_run() {
        let mut profile = Profile::default();
        profile.outputs.insert(
            "eDP-1".to_owned(),
            OutputProfile {
                off: true,
                ..OutputProfile::default()
            },
        );
        let current: BTreeMap<String, Output> = BTreeMap::new();
        let plan = plan_profile(&profile, &current, None);
        let report = ApplyReport::from_plan("x", true, plan);
        assert_eq!(report.status, ApplyStatus::Refused);
        assert!(!report.errors.is_empty());
        assert_eq!(report.actions[0].phase, ActionPhase::Off);
    }
}
