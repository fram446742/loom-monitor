use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use loom_monitor::{
    ApplyReport, ApplyStatus, OutputMatch, Rule, RuleStore, SCHEMA_VERSION, Store, WatchOptions,
    apply_named, apply_profile_object, arrange, build_rules_profile, cancel, confirm,
    current_positions, evaluate_rules, parse_plan, pending_path, plan_profile, query_outputs,
    restore, run_watch, snapshot, state_path, watch_once, watch_pending, write_pending,
};
use serde_json::{Value, json};

const DEFAULT_DEBOUNCE_MS: u64 = 500;
const DEFAULT_POLL_MS: u64 = 1_000;
const DEFAULT_COOLDOWN_MS: u64 = 3_000;

fn usage() -> &'static str {
    "loom-monitor [options] <command>\n\n\
     Commands:\n\
       list                         List saved profiles\n\
       show <name>                  Print a profile\n\
       save <name> [output...]      Snapshot current outputs into <name>\n\
       apply <name> [--dry-run]     Apply <name> (stores __previous first)\n\
       restore [--dry-run]          Re-apply __previous\n\
       position                     Print current logical output geometry\n\
       position <output> <x> <y>    Move one output (stores __previous first)\n\
       arrange --plan JSON          Apply logical positions from a JSON plan\n\
       delete <name>                Delete a profile\n\
       migrate                      Rewrite all profiles in schema v2\n\
       rules list                   List rules\n\
       rules add <name> --profile P [--priority N] [match flags]\n\
       rules remove <name>          Delete a rule\n\
       rules explain [--output O]   Explain which rule wins and the planned actions\n\
       rules apply [--dry-run]      Apply the winning rule for each output\n\
       watch [--once]               Watch outputs, apply --profile or the rules\n\n\
     Options:\n\
       --socket PATH                niri IPC socket (default $NIRI_SOCKET)\n\
       --state PATH                 profile store (default $LOOM_MONITOR_STATE)\n\
       --rules-file PATH            rules store (sibling rules.json by default)\n\
       --plan JSON                  Position plan for `arrange` (`@file` or `-` for stdin)\n\
       --json                       Machine-readable JSON envelope output\n\
       --dry-run                    Plan without changing the compositor\n\
       --once                       Evaluate watch once and exit\n\
       --profile NAME               Profile for watch or rules add\n\
       --priority N                 Rule priority (higher wins)\n\
       --match-name NAME            Rule/profile output connector match\n\
       --make NAME --model NAME --serial NAME   Rule output matches\n\
       --output NAME                Filter rules explain\n\
       --debounce-ms N --poll-ms N --cooldown-ms N   Watch timings\n\n\
     Socket defaults to $NIRI_SOCKET; state to $XDG_STATE_HOME/loom-monitor/.\n\
     `mirror` uses wl-mirror from PATH; set LOOM_MONITOR_WL_MIRROR to override."
}

#[derive(Debug)]
struct Cli {
    socket: Option<PathBuf>,
    json: bool,
    dry_run: bool,
    once: bool,
    help: bool,
    state: PathBuf,
    rules: PathBuf,
    lock: PathBuf,
    profile: Option<String>,
    priority: i32,
    match_name: Option<String>,
    make: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    output: Option<String>,
    debounce: Duration,
    poll: Duration,
    cooldown: Duration,
    confirm_timeout: Option<Duration>,
    pending: PathBuf,
    watch_id: Option<String>,
    plan: Option<String>,
    positionals: Vec<String>,
}

impl Cli {
    fn parse() -> Result<Self, String> {
        let state = std::env::var_os("LOOM_MONITOR_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(state_path);
        let cli = Self {
            socket: std::env::var_os("NIRI_SOCKET").map(PathBuf::from),
            json: false,
            dry_run: false,
            once: false,
            help: false,
            rules: std::env::var_os("LOOM_MONITOR_RULES")
                .map(PathBuf::from)
                .unwrap_or_default(),
            lock: state.with_file_name("watch.lock"),
            state,
            profile: None,
            priority: 0,
            match_name: None,
            make: None,
            model: None,
            serial: None,
            output: None,
            debounce: Duration::from_millis(DEFAULT_DEBOUNCE_MS),
            poll: Duration::from_millis(DEFAULT_POLL_MS),
            cooldown: Duration::from_millis(DEFAULT_COOLDOWN_MS),
            confirm_timeout: None,
            pending: PathBuf::new(),
            watch_id: None,
            plan: None,
            positionals: Vec::new(),
        };
        cli.parse_args()
    }

    fn parse_args(mut self) -> Result<Self, String> {
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--socket" => self.socket = Some(PathBuf::from(next_value(&mut iter, "--socket")?)),
                "--state" => self.state = PathBuf::from(next_value(&mut iter, "--state")?),
                "--rules-file" => {
                    self.rules = PathBuf::from(next_value(&mut iter, "--rules-file")?)
                }
                "--json" => self.json = true,
                "--dry-run" => self.dry_run = true,
                "--once" => self.once = true,
                "--help" | "-h" => self.help = true,
                "--profile" => self.profile = Some(next_value(&mut iter, "--profile")?),
                "--priority" => {
                    self.priority = next_value(&mut iter, "--priority")?
                        .parse()
                        .map_err(|_| "--priority needs an integer".to_owned())?;
                }
                "--match-name" => self.match_name = Some(next_value(&mut iter, "--match-name")?),
                "--make" => self.make = Some(next_value(&mut iter, "--make")?),
                "--model" => self.model = Some(next_value(&mut iter, "--model")?),
                "--serial" => self.serial = Some(next_value(&mut iter, "--serial")?),
                "--output" => self.output = Some(next_value(&mut iter, "--output")?),
                "--debounce-ms" => {
                    self.debounce = Duration::from_millis(parse_u64(&mut iter, "--debounce-ms")?);
                }
                "--poll-ms" => {
                    self.poll = Duration::from_millis(parse_u64(&mut iter, "--poll-ms")?);
                }
                "--cooldown-ms" => {
                    self.cooldown = Duration::from_millis(parse_u64(&mut iter, "--cooldown-ms")?);
                }
                "--confirm-timeout" => {
                    let secs = parse_u64(&mut iter, "--confirm-timeout")?;
                    if secs == 0 {
                        return Err(
                            "--confirm-timeout needs a positive number of seconds".to_owned()
                        );
                    }
                    self.confirm_timeout = Some(Duration::from_secs(secs));
                }
                "--pending" => self.pending = PathBuf::from(next_value(&mut iter, "--pending")?),
                "--watch-id" => self.watch_id = Some(next_value(&mut iter, "--watch-id")?),
                "--plan" => self.plan = Some(next_value(&mut iter, "--plan")?),
                other if other.starts_with("--") => {
                    return Err(format!("unknown option `{other}`\n\n{}", usage()));
                }
                _ => self.positionals.push(arg),
            }
        }
        if self.rules.as_os_str().is_empty() {
            self.rules = self.state.with_file_name("rules.json");
        }
        if self.pending.as_os_str().is_empty() {
            self.pending = pending_path(&self.state);
        }
        self.lock = self.state.with_file_name("watch.lock");
        Ok(self)
    }

    fn socket(&self) -> Result<&std::path::Path, String> {
        self.socket
            .as_deref()
            .ok_or_else(|| "NIRI_SOCKET is not set; run inside niri or pass --socket".to_owned())
    }

    fn match_(&self) -> OutputMatch {
        OutputMatch {
            name: self.match_name.clone(),
            make: self.make.clone(),
            model: self.model.clone(),
            serial: self.serial.clone(),
        }
    }
}

fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_u64(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    next_value(iter, flag)?
        .parse()
        .map_err(|_| format!("{flag} needs an integer"))
}

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("loom-monitor {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let mut json = std::env::args().any(|arg| arg == "--json");
    let mut command = std::env::args()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .unwrap_or_default();

    let mut cli = match Cli::parse() {
        Ok(cli) => cli,
        Err(err) => {
            if json {
                println!(
                    "{}",
                    json!({ "command": command, "ok": false, "error": err })
                );
            } else {
                eprintln!("loom-monitor: {err}");
            }
            return ExitCode::FAILURE;
        }
    };
    json = cli.json;
    if let Some(first) = cli.positionals.first() {
        command = first.clone();
    }

    if cli.help || command == "help" {
        print!("{}", usage());
        return ExitCode::SUCCESS;
    }

    match run(&mut cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            if json {
                println!(
                    "{}",
                    json!({ "command": command, "ok": false, "error": err })
                );
            } else {
                eprintln!("loom-monitor: {err}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &mut Cli) -> Result<bool, String> {
    let Some(command) = cli.positionals.first().cloned() else {
        return Err(format!("missing command\n\n{}", usage()));
    };

    let mut store = Store::load(&cli.state).map_err(|err| format!("cannot load state: {err}"))?;

    match command.as_str() {
        "list" => {
            let names: Vec<_> = store.profiles.keys().cloned().collect();
            if cli.json {
                emit(
                    cli,
                    "list",
                    true,
                    json!({ "version": store.version, "profiles": names }),
                );
            } else {
                print_json(&names);
            }
            Ok(true)
        }
        "show" => {
            let name = positional(cli, 1, "show needs a profile name")?;
            let profile = store
                .get(name)
                .ok_or_else(|| format!("unknown profile `{name}`"))?;
            if cli.json {
                emit(
                    cli,
                    "show",
                    true,
                    json!({ "name": name, "profile": profile }),
                );
            } else {
                print_json(profile);
            }
            Ok(true)
        }
        "delete" => {
            let name = positional(cli, 1, "delete needs a profile name")?;
            if !store.remove(name) {
                return Err(format!("unknown profile `{name}`"));
            }
            store.version = SCHEMA_VERSION;
            store
                .save(&cli.state)
                .map_err(|err| format!("cannot save state: {err}"))?;
            emit(cli, "delete", true, json!({ "deleted": name }));
            Ok(true)
        }
        "save" => {
            let name = positional(cli, 1, "save needs a profile name")?.to_owned();
            reject_reserved(&name)?;
            let outputs = query_outputs(cli.socket()?)?;
            let mut profile = snapshot(&outputs);
            let only: Vec<String> = cli.positionals.iter().skip(2).cloned().collect();
            if !only.is_empty() {
                profile
                    .outputs
                    .retain(|output, _| only.iter().any(|name| name == output));
            }
            if profile.outputs.is_empty() {
                return Err("no matching outputs to save".to_owned());
            }
            store.insert(name.clone(), profile.clone());
            store.version = SCHEMA_VERSION;
            store
                .save(&cli.state)
                .map_err(|err| format!("cannot save state: {err}"))?;
            if cli.json {
                emit(
                    cli,
                    "save",
                    true,
                    json!({ "name": name, "profile": profile }),
                );
            } else {
                print_json(&profile);
            }
            Ok(true)
        }
        "apply" => {
            let name = positional(cli, 1, "apply needs a profile name")?.to_owned();
            let mut report =
                apply_named(cli.socket()?, &mut store, &name, &cli.state, cli.dry_run)?;
            if report.status == ApplyStatus::Applied
                && let Some(timeout) = cli.confirm_timeout
            {
                let confirmation = write_pending(&cli.state, cli.socket()?, &name, timeout)?;
                match spawn_confirm_watcher(cli, &confirmation) {
                    Ok(()) => report.confirmation = Some(confirmation),
                    Err(err) => {
                        let _ = loom_monitor::clear_pending(&confirmation.pending_path);
                        report
                            .warnings
                            .push(format!("confirmation watcher did not start: {err}"));
                    }
                }
            }
            Ok(emit_report(cli, "apply", &report))
        }
        "confirm" => {
            let removed = confirm(&cli.pending)?;
            emit(
                cli,
                "confirm",
                true,
                json!({ "confirmed": removed, "pending": cli.pending }),
            );
            Ok(true)
        }
        "cancel" => {
            let restored = cancel(&cli.pending)?;
            emit(
                cli,
                "cancel",
                true,
                json!({ "cancelled": restored, "pending": cli.pending }),
            );
            Ok(true)
        }
        "__confirm-watch" => {
            let id = cli
                .watch_id
                .clone()
                .ok_or_else(|| "__confirm-watch needs --watch-id".to_owned())?;
            match watch_pending(&cli.pending, &id, cli.poll)? {
                Some(report) => {
                    emit_report(cli, "confirm-watch", &report);
                }
                None => emit(
                    cli,
                    "confirm-watch",
                    true,
                    json!({ "confirmed": true, "pending": cli.pending }),
                ),
            }
            Ok(true)
        }
        "restore" => {
            let report = restore(cli.socket()?, &store, &cli.state, cli.dry_run)?;
            Ok(emit_report(cli, "restore", &report))
        }
        "position" => run_position(cli, &mut store),
        "arrange" => run_arrange(cli, &mut store),
        "migrate" => {
            let migrated = store.migrate();
            store
                .save(&cli.state)
                .map_err(|err| format!("cannot save state: {err}"))?;
            let mut rules =
                RuleStore::load(&cli.rules).map_err(|err| format!("cannot load rules: {err}"))?;
            rules.version = SCHEMA_VERSION;
            rules
                .save(&cli.rules)
                .map_err(|err| format!("cannot save rules: {err}"))?;
            emit(
                cli,
                "migrate",
                true,
                json!({ "migrated_profiles": migrated, "version": SCHEMA_VERSION }),
            );
            Ok(true)
        }
        "rules" => run_rules(cli, &mut store),
        "watch" => run_watch_command(cli, &mut store),
        other => Err(format!("unknown command `{other}`\n\n{}", usage())),
    }
}

fn run_position(cli: &Cli, store: &mut Store) -> Result<bool, String> {
    let rest: Vec<String> = cli.positionals.iter().skip(1).cloned().collect();
    if rest.is_empty() {
        let outputs = query_outputs(cli.socket()?)?;
        emit(
            cli,
            "position",
            true,
            json!({ "positions": current_positions(&outputs) }),
        );
        return Ok(true);
    }
    if rest.len() != 3 {
        return Err("position needs `<output> <x> <y>` or no arguments to read".to_owned());
    }
    let output = rest[0].clone();
    let x: i32 = rest[1]
        .parse()
        .map_err(|_| format!("position x `{}` is not an integer", rest[1]))?;
    let y: i32 = rest[2]
        .parse()
        .map_err(|_| format!("position y `{}` is not an integer", rest[2]))?;
    let wanted = std::collections::BTreeMap::from([(output, (x, y))]);
    let report = arrange(cli.socket()?, store, &cli.state, &wanted, cli.dry_run)?;
    let ok = report.report.status.is_ok();
    if cli.json {
        emit(cli, "position", ok, serde_json::to_value(&report).unwrap());
    } else {
        print_json(&report);
    }
    Ok(ok)
}

fn run_arrange(cli: &Cli, store: &mut Store) -> Result<bool, String> {
    let plan_text = cli
        .plan
        .as_deref()
        .ok_or_else(|| "arrange needs --plan JSON".to_owned())?;
    let plan_text = load_plan(plan_text)?;
    let wanted = parse_plan(&plan_text)?;
    let report = arrange(cli.socket()?, store, &cli.state, &wanted, cli.dry_run)?;
    let ok = report.report.status.is_ok();
    if cli.json {
        emit(cli, "arrange", ok, serde_json::to_value(&report).unwrap());
    } else {
        print_json(&report);
    }
    Ok(ok)
}

fn load_plan(value: &str) -> Result<String, String> {
    if value == "-" {
        use std::io::Read;
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|err| format!("cannot read the plan from stdin: {err}"))?;
        return Ok(text);
    }
    if let Some(path) = value.strip_prefix('@') {
        return std::fs::read_to_string(path)
            .map_err(|err| format!("cannot read plan file `{path}`: {err}"));
    }
    Ok(value.to_owned())
}

fn spawn_confirm_watcher(
    cli: &Cli,
    confirmation: &loom_monitor::PendingConfirmation,
) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("cannot find executable: {err}"))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("__confirm-watch")
        .arg("--pending")
        .arg(&confirmation.pending_path)
        .arg("--state")
        .arg(&cli.state)
        .arg("--watch-id")
        .arg(&confirmation.id)
        .arg("--poll-ms")
        .arg(cli.poll.as_millis().to_string());
    if let Some(socket) = &cli.socket {
        command.arg("--socket").arg(socket);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|err| format!("cannot spawn confirmation watcher: {err}"))
}

fn run_rules(cli: &Cli, store: &mut Store) -> Result<bool, String> {
    let mut rules =
        RuleStore::load(&cli.rules).map_err(|err| format!("cannot load rules: {err}"))?;
    let sub = cli.positionals.get(1).map(String::as_str).unwrap_or("list");

    match sub {
        "list" => {
            if cli.json {
                emit(
                    cli,
                    "rules list",
                    true,
                    json!({ "version": rules.version, "rules": rules.rules }),
                );
            } else {
                print_json(&rules.rules);
            }
            Ok(true)
        }
        "add" => {
            let name = positional(cli, 2, "rules add needs a rule name")?.to_owned();
            let profile = cli
                .profile
                .clone()
                .ok_or_else(|| "rules add needs --profile".to_owned())?;
            if store.get(&profile).is_none() {
                return Err(format!("unknown profile `{profile}`"));
            }
            if rules.get(&name).is_some() {
                return Err(format!("rule `{name}` already exists"));
            }
            let rule = Rule {
                name: name.clone(),
                profile,
                priority: cli.priority,
                match_: cli.match_(),
            };
            let data = json!({ "rule": rule });
            rules.insert(rule);
            rules.version = SCHEMA_VERSION;
            rules
                .save(&cli.rules)
                .map_err(|err| format!("cannot save rules: {err}"))?;
            emit(cli, "rules add", true, data);
            Ok(true)
        }
        "remove" => {
            let name = positional(cli, 2, "rules remove needs a rule name")?;
            if !rules.remove(name) {
                return Err(format!("unknown rule `{name}`"));
            }
            rules.version = SCHEMA_VERSION;
            rules
                .save(&cli.rules)
                .map_err(|err| format!("cannot save rules: {err}"))?;
            emit(cli, "rules remove", true, json!({ "deleted": name }));
            Ok(true)
        }
        "explain" => {
            let outputs = query_outputs(cli.socket()?)?;
            let decisions = evaluate_rules(&rules.rules, &outputs);
            let (profile, _, warnings) = build_rules_profile(&rules.rules, &outputs, store);
            let plan = plan_profile(&profile, &outputs, None);
            let decisions: Vec<_> = decisions
                .into_iter()
                .filter(|decision| {
                    cli.output
                        .as_ref()
                        .is_none_or(|output| &decision.output == output)
                })
                .collect();
            let ok = if profile.outputs.is_empty() {
                warnings.is_empty()
            } else {
                plan.ok()
            };
            emit(
                cli,
                "rules explain",
                ok,
                json!({
                    "decisions": decisions,
                    "warnings": warnings,
                    "profile": profile,
                    "plan": plan,
                }),
            );
            Ok(ok)
        }
        "apply" => {
            let outputs = query_outputs(cli.socket()?)?;
            let (profile, decisions, warnings) = build_rules_profile(&rules.rules, &outputs, store);
            if profile.outputs.is_empty() {
                let ok = warnings.is_empty();
                emit(
                    cli,
                    "rules apply",
                    ok,
                    json!({
                        "decision": "no_matching_rules",
                        "report": Value::Null,
                        "decisions": decisions,
                        "warnings": warnings,
                    }),
                );
                return Ok(ok);
            }
            let report = apply_profile_object(
                cli.socket()?,
                store,
                &profile,
                "rules",
                &cli.state,
                cli.dry_run,
                None,
            )?;
            let ok = report.status.is_ok() && warnings.is_empty();
            if cli.json {
                emit(
                    cli,
                    "rules apply",
                    ok,
                    json!({
                        "report": report,
                        "decisions": decisions,
                        "warnings": warnings,
                    }),
                );
            } else {
                print_report_default(&report);
                for warning in &warnings {
                    eprintln!("warning: {warning}");
                }
            }
            Ok(ok)
        }
        other => Err(format!("unknown rules command `{other}`\n\n{}", usage())),
    }
}

fn run_watch_command(cli: &Cli, store: &mut Store) -> Result<bool, String> {
    let rules = if cli.profile.is_some() {
        RuleStore::default()
    } else {
        RuleStore::load(&cli.rules).map_err(|err| format!("cannot load rules: {err}"))?
    };
    let options = WatchOptions {
        profile: cli.profile.clone(),
        debounce: cli.debounce,
        poll: cli.poll,
        cooldown: cli.cooldown,
        once: cli.once,
        dry_run: cli.dry_run,
    };
    let socket = cli.socket()?;

    if cli.once {
        let cycle = watch_once(socket, store, &rules, &cli.state, &options)?;
        emit(cli, "watch", true, serde_json::to_value(&cycle).unwrap());
        return Ok(cycle.reports.iter().all(|report| report.status.is_ok()));
    }

    run_watch(
        socket,
        store,
        &rules,
        &cli.state,
        &options,
        &cli.lock,
        |cycle| {
            if cli.json {
                emit(cli, "watch", true, serde_json::to_value(cycle).unwrap());
            } else {
                println!(
                    "[watch] decision={} changed={} outputs={}",
                    cycle.decision,
                    cycle.changed,
                    cycle.outputs.len()
                );
                for report in &cycle.reports {
                    print_report_default(report);
                }
            }
        },
    )?;
    Ok(true)
}

fn positional<'a>(cli: &'a Cli, index: usize, message: &str) -> Result<&'a str, String> {
    cli.positionals
        .get(index)
        .map(String::as_str)
        .ok_or_else(|| message.to_owned())
}

fn reject_reserved(name: &str) -> Result<(), String> {
    if name.starts_with("__") {
        Err(format!(
            "profile name `{name}` is reserved for loom-monitor state"
        ))
    } else {
        Ok(())
    }
}

fn print_json(value: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

fn emit(cli: &Cli, command: &str, ok: bool, data: Value) {
    if cli.json {
        println!("{}", json!({ "command": command, "ok": ok, "data": data }));
    } else {
        print_json(&data);
    }
}

fn emit_report(cli: &Cli, command: &str, report: &ApplyReport) -> bool {
    let ok = report.status.is_ok();
    if cli.json {
        emit(cli, command, ok, serde_json::to_value(report).unwrap());
    } else {
        print_report_default(report);
    }
    ok
}

fn print_report_default(report: &ApplyReport) {
    let actions: Vec<Value> = report
        .actions
        .iter()
        .map(|action| json!([action.output, action.action]))
        .collect();
    print_json(&actions);
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    for note in &report.config_only {
        eprintln!(
            "config-only: {} `{}`: {}",
            note.output, note.field, note.detail
        );
    }
    for step in &report.mirrors {
        eprintln!("{}", step.describe());
    }
    for error in &report.errors {
        eprintln!("error: {error}");
    }
    if report.status == ApplyStatus::RolledBack {
        eprintln!(
            "rolled back: {} action(s) sent to restore the previous layout",
            report.rollback_actions.len()
        );
    }
    for error in &report.rollback_errors {
        eprintln!("rollback error: {error}");
    }
    match report.status {
        ApplyStatus::Refused => eprintln!("refused: nothing was sent to niri"),
        ApplyStatus::RolledBack | ApplyStatus::RollbackFailed => {
            eprintln!("apply failed; previous layout restore was attempted")
        }
        ApplyStatus::Planned => eprintln!("dry run: {} action(s) planned", report.actions.len()),
        ApplyStatus::Applied => eprintln!("applied {} action(s)", report.applied.len()),
        ApplyStatus::Restored => eprintln!("restored {} action(s)", report.applied.len()),
    }
}
