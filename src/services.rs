use crate::backend::{snapshot, validate_layout, Layout, OutputConfig, Outputs};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
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
                    bail!("Corrupt config at '{}': output {index} is missing required field '{field}'", path.display());
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
    pub fn save(&self, name: &str, layout: &[OutputConfig]) -> Result<()> {
        let name = Self::normalize(name)?;
        validate_layout(layout).with_context(|| format!("Invalid profile '{name}'"))?;
        fs::create_dir_all(&self.directory)?;
        let path = self.directory.join(Self::filename(&name)?);
        let temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        ConfigStore::save(temporary.path(), layout)?;
        match fs::hard_link(temporary.path(), &path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("Profile '{name}' already exists.")
            }
            Err(e) => Err(e).with_context(|| format!("Cannot save profile '{name}'")),
        }
    }
    pub fn load(&self, name: &str) -> Result<Layout> {
        let name = Self::normalize(name)?;
        let path = self.directory.join(Self::filename(&name)?);
        ConfigStore::load(&path).map_err(|e| {
            if !path.exists() {
                anyhow!("Profile '{name}' does not exist.")
            } else {
                e
            }
        })
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

pub fn merge_profile(layout: &[OutputConfig], outputs: &Outputs) -> Result<Layout> {
    validate_layout(layout).context("Invalid profile")?;
    let saved: HashSet<_> = layout.iter().map(|r| r.name.as_str()).collect();
    let missing: Vec<_> = saved
        .iter()
        .filter(|name| !outputs.contains_key(**name))
        .copied()
        .collect();
    if !missing.is_empty() {
        bail!("Profile has missing monitors: {}", missing.join(", "));
    }
    let current = snapshot(outputs)?;
    let by_name: std::collections::HashMap<_, _> =
        layout.iter().map(|r| (r.name.as_str(), r)).collect();
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
    Ok(merged)
}

#[derive(Default)]
pub struct MirrorManager {
    child: Option<Child>,
    stderr_path: Option<PathBuf>,
    pending_error: Option<String>,
}
impl MirrorManager {
    pub fn start(&mut self, source: &str, target: &str) -> Result<()> {
        if source.trim().is_empty() {
            bail!("Mirror source output must not be empty.");
        }
        if target.trim().is_empty() {
            bail!("Mirror target output must not be empty.");
        }
        if source == target {
            bail!("Mirror source and target outputs must be different.");
        }
        if self
            .child
            .as_mut()
            .is_some_and(|p| p.try_wait().ok().flatten().is_none())
        {
            bail!("A mirror process is already running.");
        }
        self.collect_exit();
        let binary = which("wl-mirror").ok_or_else(|| anyhow!("Cannot start mirroring: 'wl-mirror' was not found. Install wl-mirror and ensure it is on PATH."))?;
        let stderr = tempfile::NamedTempFile::new().context("Cannot capture wl-mirror errors")?;
        let stderr_path = stderr
            .into_temp_path()
            .keep()
            .context("Cannot retain wl-mirror error capture")?;
        let nix = which("nixGL")
            .filter(|_| fs::canonicalize(&binary).is_ok_and(|path| path.starts_with("/nix/store")));
        let mut command = Command::new(nix.as_deref().unwrap_or(&binary));
        if nix.is_some() {
            command.arg(&binary);
        }
        command.args(["--fullscreen-output", target, source]);
        let stderr_handle = match fs::OpenOptions::new().append(true).open(&stderr_path) {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_file(&stderr_path);
                return Err(error).context("Cannot open wl-mirror error capture");
            }
        };
        let child = match command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr_handle)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(&stderr_path);
                return Err(error).context("Cannot start wl-mirror");
            }
        };
        self.stderr_path = Some(stderr_path);
        self.child = Some(child);
        Ok(())
    }
    pub fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            if child.try_wait()?.is_none() {
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
                }
                let start = std::time::Instant::now();
                loop {
                    if child.try_wait()?.is_some() {
                        break;
                    }
                    if start.elapsed() >= Duration::from_secs(2) {
                        child.kill()?;
                        child.wait()?;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        self.cleanup_stderr();
        Ok(())
    }
    pub fn running(&mut self) -> bool {
        self.collect_exit();
        self.child.is_some()
    }
    pub fn poll_error(&mut self) -> Option<String> {
        if self.pending_error.is_some() {
            return self.pending_error.take();
        }
        self.collect_exit();
        self.pending_error.take()
    }
    fn collect_exit(&mut self) {
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                let code = status.code().unwrap_or(-1);
                let detail = self
                    .stderr_path
                    .as_ref()
                    .and_then(|path| {
                        use std::io::{Read, Seek, SeekFrom};
                        let mut file = fs::File::open(path).ok()?;
                        let length = file.metadata().ok()?.len();
                        file.seek(SeekFrom::Start(length.saturating_sub(4096)))
                            .ok()?;
                        let mut bytes = Vec::with_capacity(4096);
                        file.take(4096).read_to_end(&mut bytes).ok()?;
                        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
                        Some(if length > 4096 {
                            format!("...(earlier stderr omitted)...\n{text}")
                        } else {
                            text
                        })
                    })
                    .unwrap_or_default();
                self.child = None;
                self.cleanup_stderr();
                if !status.success() {
                    self.pending_error = Some(format!("wl-mirror exited with status {code}. Check wl-mirror and display configuration.{}", if detail.is_empty() { String::new() } else { format!("\n{detail}") }));
                }
            }
        }
    }
    fn cleanup_stderr(&mut self) {
        if let Some(path) = self.stderr_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}
impl Drop for MirrorManager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn which(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}
