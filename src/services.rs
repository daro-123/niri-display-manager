use crate::backend::{snapshot, validate_layout, Layout, NiriClient, OutputConfig, Outputs};
use crate::mirror::{MirrorConfig, MirrorManager};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use serde::Serialize;
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Profile {
    pub outputs: Layout,
    pub mirror: Option<MirrorConfig>,
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        validate_layout(&self.outputs).context("Invalid profile layout")?;
        if let Some(mirror) = &self.mirror {
            if mirror.source.trim().is_empty() || mirror.target.trim().is_empty() {
                bail!("Mirror source and target must not be empty.");
            }
            if mirror.source == mirror.target {
                bail!("Mirror source and target outputs must be different.");
            }
            if !self
                .outputs
                .iter()
                .any(|row| row.name == mirror.source && row.enabled)
            {
                bail!("Mirror source must be present and enabled in the profile.");
            }
            if !self.outputs.iter().any(|row| row.name == mirror.target) {
                bail!("Mirror target must be present in the profile.");
            }
        }
        Ok(())
    }

    pub fn capture(client: &NiriClient, mirrors: &mut MirrorManager) -> Result<Self> {
        Ok(Self {
            outputs: snapshot(&client.outputs()?)?,
            mirror: mirrors.current()?,
        })
    }

    pub fn apply(&self, client: &NiriClient, mirrors: &mut MirrorManager) -> Result<()> {
        let focus = client.focused_workspace_id().unwrap_or(None);
        let desired = merge_profile(self, &client.outputs()?)?;
        mirrors.stop()?;
        match &desired.mirror {
            Some(mirror) => client.apply_with_source(&desired.outputs, &mirror.source)?,
            None => client.apply(&desired.outputs)?,
        }
        if let Some(mirror) = &desired.mirror {
            client.prepare_mirror(&mirror.source, &mirror.target)?;
            mirrors.start(&mirror.source, &mirror.target)?;
        }
        // The mirror window opens fullscreen on the target output and can take
        // focus there; keep the user on the workspace they were using.
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
        let order_path = self.directory.join(".order.json");
        let order: Vec<String> = match fs::read(&order_path) {
            Ok(data) => serde_json::from_slice(&data).context("Cannot read profile order")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(e) => return Err(e).context("Cannot read profile order"),
        };
        let available: HashSet<_> = names.iter().cloned().collect();
        let mut seen = HashSet::new();
        let mut ordered: Vec<_> = order
            .into_iter()
            .filter(|name| available.contains(name) && seen.insert(name.clone()))
            .collect();
        ordered.extend(names.into_iter().filter(|name| !seen.contains(name)));
        Ok(ordered)
    }
    pub fn save_order(&self, names: &[String]) -> Result<()> {
        let unique: HashSet<_> = names.iter().collect();
        let current: HashSet<_> = self.list_names()?.iter().cloned().collect();
        if unique.len() != names.len()
            || unique.iter().map(|s| (*s).clone()).collect::<HashSet<_>>() != current
        {
            bail!("Profiles changed. Reopen the picker before reordering.");
        }
        fs::create_dir_all(&self.directory)?;
        atomic_json(&self.directory.join(".order.json"), names)
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
        let (outputs, mirror) = if value.is_array() {
            (value, None)
        } else {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("Profile '{name}' must be an object or a layout list"))?;
            let outputs = object
                .get("outputs")
                .cloned()
                .ok_or_else(|| anyhow!("Profile '{name}' is missing outputs"))?;
            let mirror = object
                .get("mirror")
                .filter(|value| !value.is_null())
                .map(|value| serde_json::from_value(value.clone()))
                .transpose()
                .with_context(|| format!("Invalid mirror configuration in profile '{name}'"))?;
            (outputs, mirror)
        };
        let profile = Profile {
            outputs: decode_layout(outputs, &path)?,
            mirror,
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
    let saved: HashSet<_> = profile.outputs.iter().map(|r| r.name.as_str()).collect();
    let missing: Vec<_> = saved
        .iter()
        .filter(|name| !outputs.contains_key(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        bail!("Profile has missing monitors: {}", missing.join(", "));
    }
    let current = snapshot(outputs)?;
    let by_name: std::collections::HashMap<_, _> = profile
        .outputs
        .iter()
        .map(|r| (r.name.as_str(), r))
        .collect();
    let merged: Layout = current
        .into_iter()
        .map(|row| {
            by_name
                .get(row.name.as_str())
                .map(|r| (*r).clone())
                .unwrap_or(row)
        })
        .collect();
    validate_layout(&merged).context("Invalid merged profile")?;
    Ok(Profile {
        outputs: merged,
        mirror: profile.mirror.clone(),
    })
}
