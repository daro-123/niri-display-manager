use niri_display::backend::{dimensions, snapshot, Layout, NiriClient, OutputConfig, Outputs};
use niri_display::daemon::{LayoutMemory, RestoreDaemon};
use niri_display::services::{
    merge_profile, ConfigStore, DisplaySettings, PresetKind, PresetSettings, Profile, ProfileId,
    ProfileMode, ProfileStore,
};
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
    let original = Profile {
        outputs: layout(),
        mirrors: Vec::new(),
        mode: ProfileMode::Exact,
    };
    fs::write(
        temp.path().join("RGVzayAvIPCflqU=.json"),
        serde_json::to_vec(&original.outputs).unwrap(),
    )
    .unwrap();
    assert_eq!(store.load("Desk / 🖥").unwrap(), original);
    let mut changed = original.clone();
    changed.outputs[0].x = 10;
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
    let rows = Profile {
        outputs: snapshot(&outputs(&[("DP-1", true, "Normal")])).unwrap(),
        mirrors: Vec::new(),
        mode: ProfileMode::Exact,
    };
    let current = outputs(&[("DP-1", true, "Normal"), ("DP-2", true, "Normal")]);
    let merged = merge_profile(&rows, &current).unwrap();
    assert_eq!(merged.outputs[1], snapshot(&current).unwrap()[1]);
    let saved = Profile {
        outputs: layout(),
        mirrors: Vec::new(),
        mode: ProfileMode::Exact,
    };
    assert!(merge_profile(&saved, &outputs(&[("DP-1", true, "Normal")])).is_err());
}
#[test]
fn dynamic_profiles_resolve_all_current_externals_and_round_trip() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProfileStore::new(temp.path().into());
    let saved = outputs(&[("eDP-1", true, "Normal"), ("HDMI-1", false, "Normal")]);
    for (name, mode) in [
        ("external", ProfileMode::ExternalOnly),
        ("mirror", ProfileMode::MirrorBuiltIn),
    ] {
        let profile = Profile {
            outputs: snapshot(&saved).unwrap(),
            mirrors: Vec::new(),
            mode,
        };
        store.save(name, &profile).unwrap();
        assert_eq!(store.load(name).unwrap(), profile);
        let current = outputs(&[
            ("eDP-1", true, "Normal"),
            ("DP-1", false, "Normal"),
            ("DP-2", false, "Normal"),
        ]);
        let resolved = merge_profile(&store.load(name).unwrap(), &current).unwrap();
        assert_eq!(resolved.mode, ProfileMode::Exact);
        assert!(resolved
            .outputs
            .iter()
            .filter(|r| r.name.starts_with("DP-"))
            .all(|r| r.enabled));
        assert_eq!(
            resolved.mirrors.len(),
            if mode == ProfileMode::MirrorBuiltIn {
                2
            } else {
                0
            }
        );
        assert_eq!(
            resolved
                .outputs
                .iter()
                .find(|r| r.name == "DP-1")
                .unwrap()
                .mode
                .as_deref(),
            Some("1920x1080@60")
        );
    }
}

#[test]
fn profile_loader_accepts_legacy_mirror_and_rejects_unknown_mode() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProfileStore::new(temp.path().into());
    let profile = Profile {
        outputs: snapshot(&outputs(&[
            ("DP-1", true, "Normal"),
            ("DP-2", true, "Normal"),
        ]))
        .unwrap(),
        mirrors: Vec::new(),
        mode: ProfileMode::Exact,
    };
    let name = "legacy";
    let path = temp.path().join("bGVnYWN5.json");
    fs::write(
        &path,
        json!({"outputs": profile.outputs, "mirror": {"source":"DP-1","target":"DP-2"}})
            .to_string(),
    )
    .unwrap();
    assert_eq!(store.load(name).unwrap().mirrors.len(), 1);
    fs::write(
        &path,
        json!({"outputs": profile.outputs, "mode":"future"}).to_string(),
    )
    .unwrap();
    assert!(store.load(name).is_err());
}

#[test]
fn dynamic_profiles_fail_when_required_external_or_builtin_missing() {
    for (mode, current) in [
        (
            ProfileMode::ExternalOnly,
            outputs(&[("DP-1", true, "Normal")]),
        ),
        (
            ProfileMode::ExternalOnly,
            outputs(&[("eDP-1", true, "Normal")]),
        ),
        (
            ProfileMode::MirrorBuiltIn,
            outputs(&[("DP-1", true, "Normal")]),
        ),
    ] {
        let profile = Profile {
            outputs: layout(),
            mirrors: Vec::new(),
            mode,
        };
        assert!(merge_profile(&profile, &current).is_err());
    }
}

#[test]
fn profile_validation_rejects_duplicate_mirror_targets() {
    let profile = Profile {
        outputs: snapshot(&outputs(&[
            ("DP-1", true, "Normal"),
            ("DP-2", true, "Normal"),
            ("DP-3", true, "Normal"),
        ]))
        .unwrap(),
        mirrors: vec![
            niri_display::mirror::MirrorConfig {
                source: "DP-1".into(),
                target: "DP-2".into(),
            },
            niri_display::mirror::MirrorConfig {
                source: "DP-1".into(),
                target: "DP-2".into(),
            },
        ],
        mode: ProfileMode::Exact,
    };
    assert!(profile.validate().is_err());
}

#[test]
fn predefined_profiles_preserve_role_settings_across_ports() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProfileStore::new(temp.path().into());
    store
        .save_preset(
            PresetKind::MirrorBuiltIn,
            &PresetSettings {
                show_in_picker: true,
                built_in: DisplaySettings {
                    resolution: Some("2560x1440".into()),
                    scale: 1.5,
                },
                external: DisplaySettings {
                    resolution: Some("1920x1080".into()),
                    scale: 1.0,
                },
                ..PresetSettings::default()
            },
        )
        .unwrap();
    let store = ProfileStore::new(temp.path().into());
    for port in ["HDMI-A-1", "DP-3"] {
        let mut current = outputs(&[("eDP-1", false, "Normal"), (port, false, "Normal")]);
        current["eDP-1"]["modes"] = json!([
            {"width":1920,"height":1080,"refresh_rate":60000},
            {"width":2560,"height":1440,"refresh_rate":59998}
        ]);
        let resolved = store
            .resolve(
                &ProfileId::Predefined {
                    preset: PresetKind::MirrorBuiltIn,
                },
                &current,
            )
            .unwrap();
        let built_in = resolved
            .outputs
            .iter()
            .find(|row| row.name == "eDP-1")
            .unwrap();
        let external = resolved
            .outputs
            .iter()
            .find(|row| row.name == port)
            .unwrap();
        assert!(built_in.enabled && external.enabled);
        assert_eq!(built_in.scale, 1.5);
        assert_eq!(built_in.mode.as_deref(), Some("2560x1440@59.998"));
        assert_eq!(external.scale, 1.0);
        assert_eq!(external.mode.as_deref(), Some("1920x1080@60"));
        assert_eq!(external.x, 1707);
        assert_eq!(
            resolved.mirrors,
            vec![niri_display::mirror::MirrorConfig {
                source: "eDP-1".into(),
                target: port.into()
            }]
        );
        current[port]["modes"] = json!([{"width":1280,"height":720,"refresh_rate":60000}]);
        assert!(store
            .resolve(
                &ProfileId::Predefined {
                    preset: PresetKind::MirrorBuiltIn
                },
                &current
            )
            .is_err());
    }
    store
        .save_preset(
            PresetKind::ExternalOnly,
            &PresetSettings {
                external: DisplaySettings {
                    resolution: None,
                    scale: 1.0,
                },
                ..PresetSettings::default()
            },
        )
        .unwrap();
    let mut current = outputs(&[
        ("eDP-1", true, "Normal"),
        ("DP-1", false, "Normal"),
        ("DP-2", false, "Normal"),
    ]);
    current["DP-1"]["modes"] = json!([
        {"width":1920,"height":1080,"refresh_rate":60000},
        {"width":3840,"height":2160,"refresh_rate":60000,"is_preferred":true}
    ]);
    let resolved = store
        .resolve(
            &ProfileId::Predefined {
                preset: PresetKind::ExternalOnly,
            },
            &current,
        )
        .unwrap();
    assert!(
        !resolved
            .outputs
            .iter()
            .find(|row| row.name == "eDP-1")
            .unwrap()
            .enabled
    );
    let first = resolved
        .outputs
        .iter()
        .find(|row| row.name == "DP-1")
        .unwrap();
    let second = resolved
        .outputs
        .iter()
        .find(|row| row.name == "DP-2")
        .unwrap();
    assert_eq!(first.mode.as_deref(), Some("3840x2160@60"));
    assert_eq!(second.mode.as_deref(), Some("1920x1080@60"));
    assert_eq!((first.x, second.x), (0, 3840));
    assert!(resolved.mirrors.is_empty());
    // Older settings files have no built-in or arrangement presets or fields.
    let mut stored: Value =
        serde_json::from_slice(&fs::read(temp.path().join(".predefined.json")).unwrap()).unwrap();
    stored.as_object_mut().unwrap().remove("built_in");
    stored.as_object_mut().unwrap().remove("horizontal");
    stored.as_object_mut().unwrap().remove("vertical");
    for preset in stored.as_object_mut().unwrap().values_mut() {
        preset.as_object_mut().unwrap().remove("built_in_position");
        preset.as_object_mut().unwrap().remove("alignment");
    }
    fs::write(
        temp.path().join(".predefined.json"),
        serde_json::to_vec(&stored).unwrap(),
    )
    .unwrap();
    let mut settings = store.preset_settings(PresetKind::BuiltIn).unwrap();
    settings.built_in.scale = 1.5;
    settings.built_in.resolution = Some("1920x1080".into());
    store.save_preset(PresetKind::BuiltIn, &settings).unwrap();
    let store = ProfileStore::new(temp.path().into());
    let builtin = ProfileId::Predefined {
        preset: PresetKind::BuiltIn,
    };
    for current in [
        outputs(&[("eDP-1", false, "Normal")]),
        outputs(&[("eDP-1", false, "Normal"), ("DP-3", true, "Normal")]),
    ] {
        let resolved = store.resolve(&builtin, &current).unwrap();
        let panel = resolved
            .outputs
            .iter()
            .find(|row| row.name == "eDP-1")
            .unwrap();
        assert!(panel.enabled);
        assert_eq!((panel.x, panel.y, panel.scale), (0, 0, 1.5));
        assert_eq!(panel.mode.as_deref(), Some("1920x1080@60"));
        assert!(resolved
            .outputs
            .iter()
            .filter(|row| row.name != "eDP-1")
            .all(|row| !row.enabled));
        assert!(resolved.mirrors.is_empty());
    }
    assert!(store
        .resolve(&builtin, &outputs(&[("DP-1", true, "Normal")]))
        .is_err());
}

#[test]
fn predefined_visibility_and_order_do_not_collide_with_named_profiles() {
    let temp = tempfile::tempdir().unwrap();
    let store = ProfileStore::new(temp.path().into());
    let named = ProfileId::Named("External".into());
    store
        .save(
            named.name(),
            &Profile {
                outputs: layout(),
                mirrors: vec![],
                mode: ProfileMode::Exact,
            },
        )
        .unwrap();
    let external = ProfileId::Predefined {
        preset: PresetKind::ExternalOnly,
    };
    let mirror = ProfileId::Predefined {
        preset: PresetKind::MirrorBuiltIn,
    };
    for kind in [PresetKind::ExternalOnly, PresetKind::MirrorBuiltIn] {
        store
            .save_preset(
                kind,
                &PresetSettings {
                    show_in_picker: true,
                    ..PresetSettings::default()
                },
            )
            .unwrap();
    }
    store
        .save_order(&[mirror.clone(), named.clone(), external.clone()])
        .unwrap();
    let mut settings = store.preset_settings(PresetKind::MirrorBuiltIn).unwrap();
    settings.show_in_picker = false;
    store
        .save_preset(PresetKind::MirrorBuiltIn, &settings)
        .unwrap();
    assert_eq!(
        store.list_profiles(true).unwrap(),
        vec![named.clone(), external.clone()]
    );
    assert_eq!(
        store.list_profiles(false).unwrap(),
        vec![
            mirror.clone(),
            named.clone(),
            external.clone(),
            ProfileId::Predefined {
                preset: PresetKind::BuiltIn,
            },
            ProfileId::Predefined {
                preset: PresetKind::Horizontal,
            },
            ProfileId::Predefined {
                preset: PresetKind::Vertical,
            },
        ]
    );
    store
        .save_order(&[external.clone(), named.clone()])
        .unwrap();
    assert!(store
        .save_order(&[mirror.clone(), named.clone(), external.clone()])
        .is_err());
    settings.show_in_picker = true;
    store
        .save_preset(PresetKind::MirrorBuiltIn, &settings)
        .unwrap();
    assert_eq!(
        store.list_profiles(true).unwrap(),
        vec![mirror, external, named]
    );
    let valid = store.preset_settings(PresetKind::ExternalOnly).unwrap();
    let mut invalid = valid.clone();
    invalid.external.scale = 0.0;
    assert!(store
        .save_preset(PresetKind::ExternalOnly, &invalid)
        .is_err());
    assert_eq!(
        store.preset_settings(PresetKind::ExternalOnly).unwrap(),
        valid
    );
}

#[test]
fn arrangement_presets_position_scaled_displays_and_keep_independent_settings() {
    use niri_display::services::{Alignment, BuiltInPosition};

    let temp = tempfile::tempdir().unwrap();
    let store = ProfileStore::new(temp.path().into());
    let mut current = outputs(&[
        ("eDP-1", false, "Normal"),
        ("DP-1", true, "Flipped90"),
        ("DP-2", false, "Normal"),
    ]);
    current["eDP-1"]["modes"] = json!([{"width":2560,"height":1440,"refresh_rate":60000}]);
    current["DP-2"]["modes"] = json!([{"width":1280,"height":720,"refresh_rate":60000}]);
    for (kind, position, alignment, expected) in [
        (
            PresetKind::Horizontal,
            BuiltInPosition::Before,
            Alignment::Start,
            [(0, 0), (1707, 0), (3627, 0)],
        ),
        (
            PresetKind::Horizontal,
            BuiltInPosition::Before,
            Alignment::Center,
            [(0, 60), (1707, 0), (3627, 180)],
        ),
        (
            PresetKind::Horizontal,
            BuiltInPosition::After,
            Alignment::End,
            [(3200, 120), (0, 0), (1920, 360)],
        ),
        (
            PresetKind::Vertical,
            BuiltInPosition::Before,
            Alignment::Start,
            [(0, 0), (0, 960), (0, 2040)],
        ),
        (
            PresetKind::Vertical,
            BuiltInPosition::After,
            Alignment::Center,
            [(106, 1800), (0, 0), (320, 1080)],
        ),
        (
            PresetKind::Vertical,
            BuiltInPosition::After,
            Alignment::End,
            [(213, 1800), (0, 0), (640, 1080)],
        ),
    ] {
        let settings = PresetSettings {
            show_in_picker: true,
            built_in: DisplaySettings {
                resolution: None,
                scale: 1.5,
            },
            built_in_position: position,
            alignment,
            ..PresetSettings::default()
        };
        store.save_preset(kind, &settings).unwrap();
        let reopened = ProfileStore::new(temp.path().into());
        assert_eq!(reopened.preset_settings(kind).unwrap(), settings);
        let id = ProfileId::Predefined { preset: kind };
        assert!(reopened.list_profiles(true).unwrap().contains(&id));
        let resolved = reopened.resolve(&id, &current).unwrap();
        for (name, expected) in ["eDP-1", "DP-1", "DP-2"].into_iter().zip(expected) {
            let row = resolved
                .outputs
                .iter()
                .find(|row| row.name == name)
                .unwrap();
            assert!(row.enabled);
            assert_eq!((row.x, row.y), expected, "{kind:?}: {name}");
            assert_eq!(row.transform, "normal");
        }
        assert!(resolved.mirrors.is_empty());
        for missing_role in [
            outputs(&[("eDP-1", true, "Normal")]),
            outputs(&[("DP-1", true, "Normal")]),
        ] {
            assert!(reopened.resolve(&id, &missing_role).is_err());
        }
    }
    assert_eq!(
        store
            .preset_settings(PresetKind::Horizontal)
            .unwrap()
            .alignment,
        Alignment::End
    );
    assert_eq!(
        store.preset_settings(PresetKind::MirrorBuiltIn).unwrap(),
        PresetSettings::default()
    );
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
    fs::write(&binary, "#!/bin/sh\nroot=\"$NIRI_DISPLAY_TEST_ROOT\"\nif [ \"$2\" = --json ]; then\n  if [ \"$3\" = workspaces ]; then\n    if [ -f \"$root/workspaces-after.json\" ] && grep -q off \"$root/commands\" 2>/dev/null; then cat \"$root/workspaces-after.json\";\n    elif [ -f \"$root/workspaces.json\" ]; then cat \"$root/workspaces.json\";\n    else printf '[]'; fi;\n  elif [ \"$3\" = focused-output ]; then\n    if [ -f \"$root/focused-output.json\" ]; then cat \"$root/focused-output.json\"; else printf '{\"name\":null}'; fi;\n  elif [ -f \"$root/outputs-after.json\" ] && grep -q off \"$root/commands\" 2>/dev/null; then cat \"$root/outputs-after.json\";\n  else cat \"$root/outputs.json\"; fi;\nelse\n  printf '%s\\t' \"$@\" >> \"$root/commands\"; printf '\\n' >> \"$root/commands\";\n  if [ \"$4\" = on ] && [ -f \"$root/activated.json\" ]; then cp \"$root/activated.json\" \"$root/outputs.json\"; fi;\nfi\n").unwrap();
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
    let activated = outputs(&[("DP-1", true, "Flipped90"), ("DP-2", true, "Normal")]);
    fs::write(
        root.join("activated.json"),
        serde_json::to_vec(&activated).unwrap(),
    )
    .unwrap();
    assert_eq!(client.prepare_mirror("DP-1", "DP-2").unwrap(), activated);
    assert_eq!(commands(root), vec![vec!["msg", "output", "DP-2", "on"]]);
    fs::remove_file(root.join("activated.json")).unwrap();
    set_outputs(root, &activated);
    assert_eq!(client.prepare_mirror("DP-1", "DP-2").unwrap(), activated);
    assert!(commands(root).is_empty(), "active target was reconfigured");
    for (connected, source, target) in [
        (outputs(&[("DP-1", true, "Normal")]), "DP-1", "DP-2"),
        (connected.clone(), "DP-1", "DP-1"),
        (connected.clone(), "DP-1", "missing"),
        (connected.clone(), "DP-2", "DP-1"),
    ] {
        set_outputs(root, &connected);
        assert!(client.prepare_mirror(source, target).is_err());
        assert!(
            commands(root).is_empty(),
            "invalid mirror changed compositor"
        );
    }
    // An inactive mirror source is enabled alone first, so Niri moves the
    // workspaces onto it; the previous workspace is focused there, then the
    // full layout and destination follow.
    set_outputs(root, &connected);
    fs::write(
        root.join("workspaces.json"),
        serde_json::to_vec(&json!([
            {"id": 1, "idx": 1, "name": null, "output": "DP-1", "is_urgent": false, "is_active": true, "is_focused": true},
            {"id": 2, "idx": 2, "name": null, "output": "DP-1", "is_urgent": false, "is_active": false, "is_focused": false}
        ]))
        .unwrap(),
    )
    .unwrap();
    // Once DP-1 is disabled, Niri focuses DP-2's empty workspace instead.
    fs::write(
        root.join("workspaces-after.json"),
        serde_json::to_vec(&json!([
            {"id": 1, "idx": 1, "name": null, "output": "DP-2", "is_urgent": false, "is_active": false, "is_focused": false},
            {"id": 9, "idx": 2, "name": null, "output": "DP-2", "is_urgent": false, "is_active": true, "is_focused": true}
        ]))
        .unwrap(),
    )
    .unwrap();
    fs::write(root.join("focused-output.json"), r#"{"name":"DP-2"}"#).unwrap();
    fs::write(
        root.join("outputs-after.json"),
        serde_json::to_vec(&outputs(&[
            ("DP-1", false, "Flipped90"),
            ("DP-2", true, "Normal"),
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut mirror_rows = snapshot(&connected).unwrap();
    mirror_rows[1].enabled = true;
    client.apply_with_source(&mirror_rows, "DP-2").unwrap();
    let log = commands(root);
    let isolated_on = log
        .iter()
        .position(|line| line == &["msg", "output", "DP-2", "on"])
        .unwrap();
    let isolated_off = log
        .iter()
        .position(|line| line == &["msg", "output", "DP-1", "off"])
        .unwrap();
    let focused = log
        .iter()
        .position(|line| line == &["msg", "action", "focus-workspace", "1"])
        .unwrap();
    assert!(
        isolated_on < isolated_off,
        "source was not enabled before other displays were disabled"
    );
    assert!(
        isolated_off < focused,
        "workspace was focused before the source became the only output"
    );
    let destination_on = log
        .iter()
        .skip(isolated_off + 1)
        .position(|line| line == &["msg", "output", "DP-1", "on"])
        .map(|index| index + isolated_off + 1)
        .unwrap();
    assert!(
        focused < destination_on,
        "destination was enabled before the source workspace was focused"
    );
    fs::remove_file(root.join("workspaces.json")).unwrap();
    fs::remove_file(root.join("workspaces-after.json")).unwrap();
    fs::remove_file(root.join("focused-output.json")).unwrap();
    fs::remove_file(root.join("outputs-after.json")).unwrap();
    set_outputs(root, &connected);
    client.apply_with_source(&mirror_rows, "DP-1").unwrap();
    let log = commands(root);
    assert_eq!(
        log.iter()
            .filter(|line| **line == ["msg", "output", "DP-2", "on"])
            .count(),
        1,
        "active source was applied more than once"
    );
    set_outputs(root, &connected);
    assert!(client.apply_with_source(&mirror_rows, "missing").is_err());
    assert!(client
        .apply_with_source(&snapshot(&connected).unwrap(), "DP-2")
        .is_err());
    assert!(
        commands(root).is_empty(),
        "invalid mirror source changed compositor"
    );
    set_outputs(root, &connected);
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
    // Disabling the focused output moves its workspaces, but Niri leaves the
    // remaining output's empty workspace focused. The previous workspace must
    // be focused again once the display is off.
    set_outputs(root, &connected);
    fs::write(
        root.join("workspaces.json"),
        serde_json::to_vec(&json!([
            {"id": 7, "idx": 1, "name": null, "output": "DP-1", "is_urgent": false, "is_active": true, "is_focused": true, "active_window_id": 3}
        ]))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        root.join("workspaces-after.json"),
        serde_json::to_vec(&json!([
            {"id": 8, "idx": 1, "name": null, "output": "DP-2", "is_urgent": false, "is_active": true, "is_focused": true},
            {"id": 7, "idx": 2, "name": null, "output": "DP-2", "is_urgent": false, "is_active": false, "is_focused": false}
        ]))
        .unwrap(),
    )
    .unwrap();
    fs::write(root.join("focused-output.json"), r#"{"name":"DP-2"}"#).unwrap();
    fs::write(
        root.join("outputs-after.json"),
        serde_json::to_vec(&outputs(&[
            ("DP-1", false, "Normal"),
            ("DP-2", true, "Normal"),
        ]))
        .unwrap(),
    )
    .unwrap();
    let mut rows = snapshot(&connected).unwrap();
    rows[0].enabled = false;
    rows[1].enabled = true;
    client.apply(&rows).unwrap();
    let log = commands(root);
    let off = log
        .iter()
        .position(|line| line == &["msg", "output", "DP-1", "off"])
        .unwrap();
    let restored = log
        .iter()
        .position(|line| line == &["msg", "action", "focus-workspace", "2"])
        .unwrap();
    assert!(
        off < restored,
        "previous workspace was focused before the display was disabled"
    );
    // No redundant focus change when the workspace already holds focus.
    set_outputs(root, &connected);
    fs::write(
        root.join("workspaces-after.json"),
        serde_json::to_vec(&json!([
            {"id": 7, "idx": 2, "name": null, "output": "DP-2", "is_urgent": false, "is_active": true, "is_focused": true}
        ]))
        .unwrap(),
    )
    .unwrap();
    client.apply(&rows).unwrap();
    assert!(
        commands(root).iter().all(|line| {
            !(line.len() >= 3
                && line[0] == "msg"
                && line[1] == "action"
                && line[2] == "focus-workspace")
        }),
        "focused workspace was changed even though it kept focus"
    );
    for name in [
        "workspaces.json",
        "workspaces-after.json",
        "focused-output.json",
        "outputs-after.json",
    ] {
        fs::remove_file(root.join(name)).unwrap();
    }
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
