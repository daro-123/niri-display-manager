use crate::{
    backend::{Layout, NiriClient, OutputConfig, Outputs},
    services::ConfigStore,
};
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const SERVICE: &str = "niri-display-daemon.service";
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
}
fn refresh_niri_socket() -> bool {
    use std::os::unix::fs::FileTypeExt;
    let current = std::env::var_os("NIRI_SOCKET");
    if current
        .as_ref()
        .is_some_and(|path| fs::metadata(path).is_ok_and(|m| m.file_type().is_socket()))
    {
        return false;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) else {
        return false;
    };
    let display = std::env::var("WAYLAND_DISPLAY").ok();
    let prefix = display
        .map(|v| format!("niri.{v}."))
        .unwrap_or_else(|| "niri.".to_owned());
    let Ok(entries) = fs::read_dir(runtime) else {
        return false;
    };
    let candidates: Vec<_> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .filter(|path| fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket()))
        .collect();
    if candidates.len() != 1 {
        return false;
    }
    std::env::set_var("NIRI_SOCKET", &candidates[0]);
    true
}
#[derive(Clone)]
pub struct LayoutMemory {
    directory: PathBuf,
}
impl Default for LayoutMemory {
    fn default() -> Self {
        Self::new(config_home().join("niri-display").join("layouts"))
    }
}
impl LayoutMemory {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    pub fn path(&self, names: impl IntoIterator<Item = String>) -> PathBuf {
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        // Match Python json.dumps(sorted(names), ensure_ascii=True), including default separators.
        let escaped = names
            .iter()
            .map(|name| format!("\"{}\"", python_escape(name)))
            .collect::<Vec<_>>()
            .join(", ");
        let bytes = format!("[{escaped}]");
        let digest = Sha256::digest(bytes.as_bytes());
        let hash = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.directory.join(format!("{hash}.json"))
    }
    pub fn remember(&self, layout: &[OutputConfig]) -> Result<()> {
        ConfigStore::save(&self.path(layout.iter().map(|r| r.name.clone())), layout)
    }
    pub fn recall(&self, names: impl IntoIterator<Item = String>) -> Result<Option<Layout>> {
        let names: HashSet<String> = names.into_iter().collect();
        let path = self.path(names.iter().cloned());
        if !path.exists() {
            return Ok(None);
        }
        let layout = ConfigStore::load(&path)?;
        if layout
            .iter()
            .map(|r| r.name.clone())
            .collect::<HashSet<_>>()
            != names
        {
            bail!(
                "Saved layout does not match connected outputs: {}",
                path.display()
            );
        }
        Ok(Some(layout))
    }
}
fn python_escape(value: &str) -> String {
    let mut out = String::new();
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if c.is_ascii() && c <= '~' => out.push(c),
            c if (c as u32) <= 0xffff => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => {
                let n = c as u32 - 0x10000;
                let high = 0xd800 + (n >> 10);
                let low = 0xdc00 + (n & 0x3ff);
                out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    out
}

#[derive(Clone)]
pub struct RestoreDaemon {
    client: NiriClient,
    memory: LayoutMemory,
    connected: Option<HashSet<String>>,
    observed: Option<Layout>,
    restore_failed: bool,
}
impl RestoreDaemon {
    pub fn new(client: NiriClient, memory: LayoutMemory) -> Self {
        Self {
            client,
            memory,
            connected: None,
            observed: None,
            restore_failed: false,
        }
    }
    pub fn poll(&mut self) -> Result<()> {
        let outputs: Outputs = self.client.outputs()?;
        let names: HashSet<String> = outputs.keys().cloned().collect();
        let previous = crate::backend::snapshot(&outputs)?;
        if self.connected.as_ref() == Some(&names) {
            if !self.restore_failed && previous.iter().any(|row| row.enabled) {
                self.observed = Some(previous);
            }
            return Ok(());
        }
        if self.connected.is_some() {
            if let Some(observed) = &self.observed {
                self.memory.remember(observed)?;
            }
        }
        self.connected = Some(names.clone());
        self.observed = None;
        self.restore_failed = false;
        let desired = self.memory.recall(names.iter().cloned())?;
        let Some(desired) = desired else {
            if previous.len() == 1
                && ["eDP-", "LVDS-", "DSI-"]
                    .iter()
                    .any(|p| previous[0].name.starts_with(p))
                && !previous[0].enabled
            {
                let mut recovered = previous.clone();
                recovered[0].enabled = true;
                self.client.apply(&recovered)?;
                self.observed = Some(recovered);
            } else if previous.iter().any(|row| row.enabled) {
                self.observed = Some(previous);
            }
            return Ok(());
        };
        let mut left = previous.clone();
        let mut right = desired.clone();
        left.sort_by(|a, b| a.name.cmp(&b.name));
        right.sort_by(|a, b| a.name.cmp(&b.name));
        if left == right {
            self.observed = Some(previous);
            return Ok(());
        }
        self.restore_failed = true;
        if let Err(error) = self.client.apply(&desired) {
            if let Err(rollback) = self.client.apply(&previous) {
                return Err(anyhow!(
                    "Restore failed: {error:#}. Rollback failed: {rollback:#}"
                ));
            }
            return Err(anyhow!(
                "Restore failed; previous layout restored: {error:#}"
            ));
        }
        self.restore_failed = false;
        self.observed = Some(desired);
        Ok(())
    }
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    use std::io::{Read, Seek, SeekFrom};
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()
        .context("Cannot control user daemon")?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("Cannot wait for user daemon command")?
        {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Cannot control user daemon: command timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.seek(SeekFrom::Start(0))?;
    stdout.read_to_end(&mut out)?;
    stderr.seek(SeekFrom::Start(0))?;
    stderr.read_to_end(&mut err)?;
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}
pub fn enabled() -> Result<bool> {
    Ok(String::from_utf8_lossy(&systemctl(&["is-enabled", SERVICE])?.stdout).trim() == "enabled")
}
fn checked(args: &[&str]) -> Result<()> {
    let out = systemctl(args)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "{}",
            if !err.trim().is_empty() {
                err.trim()
            } else if !stdout.trim().is_empty() {
                stdout.trim()
            } else {
                "User daemon command failed."
            }
        );
    }
    Ok(())
}
fn unit_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    )
}
pub fn set_enabled(active: bool) -> Result<()> {
    if !active {
        checked(&["disable", "--now", SERVICE])?;
        return Ok(());
    }
    let unit = config_home().join("systemd/user").join(SERVICE);
    fs::create_dir_all(unit.parent().unwrap())?;
    let exe = std::env::current_exe().context("Cannot locate daemon executable")?;
    let exe = exe.to_string_lossy();
    let command = [unit_quote(&exe), unit_quote("--daemon")].join(" ");
    fs::write(&unit, format!("[Unit]\nDescription=Restore saved Niri display layouts\n\n[Service]\nType=simple\nExecStart={command}\n\n[Install]\nWantedBy=default.target\n"))?;
    checked(&["daemon-reload"])?;
    let variables: Vec<&str> = ["NIRI_SOCKET", "WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    if !variables.is_empty() {
        let mut args = vec!["import-environment"];
        args.extend(variables);
        checked(&args)?;
    }
    checked(&["reenable", SERVICE])?;
    checked(&["start", SERVICE])
}

pub fn run() -> Result<()> {
    let mut daemon = RestoreDaemon::new(NiriClient, LayoutMemory::default());
    loop {
        if refresh_niri_socket() {
            daemon.connected = None;
        }
        if let Err(error) = daemon.poll() {
            eprintln!("ERROR: {error}");
        }
        thread::sleep(Duration::from_secs(2));
    }
}
