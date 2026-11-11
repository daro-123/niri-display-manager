use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const LOG_LIMIT: u64 = 4096;
const STARTUP_WAIT: Duration = Duration::from_millis(250);
const STOP_WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorConfig {
    pub source: String,
    pub target: String,
}

#[derive(Clone, Serialize, Deserialize, Default)]
struct Session {
    pid: Option<u32>,
    start_time: Option<u64>,
    config: Option<MirrorConfig>,
}

#[derive(Serialize, Default)]
struct State {
    sessions: Vec<Session>,
    pending_errors: Vec<String>,
}

#[derive(Deserialize, Default)]
struct LegacyState {
    pid: Option<u32>,
    start_time: Option<u64>,
    config: Option<MirrorConfig>,
    pending_error: Option<String>,
}

impl<'de> Deserialize<'de> for State {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Multiple {
                sessions: Vec<Session>,
                #[serde(default)]
                pending_errors: Vec<String>,
            },
            Single(LegacyState),
        }
        Ok(match Stored::deserialize(deserializer)? {
            Stored::Multiple {
                sessions,
                pending_errors,
            } => Self {
                sessions,
                pending_errors,
            },
            Stored::Single(old) => {
                let mut state = Self::default();
                if old.pid.is_some() || old.start_time.is_some() || old.config.is_some() {
                    state.sessions.push(Session {
                        pid: old.pid,
                        start_time: old.start_time,
                        config: old.config,
                    });
                }
                state.pending_errors.extend(old.pending_error);
                state
            }
        })
    }
}

pub struct MirrorManager {
    directory: PathBuf,
}

impl Default for MirrorManager {
    fn default() -> Self {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
        Self::new(runtime.join("niri-display").join("mirror-private"))
    }
}

impl MirrorManager {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn open(&self) -> Result<(File, PathBuf)> {
        if let Some(parent) = self.directory.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Cannot create mirror runtime directory '{}'.",
                    parent.display()
                )
            })?;
        }
        match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || metadata.uid() != unsafe { libc::getuid() }
                    || metadata.mode() & 0o077 != 0
                {
                    bail!("Mirror runtime directory is not private or is not owned by this user.");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder.create(&self.directory)?;
            }
            Err(error) => return Err(error).context("Cannot inspect mirror runtime directory"),
        }
        let lock_path = self.directory.join("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(lock_path)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot lock mirror runtime state");
        }
        Ok((lock, self.directory.join("state.json")))
    }

    fn read_state(path: &Path) -> Result<State> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("Invalid mirror runtime state"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error).context("Cannot read mirror runtime state"),
        }
    }

    fn write_state(path: &Path, state: &State) -> Result<()> {
        let temp = path.with_extension("tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        serde_json::to_writer(&mut file, state)?;
        use std::io::Write;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(temp, path)?;
        Ok(())
    }

    pub fn start(&mut self, source: &str, target: &str) -> Result<()> {
        self.start_all(&[MirrorConfig {
            source: source.to_owned(),
            target: target.to_owned(),
        }])
    }

    pub fn start_all(&mut self, configs: &[MirrorConfig]) -> Result<()> {
        if configs.is_empty() {
            bail!("At least one mirror target is required.");
        }
        for config in configs {
            if config.source.trim().is_empty() {
                bail!("Mirror source output must not be empty.");
            }
            if config.target.trim().is_empty() {
                bail!("Mirror target output must not be empty.");
            }
            if config.source == config.target {
                bail!("Mirror source and target outputs must be different.");
            }
        }
        let (_lock, path) = self.open()?;
        let mut state = Self::read_state(&path)?;
        Self::stop_state(&self.directory, &mut state)?;
        state.pending_errors.clear();
        Self::write_state(&path, &state)?;
        let result = (|| -> Result<()> {
            for config in configs {
                let session = Self::start_session(&self.directory, config)?;
                state.sessions.push(session);
                Self::write_state(&path, &state)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let cleanup = Self::stop_state(&self.directory, &mut state);
            let _ = Self::write_state(&path, &state);
            return match cleanup {
                Ok(()) => Err(error).context("Cannot start all mirror targets"),
                Err(cleanup_error) => Err(error).context(format!(
                    "Cannot start all mirror targets; cleanup failed: {cleanup_error:#}"
                )),
            };
        }
        Ok(())
    }

    fn start_session(directory: &Path, config: &MirrorConfig) -> Result<Session> {
        let binary = which("wl-mirror").ok_or_else(|| anyhow!("Cannot start mirroring: 'wl-mirror' was not found. Install wl-mirror and ensure it is on PATH."))?;
        let nix = which("nixGL")
            .filter(|_| fs::canonicalize(&binary).is_ok_and(|p| p.starts_with("/nix/store")));
        let log_path = Self::log_path(directory, config);
        let stderr = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&log_path)
            .context("Cannot capture wl-mirror errors")?;
        let mut command = Command::new(nix.as_deref().unwrap_or(&binary));
        if nix.is_some() {
            command.arg(&binary);
        }
        command.args(["--fullscreen-output", &config.target, &config.source]);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .context("Cannot start wl-mirror")?;
        let pid = child.id();
        let identity = proc_identity(pid).filter(|i| i.group == pid && i.session == pid);
        let started = (|| -> Result<Session> {
            let identity =
                identity.ok_or_else(|| anyhow!("Cannot identify managed wl-mirror process."))?;
            let until = Instant::now() + STARTUP_WAIT;
            loop {
                if let Some(status) = child.try_wait()? {
                    bail!("wl-mirror exited during startup ({status}).");
                }
                if !proc_identity(pid).is_some_and(|i| {
                    i.start_time == identity.start_time
                        && i.group == pid
                        && i.session == pid
                        && i.alive
                }) {
                    bail!("wl-mirror exited during startup.");
                }
                if Instant::now() >= until {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Ok(Session {
                pid: Some(pid),
                start_time: Some(identity.start_time),
                config: Some(config.clone()),
            })
        })();
        match started {
            Ok(session) => Ok(session),
            Err(error) => {
                if identity.is_some_and(|i| {
                    proc_identity(pid).is_some_and(|c| {
                        c.start_time == i.start_time && c.group == pid && c.session == pid
                    })
                }) {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                let _ = child.kill();
                let _ = child.wait();
                let detail = Self::stderr_detail(&log_path);
                Err(error).context(if detail.is_empty() {
                    "Cannot start mirroring".to_owned()
                } else {
                    format!("Cannot start mirroring:\n{detail}")
                })
            }
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        let (_lock, path) = self.open()?;
        let mut state = Self::read_state(&path)?;
        Self::stop_state(&self.directory, &mut state)?;
        state.pending_errors.clear();
        Self::write_state(&path, &state)
    }

    fn stop_state(directory: &Path, state: &mut State) -> Result<()> {
        let mut first_error = None;
        for session in &mut state.sessions {
            if let (Some(pid), Some(start)) = (session.pid, session.start_time) {
                if proc_identity(pid)
                    .is_some_and(|i| i.start_time == start && i.group == pid && i.session == pid)
                {
                    let group = -(pid as i32);
                    if unsafe { libc::kill(group, libc::SIGTERM) } != 0
                        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                    {
                        first_error.get_or_insert_with(|| {
                            anyhow!(std::io::Error::last_os_error())
                                .context("Cannot stop managed wl-mirror process group")
                        });
                        continue;
                    }
                    let until = Instant::now() + STOP_WAIT;
                    while Instant::now() < until && group_exists(pid) {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if group_exists(pid) {
                        if proc_identity(pid).is_some_and(|i| {
                            i.start_time == start && i.group == pid && i.session == pid
                        }) {
                            if unsafe { libc::kill(group, libc::SIGKILL) } != 0
                                && std::io::Error::last_os_error().raw_os_error()
                                    != Some(libc::ESRCH)
                            {
                                first_error.get_or_insert_with(|| {
                                    anyhow!(std::io::Error::last_os_error())
                                        .context("Cannot kill managed wl-mirror process group")
                                });
                                continue;
                            }
                        } else {
                            first_error.get_or_insert_with(|| anyhow!("Managed wl-mirror process identity changed; refusing to signal process group."));
                            continue;
                        }
                    }
                    let until = Instant::now() + STOP_WAIT;
                    while group_exists(pid) && Instant::now() < until {
                        thread::sleep(Duration::from_millis(20));
                    }
                    if group_exists(pid) {
                        first_error.get_or_insert_with(|| {
                            anyhow!("Managed wl-mirror process group did not stop.")
                        });
                        continue;
                    }
                    reap_child(pid);
                } else if session.config.is_some() {
                    let log_path = Self::log_path(directory, session.config.as_ref().unwrap());
                    let detail = Self::stderr_detail(&log_path);
                    state.pending_errors.push(format!(
                        "wl-mirror exited unexpectedly.{}",
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!("\n{detail}")
                        }
                    ));
                }
            }
            session.pid = None;
            session.start_time = None;
            session.config = None;
        }
        state
            .sessions
            .retain(|s| s.pid.is_some() || s.config.is_some());
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    fn log_path(directory: &Path, config: &MirrorConfig) -> PathBuf {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        config.target.hash(&mut hasher);
        directory.join(format!("stderr-{:016x}.log", hasher.finish()))
    }

    pub fn current_all(&mut self) -> Result<Vec<MirrorConfig>> {
        let (_lock, path) = self.open()?;
        let mut state = Self::read_state(&path)?;
        let mut active = Vec::new();
        let mut changed = false;
        for session in &mut state.sessions {
            if let (Some(pid), Some(start)) = (session.pid, session.start_time) {
                if proc_identity(pid).is_some_and(|i| {
                    i.start_time == start && i.group == pid && i.session == pid && i.alive
                }) {
                    if let Some(config) = session.config.clone() {
                        active.push(config);
                    }
                    continue;
                }
                reap_child(pid);
                let detail = session
                    .config
                    .as_ref()
                    .map(|c| Self::stderr_detail(&Self::log_path(&self.directory, c)))
                    .unwrap_or_default();
                state.pending_errors.push(format!(
                    "wl-mirror exited unexpectedly.{}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!("\n{detail}")
                    }
                ));
                session.pid = None;
                session.start_time = None;
                session.config = None;
                changed = true;
            }
        }
        state.sessions.retain(|s| s.pid.is_some());
        if changed {
            Self::write_state(&path, &state)?;
        }
        Ok(active)
    }

    pub fn poll_error(&mut self) -> Option<String> {
        let _ = self.current_all();
        let (_lock, path) = self.open().ok()?;
        let mut state = Self::read_state(&path).ok()?;
        if state.pending_errors.is_empty() {
            return None;
        }
        let error = state.pending_errors.remove(0);
        let _ = Self::write_state(&path, &state);
        Some(error)
    }

    fn stderr_detail(path: &Path) -> String {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(_) => return String::new(),
        };
        let length = file.metadata().ok().map(|m| m.len()).unwrap_or(0);
        if file
            .seek(SeekFrom::Start(length.saturating_sub(LOG_LIMIT)))
            .is_err()
        {
            return String::new();
        }
        let mut bytes = Vec::with_capacity(LOG_LIMIT as usize);
        if file.take(LOG_LIMIT).read_to_end(&mut bytes).is_err() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        if length > LOG_LIMIT {
            format!("...(earlier stderr omitted)...\n{text}")
        } else {
            text
        }
    }
}

#[derive(Clone, Copy)]
struct ProcIdentity {
    start_time: u64,
    alive: bool,
    group: u32,
    session: u32,
}

fn proc_identity(pid: u32) -> Option<ProcIdentity> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = text.rsplit_once(") ")?.1;
    let mut fields = tail.split_whitespace();
    let state = fields.next()?;
    fields.next()?;
    let group = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    for _ in 4..19 {
        fields.next()?;
    }
    let start_time = fields.next()?.parse().ok()?;
    Some(ProcIdentity {
        start_time,
        alive: state != "Z" && state != "X",
        group,
        session,
    })
}

fn group_exists(pid: u32) -> bool {
    if proc_identity(pid)
        .is_some_and(|identity| identity.alive && identity.group == pid && identity.session == pid)
    {
        return true;
    }
    if unsafe { libc::kill(-(pid as i32), 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return false;
    }
    // A zombie leader keeps kill(0) succeeding until its parent reaps it.
    // Only live group members require more time or a termination signal.
    let Ok(entries) = fs::read_dir("/proc") else {
        return true;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .and_then(proc_identity)
            .is_some_and(|identity| {
                identity.alive && identity.group == pid && identity.session == pid
            })
    })
}

fn reap_child(pid: u32) {
    unsafe {
        libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
    }
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0))
}
