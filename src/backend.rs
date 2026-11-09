use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::HashSet,
    process::{Command, Stdio},
    time::Duration,
};

pub type Outputs = Map<String, Value>;
pub type Layout = Vec<OutputConfig>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub name: String,
    pub enabled: bool,
    pub mode: Option<String>,
    pub scale: f64,
    pub x: i32,
    pub y: i32,
    pub transform: String,
}

pub fn mode_string(mode: &Value) -> Result<String> {
    let width = mode
        .get("width")
        .and_then(Value::as_i64)
        .filter(|v| *v >= 0)
        .ok_or_else(|| anyhow!("Invalid mode reported by Niri: {mode}"))?;
    let height = mode
        .get("height")
        .and_then(Value::as_i64)
        .filter(|v| *v >= 0)
        .ok_or_else(|| anyhow!("Invalid mode reported by Niri: {mode}"))?;
    let refresh = mode
        .get("refresh_rate")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("Invalid mode reported by Niri: {mode}"))?;
    let hz = refresh as f64 / 1000.0;
    if !hz.is_finite() {
        bail!("Invalid mode reported by Niri: {mode}");
    }
    Ok(format!("{width}x{height}@{}", format_float(hz)))
}

fn format_float(value: f64) -> String {
    let text = value.to_string();
    if let Some(s) = text.strip_suffix(".0") {
        s.to_owned()
    } else {
        text
    }
}

fn current_mode(output: &Value) -> Result<Option<String>> {
    let Some(current) = output.get("current_mode").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if let Some(index) = current.as_u64() {
        let modes = output
            .get("modes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mode = modes.get(index as usize).ok_or_else(|| {
            anyhow!(
                "Niri reported invalid current_mode index {index} for {:?}",
                output.get("name")
            )
        })?;
        return mode_string(mode).map(Some);
    }
    if current.is_object() {
        return mode_string(current).map(Some);
    }
    bail!(
        "Niri reported invalid current_mode for {:?}",
        output.get("name")
    )
}

fn normalized_transform(value: &str) -> String {
    value
        .to_lowercase()
        .replace("flipped90", "flipped-90")
        .replace("flipped180", "flipped-180")
        .replace("flipped270", "flipped-270")
}

pub fn snapshot(outputs: &Outputs) -> Result<Layout> {
    let mut layout = Vec::with_capacity(outputs.len());
    for (name, output) in outputs {
        if !output.is_object() {
            bail!("Niri outputs contains an invalid output entry");
        }
        let logical = output.get("logical");
        let mode = current_mode(output)?;
        let enabled = logical.is_some_and(|v| !v.is_null()) && mode.is_some();
        let (x, y, scale, transform) = if enabled {
            let logical = logical.unwrap();
            let x = logical
                .get("x")
                .and_then(Value::as_i64)
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| anyhow!("Niri reported invalid logical position for {name:?}"))?;
            let y = logical
                .get("y")
                .and_then(Value::as_i64)
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| anyhow!("Niri reported invalid logical position for {name:?}"))?;
            let scale = logical
                .get("scale")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
                .ok_or_else(|| anyhow!("Niri reported invalid scale for {name:?}"))?;
            let transform = match logical.get("transform") {
                None => "Normal",
                Some(value) => value
                    .as_str()
                    .ok_or_else(|| anyhow!("Niri reported invalid transform for {name:?}"))?,
            };
            (x, y, scale, normalized_transform(transform))
        } else {
            let transform = logical
                .and_then(|v| v.get("transform"))
                .and_then(Value::as_str)
                .map(normalized_transform)
                .unwrap_or_else(|| "normal".to_owned());
            (0, 0, 1.0, transform)
        };
        layout.push(OutputConfig {
            name: name.clone(),
            enabled,
            mode,
            scale,
            x,
            y,
            transform,
        });
    }
    Ok(layout)
}

pub fn dimensions(row: &OutputConfig) -> (i32, i32) {
    let (mut width, mut height) = match row.mode.as_deref() {
        None => (1920.0, 1080.0),
        Some(mode) => {
            let Some((size, _)) = mode.split_once('@') else {
                return (0, 0);
            };
            let Some((width, height)) = size.split_once('x') else {
                return (0, 0);
            };
            let (Ok(width), Ok(height)) = (width.parse::<f64>(), height.parse::<f64>()) else {
                return (0, 0);
            };
            (width, height)
        }
    };
    if !row.scale.is_finite() || row.scale <= 0.0 {
        return (0, 0);
    }
    if ["90", "270", "flipped-90", "flipped-270"].contains(&row.transform.as_str()) {
        std::mem::swap(&mut width, &mut height);
    }
    (
        (width / row.scale).ceil().max(1.0) as i32,
        (height / row.scale).ceil().max(1.0) as i32,
    )
}

pub fn validate_layout(layout: &[OutputConfig]) -> Result<()> {
    if layout.is_empty() {
        bail!("Layout must contain at least one output.");
    }
    let mut names = HashSet::new();
    let mut enabled = false;
    for (index, row) in layout.iter().enumerate() {
        let location = format!("Config output {index}");
        if row.name.trim().is_empty() {
            bail!("{location} field 'name' must not be empty.");
        }
        if !names.insert(&row.name) {
            bail!("{location} has duplicate output name '{}'.", row.name);
        }
        if !(-100000..=100000).contains(&row.x) {
            bail!("{location} field 'x' must be between -100000 and 100000.");
        }
        if !(-100000..=100000).contains(&row.y) {
            bail!("{location} field 'y' must be between -100000 and 100000.");
        }
        if !row.scale.is_finite() || !(0.1..=10.0).contains(&row.scale) {
            bail!("{location} field 'scale' must be between 0.1 and 10.");
        }
        if let Some(mode) = &row.mode {
            validate_mode(mode, true).with_context(|| format!("{location} field 'mode'"))?;
        }
        if ![
            "normal",
            "90",
            "180",
            "270",
            "flipped",
            "flipped-90",
            "flipped-180",
            "flipped-270",
        ]
        .contains(&row.transform.as_str())
        {
            bail!("{location} field 'transform' is invalid.");
        }
        enabled |= row.enabled;
    }
    if !enabled {
        bail!("Config must have at least one enabled output.");
    }
    Ok(())
}

fn validate_mode(mode: &str, config: bool) -> Result<(u32, u32, f64)> {
    let (dimensions, refresh) = mode
        .split_once('@')
        .ok_or_else(|| anyhow!("Invalid mode: {mode}"))?;
    let (width, height) = dimensions
        .split_once('x')
        .ok_or_else(|| anyhow!("Invalid mode: {mode}"))?;
    let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !decimal(width)
        || !decimal(height)
        || refresh.is_empty()
        || refresh.starts_with('.')
        || refresh.ends_with('.')
        || refresh.matches('.').count() > 1
        || !refresh.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        bail!("Invalid mode: {mode}");
    }
    let (Ok(width), Ok(height), Ok(refresh)) = (
        width.parse::<u32>(),
        height.parse::<u32>(),
        refresh.parse::<f64>(),
    ) else {
        bail!("Invalid mode: {mode}");
    };
    if width == 0 || height == 0 || !refresh.is_finite() || refresh <= 0.0 {
        bail!("Mode dimensions and refresh rate must be positive and finite");
    }
    if config && (width > 100000 || height > 100000 || !(0.001..=1000.0).contains(&refresh)) {
        bail!("Mode is outside supported dimensions and refresh rate");
    }
    Ok((width, height, refresh))
}

fn run_niri(args: &[&str]) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = Command::new("niri")
        .arg("msg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()?;
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Niri command timed out: {}", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut output = Vec::new();
    let mut errors = Vec::new();
    stdout.seek(SeekFrom::Start(0))?;
    stdout.read_to_end(&mut output)?;
    stderr.seek(SeekFrom::Start(0))?;
    stderr.read_to_end(&mut errors)?;
    Ok((status, output, errors))
}
#[derive(Debug, Clone, Default)]
pub struct NiriClient;
impl NiriClient {
    fn run(&self, args: &[&str]) -> Result<String> {
        let rendered = args.join(" ");
        let (status, stdout, stderr) =
            run_niri(args).with_context(|| format!("Could not run Niri command: {rendered}"))?;
        if !status.success() {
            bail!(
                "Niri command failed ({rendered}): {}",
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        String::from_utf8(stdout).context("Niri returned non-UTF-8 output")
    }
    pub fn outputs(&self) -> Result<Outputs> {
        let text = self.run(&["--json", "outputs"])?;
        let value: Value =
            serde_json::from_str(&text).context("Niri returned invalid outputs JSON")?;
        value
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow!("Niri outputs response was not a JSON object"))
    }
    pub fn prepare_mirror(&self, source: &str, target: &str) -> Result<Outputs> {
        let mut outputs = self.outputs()?;
        if outputs.len() < 2 {
            bail!("Mirroring requires at least two connected displays.");
        }
        if source == target {
            bail!("Mirror source and target outputs must be different.");
        }
        for name in [source, target] {
            if !outputs.contains_key(name) {
                bail!("Output is not connected: {name}");
            }
        }
        let active = |outputs: &Outputs, name: &str| {
            outputs.get(name).is_some_and(|output| {
                output.get("logical").is_some_and(|value| !value.is_null())
            })
        };
        if !active(&outputs, source) {
            bail!("{source} is not active. Enable the source and apply its display settings before mirroring.");
        }
        if !active(&outputs, target) {
            self.command(&["output", target, "on"])?;
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                outputs = self.outputs()?;
                if !active(&outputs, source) {
                    bail!("Mirror source {source} is no longer active.");
                }
                if active(&outputs, target) {
                    break;
                }
                if !outputs.contains_key(target) {
                    bail!("Output is not connected: {target}");
                }
                if std::time::Instant::now() >= deadline {
                    bail!("Mirror target {target} did not become active.");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        Ok(outputs)
    }
    pub fn apply(&self, layout: &[OutputConfig]) -> Result<()> {
        if layout.is_empty() {
            bail!("Layout must be a non-empty list");
        }
        let connected = self.outputs()?;
        let mut seen = HashSet::new();
        let mut enabled_count = 0;
        for row in layout {
            if row.name.is_empty() {
                bail!("Each layout entry needs a non-empty output name");
            }
            if !seen.insert(row.name.as_str()) {
                bail!("Duplicate output in layout: {}", row.name);
            }
            if !connected.contains_key(&row.name) {
                bail!("Output is not connected: {}", row.name);
            }
            if row.enabled {
                enabled_count += 1;
                if let Some(mode) = &row.mode {
                    validate_mode(mode, false)
                        .with_context(|| format!("Invalid mode for {:?}", row.name))?;
                }
                if !row.scale.is_finite() || !(0.1..=10.0).contains(&row.scale) {
                    bail!(
                        "Scale for {:?} must be finite and between 0.1 and 10",
                        row.name
                    );
                }
                if ![
                    "normal",
                    "90",
                    "180",
                    "270",
                    "flipped",
                    "flipped-90",
                    "flipped-180",
                    "flipped-270",
                ]
                .contains(&row.transform.to_lowercase().as_str())
                {
                    bail!("Invalid transform for {:?}: {}", row.name, row.transform);
                }
            }
        }
        let missing: Vec<_> = connected
            .keys()
            .filter(|name| !seen.contains(name.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            bail!(
                "Layout must include every connected output: {}",
                missing.join(", ")
            );
        }
        if enabled_count == 0 {
            bail!("At least one output must remain enabled");
        }
        for row in layout.iter().filter(|row| row.enabled) {
            self.command(&["output", &row.name, "on"])?;
            if let Some(mode) = &row.mode {
                let output = &connected[&row.name];
                let modes = output
                    .get("modes")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let available = modes
                    .iter()
                    .filter_map(|m| mode_string(m).ok())
                    .any(|m| m == *mode);
                let custom = !available
                    || (output.get("is_custom_mode") == Some(&Value::Bool(true))
                        && current_mode(output)?.as_deref() == Some(mode));
                self.command(&[
                    "output",
                    &row.name,
                    if custom { "custom-mode" } else { "mode" },
                    mode,
                ])?;
            }
            self.command(&["output", &row.name, "scale", &format_float(row.scale)])?;
            self.command(&[
                "output",
                &row.name,
                "transform",
                &row.transform.to_lowercase(),
            ])?;
            self.command(&[
                "output",
                &row.name,
                "position",
                "set",
                "--",
                &row.x.to_string(),
                &row.y.to_string(),
            ])?;
        }
        for row in layout.iter().filter(|row| !row.enabled) {
            self.command(&["output", &row.name, "off"])?;
        }
        Ok(())
    }
    fn command(&self, args: &[&str]) -> Result<()> {
        let rendered = args.join(" ");
        let (status, _stdout, stderr) =
            run_niri(args).with_context(|| format!("Could not run Niri command: {rendered}"))?;
        if !status.success() {
            bail!(
                "Niri command failed ({rendered}): {}",
                String::from_utf8_lossy(&stderr).trim()
            );
        }
        Ok(())
    }
}
