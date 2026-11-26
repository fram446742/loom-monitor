//! Profile schema v2, matching and persistence.
//!
//! Profiles are JSON snapshots of niri output settings. Version 1 stored only
//! the IPC-visible fields under `outputs`. Version 2 adds a profile-level
//! `match` block plus per-output fields for bit depth, HDR (config-only),
//! adaptive sync, startup focus, isolation and mirroring (config-only).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use niri_ipc::{MaxBpc, Transform};
use serde::{Deserialize, Serialize};

/// Current profile schema version.
pub const SCHEMA_VERSION: u32 = 2;

/// State directory: `$XDG_STATE_HOME/loom-monitor` or
/// `~/.local/state/loom-monitor`.
pub fn state_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/state")
        });
    dir.join("loom-monitor")
}

/// Default profile store: `$XDG_STATE_HOME/loom-monitor/profiles.json`.
pub fn state_path() -> PathBuf {
    state_dir().join("profiles.json")
}

/// Default rules store: `$XDG_STATE_HOME/loom-monitor/rules.json`.
pub fn rules_path() -> PathBuf {
    state_dir().join("rules.json")
}

/// Default watch lock: `$XDG_STATE_HOME/loom-monitor/watch.lock`.
pub fn lock_path() -> PathBuf {
    state_dir().join("watch.lock")
}

/// Matching predicate for an output.
///
/// Every present field must match; absent fields are wildcards. Matching is
/// case-insensitive and supports `*` wildcards. A completely empty match
/// matches every output, so rules using it act as a fallback.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputMatch {
    /// Connector name, for example `eDP-1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Manufacturer string as reported by the compositor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    /// Model string as reported by the compositor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Serial string as reported by the compositor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl OutputMatch {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.make.is_none() && self.model.is_none() && self.serial.is_none()
    }

    /// Whether this predicate matches `output`.
    pub fn matches(&self, output: &niri_ipc::Output) -> bool {
        if self.is_empty() {
            return true;
        }
        matches_field(self.name.as_deref(), &output.name)
            && matches_field(self.make.as_deref(), &output.make)
            && matches_field(self.model.as_deref(), &output.model)
            && matches_field(
                self.serial.as_deref(),
                output.serial.as_deref().unwrap_or(""),
            )
    }

    /// Human-readable description of the predicate.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(name) = &self.name {
            parts.push(format!("name={name}"));
        }
        if let Some(make) = &self.make {
            parts.push(format!("make={make}"));
        }
        if let Some(model) = &self.model {
            parts.push(format!("model={model}"));
        }
        if let Some(serial) = &self.serial {
            parts.push(format!("serial={serial}"));
        }
        if parts.is_empty() {
            "any output".to_owned()
        } else {
            parts.join(" ")
        }
    }
}

fn matches_field(pattern: Option<&str>, value: &str) -> bool {
    let Some(pattern) = pattern else {
        return true;
    };
    wildcard_match(&pattern.to_lowercase(), &value.to_lowercase())
}

/// Small `*`-only wildcard matcher. `?` is treated literally.
fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let value: Vec<char> = value.chars().collect();
    let (mut p, mut v) = (0, 0);
    let mut star: Option<usize> = None;
    let mut star_value = 0;
    while v < value.len() {
        if p < pattern.len() && pattern[p] == value[v] {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            star_value = v;
            p += 1;
        } else if let Some(star_p) = star {
            p = star_p + 1;
            star_value += 1;
            v = star_value;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// HDR mode from the niri config (`hdr { mode auto|on; }`).
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HdrMode {
    /// HDR is signalled only while HDR content is fullscreen.
    #[default]
    Auto,
    /// The output always stays in HDR.
    On,
}

/// HDR settings. These are config-only: niri does not expose HDR over IPC.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct HdrProfile {
    #[serde(default)]
    pub mode: HdrMode,
    /// cd/m² that SDR white maps to while HDR is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_luminance: Option<f64>,
}

/// Settings for one output.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputProfile {
    /// Whether the output is disabled.
    #[serde(default)]
    pub off: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    /// `[width, height, refresh-millihertz]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<(u16, u16, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<Transform>,
    /// Logical position `[x, y]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<(i32, i32)>,
    /// Variable refresh rate / adaptive sync. `adaptive_sync` is accepted as
    /// an input alias.
    #[serde(
        default,
        alias = "adaptive_sync",
        alias = "variable_refresh_rate",
        skip_serializing_if = "Option::is_none"
    )]
    pub vrr: Option<bool>,
    /// Only engage VRR while a VRR window rule matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vrr_on_demand: Option<bool>,
    /// Maximum bits per channel (6, 8, 10, 12, 14 or 16). Accepts the niri
    /// string form (`"10"`) and a plain number (`10`).
    #[serde(
        default,
        deserialize_with = "deserialize_max_bpc",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_bpc: Option<MaxBpc>,
    /// HDR settings. Config-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<HdrProfile>,
    /// Focus this output at startup. Config-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_at_startup: Option<bool>,
    /// Mark the output isolated. Config-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolated: Option<bool>,
    /// Mirror another output (profile key or connector name). Config-only:
    /// this niri tree does not implement output mirroring over IPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
}

fn deserialize_max_bpc<'de, D>(deserializer: D) -> Result<Option<MaxBpc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u8),
        String(String),
    }

    match Option::<Raw>::deserialize(deserializer)? {
        None => Ok(None),
        Some(Raw::Number(value)) => MaxBpc::try_from(value).map(Some).map_err(D::Error::custom),
        Some(Raw::String(value)) => value.parse::<MaxBpc>().map(Some).map_err(D::Error::custom),
    }
}

fn default_profile_version() -> u32 {
    1
}

/// A named output profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// Schema version. Missing means version 1.
    #[serde(default = "default_profile_version")]
    pub version: u32,
    /// Optional profile-level matcher. When present the profile is applied to
    /// the output that matches instead of the keys in `outputs`.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub match_: Option<OutputMatch>,
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputProfile>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            match_: None,
            outputs: BTreeMap::new(),
        }
    }
}

impl Profile {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the schema version to the current one.
    ///
    /// Returns `true` when the profile actually changed.
    pub fn migrate(&mut self) -> bool {
        if self.version == 0 {
            self.version = 1;
        }
        if self.version < SCHEMA_VERSION {
            self.version = SCHEMA_VERSION;
            true
        } else {
            false
        }
    }

    /// Returns an error for schema versions this binary cannot understand.
    pub fn check_version(&self) -> Result<(), String> {
        if self.version == 0 || self.version == 1 || self.version == SCHEMA_VERSION {
            Ok(())
        } else {
            Err(format!(
                "profile schema version {} is newer than supported version {SCHEMA_VERSION}",
                self.version
            ))
        }
    }
}

fn default_store_version() -> u32 {
    1
}

/// On-disk profile store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default = "default_store_version")]
    pub version: u32,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            profiles: BTreeMap::new(),
        }
    }
}

impl Store {
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

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn insert(&mut self, name: impl Into<String>, profile: Profile) {
        self.profiles.insert(name.into(), profile);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.profiles.remove(name).is_some()
    }

    /// Migrates every profile to the current schema version.
    ///
    /// Returns the names that changed.
    pub fn migrate(&mut self) -> Vec<String> {
        let mut changed = Vec::new();
        for (name, profile) in &mut self.profiles {
            if profile.migrate() {
                changed.push(name.clone());
            }
        }
        if self.version == 0 {
            self.version = 1;
        }
        if self.version < SCHEMA_VERSION {
            self.version = SCHEMA_VERSION;
        }
        changed
    }
}

pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_profile() -> Profile {
        let mut profile = Profile::new();
        profile.match_ = Some(OutputMatch {
            make: Some("Dell".to_owned()),
            model: Some("U2720Q".to_owned()),
            serial: Some("ABC*".to_owned()),
            name: None,
        });
        profile.outputs.insert(
            "DP-1".to_owned(),
            OutputProfile {
                off: false,
                scale: Some(1.5),
                mode: Some((3840, 2160, 60000)),
                transform: Some(Transform::_90),
                position: Some((1920, 0)),
                vrr: Some(true),
                vrr_on_demand: Some(true),
                max_bpc: Some(MaxBpc::_10),
                hdr: Some(HdrProfile {
                    mode: HdrMode::On,
                    reference_luminance: Some(203.),
                }),
                focus_at_startup: Some(true),
                isolated: Some(true),
                mirror: Some("eDP-1".to_owned()),
            },
        );
        profile
    }

    #[test]
    fn schema_v2_round_trips() {
        let profile = full_profile();
        let json = serde_json::to_string(&profile).unwrap();
        let back: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
        assert_eq!(back.version, SCHEMA_VERSION);
        assert!(json.contains("\"match\""));
        assert!(json.contains("\"max_bpc\":\"10\""));
        assert!(json.contains("\"hdr\""));
    }

    #[test]
    fn v1_profile_migrates_without_losing_fields() {
        let old = serde_json::json!({
            "outputs": {
                "eDP-1": {
                    "off": false,
                    "scale": 1.5,
                    "mode": [1920, 1080, 60000],
                    "transform": "Normal",
                    "position": [0, 0],
                    "vrr": true
                },
                "HDMI-1": { "off": true }
            }
        });
        let mut profile: Profile = serde_json::from_value(old).unwrap();
        assert_eq!(profile.version, 1);
        assert_eq!(profile.outputs.len(), 2);
        assert_eq!(profile.outputs["eDP-1"].scale, Some(1.5));
        assert_eq!(profile.outputs["eDP-1"].vrr, Some(true));

        assert!(profile.migrate());
        assert_eq!(profile.version, SCHEMA_VERSION);
        assert!(!profile.migrate(), "migration is idempotent");

        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["version"], SCHEMA_VERSION);
        assert_eq!(json["outputs"]["eDP-1"]["mode"][0], 1920);
    }

    #[test]
    fn max_bpc_accepts_numbers_and_strings() {
        for value in [serde_json::json!(10), serde_json::json!("10")] {
            let profile: Profile = serde_json::from_value(serde_json::json!({
                "version": 2,
                "outputs": { "DP-1": { "max_bpc": value } }
            }))
            .unwrap();
            assert_eq!(profile.outputs["DP-1"].max_bpc, Some(MaxBpc::_10));
        }

        let bad: Result<Profile, _> = serde_json::from_value(serde_json::json!({
            "version": 2,
            "outputs": { "DP-1": { "max_bpc": 9 } }
        }));
        assert!(bad.is_err());
    }

    #[test]
    fn adaptive_sync_aliases_parse_as_vrr() {
        let profile: Profile = serde_json::from_value(serde_json::json!({
            "version": 2,
            "outputs": { "DP-1": { "adaptive_sync": false } }
        }))
        .unwrap();
        assert_eq!(profile.outputs["DP-1"].vrr, Some(false));

        let profile: Profile = serde_json::from_value(serde_json::json!({
            "version": 2,
            "outputs": { "DP-1": { "variable_refresh_rate": true } }
        }))
        .unwrap();
        assert_eq!(profile.outputs["DP-1"].vrr, Some(true));
    }

    #[test]
    fn newer_schema_versions_are_rejected() {
        let profile = Profile {
            version: 99,
            ..Profile::new()
        };
        assert!(profile.check_version().is_err());
    }

    #[test]
    fn match_applies_to_make_model_and_serial() {
        let output = super::tests_support::output("DP-1", "Dell Inc.", "U2720Q", Some("ABC123"));
        let matcher = OutputMatch {
            make: Some("dell inc.".to_owned()),
            model: Some("u2720*".to_owned()),
            serial: Some("abc123".to_owned()),
            name: None,
        };
        assert!(matcher.matches(&output));

        let matcher = OutputMatch {
            make: Some("Dell Inc.".to_owned()),
            model: Some("U2720Q".to_owned()),
            serial: Some("OTHER".to_owned()),
            name: None,
        };
        assert!(!matcher.matches(&output));

        let matcher = OutputMatch {
            name: Some("dp-1".to_owned()),
            ..OutputMatch::default()
        };
        assert!(matcher.matches(&output));
    }

    #[test]
    fn empty_match_matches_everything() {
        let output = super::tests_support::output("HDMI-A-1", "Unknown", "Unknown", None);
        assert!(OutputMatch::default().matches(&output));
        assert_eq!(OutputMatch::default().describe(), "any output");
    }

    #[test]
    fn store_round_trips_migrates_and_removes() {
        let path = super::tests_support::temp_path("store");
        let mut store = Store::default();
        let old: Profile = serde_json::from_value(serde_json::json!({
            "outputs": { "eDP-1": { "off": true } }
        }))
        .unwrap();
        store.insert("old", old);
        store.save(&path).unwrap();

        let mut reloaded = Store::load(&path).unwrap();
        assert_eq!(reloaded.get("old").unwrap().version, 1);
        let migrated = reloaded.migrate();
        assert_eq!(migrated, vec!["old".to_owned()]);
        reloaded.save(&path).unwrap();

        let mut reloaded = Store::load(&path).unwrap();
        assert_eq!(reloaded.get("old").unwrap().version, SCHEMA_VERSION);
        assert!(reloaded.remove("old"));
        reloaded.save(&path).unwrap();
        assert!(Store::load(&path).unwrap().get("old").is_none());
    }

    #[test]
    fn wildcard_matcher_handles_prefix_and_suffix() {
        assert!(wildcard_match("*b*", "abc"));
        assert!(wildcard_match("a*", "abc"));
        assert!(wildcard_match("*c", "abc"));
        assert!(wildcard_match("abc", "abc"));
        assert!(!wildcard_match("abc", "abcd"));
        assert!(!wildcard_match("a*c", "abd"));
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use std::path::PathBuf;

    use niri_ipc::Output;

    pub fn temp_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-monitor-unit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("profiles.json")
    }

    pub fn output(name: &str, make: &str, model: &str, serial: Option<&str>) -> Output {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "make": make,
            "model": model,
            "serial": serial,
            "physical_size": null,
            "modes": [
                { "width": 1920, "height": 1080, "refresh_rate": 60000, "is_preferred": true }
            ],
            "current_mode": 0,
            "is_custom_mode": false,
            "vrr_supported": true,
            "vrr_enabled": true,
            "logical": {
                "x": 0,
                "y": 0,
                "width": 1920,
                "height": 1080,
                "scale": 1.0,
                "transform": "Normal"
            },
            "max_bpc": null
        }))
        .unwrap()
    }
}
