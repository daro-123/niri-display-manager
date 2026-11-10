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

#[derive(Serialize, Deserialize, Default)]
struct State {
    pid: Option<u32>,
    start_time: Option<u64>,
    config: Option<MirrorConfig>,
    pending_error: Option<String>,
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
        if source.trim().is_empty() {
            bail!("Mirror source output must not be empty.");
        }
        if target.trim().is_empty() {
            bail!("Mirror target output must not be empty.");
        }
        if source == target {
            bail!("Mirror source and target outputs must be different.");
        }
        let (_lock, path) = self.open()?;
        let mut state = Self::read_state(&path)?;
        Self::stop_state(&self.directory, &mut state)?;
        state.pending_error = None;
        Self::write_state(&path, &state)?;
        let binary = which("wl-mirror").ok_or_else(|| anyhow!("Cannot start mirroring: 'wl-mirror' was not found. Install wl-mirror and ensure it is on PATH."))?;
        let nix = which("nixGL")
            .filter(|_| fs::canonicalize(&binary).is_ok_and(|p| p.starts_with("/nix/store")));
        let log_path = self.directory.join("stderr.log");
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
        command.args(["--fullscreen-output", target, source]);
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
        let identity =
            proc_identity(pid).filter(|identity| identity.group == pid && identity.session == pid);
        let started = (|| -> Result<()> {
            let identity =
                identity.ok_or_else(|| anyhow!("Cannot identify managed wl-mirror process."))?;
            let until = Instant::now() + STARTUP_WAIT;
            loop {
                if let Some(status) = child.try_wait()? {
                    bail!("wl-mirror exited during startup ({status}).");
                }
                if !proc_identity(pid).is_some_and(|current| {
                    current.start_time == identity.start_time
                        && current.group == pid
                        && current.session == pid
                        && current.alive
                }) {
                    bail!("wl-mirror exited during startup.");
                }
                if Instant::now() >= until {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            state.pid = Some(pid);
            state.start_time = Some(identity.start_time);
            state.config = Some(MirrorConfig {
                source: source.to_owned(),
                target: target.to_owned(),
            });
            Self::write_state(&path, &state)
        })();
        if let Err(error) = started {
            if identity.is_some_and(|identity| {
                proc_identity(pid).is_some_and(|current| {
                    current.start_time == identity.start_time
                        && current.group == pid
                        && current.session == pid
                })
            }) {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill();
            let _ = child.wait();
            let detail = Self::stderr_detail(&log_path);
            return Err(error).context(if detail.is_empty() {
                "Cannot start mirroring".to_owned()
            } else {
                format!("Cannot start mirroring:\n{detail}")
            });
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        let (_lock, path) = self.open()?;
        let mut state = Self::read_state(&path)?;
        Self::stop_state(&self.directory, &mut state)?;
        state.pending_error = None;
        Self::write_state(&path, &state)
    }

    fn stop_state(directory: &Path, state: &mut State) -> Result<()> {
        if let (Some(pid), Some(start)) = (state.pid, state.start_time) {
            if proc_identity(pid)
                .is_some_and(|i| i.start_time == start && i.group == pid && i.session == pid)
            {
                let group = -(pid as i32);
                if unsafe { libc::kill(group, libc::SIGTERM) } != 0
                    && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(std::io::Error::last_os_error())
                        .context("Cannot stop managed wl-mirror process group");
                }
                let until = Instant::now() + STOP_WAIT;
                while Instant::now() < until {
                    if !group_exists(pid) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                if group_exists(pid) {
                    if proc_identity(pid).is_some_and(|i| {
                        i.start_time == start && i.group == pid && i.session == pid
                    }) {
                        if unsafe { libc::kill(group, libc::SIGKILL) } != 0
                            && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                        {
                            return Err(std::io::Error::last_os_error())
                                .context("Cannot kill managed wl-mirror process group");
                        }
                    } else {
                        bail!("Managed wl-mirror process identity changed; refusing to signal process group.");
                    }
                }
                let until = Instant::now() + STOP_WAIT;
                loop {
                    if !group_exists(pid) {
                        break;
                    }
                    if Instant::now() >= until {
                        bail!("Managed wl-mirror process group did not stop.");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                reap_child(pid);
            } else if state.config.is_some() {
                let detail = Self::stderr_detail(&directory.join("stderr.log"));
                state.pending_error = Some(format!(
                    "wl-mirror exited unexpectedly.{}",
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!("\n{detail}")
                    }
                ));
            }
        }
        state.pid = None;
        state.start_time = None;
        state.config = None;
        Ok(())
    }

    pub fn current(&mut self) -> Result<Option<MirrorConfig>> {
        let (_lock, path) = self.open()?;
        let mut state = Self::read_state(&path)?;
        if let (Some(pid), Some(start)) = (state.pid, state.start_time) {
            match proc_identity(pid) {
                Some(identity)
                    if identity.start_time == start
                        && identity.group == pid
                        && identity.session == pid
                        && identity.alive =>
                {
                    return Ok(state.config);
                }
                _ => {
                    reap_child(pid);
                    let detail = Self::stderr_detail(&self.directory.join("stderr.log"));
                    state.pid = None;
                    state.start_time = None;
                    state.config = None;
                    state.pending_error = Some(format!(
                        "wl-mirror exited unexpectedly.{}",
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!("\n{detail}")
                        }
                    ));
                    Self::write_state(&path, &state)?;
                }
            }
        }
        Ok(None)
    }

    pub fn running(&mut self) -> bool {
        self.current().ok().flatten().is_some()
    }

    pub fn poll_error(&mut self) -> Option<String> {
        let _ = self.current();
        let (_lock, path) = self.open().ok()?;
        let mut state = Self::read_state(&path).ok()?;
        let error = state.pending_error.take()?;
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
