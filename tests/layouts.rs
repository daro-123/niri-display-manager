use niri_display::backend::{dimensions, snapshot, Layout, NiriClient, OutputConfig, Outputs};
use niri_display::daemon::{LayoutMemory, RestoreDaemon};
use niri_display::services::{merge_profile, ConfigStore, ProfileStore};
use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

fn output(active: bool, transform: &str) -> Value {
    json!({"modes":[{"width":1920,"height":1080,"refresh_rate":60000}],
        "current_mode":if active {json!(0)} else {Value::Null},
        "logical":if active {json!({"x":0,"y":0,"scale":1.0,"transform":transform})} else {Value::Null}})
}

fn outputs(entries: &[(&str, bool, &str)]) -> Outputs {
    entries
        .iter()
        .map(|(name, active, transform)| ((*name).to_owned(), output(*active, transform)))
        .collect()
}

fn layout() -> Layout {
    snapshot(&outputs(&[
        ("DP-1", true, "Normal"),
        ("DP-2", false, "Normal"),
    ]))
    .unwrap()
}

#[test]
fn fractional_geometry_rounds_outward_after_rotation() {
    let row = |transform: &str| OutputConfig {
        name: "DP-1".into(),
        enabled: true,
        mode: Some("2560x1440@59.998".into()),
        scale: 1.65,
        x: 0,
        y: 0,
        transform: transform.into(),
    };
    assert_eq!(dimensions(&row("normal")), (1552, 873));
    assert_eq!(dimensions(&row("90")), (873, 1552));
    assert_eq!(dimensions(&row("flipped-270")), (873, 1552));
    let mut automatic = row("normal");
    automatic.mode = None;
    automatic.scale = 1.0;
    assert_eq!(dimensions(&automatic), (1920, 1080));
}

#[test]
fn config_round_trip_preserves_disabled_outputs_and_negative_positions() {
    let mut layout = layout();
    layout[0].x = -1920;
    layout[0].y = -100;
    layout[0].scale = 1.25;
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("nested/display.json");
    ConfigStore::save(&path, &layout).unwrap();
    assert_eq!(ConfigStore::load(&path).unwrap(), layout);
}

#[test]
fn config_accepts_supported_boundaries() {
    let mut layout = layout();
    layout[0].x = -100000;
    layout[0].y = 100000;
    layout[0].mode = Some("100000x100000@1000".into());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bounds.json");
    ConfigStore::save(&path, &layout).unwrap();
    assert_eq!(ConfigStore::load(&path).unwrap(), layout);
}

#[test]
fn corrupt_json_duplicate_outputs_and_all_disabled_configs_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("broken.json");
    fs::write(&path, "{").unwrap();
    assert!(ConfigStore::load(&path).is_err());
    let mut rows = layout();
    rows.push(rows[0].clone());
    fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
    assert!(ConfigStore::load(&path).is_err());
    let mut rows = layout();
    for row in &mut rows {
        row.enabled = false;
    }
    fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
    assert!(ConfigStore::load(&path).is_err());
}

#[test]
fn imported_configs_reject_out_of_bounds_and_malformed_modes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid.json");
    for mode in [
        "100001x1080@60",
        "1920x100001@60",
        "1920x1080@1000.001",
        "1920x1080@0.0009",
        "1920x1080@.5",
        "1920x1080@60.",
        "1920x1080@",
        "1920x1080@abc",
        "0x600@60",
    ] {
        let mut rows = layout();
        rows[0].mode = Some(mode.into());
        fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
        assert!(ConfigStore::load(&path).is_err(), "accepted {mode}");
    }
    let huge = "9".repeat(310);
    for mode in [format!("{huge}x1080@60"), format!("1920x{huge}@60")] {
        let mut rows = layout();
        rows[0].mode = Some(mode);
        fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
        assert!(ConfigStore::load(&path).is_err());
    }
    for (field, value) in [("x", 100001), ("x", -100001), ("y", 100001), ("y", -100001)] {
        let mut value_rows = serde_json::to_value(layout()).unwrap();
        value_rows[0][field] = json!(value);
        fs::write(&path, serde_json::to_vec(&value_rows).unwrap()).unwrap();
        assert!(
            ConfigStore::load(&path).is_err(),
            "accepted {field}={value}"
        );
    }
}

#[test]
fn imported_configs_require_every_field_and_strict_types() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("invalid.json");
    for field in ["name", "enabled", "mode", "scale", "x", "y", "transform"] {
        let mut rows = serde_json::to_value(layout()).unwrap();
        rows[0].as_object_mut().unwrap().remove(field);
        fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
        assert!(
            ConfigStore::load(&path).is_err(),
            "accepted missing {field}"
        );
    }
    for (field, invalid) in [
        ("x", json!(true)),
        ("y", json!(1.5)),
        ("scale", json!(true)),
        ("enabled", json!(1)),
        ("transform", json!("Flipped90")),
    ] {
        let mut rows = serde_json::to_value(layout()).unwrap();
        rows[0][field] = invalid;
        fs::write(&path, serde_json::to_vec(&rows).unwrap()).unwrap();
        assert!(
            ConfigStore::load(&path).is_err(),
            "accepted invalid {field}"
        );
    }
    fs::write(&path, br#"[{"name":"DP-1","enabled":true,"mode":null,"scale":NaN,"x":0,"y":0,"transform":"normal"}]"#).unwrap();
    assert!(ConfigStore::load(&path).is_err());
}

#[test]
fn invalid_save_preserves_existing_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("existing.json");
    fs::write(&path, "original").unwrap();
    let mut rows = layout();
    rows[0].scale = 0.0;
    assert!(ConfigStore::save(&path, &rows).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "original");
}

#[test]
fn profiles_preserve_unicode_names_order_and_no_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProfileStore::new(temp.path().into());
    let original = layout();
    fs::write(
        temp.path().join("RGVzayAvIPCflqU=.json"),
        serde_json::to_vec(&original).unwrap(),
    )
    .unwrap();
    assert_eq!(store.load("Desk / 🖥").unwrap(), original);
    let mut changed = original.clone();
    changed[0].x = 10;
    assert!(store.save("Desk / 🖥", &changed).is_err());
    assert_eq!(store.load("Desk / 🖥").unwrap(), original);
    store.save("  Travel  ", &original).unwrap();
    store
        .save_order(&["Travel".into(), "Desk / 🖥".into()])
        .unwrap();
    assert_eq!(store.list_names().unwrap(), ["Travel", "Desk / 🖥"]);
    assert!(store.save_order(&["Travel".into()]).is_err());
    store.delete("Travel").unwrap();
    assert_eq!(store.list_names().unwrap(), ["Desk / 🖥"]);
}

#[test]
fn profile_merge_keeps_extra_monitors_and_rejects_missing_saved_monitors() {
    let rows = snapshot(&outputs(&[("DP-1", true, "Normal")])).unwrap();
    let current = outputs(&[("DP-1", true, "Normal"), ("DP-2", true, "Normal")]);
    let merged = merge_profile(&rows, &current).unwrap();
    assert_eq!(merged[1], snapshot(&current).unwrap()[1]);
    assert!(merge_profile(&layout(), &outputs(&[("DP-1", true, "Normal")])).is_err());
}

#[test]
fn automatic_layout_keys_match_python_including_unicode_and_control_names() {
    let temp = tempfile::tempdir().unwrap();
    let memory = LayoutMemory::new(temp.path().into());
    assert_eq!(
        memory
            .path(["eDP-1".into(), "DP-1".into()])
            .file_name()
            .unwrap(),
        "887038e35c8345cf8aa90c1da11315179b8d7ed8a62795ba0e90f6d2872305e6.json"
    );
    assert_eq!(
        memory
            .path(["DP-😀".into(), "éDP-1".into(), "DP-\u{7f}".into()])
            .file_name()
            .unwrap(),
        "39c2b6ed1008972b75012242af96283adfd199fa9f9d66694e29abf1d93e12bd.json"
    );
    memory.remember(&layout()).unwrap();
    assert_eq!(
        memory.recall(["DP-2".into(), "DP-1".into()]).unwrap(),
        Some(layout())
    );
    assert_eq!(memory.recall(["DP-1".into()]).unwrap(), None);
}

// Run the public client against an isolated executable in a child test process.
// PATH is never changed in the shared test process or the user's desktop.
#[test]
fn compositor_mutation_safety_and_daemon_transitions() {
    if std::env::var_os("NIRI_DISPLAY_TEST_CHILD").is_some() {
        client_and_daemon_cases();
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let binary = temp.path().join("niri");
    fs::write(&binary, "#!/bin/sh\nif [ \"$2\" = --json ]; then cat \"$NIRI_DISPLAY_TEST_ROOT/outputs.json\"; else printf '%s\\t' \"$@\" >> \"$NIRI_DISPLAY_TEST_ROOT/commands\"; printf '\\n' >> \"$NIRI_DISPLAY_TEST_ROOT/commands\"; fi\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "compositor_mutation_safety_and_daemon_transitions",
            "--nocapture",
        ])
        .env("NIRI_DISPLAY_TEST_CHILD", "1")
        .env("NIRI_DISPLAY_TEST_ROOT", temp.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                temp.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn set_outputs(root: &Path, outputs: &Outputs) {
    fs::write(
        root.join("outputs.json"),
        serde_json::to_vec(outputs).unwrap(),
    )
    .unwrap();
    fs::write(root.join("commands"), "").unwrap();
}

fn commands(root: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(root.join("commands"))
        .unwrap()
        .lines()
        .map(|line| {
            line.trim_end_matches('\t')
                .split('\t')
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

fn client_and_daemon_cases() {
    let root = std::env::var_os("NIRI_DISPLAY_TEST_ROOT").unwrap();
    let root = Path::new(&root);
    let client = NiriClient;
    let connected = outputs(&[("DP-1", true, "Flipped90"), ("DP-2", false, "Normal")]);
    set_outputs(root, &connected);
    let mut rows = snapshot(&connected).unwrap();
    rows[0].enabled = false;
    rows[1].enabled = true;
    client.apply(&rows).unwrap();
    let log = commands(root);
    let on = log
        .iter()
        .position(|line| line == &["msg", "output", "DP-2", "on"])
        .unwrap();
    let off = log
        .iter()
        .position(|line| line == &["msg", "output", "DP-1", "off"])
        .unwrap();
    assert!(
        on < off,
        "disabled working display before replacement was enabled"
    );
    for change in [0, 1, 2, 3] {
        set_outputs(root, &connected);
        let mut rows = snapshot(&connected).unwrap();
        match change {
            0 => rows.iter_mut().for_each(|row| row.enabled = false),
            1 => {
                rows[1].enabled = true;
                rows[1].mode = Some("0x600@60".into());
            }
            2 => {
                rows.pop();
            }
            _ => rows.push(rows[0].clone()),
        }
        assert!(client.apply(&rows).is_err());
        assert!(
            commands(root).is_empty(),
            "invalid layout partially changed compositor"
        );
    }
    set_outputs(root, &connected);
    let mut rows = snapshot(&connected).unwrap();
    rows[0].x = -1920;
    client.apply(&rows).unwrap();
    let log = commands(root);
    assert!(log
        .iter()
        .any(|line| line == &["msg", "output", "DP-1", "transform", "flipped-90"]));
    assert!(log
        .iter()
        .any(|line| line == &["msg", "output", "DP-1", "position", "set", "--", "-1920", "0"]));
    assert!(log
        .iter()
        .any(|line| line == &["msg", "output", "DP-1", "mode", "1920x1080@60"]));
    set_outputs(root, &connected);
    rows[0].mode = Some("2304x1296@75".into());
    client.apply(&rows).unwrap();
    assert!(commands(root)
        .iter()
        .any(|line| line == &["msg", "output", "DP-1", "custom-mode", "2304x1296@75"]));

    let memory = LayoutMemory::new(root.join("layouts"));
    let mut desired = snapshot(&connected).unwrap();
    desired[0].x = 123;
    memory.remember(&desired).unwrap();
    set_outputs(root, &connected);
    let mut daemon = RestoreDaemon::new(client.clone(), memory.clone());
    daemon.poll().unwrap();
    assert!(commands(root)
        .iter()
        .any(|line| line == &["msg", "output", "DP-1", "position", "set", "--", "123", "0"]));
    set_outputs(root, &connected);
    daemon.poll().unwrap();
    assert!(
        commands(root).is_empty(),
        "daemon enforced layout without connector change"
    );
    let mut edited = connected.clone();
    edited["DP-1"]["logical"]["x"] = json!(777);
    set_outputs(root, &edited);
    daemon.poll().unwrap();
    set_outputs(root, &outputs(&[("eDP-1", false, "Normal")]));
    daemon.poll().unwrap();
    assert!(commands(root)
        .iter()
        .any(|line| line == &["msg", "output", "eDP-1", "on"]));
    assert_eq!(
        memory
            .recall(["DP-1".into(), "DP-2".into()])
            .unwrap()
            .unwrap()[0]
            .x,
        777
    );
    let memory = LayoutMemory::new(root.join("external-layouts"));
    set_outputs(root, &outputs(&[("HDMI-A-1", false, "Normal")]));
    RestoreDaemon::new(client, memory).poll().unwrap();
    assert!(
        commands(root).is_empty(),
        "daemon enabled lone external display"
    );
}
