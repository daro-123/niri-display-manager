use crate::backend::{snapshot, validate_layout, Layout, NiriClient, OutputConfig, Outputs};
use crate::mirror::{MirrorConfig, MirrorManager};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default)]
pub struct ConfigStore;
impl ConfigStore {
    pub fn save(path: &Path, layout: &[OutputConfig]) -> Result<()> {
        validate_layout(layout)
            .with_context(|| format!("Invalid config at '{}'", path.display()))?;
        let payload = serde_json::to_vec_pretty(layout)
            .with_context(|| format!("Cannot encode config at '{}' as JSON", path.display()))?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Cannot save config at '{}': create directory",
                path.display()
            )
        })?;
        let mut file = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!(
                "Cannot save config at '{}': create temporary file",
                path.display()
            )
        })?;
        use std::io::Write;
        file.write_all(&payload)
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.as_file().sync_all())
            .with_context(|| format!("Cannot save config at '{}': write failed", path.display()))?;
        file.persist(path).with_context(|| {
            format!(
                "Cannot save config at '{}': atomic replace failed",
                path.display()
            )
        })?;
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Layout> {
        let bytes = fs::read(path).with_context(|| {
            if path.exists() {
                format!("Cannot read config at '{}': read failed", path.display())
            } else {
                format!("Config does not exist at '{}'.", path.display())
            }
        })?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("Cannot read config at '{}': invalid JSON", path.display()))?;
        decode_layout(value, path)
    }
}

fn decode_layout(value: Value, path: &Path) -> Result<Layout> {
    let rows = value.as_array().ok_or_else(|| {
        anyhow!(
            "Corrupt config at '{}': layout must be a list",
            path.display()
        )
    })?;
    const REQUIRED: &[&str] = &["name", "enabled", "mode", "scale", "x", "y", "transform"];
    for (index, row) in rows.iter().enumerate() {
        let object = row.as_object().ok_or_else(|| {
            anyhow!(
                "Corrupt config at '{}': output {index} must be an object",
                path.display()
            )
        })?;
        for field in REQUIRED {
            if !object.contains_key(*field) {
                bail!(
                    "Corrupt config at '{}': output {index} is missing required field '{field}'",
                    path.display()
                );
            }
        }
    }
    let layout: Layout = serde_json::from_value(value).with_context(|| {
        format!(
            "Corrupt config at '{}': invalid layout fields",
            path.display()
        )
    })?;
    validate_layout(&layout)
        .with_context(|| format!("Corrupt config at '{}': invalid layout", path.display()))?;
    Ok(layout)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMode {
    #[default]
    Exact,
    ExternalOnly,
    MirrorBuiltIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetKind {
    BuiltIn,
    ExternalOnly,
    MirrorBuiltIn,
    Horizontal,
    Vertical,
}

impl PresetKind {
    pub const ALL: [Self; 5] = [
        Self::BuiltIn,
        Self::ExternalOnly,
        Self::MirrorBuiltIn,
        Self::Horizontal,
        Self::Vertical,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::BuiltIn => "Build-in",
            Self::ExternalOnly => "External",
            Self::MirrorBuiltIn => "Mirror build-in",
            Self::Horizontal => "Horizontal",
            Self::Vertical => "Vertical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProfileId {
    Named(String),
    Predefined { preset: PresetKind },
}

impl ProfileId {
    pub fn name(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::Predefined { preset } => preset.name(),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Named(name) => name.clone(),
            Self::Predefined { preset } => format!("{} (predefined)", preset.name()),
        }
    }
}

impl From<&str> for ProfileId {
    fn from(name: &str) -> Self {
        Self::Named(name.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplaySettings {
    pub resolution: Option<String>,
    pub scale: f64,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            resolution: None,
            scale: 1.0,
        }
    }
}

impl DisplaySettings {
    fn validate(&self) -> Result<()> {
        if !self.scale.is_finite() || !(0.1..=10.0).contains(&self.scale) {
            bail!("Scale must be between 10% and 1000%.");
        }
        if let Some(resolution) = &self.resolution {
            let valid = resolution.split_once('x').is_some_and(|(width, height)| {
                [width, height].iter().all(|part| {
                    !part.is_empty()
                        && part.bytes().all(|byte| byte.is_ascii_digit())
                        && part
                            .parse::<u32>()
                            .is_ok_and(|value| (1..=100000).contains(&value))
                })
            });
            if !valid {
                bail!("Resolution must be WIDTHxHEIGHT, for example 1920x1080.");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltInPosition {
    #[default]
    Before,
    After,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Alignment {
    Start,
    #[default]
    Center,
    End,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PresetSettings {
    pub show_in_picker: bool,
    pub built_in: DisplaySettings,
    pub external: DisplaySettings,
    #[serde(default)]
    pub built_in_position: BuiltInPosition,
    #[serde(default)]
    pub alignment: Alignment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct PredefinedSettings {
    built_in: PresetSettings,
    external_only: PresetSettings,
    mirror_built_in: PresetSettings,
    horizontal: PresetSettings,
    vertical: PresetSettings,
}

impl Default for PredefinedSettings {
    fn default() -> Self {
        Self {
            built_in: PresetSettings::default(),
            external_only: PresetSettings::default(),
            mirror_built_in: PresetSettings::default(),
            horizontal: PresetSettings::default(),
            vertical: PresetSettings {
                built_in_position: BuiltInPosition::After,
                ..PresetSettings::default()
            },
        }
    }
}

impl PredefinedSettings {
    fn get(&self, kind: PresetKind) -> &PresetSettings {
        match kind {
            PresetKind::BuiltIn => &self.built_in,
            PresetKind::ExternalOnly => &self.external_only,
            PresetKind::MirrorBuiltIn => &self.mirror_built_in,
            PresetKind::Horizontal => &self.horizontal,
            PresetKind::Vertical => &self.vertical,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    pub outputs: Layout,
    pub mirrors: Vec<MirrorConfig>,
    pub mode: ProfileMode,
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        validate_layout(&self.outputs).context("Invalid profile layout")?;
        if self.mode != ProfileMode::Exact && !self.mirrors.is_empty() {
            bail!("Dynamic profiles cannot store fixed mirror pairs.");
        }
        let mut targets = HashSet::new();
        let mut source: Option<&str> = None;
        for mirror in &self.mirrors {
            if mirror.source.trim().is_empty() || mirror.target.trim().is_empty() {
                bail!("Mirror source and target must not be empty.");
            }
            if mirror.source == mirror.target {
                bail!("Mirror source and target outputs must be different.");
            }
            if !targets.insert(&mirror.target) {
                bail!("Mirror targets must be unique.");
            }
            if let Some(existing) = source {
                if existing != mirror.source {
                    bail!("All mirrors must share one source.");
                }
            } else {
                source = Some(&mirror.source);
            }
            if !self
                .outputs
                .iter()
                .any(|r| r.name == mirror.source && r.enabled)
            {
                bail!("Mirror source must be present and enabled in the profile.");
            }
            if !self.outputs.iter().any(|r| r.name == mirror.target) {
                bail!("Mirror target must be present in the profile.");
            }
        }
        Ok(())
    }

    pub fn capture(client: &NiriClient, mirrors: &mut MirrorManager) -> Result<Self> {
        Ok(Self {
            outputs: snapshot(&client.outputs()?)?,
            mirrors: mirrors.current_all()?,
            mode: ProfileMode::Exact,
        })
    }

    pub fn apply(&self, client: &NiriClient, mirrors: &mut MirrorManager) -> Result<()> {
        let focus = client.focused_workspace_id().unwrap_or(None);
        let desired = merge_profile(self, &client.outputs()?)?;
        mirrors.stop()?;
        if !desired.mirrors.is_empty() {
            let source = &desired.mirrors[0].source;
            client.apply_with_source(&desired.outputs, source)?;
            for mirror in &desired.mirrors {
                client.prepare_mirror(&mirror.source, &mirror.target)?;
            }
            mirrors.start_all(&desired.mirrors)?;
        } else {
            client.apply(&desired.outputs)?;
        }
        // Fullscreen wl-mirror can take focus on its target output.
        // Restore the workspace the user used before profile application.
        if let Some(id) = focus {
            let _ = client.focus_workspace_id(id);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ProfileStore {
    directory: PathBuf,
}
impl Default for ProfileStore {
    fn default() -> Self {
        let home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs_home().join(".config"));
        Self::new(home.join("niri-display").join("profiles"))
    }
}
fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
impl ProfileStore {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    fn normalize(name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            bail!("Profile name must not be empty.");
        }
        if name.chars().any(|c| c.is_control()) {
            bail!("Profile name must not contain control characters.");
        }
        Ok(name.to_owned())
    }
    fn filename(name: &str) -> Result<String> {
        Ok(format!(
            "{}.json",
            URL_SAFE.encode(Self::normalize(name)?.as_bytes())
        ))
    }
    pub fn list_names(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context("Cannot list profiles"),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|v| v.to_str()) else {
                continue;
            };
            let Ok(bytes) = URL_SAFE.decode(stem) else {
                continue;
            };
            let Ok(name) = String::from_utf8(bytes) else {
                continue;
            };
            if Self::filename(&name).ok().as_deref() == path.file_name().and_then(|v| v.to_str())
                && path.is_file()
            {
                names.push(name);
            }
        }
        names.sort_by_cached_key(|name| name.to_lowercase());
        let order = self.read_order()?;
        let available: HashSet<_> = names.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        let mut ordered: Vec<_> = order
            .into_iter()
            .filter_map(|id| match id {
                ProfileId::Named(name)
                    if available.contains(name.as_str()) && seen.insert(name.clone()) =>
                {
                    Some(name)
                }
                _ => None,
            })
            .collect();
        ordered.extend(names.into_iter().filter(|name| !seen.contains(name)));
        Ok(ordered)
    }

    fn read_order(&self) -> Result<Vec<ProfileId>> {
        match fs::read(self.directory.join(".order.json")) {
            Ok(data) => serde_json::from_slice(&data).context("Cannot read profile order"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error).context("Cannot read profile order"),
        }
    }

    pub fn list_profiles(&self, picker: bool) -> Result<Vec<ProfileId>> {
        let settings = self.read_predefined()?;
        let mut available: Vec<_> = self
            .list_names()?
            .into_iter()
            .map(ProfileId::Named)
            .collect();
        available.extend(
            PresetKind::ALL
                .into_iter()
                .filter(|kind| !picker || settings.get(*kind).show_in_picker)
                .map(|preset| ProfileId::Predefined { preset }),
        );
        let mut ordered = self.read_order()?;
        let mut seen = HashSet::new();
        ordered.retain(|id| available.contains(id) && seen.insert(id.clone()));
        ordered.extend(available.into_iter().filter(|id| !seen.contains(id)));
        Ok(ordered)
    }

    pub fn save_order(&self, profiles: &[ProfileId]) -> Result<()> {
        let unique: HashSet<_> = profiles.iter().collect();
        let current = self.list_profiles(true)?;
        if unique.len() != profiles.len()
            || current.len() != profiles.len()
            || !current.iter().all(|id| unique.contains(id))
        {
            bail!("Profiles changed. Reopen the picker before reordering.");
        }
        // Keep hidden presets in their old slots while reordering visible rows.
        let mut visible = profiles.iter();
        let mut order = self.read_order()?;
        let mut seen = HashSet::new();
        order.retain(|id| {
            (matches!(id, ProfileId::Predefined { .. }) || current.contains(id))
                && seen.insert(id.clone())
        });
        for id in &mut order {
            if current.contains(id) {
                *id = visible.next().unwrap().clone();
            }
        }
        order.extend(visible.cloned());
        fs::create_dir_all(&self.directory)?;
        atomic_json(&self.directory.join(".order.json"), &order)
    }

    fn read_predefined(&self) -> Result<PredefinedSettings> {
        let settings: PredefinedSettings = match fs::read(self.directory.join(".predefined.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("Cannot read predefined profile settings")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                PredefinedSettings::default()
            }
            Err(error) => return Err(error).context("Cannot read predefined profile settings"),
        };
        for kind in PresetKind::ALL {
            settings.get(kind).external.validate()?;
            settings.get(kind).built_in.validate()?;
        }
        Ok(settings)
    }

    pub fn preset_settings(&self, kind: PresetKind) -> Result<PresetSettings> {
        Ok(self.read_predefined()?.get(kind).clone())
    }

    pub fn save_preset(&self, kind: PresetKind, preset: &PresetSettings) -> Result<()> {
        preset.external.validate()?;
        preset.built_in.validate()?;
        let mut settings = self.read_predefined()?;
        match kind {
            PresetKind::BuiltIn => settings.built_in = preset.clone(),
            PresetKind::ExternalOnly => settings.external_only = preset.clone(),
            PresetKind::MirrorBuiltIn => settings.mirror_built_in = preset.clone(),
            PresetKind::Horizontal => settings.horizontal = preset.clone(),
            PresetKind::Vertical => settings.vertical = preset.clone(),
        }
        fs::create_dir_all(&self.directory)?;
        atomic_json(&self.directory.join(".predefined.json"), &settings)
    }

    pub fn resolve(&self, id: &ProfileId, outputs: &Outputs) -> Result<Profile> {
        match id {
            ProfileId::Named(name) => merge_profile(&self.load(name)?, outputs),
            ProfileId::Predefined { preset } => {
                resolve_preset(*preset, &self.preset_settings(*preset)?, outputs)
            }
        }
    }
    pub fn save(&self, name: &str, profile: &Profile) -> Result<()> {
        let name = Self::normalize(name)?;
        profile
            .validate()
            .with_context(|| format!("Invalid profile '{name}'"))?;
        fs::create_dir_all(&self.directory)?;
        let path = self.directory.join(Self::filename(&name)?);
        let temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        atomic_json(temporary.path(), profile)?;
        match fs::hard_link(temporary.path(), &path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("Profile '{name}' already exists.")
            }
            Err(e) => Err(e).with_context(|| format!("Cannot save profile '{name}'")),
        }
    }
    pub fn load(&self, name: &str) -> Result<Profile> {
        let name = Self::normalize(name)?;
        let path = self.directory.join(Self::filename(&name)?);
        let bytes = fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("Profile '{name}' does not exist.")
            } else {
                anyhow!(error).context(format!("Cannot read profile '{name}'"))
            }
        })?;
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("Cannot read profile '{name}': invalid JSON"))?;
        let (outputs, mirrors, mode) = if value.is_array() {
            (value, Vec::new(), ProfileMode::Exact)
        } else {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("Profile '{name}' must be an object or a layout list"))?;
            let outputs = object
                .get("outputs")
                .cloned()
                .ok_or_else(|| anyhow!("Profile '{name}' is missing outputs"))?;
            let mirrors = if let Some(value) = object.get("mirrors").filter(|v| !v.is_null()) {
                serde_json::from_value(value.clone())
                    .with_context(|| format!("Invalid mirror configuration in profile '{name}'"))?
            } else if let Some(value) = object.get("mirror").filter(|v| !v.is_null()) {
                vec![serde_json::from_value(value.clone())
                    .with_context(|| format!("Invalid mirror configuration in profile '{name}'"))?]
            } else {
                Vec::new()
            };
            let mode = object
                .get("mode")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .with_context(|| format!("Invalid profile mode in '{name}'"))?
                .unwrap_or_default();
            (outputs, mirrors, mode)
        };
        let profile = Profile {
            outputs: decode_layout(outputs, &path)?,
            mirrors,
            mode,
        };
        profile
            .validate()
            .with_context(|| format!("Invalid profile '{name}'"))?;
        Ok(profile)
    }
    pub fn delete(&self, name: &str) -> Result<()> {
        let name = Self::normalize(name)?;
        let path = self.directory.join(Self::filename(&name)?);
        fs::remove_file(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!("Profile '{name}' does not exist.")
            } else {
                e.into()
            }
        })
    }
}
fn atomic_json<T: serde::Serialize + ?Sized>(path: &Path, value: &T) -> Result<()> {
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    use std::io::Write;
    serde_json::to_writer(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    Ok(())
}

pub fn merge_profile(profile: &Profile, outputs: &Outputs) -> Result<Profile> {
    profile.validate().context("Invalid profile")?;
    let current = snapshot(outputs)?;
    let merged = match profile.mode {
        ProfileMode::Exact => {
            let saved: HashSet<_> = profile.outputs.iter().map(|r| r.name.as_str()).collect();
            let missing: Vec<_> = saved
                .iter()
                .filter(|name| !outputs.contains_key(**name))
                .copied()
                .collect();
            if !missing.is_empty() {
                bail!("Profile has missing monitors: {}", missing.join(", "));
            }
            let by_name: std::collections::HashMap<_, _> = profile
                .outputs
                .iter()
                .map(|r| (r.name.as_str(), r))
                .collect();
            current
                .into_iter()
                .map(|row| {
                    by_name
                        .get(row.name.as_str())
                        .map(|r| (*r).clone())
                        .unwrap_or(row)
                })
                .collect()
        }
        ProfileMode::ExternalOnly | ProfileMode::MirrorBuiltIn => {
            let builtin = |name: &str| {
                ["eDP-", "LVDS-", "DSI-"]
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            };
            let builtin_count = current.iter().filter(|row| builtin(&row.name)).count();
            if current.iter().all(|row| builtin(&row.name)) {
                bail!("Dynamic profile requires at least one external display.");
            }
            if builtin_count == 0
                || (profile.mode == ProfileMode::MirrorBuiltIn && builtin_count != 1)
            {
                bail!("Dynamic profile requires a built-in display; mirror-built-in requires exactly one.");
            }
            let mut rows = current;
            for row in &mut rows {
                if row.mode.is_none() {
                    row.mode = outputs[&row.name]
                        .get("modes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|mode| crate::backend::mode_string(mode).ok())
                        .next();
                }
            }
            let source_width = rows
                .iter()
                .find(|row| builtin(&row.name))
                .map(|row| crate::backend::dimensions(row).0)
                .unwrap_or(0);
            let mut x = if profile.mode == ProfileMode::MirrorBuiltIn {
                source_width
            } else {
                0_i32
            };
            for row in &mut rows {
                if profile.mode == ProfileMode::ExternalOnly && builtin(&row.name) {
                    row.enabled = false;
                } else if !builtin(&row.name) {
                    row.enabled = true;
                    row.x = x;
                    row.y = 0;
                    let (width, _) = crate::backend::dimensions(row);
                    x = x.saturating_add(width);
                } else if profile.mode == ProfileMode::MirrorBuiltIn {
                    row.enabled = true;
                    row.x = 0;
                    row.y = 0;
                }
            }
            rows
        }
    };
    let mirrors = if profile.mode == ProfileMode::MirrorBuiltIn {
        let source = merged
            .iter()
            .find(|r| {
                ["eDP-", "LVDS-", "DSI-"]
                    .iter()
                    .any(|p| r.name.starts_with(p))
            })
            .unwrap()
            .name
            .clone();
        merged
            .iter()
            .filter(|r| {
                !["eDP-", "LVDS-", "DSI-"]
                    .iter()
                    .any(|p| r.name.starts_with(p))
            })
            .map(|r| MirrorConfig {
                source: source.clone(),
                target: r.name.clone(),
            })
            .collect()
    } else {
        profile.mirrors.clone()
    };
    validate_layout(&merged).context("Invalid merged profile")?;
    Ok(Profile {
        outputs: merged,
        mirrors,
        mode: ProfileMode::Exact,
    })
}

fn is_builtin(name: &str) -> bool {
    ["eDP-", "LVDS-", "DSI-"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn resolve_preset(
    kind: PresetKind,
    settings: &PresetSettings,
    outputs: &Outputs,
) -> Result<Profile> {
    settings.external.validate()?;
    settings.built_in.validate()?;
    let mut profile = Profile {
        outputs: snapshot(outputs)?,
        mirrors: Vec::new(),
        mode: ProfileMode::Exact,
    };
    let builtin_count = profile
        .outputs
        .iter()
        .filter(|row| is_builtin(&row.name))
        .count();
    if builtin_count == 0 {
        bail!("Predefined profiles require a built-in display.");
    }
    if kind != PresetKind::BuiltIn && builtin_count == profile.outputs.len() {
        bail!("This predefined profile requires at least one external display.");
    }
    if kind == PresetKind::MirrorBuiltIn && builtin_count != 1 {
        bail!("Mirroring requires exactly one built-in display.");
    }
    for row in &mut profile.outputs {
        row.enabled = match kind {
            PresetKind::BuiltIn => is_builtin(&row.name),
            PresetKind::ExternalOnly => !is_builtin(&row.name),
            PresetKind::MirrorBuiltIn | PresetKind::Horizontal | PresetKind::Vertical => true,
        };
        if !row.enabled {
            continue;
        }
        row.x = 0;
        row.y = 0;
        let display = if is_builtin(&row.name) {
            &settings.built_in
        } else {
            &settings.external
        };
        let modes = outputs[&row.name]
            .get("modes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("No advertised modes for {}.", row.name))?;
        let chosen = if let Some(resolution) = &display.resolution {
            let (width, height) = resolution.split_once('x').unwrap();
            let width = width.parse::<u64>()?;
            let height = height.parse::<u64>()?;
            modes
                .iter()
                .filter(|mode| {
                    mode["width"].as_u64() == Some(width) && mode["height"].as_u64() == Some(height)
                })
                .max_by_key(|mode| mode["refresh_rate"].as_u64().unwrap_or(0))
        } else {
            modes
                .iter()
                .find(|mode| mode["is_preferred"].as_bool() == Some(true))
                .or_else(|| modes.first())
        }
        .ok_or_else(|| {
            anyhow!(
                "{} does not support {}.",
                row.name,
                display
                    .resolution
                    .as_deref()
                    .unwrap_or("an automatic resolution")
            )
        })?;
        row.mode = Some(crate::backend::mode_string(chosen)?);
        row.scale = display.scale;
        if matches!(kind, PresetKind::Horizontal | PresetKind::Vertical) {
            row.transform = "normal".into();
        }
    }
    if matches!(kind, PresetKind::Horizontal | PresetKind::Vertical) {
        // Keep connector order within each role, but place the built-in group
        // before or after the external group according to the saved setting.
        profile.outputs.sort_by_key(|row| {
            is_builtin(&row.name) != (settings.built_in_position == BuiltInPosition::Before)
        });
        let vertical = kind == PresetKind::Vertical;
        let cross_extent = profile
            .outputs
            .iter()
            .map(|row| {
                let (width, height) = crate::backend::dimensions(row);
                if vertical {
                    width
                } else {
                    height
                }
            })
            .max()
            .unwrap_or(0);
        let mut position = 0_i32;
        for row in &mut profile.outputs {
            let (width, height) = crate::backend::dimensions(row);
            let (along, across) = if vertical {
                (height, width)
            } else {
                (width, height)
            };
            let offset = match settings.alignment {
                Alignment::Start => 0,
                Alignment::Center => (cross_extent - across) / 2,
                Alignment::End => cross_extent - across,
            };
            (row.x, row.y) = if vertical {
                (offset, position)
            } else {
                (position, offset)
            };
            position = position
                .checked_add(along)
                .ok_or_else(|| anyhow!("Display arrangement is too large."))?;
        }
        profile.validate()?;
        return Ok(profile);
    }
    let mut x = if kind == PresetKind::MirrorBuiltIn {
        profile
            .outputs
            .iter()
            .find(|row| is_builtin(&row.name))
            .map(|row| crate::backend::dimensions(row).0)
            .unwrap_or(0)
    } else {
        0
    };
    for row in &mut profile.outputs {
        if row.enabled && (kind == PresetKind::BuiltIn || !is_builtin(&row.name)) {
            row.x = x;
            row.y = 0;
            x = x
                .checked_add(crate::backend::dimensions(row).0)
                .ok_or_else(|| anyhow!("Display arrangement is too wide."))?;
        }
    }
    if kind == PresetKind::MirrorBuiltIn {
        let source = profile
            .outputs
            .iter()
            .find(|row| is_builtin(&row.name))
            .unwrap()
            .name
            .clone();
        profile.mirrors = profile
            .outputs
            .iter()
            .filter(|row| !is_builtin(&row.name))
            .map(|row| MirrorConfig {
                source: source.clone(),
                target: row.name.clone(),
            })
            .collect();
    }
    profile.validate()?;
    Ok(profile)
}
