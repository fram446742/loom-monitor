//! Output rules: match + profile + priority, with explain support.

use std::collections::BTreeMap;
use std::path::Path;

use niri_ipc::Output;
use serde::{Deserialize, Serialize};

use crate::plan::resolve_profile;
use crate::schema::{OutputMatch, Profile, SCHEMA_VERSION, Store, write_json_atomic};

fn default_rule_version() -> u32 {
    1
}

/// A rule binds an output matcher to a profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub name: String,
    /// Name of the profile to apply.
    pub profile: String,
    /// Higher priority wins. Ties are broken by rule name.
    #[serde(default)]
    pub priority: i32,
    #[serde(rename = "match", default)]
    pub match_: OutputMatch,
}

/// On-disk rules store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleStore {
    #[serde(default = "default_rule_version")]
    pub version: u32,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Default for RuleStore {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            rules: Vec::new(),
        }
    }
}

impl RuleStore {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        write_json_atomic(path, self)
    }

    pub fn get(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.name == name)
    }

    pub fn insert(&mut self, rule: Rule) -> Option<Rule> {
        if let Some(slot) = self.rules.iter_mut().find(|slot| slot.name == rule.name) {
            Some(std::mem::replace(slot, rule))
        } else {
            self.rules.push(rule);
            None
        }
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|rule| rule.name != name);
        self.rules.len() != before
    }

    pub fn save_migrated(&mut self, path: &Path) -> std::io::Result<()> {
        if self.version < SCHEMA_VERSION {
            self.version = SCHEMA_VERSION;
        }
        self.save(path)
    }
}

/// One rule considered for an output.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuleCandidate {
    pub name: String,
    pub priority: i32,
    pub matched: bool,
}

/// Explanation of which rule wins for an output.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuleDecision {
    pub output: String,
    pub rule: Option<String>,
    pub profile: Option<String>,
    pub priority: Option<i32>,
    pub considered: Vec<RuleCandidate>,
    pub reason: String,
}

/// Evaluates every rule against every connected output.
pub fn evaluate_rules(rules: &[Rule], outputs: &BTreeMap<String, Output>) -> Vec<RuleDecision> {
    let mut decisions = Vec::new();
    for (name, output) in outputs {
        let mut considered: Vec<RuleCandidate> = rules
            .iter()
            .map(|rule| RuleCandidate {
                name: rule.name.clone(),
                priority: rule.priority,
                matched: rule.match_.matches(output),
            })
            .collect();
        considered.sort_by(|a, b| a.name.cmp(&b.name));

        let mut matching: Vec<&Rule> = rules
            .iter()
            .filter(|rule| rule.match_.matches(output))
            .collect();
        matching.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.name.cmp(&b.name))
        });

        let (rule, profile, priority, reason) = match matching.first() {
            Some(winner) => {
                let tie = matching
                    .iter()
                    .skip(1)
                    .find(|other| other.priority == winner.priority);
                let reason = if let Some(tie) = tie {
                    format!(
                        "{} rule(s) matched; `{}` (priority {}) wins the priority tie by name over `{}`",
                        matching.len(),
                        winner.name,
                        winner.priority,
                        tie.name
                    )
                } else if matching.len() > 1 {
                    format!(
                        "{} rule(s) matched; `{}` has the highest priority ({})",
                        matching.len(),
                        winner.name,
                        winner.priority
                    )
                } else {
                    format!("`{}` is the only matching rule", winner.name)
                };
                (
                    Some(winner.name.clone()),
                    Some(winner.profile.clone()),
                    Some(winner.priority),
                    reason,
                )
            }
            None => (None, None, None, "no rule matched this output".to_owned()),
        };

        decisions.push(RuleDecision {
            output: name.clone(),
            rule,
            profile,
            priority,
            considered,
            reason,
        });
    }
    decisions
}

/// Builds one synthetic profile from the winning rule for every output.
///
/// Returns the synthetic profile, the decisions and non-fatal warnings.
pub fn build_rules_profile(
    rules: &[Rule],
    outputs: &BTreeMap<String, Output>,
    store: &Store,
) -> (Profile, Vec<RuleDecision>, Vec<String>) {
    let decisions = evaluate_rules(rules, outputs);
    let mut profile = Profile::new();
    let mut warnings = Vec::new();

    for decision in &decisions {
        let (Some(rule), Some(profile_name)) = (&decision.rule, &decision.profile) else {
            continue;
        };
        let Some(stored) = store.get(profile_name) else {
            warnings.push(format!(
                "rule `{rule}` for `{}` references unknown profile `{profile_name}`",
                decision.output
            ));
            continue;
        };
        match resolve_profile(stored, outputs, Some(&decision.output)) {
            Ok(resolved) => {
                for (output, entry) in resolved.desired {
                    profile.outputs.insert(output, entry);
                }
                warnings.extend(resolved.warnings);
            }
            Err(errors) => {
                warnings.push(format!(
                    "rule `{rule}` for `{}` cannot use profile `{profile_name}`: {}",
                    decision.output,
                    errors.join("; ")
                ));
            }
        }
    }

    (profile, decisions, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::tests_support;

    fn outputs() -> BTreeMap<String, Output> {
        let mut map = BTreeMap::new();
        map.insert(
            "eDP-1".to_owned(),
            tests_support::output("eDP-1", "BOE", "Panel", None),
        );
        let mut dp = tests_support::output("DP-1", "Dell Inc.", "U2720Q", Some("ABC"));
        if let Some(logical) = &mut dp.logical {
            logical.x = 1920;
        }
        map.insert("DP-1".to_owned(), dp);
        map
    }

    fn rule(name: &str, profile: &str, priority: i32, make: &str) -> Rule {
        Rule {
            name: name.to_owned(),
            profile: profile.to_owned(),
            priority,
            match_: OutputMatch {
                make: Some(make.to_owned()),
                ..OutputMatch::default()
            },
        }
    }

    #[test]
    fn highest_priority_rule_wins_and_explains_losers() {
        let rules = vec![
            rule("fallback", "generic", 0, "Dell Inc."),
            rule("dock", "desk", 20, "Dell Inc."),
            rule("other", "laptop", 5, "BOE"),
        ];
        let decisions = evaluate_rules(&rules, &outputs());
        let dock = decisions.iter().find(|d| d.output == "DP-1").unwrap();
        assert_eq!(dock.rule.as_deref(), Some("dock"));
        assert_eq!(dock.profile.as_deref(), Some("desk"));
        assert_eq!(dock.priority, Some(20));
        assert!(dock.reason.contains("highest priority"));
        assert_eq!(
            dock.considered
                .iter()
                .filter(|candidate| candidate.matched)
                .count(),
            2
        );

        let edp = decisions.iter().find(|d| d.output == "eDP-1").unwrap();
        assert_eq!(edp.rule.as_deref(), Some("other"));
    }

    #[test]
    fn priority_ties_are_broken_by_name() {
        let rules = vec![
            rule("zulu", "a", 10, "Dell Inc."),
            rule("alpha", "b", 10, "Dell Inc."),
        ];
        let decisions = evaluate_rules(&rules, &outputs());
        let dock = decisions.iter().find(|d| d.output == "DP-1").unwrap();
        assert_eq!(dock.rule.as_deref(), Some("alpha"));
        assert!(dock.reason.contains("tie"));
    }

    #[test]
    fn no_matching_rule_is_reported() {
        let rules = vec![rule("only", "p", 1, "Nope")];
        let decisions = evaluate_rules(&rules, &outputs());
        assert!(
            decisions
                .iter()
                .all(|decision| decision.rule.is_none() && decision.reason.contains("no rule"))
        );
    }

    #[test]
    fn rules_profile_merges_winning_profiles() {
        let mut store = Store::default();
        let mut laptop = Profile::default();
        laptop.outputs.insert(
            "eDP-1".to_owned(),
            crate::schema::OutputProfile {
                scale: Some(1.5),
                ..crate::schema::OutputProfile::default()
            },
        );
        store.insert("laptop", laptop);
        let mut desk = Profile::default();
        desk.outputs.insert(
            "DP-1".to_owned(),
            crate::schema::OutputProfile {
                scale: Some(2.),
                ..crate::schema::OutputProfile::default()
            },
        );
        store.insert("desk", desk);

        let rules = vec![
            rule("laptop-rule", "laptop", 1, "BOE"),
            rule("desk-rule", "desk", 10, "Dell Inc."),
        ];
        let (profile, decisions, warnings) = build_rules_profile(&rules, &outputs(), &store);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(decisions.len(), 2);
        assert_eq!(profile.outputs["eDP-1"].scale, Some(1.5));
        assert_eq!(profile.outputs["DP-1"].scale, Some(2.));
    }

    #[test]
    fn rules_profile_merges_positions_before_validating() {
        // Each rule alone would overlap the other output at its *current*
        // position, but the merged result is a valid side-by-side layout.
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".to_owned(),
            tests_support::output("eDP-1", "BOE", "Panel", None),
        );
        let mut dp = tests_support::output("DP-1", "Dell Inc.", "U2720Q", Some("ABC"));
        if let Some(logical) = &mut dp.logical {
            logical.x = 1920;
        }
        outputs.insert("DP-1".to_owned(), dp);

        let mut store = Store::default();
        let mut laptop = Profile::default();
        laptop.outputs.insert(
            "eDP-1".to_owned(),
            crate::schema::OutputProfile {
                position: Some((0, 0)),
                ..crate::schema::OutputProfile::default()
            },
        );
        store.insert("laptop", laptop);
        let mut desk = Profile::default();
        desk.outputs.insert(
            "DP-1".to_owned(),
            crate::schema::OutputProfile {
                position: Some((1920, 0)),
                ..crate::schema::OutputProfile::default()
            },
        );
        store.insert("desk", desk);

        let rules = vec![
            Rule {
                name: "laptop-rule".to_owned(),
                profile: "laptop".to_owned(),
                priority: 1,
                match_: OutputMatch {
                    name: Some("eDP-1".to_owned()),
                    ..OutputMatch::default()
                },
            },
            Rule {
                name: "desk-rule".to_owned(),
                profile: "desk".to_owned(),
                priority: 1,
                match_: OutputMatch {
                    name: Some("DP-1".to_owned()),
                    ..OutputMatch::default()
                },
            },
        ];
        let (profile, _, warnings) = build_rules_profile(&rules, &outputs, &store);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(profile.outputs.len(), 2);
        let plan = crate::plan::plan_profile(&profile, &outputs, None);
        assert!(plan.ok(), "{:?}", plan.errors);
    }

    #[test]
    fn missing_profile_is_a_warning() {
        let store = Store::default();
        let rules = vec![rule("r", "missing", 1, "Dell Inc.")];
        let (profile, _, warnings) = build_rules_profile(&rules, &outputs(), &store);
        assert!(profile.outputs.is_empty());
        assert!(warnings[0].contains("unknown profile"));
    }

    #[test]
    fn store_round_trips_and_removes() {
        let path = std::env::temp_dir().join(format!(
            "loom-monitor-rules-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut store = RuleStore::default();
        store.insert(rule("desk", "desk-profile", 10, "Dell"));
        store.save(&path).unwrap();
        let mut loaded = RuleStore::load(&path).unwrap();
        assert_eq!(loaded.get("desk").unwrap().priority, 10);
        assert!(loaded.remove("desk"));
        loaded.save(&path).unwrap();
        assert!(RuleStore::load(&path).unwrap().get("desk").is_none());
        let _ = std::fs::remove_file(path);
    }
}
