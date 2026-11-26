//! `loom-monitor`: output profiles, rules and safe apply/rollback for niri.
//!
//! Profiles are JSON snapshots of output settings. `apply` stores the current
//! state as `__previous` first, validates the resulting layout, sends ordered
//! niri IPC actions and rolls back on a partial failure. `--dry-run` prints the
//! full ordered action list plus validation errors without touching the
//! compositor.
//!
//! Profiles, rules and the watch lock live under
//! `$XDG_STATE_HOME/loom-monitor/`. See `README.md` for the schema and for
//! which fields are IPC-visible versus config-only.

pub mod arrange;
pub mod ipc;
pub mod mirror;
pub mod plan;
pub mod rules;
pub mod safe;
pub mod schema;
pub mod watch;

pub use arrange::{
    ArrangeReport, MAX_COORDINATE, OutputPosition, PositionPlan, arrange, current_positions,
    parse_plan, plan_positions,
};
pub use ipc::{
    ApplyReport, ApplyStatus, apply_actions, apply_named, apply_profile_object, query_outputs,
    restore, send_action,
};
pub use mirror::{MirrorChild, MirrorRelation, MirrorStatus, MirrorStep};
pub use plan::{
    ActionPhase, ConfigOnlyNote, Plan, PlannedAction, action_description, plan_profile,
    plan_snapshot, snapshot, validate_layout,
};
pub use rules::{
    Rule, RuleCandidate, RuleDecision, RuleStore, build_rules_profile, evaluate_rules,
};
pub use safe::{
    PendingApply, PendingConfirmation, cancel, clear_pending, confirm, expire_pending,
    load_pending, pending_path, watch_pending, write_pending,
};
pub use schema::{
    HdrMode, HdrProfile, OutputMatch, OutputProfile, Profile, SCHEMA_VERSION, Store, lock_path,
    rules_path, state_dir, state_path,
};
pub use watch::{
    OutputSummary, WatchCycle, WatchDecision, WatchLock, WatchOptions, WatchState, run_watch,
    topology_fingerprint, watch_once,
};
