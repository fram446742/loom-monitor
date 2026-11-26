# loom-monitor

> Part of [Loom](https://github.com/fram446742/loom).

Output profiles, rules and watch mode for niri, with validation, safe apply and
rollback. It is its own Cargo workspace with its own lockfile and build.

`loom-monitor` talks to niri through the IPC socket only. Fields that niri
does not expose over IPC (HDR, startup focus, isolation) are stored, validated
and reported as **config-only**; a profile `mirror` is applied by the external
`wl-mirror` client. See
[IPC-visible vs config-only fields](#ipc-visible-vs-config-only-fields).

## Features

- Snapshot the current output settings into a named profile: on/off, mode,
  scale, transform, position, VRR/adaptive sync and max BPC.
- Schema v2 adds per-output `max_bpc`, `hdr`, `focus_at_startup`, `isolated` and
  `mirror`, plus a profile-level `match` so one profile can follow a monitor by
  make/model/serial/name.
- Apply through niri's IPC (`Request::Output`) as an ordered action list:
  on → mode → scale → transform → position → VRR → max BPC → off.
- Apply `mirror` relations with the external `wl-mirror` client
  (`wl-mirror <source> --fullscreen-output <target>`). Children are tracked in
  `mirrors.json` so a later apply, restore or watch cycle stops exactly the
  processes this tool started; a missing `wl-mirror` is reported as a failed
  mirror step and the rest of the apply still runs.
- Validate the resulting layout first: refuse a layout with no enabled output,
  overlapping positions, a mirror of a missing/disabled output, mirror + isolated,
  duplicate matches, or invalid scale/mode.
- `--dry-run` prints the complete ordered action list and every validation
  error without touching the compositor.
- The pre-apply layout is stored as `__previous`; `restore` re-applies it and
  does not overwrite `__previous`. A failure in the middle of an apply sends
  compensating actions to restore `__previous` and reports exactly what
  happened.
- Rules (match + profile + priority) with deterministic explain output.
- Runtime `position` / `arrange` commands validate and apply logical output
  positions through niri IPC, with `--dry-run`, `__previous` and rollback.
- Watch mode with debounce, an advisory process lock, a post-apply cooldown and
  a post-apply fingerprint so its own changes cannot loop. `--once` evaluates
  one cycle for tests/adapters.
- `--json` on every command emits one machine-readable envelope.
- Profiles written before schema v2 still load; `migrate` rewrites them.

## Usage

```sh
export NIRI_SOCKET=...            # normally set inside niri

loom-monitor save desk          # all outputs
loom-monitor save desk eDP-1    # just one output
loom-monitor apply desk --dry-run
loom-monitor apply desk
LOOM_MONITOR_WL_MIRROR=/usr/local/bin/wl-mirror loom-monitor apply desk
loom-monitor apply desk --confirm-timeout 20   # roll back unless `confirm`
loom-monitor confirm            # keep the profile applied
loom-monitor cancel             # restore __previous now
loom-monitor restore            # undo the last apply (keeps __previous)
loom-monitor position           # current logical positions and sizes
loom-monitor position DP-1 0 0 # move one output (stores __previous)
loom-monitor arrange --plan '{"eDP-1":[0,0],"DP-1":[1920,0]}'
loom-monitor arrange --plan @plan.json --dry-run
loom-monitor list
loom-monitor show desk
loom-monitor delete desk
loom-monitor migrate            # rewrite old profiles as schema v2
```

## Runtime arrangement

`position` and `arrange` change logical output positions without touching
profiles:

```sh
loom-monitor position
loom-monitor position --json
loom-monitor position DP-1 1920 0
loom-monitor arrange --plan '{"eDP-1":[0,0],"DP-1":[1920,0]}'
loom-monitor arrange --plan - < plan.json          # stdin
loom-monitor arrange --plan @plan.json --dry-run   # preview only
```

A plan is an object of `output -> [x, y]` (or `{x, y}`, or a wrapped
`{"outputs": ...}`). Every referenced output must be connected and enabled, at
least one output must stay enabled, coordinates must be within ±1,000,000, and
the resulting logical rectangles must not overlap (touching edges are fine).
Validation runs before any IPC request, so a refused plan sends nothing.

Apply uses the same safety contract as profiles: the pre-change layout is saved
as `__previous`, actions are sent as ordered `OutputAction::Position` requests,
and a mid-apply failure triggers compensating actions back to `__previous`.
`--dry-run` prints the planned actions and geometry without connecting. The
JSON report contains `status`, `actions`, `positions` (effective logical
geometry), `errors`, `previous_saved` and the rollback fields.

Rules:

```sh
loom-monitor rules add dock --profile desk --priority 10 \
  --make "Dell Inc." --model U2720Q --serial ABC123
loom-monitor rules add fallback --profile laptop --priority 0 --match-name eDP-1
loom-monitor rules list
loom-monitor rules explain --output DP-1
loom-monitor rules apply --dry-run
loom-monitor rules apply
```

Watch:

```sh
loom-monitor watch --profile desk          # apply this profile on changes
loom-monitor watch                         # evaluate rules on changes
loom-monitor watch --once --json           # one evaluation, for adapters/tests
loom-monitor watch --debounce-ms 500 --poll-ms 1000 --cooldown-ms 3000
```

Global options: `--socket PATH`, `--state PATH`, `--rules-file PATH`, `--json`,
`--dry-run`, `--help`. `--socket` overrides `$NIRI_SOCKET`; `--state` overrides
`$LOOM_MONITOR_STATE`; `--rules-file` overrides `$LOOM_MONITOR_RULES`.
Profiles and rules default to `$XDG_STATE_HOME/loom-monitor/` (see
[State files](#state-files)).

## Profile schema (v2)

```json
{
  "version": 2,
  "match": { "make": "Dell Inc.", "model": "U2720Q", "serial": "ABC123" },
  "outputs": {
    "monitor": {
      "off": false,
      "scale": 1.5,
      "mode": [3840, 2160, 60000],
      "transform": "Normal",
      "position": [0, 0],
      "vrr": true,
      "vrr_on_demand": false,
      "max_bpc": "10",
      "hdr": { "mode": "auto", "reference_luminance": 203 },
      "focus_at_startup": true,
      "isolated": false,
      "mirror": null
    }
  }
}
```

- `version` is `2` for newly saved profiles. A missing version means v1; v1
  JSON is accepted unchanged.
- `outputs` keys are connector names for name-keyed profiles. With `match`, the
  profile must contain exactly one output entry and that entry is applied to
  the matching connected output.
- `mode` is `[width, height, refresh-millihertz]` (60000 = 60 Hz).
- `vrr` is variable refresh rate / adaptive sync. `adaptive_sync` and
  `variable_refresh_rate` are accepted as input aliases. `vrr_on_demand`
  selects niri's on-demand VRR mode.
- `max_bpc` is one of 6, 8, 10, 12, 14, 16. niri serialises it as a string
  (`"10"`); a plain number (`10`) is also accepted when reading.
- `transform` is one of `Normal`, `90`, `180`, `270`, `Flipped`, `Flipped90`,
  `Flipped180`, `Flipped270`.
- `hdr.mode` is `auto` or `on`; `reference_luminance` is in cd/m².
- `mirror` names another output (a profile key or connector) whose content is
  shown on this output. It is validated as a relationship and applied with the
  external `wl-mirror` client (see [Mirroring](#mirroring)).

### Matching

`match` is a case-insensitive, `*`-wildcard predicate. Present fields must
match; absent fields are wildcards. An empty `match` matches every output, so
it works as a fallback rule predicate but is ambiguous for a profile unless
combined with a name.

```json
"match": { "model": "U2720*", "serial": "ABC*" }
```

`name` matches the connector (`eDP-1`). If more than one connected output
matches a profile's `match`, the first in sorted name order is used and the
apply report contains a warning; add `name` or `serial` to disambiguate.

### IPC-visible vs config-only fields

| Field | Applied over IPC? | Notes |
| --- | --- | --- |
| `off` | yes | `OutputAction::Off` / `On` |
| `mode` | yes | `OutputAction::Mode` |
| `scale` | yes | `OutputAction::Scale` |
| `transform` | yes | `OutputAction::Transform` |
| `position` | yes | `OutputAction::Position` |
| `vrr`, `vrr_on_demand` | yes | `OutputAction::Vrr` |
| `max_bpc` | yes | `OutputAction::MaxBpc` |
| `hdr` | **no** | config file only |
| `focus_at_startup` | **no** | startup behaviour, config only |
| `isolated` | **no** | config file only |
| `mirror` | **no** (niri IPC) | applied through `wl-mirror`, see [Mirroring](#mirroring) |

IPC output changes are temporary: niri does not write them into the config.
`save` can only capture IPC-visible fields, so HDR, startup focus, isolation
and mirror have to be added to a profile by hand. Apply sends the IPC-visible
subset; every config-only field produces a `config_only` note in the report
(and a `config-only:` line on stderr in the default output). To make those
fields persistent, add or edit the niri config:

```kdl
output "DP-1" {
    hdr mode="on" {
        reference-luminance 203
    }
    focus-at-startup
    isolated
}
```

### Mirroring

niri in this tree has no output-mirror primitive, so `loom-monitor` uses the
external [`wl-mirror`](https://github.com/Ferdi265/wl-mirror) client. For a
profile entry on `eDP-1` with `"mirror": "DP-1"`, apply runs:

```sh
wl-mirror DP-1 --fullscreen-output eDP-1
```

The `wl-mirror` binary must be on `PATH` (or set `LOOM_MONITOR_WL_MIRROR` to
its path). Children are tracked in `mirrors.json` next to the profile store
with their output, source, target and PID:

- Re-applying an unchanged relation keeps the running child.
- Changing the source or removing the field stops the old child; the new
  relation (if any) is spawned.
- `restore`, `cancel` and an expired `--confirm-timeout` rollback stop the
  children started by the applied profile, because `__previous` snapshots do
  not record mirror relations.
- `watch` reaps children whose process exited or whose output/source is no
  longer connected.
- A missing binary or a failed spawn produces a `failed` field in the report's
  `mirrors` array with the reason, leaves no PID in the state file, and does
  not stop the rest of the apply.
- `--dry-run` spawns nothing and keeps the `config_only` note for `mirror`.

Only PIDs recorded in `mirrors.json` whose `/proc/<pid>/cmdline` still carries
both output names are signalled, so a recycled PID is left alone.

A profile that mirrors an isolated output is refused; a mirror of a
disconnected output is reported as a failed mirror step.

## Safe apply with a confirmation window

A wrong mode or transform can leave the screen unreadable, so `apply` can run
with a confirmation window:

```sh
loom-monitor apply laptop --confirm-timeout 30
# the screen changes; if it is still usable:
loom-monitor confirm
# if you cannot see or the layout is wrong (run over SSH or a TTY):
loom-monitor cancel
```

`--confirm-timeout` stores `__previous` as usual, writes
`$XDG_STATE_HOME/loom-monitor/pending.json` and starts a detached
`loom-monitor __confirm-watch` process. The watcher restores `__previous`
when the deadline passes unless `confirm` removed the record. `cancel` restores
immediately. Each record carries a unique id, so a newer apply supersedes an
older watcher instead of racing it; a failed restore is retried by the next
manual `restore`. The JSON apply report includes a `confirmation` object with
the id, pending path, deadline and timeout.

The watcher is a normal child process, so it does not survive a reboot; the
pending file is left in place and `loom-monitor restore` still works.

## Safety

Before sending anything, `loom-monitor` overlays the profile on the current
layout and validates:

1. **At least one output is enabled.** Disabling every output is refused.
2. **No overlapping positions.** Enabled, non-mirrored outputs with a known
   mode and position are compared as logical rectangles (mode ÷ scale, with
   90°/270° transforms swapping width/height). Touching edges are allowed.
3. **Mirrors are consistent.** A mirror target must exist, must not be the
   output itself and must not be disabled.
4. **Mirror + isolated is refused.** If either side of a mirror relationship is
   `isolated`, the profile is refused.
5. **Matches resolve to one output.** A profile matcher with no match, or two
   entries resolving to the same connector, is refused.
6. **Scale and mode are sane.** Non-finite/non-positive scale and zero-sized
   modes are refused.

`--dry-run` still runs all validation. It prints the ordered action list and
reports validation errors with a failing exit status, so CI and adapters can
check a profile without a display.

On a real apply the previous state is saved to `__previous` first, then actions
are sent in order. Actions that disable outputs are last, so the new output is
enabled and positioned before an old one is turned off. If an action fails:

- the failure, the output and the action are reported;
- compensating actions from the saved snapshot are sent immediately;
- the status is `rolled_back` when recovery succeeded, `rollback_failed` when it
  did not, with `rollback_errors` describing the second failure;
- `__previous` remains on disk for a manual `restore`.

This is compensating rollback, not an atomic transaction: niri applies each
output action separately, and there is a short window where the layout is
between states.

`restore` re-applies `__previous` directly (no conflict validation, because it
is a recovery path) and does not overwrite `__previous`. It also attempts a
compensating rollback to the pre-restore layout if it fails halfway.

## Rules

Rules are stored in `rules.json` under the state directory. A rule has a name,
a profile, a priority and a match. For each connected output the matching rule
with the highest priority wins; equal priorities are broken by rule name, which
is reported as a tie in `rules explain`.

```json
{
  "version": 2,
  "rules": [
    {
      "name": "dock",
      "profile": "desk",
      "priority": 10,
      "match": { "make": "Dell Inc.", "serial": "ABC123" }
    },
    {
      "name": "fallback",
      "profile": "laptop",
      "priority": 0,
      "match": {}
    }
  ]
}
```

`rules explain` prints, per output, the winning rule/profile, the full list of
considered rules (matched or not) and a reason. It also plans the merged
profile so `--dry-run` shows the resulting action list. `rules apply` merges all
winning rules into one profile and applies it once, which gives one
`__previous` snapshot and one rollback path for the whole change.

A rule's profile is bound to the matched output when the profile has one
output entry and no profile-level `match`. This lets a single profile (saved
from one dock) follow an identical monitor that reconnects under a different
connector name.

## Watch

`watch` observes output topology and applies either `--profile NAME` or the
winning rules. It uses:

- **polling** of `Request::Outputs` (default every 1000 ms), plus an event-stream
  connection used only as a wake-up hint. niri's `Event` enum in this tree has
  no output-added/removed/mode-change event, so watch does not pretend to
  receive one; it detects changes from the output snapshot. This is documented
  upstream limitation, not a hidden fallback.
- **debounce** (default 500 ms): a fingerprint must stay unchanged for the
  debounce interval before applying, so a dock that enumerates several outputs
  in quick succession triggers one apply.
- **post-apply baseline and cooldown** (default 3000 ms): after applying, watch
  re-reads outputs, stores the *post-apply* fingerprint and ignores it. A
  topology change that is still settling waits for the cooldown. This is the
  loop lock: watch never re-applies a profile because of its own actions.
- **process lock**: `$XDG_STATE_HOME/loom-monitor/watch.lock` contains the
  PID. A second `watch` refuses to start; a lock whose PID no longer exists is
  removed automatically.
- **`--once`**: evaluate the current outputs immediately (no debounce wait, no
  event thread, no process lock) and exit. This is the supported integration
  point for tests and adapters.

If applying is refused, watch records the post-apply fingerprint anyway and
waits for the next topology change instead of spinning on a bad profile.

## JSON output

Every command accepts `--json` and prints one envelope:

```json
{
  "command": "apply",
  "ok": true,
  "data": {
    "profile": "desk",
    "status": "applied",
    "dry_run": false,
    "resolved": { "desk": "DP-1" },
    "actions": [ { "output": "DP-1", "phase": "on", "action": "On" } ],
    "applied": [ "..." ],
    "errors": [],
    "warnings": [],
    "config_only": [],
    "previous_saved": true,
    "failed_action": null,
    "rollback_actions": [],
    "rollback_errors": []
  }
}
```

- `ok` is false for refused/rolled-back applies, parse errors, IPC errors and
  `rollback_failed`; the process exits non-zero as well.
- Statuses are `planned`, `applied`, `refused`, `rolled_back`,
  `rollback_failed`, `restored`.
- `watch` without `--once` writes one envelope per evaluated cycle (NDJSON).
- Without `--json` the legacy shapes are kept: `apply`/`restore` print the
  planned `[[output, action], ...]` array, `show`/`save` print the profile and
  `list` prints the profile names. Diagnostics go to stderr.

## Interop

### loom-session

`loom-session` contexts store `outputs` using the same core shape as a
`loom-monitor` profile entry (`off`, `mode`, `scale`, `transform`, `position`,
`vrr`). You can paste a context's output entries into `profile.outputs`, or
paste a profile's `outputs` into a context; v2's new fields are optional and do
not break the shared shape. `loom-session` restores outputs directly, so it
does not need the `loom-monitor` binary.

### loom-companion

The existing `monitor.*` adapter preset already calls:

```sh
loom-monitor apply "${LOOM_MONITOR_PROFILE:-default}"
loom-monitor restore
```

Those commands and their default output are unchanged. For machine-readable
routing, have the adapter call:

```sh
loom-monitor apply "$LOOM_MONITOR_PROFILE" --json
loom-monitor restore --json
loom-monitor rules apply --json
loom-monitor watch --once --json
```

`watch --once` is preferable when the adapter already receives output
added/removed events: it evaluates the current snapshot once, reports the
winner and planned/applied actions, and exits. A long-running
`loom-monitor watch` is preferable when niri is the only event source, since
it debounces hotplug churn and holds the loop lock.

## State files

| Path | Contents |
| --- | --- |
| `$XDG_STATE_HOME/loom-monitor/profiles.json` | profile store (`version`, `profiles`) |
| `$XDG_STATE_HOME/loom-monitor/rules.json` | rules (`version`, `rules`) |
| `$XDG_STATE_HOME/loom-monitor/mirrors.json` | tracked `wl-mirror` children (`output`, `source`, `target`, `pid`) |
| `$XDG_STATE_HOME/loom-monitor/watch.lock` | PID lock for continuous watch |

`$XDG_STATE_HOME` defaults to `~/.local/state`. `LOOM_MONITOR_STATE` can point
`profiles.json` elsewhere; rules then default to `rules.json` beside it and the
mirror state to `mirrors.json` beside it. `LOOM_MONITOR_RULES` overrides the
rules path; `LOOM_MONITOR_WL_MIRROR` overrides the `wl-mirror` binary. Writes
use a temporary file and rename.

## Limitations

- **Output topology events do not exist in niri's IPC event stream here.** Watch
  polls `Request::Outputs`; the event stream only wakes it earlier. On a niri
  version with output events, the poll can be increased or removed.
- **Config-only fields are not applied.** HDR, `focus_at_startup` and
  `isolated` require config edits or are unsupported in this tree. They are
  stored, validated where possible and reported, never faked.
- **Mirroring needs `wl-mirror`.** niri here has no mirror primitive, so
  `mirror` is delegated to the external `wl-mirror` client. Without it the
  apply still succeeds but the mirror step is reported `failed`.
- **`wl-mirror` is a separate fullscreen client.** niri places its window; the
  tool cannot guarantee it lands on the target output if a window rule
  interferes. `--fullscreen-output` is what pins it in practice.
- **Mirror cleanup is PID-based.** A child whose process was replaced by an
  unrelated process (recycled PID) fails the cmdline check and is left alone.
  Its PID is then dropped from `mirrors.json` so it is never signalled later.
- **Overlap detection needs known geometry.** An output that is newly connected
  and has no mode/position yet cannot be checked; niri will place it. Check with
  `--dry-run` after the output appears.
- **HDR reference luminance is not readable over IPC.** It can be written to a
  profile and migrated, but applying only reports it as config-only.
- **Custom modes are restored as standard modes.** The snapshot keeps the
  width, height and refresh rate (and `is_custom_mode` is not part of the
  profile), so a custom modeline is re-applied as a regular mode entry. Use the
  config file for modelines.
- **Apply is not atomic.** Rollback is compensating output actions, not a
  transaction; a disconnected socket can prevent recovery.
- **No `--force`.** Invalid layouts are refused; fix the profile instead.
- **IPC changes are temporary.** A niri config reload or reconnect can replace
  them with config values; use `watch`/rules or a config file for persistence.

## Test

No display or real niri is needed: the integration tests run a fake niri IPC
socket.

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets
```
