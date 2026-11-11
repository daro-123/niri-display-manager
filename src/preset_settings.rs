use crate::backend::Outputs;
use crate::services::{
    Alignment, BuiltInPosition, DisplaySettings, PresetKind, PresetSettings, ProfileStore,
};
use gtk::prelude::*;

struct DisplayControls {
    resolution: gtk::DropDown,
    resolutions: Vec<Option<String>>,
    scale: gtk::SpinButton,
}

impl DisplayControls {
    fn settings(&self) -> DisplaySettings {
        DisplaySettings {
            resolution: self.resolutions[self.resolution.selected() as usize].clone(),
            scale: self.scale.value() / 100.0,
        }
    }
}

fn label(text: &str, heading: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class(if heading { "heading" } else { "dim-label" });
    label
}

fn display_controls(
    page: &gtk::Box,
    title: &str,
    settings: &DisplaySettings,
    outputs: &Outputs,
    built_in: bool,
) -> DisplayControls {
    page.append(&label(title, true));
    let mut sizes: Vec<String> = if built_in {
        Vec::new()
    } else {
        [
            "1280x720",
            "1920x1080",
            "1920x1200",
            "2560x1440",
            "2560x1600",
            "3840x2160",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    };
    for (name, output) in outputs {
        let internal = ["eDP-", "LVDS-", "DSI-"]
            .iter()
            .any(|prefix| name.starts_with(prefix));
        if internal != built_in {
            continue;
        }
        if let Some(modes) = output.get("modes").and_then(serde_json::Value::as_array) {
            for mode in modes {
                if let (Some(width), Some(height)) =
                    (mode["width"].as_u64(), mode["height"].as_u64())
                {
                    sizes.push(format!("{width}x{height}"));
                }
            }
        }
    }
    if built_in {
        sizes.retain(|size| {
            let (width, height) = size.split_once('x').unwrap();
            let width = width.parse::<u64>().unwrap();
            let height = height.parse::<u64>().unwrap();
            outputs
                .iter()
                .filter(|(name, _)| {
                    ["eDP-", "LVDS-", "DSI-"]
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                })
                .all(|(_, output)| {
                    output["modes"].as_array().is_some_and(|modes| {
                        modes.iter().any(|mode| {
                            mode["width"].as_u64() == Some(width)
                                && mode["height"].as_u64() == Some(height)
                        })
                    })
                })
        });
    } else {
        sizes.extend(settings.resolution.iter().cloned());
    }
    sizes.sort_unstable_by_key(|size| {
        size.split_once('x').map(|(width, height)| {
            (
                width.parse::<u32>().unwrap_or(0),
                height.parse::<u32>().unwrap_or(0),
            )
        })
    });
    sizes.dedup();
    let mut resolutions = vec![None];
    resolutions.extend(sizes.into_iter().map(Some));
    let names: Vec<_> = resolutions
        .iter()
        .map(|size| {
            size.as_ref()
                .map(|size| size.replace('x', " × "))
                .unwrap_or_else(|| "Automatic".into())
        })
        .collect();
    let resolution =
        gtk::DropDown::from_strings(&names.iter().map(String::as_str).collect::<Vec<_>>());
    resolution.set_selected(
        resolutions
            .iter()
            .position(|size| size == &settings.resolution)
            .unwrap_or(0) as u32,
    );
    resolution.set_hexpand(true);
    let scale = gtk::SpinButton::with_range(10.0, 1000.0, 5.0);
    scale.set_digits(0);
    scale.set_value(settings.scale * 100.0);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&gtk::Label::new(Some("Resolution")));
    row.append(&resolution);
    row.append(&gtk::Label::new(Some("Scale (%)")));
    row.append(&scale);
    page.append(&row);
    DisplayControls {
        resolution,
        resolutions,
        scale,
    }
}

fn arrangement_controls(
    page: &gtk::Box,
    kind: PresetKind,
    settings: &PresetSettings,
) -> (gtk::DropDown, gtk::DropDown) {
    let vertical = kind == PresetKind::Vertical;
    let position = gtk::DropDown::from_strings(if vertical {
        &["Above externals", "Below externals"]
    } else {
        &["Left of externals", "Right of externals"]
    });
    position.set_selected(match settings.built_in_position {
        BuiltInPosition::Before => 0,
        BuiltInPosition::After => 1,
    });
    let alignment = gtk::DropDown::from_strings(if vertical {
        &["Left", "Center", "Right"]
    } else {
        &["Top", "Center", "Bottom"]
    });
    alignment.set_selected(match settings.alignment {
        Alignment::Start => 0,
        Alignment::Center => 1,
        Alignment::End => 2,
    });
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 12);
    controls.set_hexpand(true);
    for (title, choice) in [("Built-in position", &position), ("Alignment", &alignment)] {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = gtk::Label::new(Some(title));
        title.set_xalign(0.0);
        title.set_width_chars(16);
        choice.set_hexpand(true);
        row.append(&title);
        row.append(choice);
        controls.append(&row);
    }
    let preview = gtk::Box::new(
        if vertical {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        },
        8,
    );
    preview.set_halign(gtk::Align::Center);
    preview.set_valign(gtk::Align::Center);
    let builtin = gtk::Frame::new(None);
    builtin.set_child(Some(&gtk::Label::new(Some("Built-in"))));
    builtin.set_size_request(80, 48);
    let external = gtk::Frame::new(None);
    external.set_child(Some(&gtk::Label::new(Some("External"))));
    external.set_size_request(110, 68);
    preview.append(&builtin);
    preview.append(&external);
    let update = {
        let preview = preview.clone();
        let builtin = builtin.clone();
        let external = external.clone();
        let position = position.downgrade();
        let alignment = alignment.downgrade();
        move || {
            let (Some(position), Some(alignment)) = (position.upgrade(), alignment.upgrade())
            else {
                return;
            };
            preview.reorder_child_after(
                &builtin,
                if position.selected() == 0 {
                    None
                } else {
                    Some(&external)
                },
            );
            let align = match alignment.selected() {
                0 => gtk::Align::Start,
                2 => gtk::Align::End,
                _ => gtk::Align::Center,
            };
            for frame in [&builtin, &external] {
                if vertical {
                    frame.set_halign(align);
                } else {
                    frame.set_valign(align);
                }
            }
        }
    };
    update();
    let update = std::rc::Rc::new(update);
    for choice in [&position, &alignment] {
        let update = update.clone();
        choice.connect_selected_notify(move |_| update());
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    row.append(&controls);
    row.append(&preview);
    page.append(&row);
    page.append(&label("Arrangement preview · schematic. Displays stay unrotated; positions use scaled display sizes.", false));
    (position, alignment)
}

pub fn present(parent: &gtk::ApplicationWindow, store: ProfileStore, outputs: &Outputs) {
    let settings: Result<Vec<_>, _> = PresetKind::ALL
        .into_iter()
        .map(|kind| store.preset_settings(kind))
        .collect();
    let settings = match settings {
        Ok(settings) => settings,
        Err(error) => {
            gtk::AlertDialog::builder()
                .message("Cannot open predefined profiles")
                .detail(format!("{error:#}"))
                .build()
                .show(Some(parent));
            return;
        }
    };
    let window = gtk::Window::builder()
        .title("Settings · Predefined profiles")
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .default_width(840)
        .default_height(580)
        .build();
    window.set_widget_name("predefined-profiles");
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 20);
    for margin in [0, 1, 2, 3] {
        match margin {
            0 => root.set_margin_top(20),
            1 => root.set_margin_bottom(20),
            2 => root.set_margin_start(16),
            _ => root.set_margin_end(20),
        }
    }
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    let sidebar = gtk::StackSidebar::new();
    sidebar.set_stack(&stack);
    sidebar.set_width_request(240);
    root.append(&sidebar);
    root.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    root.append(&stack);
    window.set_child(Some(&root));
    for (kind, settings) in PresetKind::ALL.into_iter().zip(settings) {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 16);
        page.append(&label(kind.name(), true));
        page.append(&label(
            match kind {
                PresetKind::BuiltIn => "Enable built-in displays and disable external displays.",
                PresetKind::ExternalOnly => "Enable all connected external displays.",
                PresetKind::MirrorBuiltIn => {
                    "Mirror the built-in display to all connected external displays."
                }
                PresetKind::Horizontal => "Enable built-in and external displays side by side.",
                PresetKind::Vertical => {
                    "Enable built-in and external displays, stacked vertically."
                }
            },
            false,
        ));
        let visibility = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let title = gtk::Label::new(Some("Show in profile picker"));
        title.set_hexpand(true);
        title.set_xalign(0.0);
        let show = gtk::Switch::new();
        show.set_active(settings.show_in_picker);
        show.set_valign(gtk::Align::Center);
        visibility.append(&title);
        visibility.append(&show);
        page.append(&visibility);
        page.append(&label(
            "Include this preset in niri-display --profiles.",
            false,
        ));
        page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let built_in = (kind != PresetKind::ExternalOnly).then(|| {
            display_controls(&page, "Built-in display", &settings.built_in, outputs, true)
        });
        let external = (kind != PresetKind::BuiltIn).then(|| {
            display_controls(
                &page,
                "External displays",
                &settings.external,
                outputs,
                false,
            )
        });
        let arrangement = matches!(kind, PresetKind::Horizontal | PresetKind::Vertical)
            .then(|| arrangement_controls(&page, kind, &settings));
        page.append(&label(
            match kind {
                PresetKind::BuiltIn => {
                    "External displays disabled. Built-in displays arranged side by side."
                }
                PresetKind::ExternalOnly => {
                    "Built-in displays disabled. External displays arranged side by side."
                }
                PresetKind::MirrorBuiltIn | PresetKind::Horizontal | PresetKind::Vertical => {
                    "Settings follow display roles, not HDMI, USB-C, or DisplayPort ports."
                }
            },
            false,
        ));
        page.append(&label("Automatic uses each display's preferred resolution. Explicit resolutions must be supported by every display in that role.", false));
        let container = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&page)
            .build();
        container.append(&scroll);
        let status = label("", false);
        status.set_selectable(true);
        container.append(&status);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_hexpand(true);
        actions.set_halign(gtk::Align::End);
        let save = gtk::Button::with_label("Save settings");
        save.add_css_class("suggested-action");
        actions.append(&save);
        container.append(&actions);
        let store = store.clone();
        save.connect_clicked(move |_| {
            let preset = PresetSettings {
                show_in_picker: show.is_active(),
                built_in: built_in
                    .as_ref()
                    .map(DisplayControls::settings)
                    .unwrap_or_else(|| settings.built_in.clone()),
                external: external
                    .as_ref()
                    .map(DisplayControls::settings)
                    .unwrap_or_else(|| settings.external.clone()),
                built_in_position: arrangement
                    .as_ref()
                    .map(|(position, _)| {
                        if position.selected() == 0 {
                            BuiltInPosition::Before
                        } else {
                            BuiltInPosition::After
                        }
                    })
                    .unwrap_or(settings.built_in_position),
                alignment: arrangement
                    .as_ref()
                    .map(|(_, alignment)| match alignment.selected() {
                        0 => Alignment::Start,
                        2 => Alignment::End,
                        _ => Alignment::Center,
                    })
                    .unwrap_or(settings.alignment),
            };
            match store.save_preset(kind, &preset) {
                Ok(()) => status.set_text(
                    "Settings saved. Displays unchanged. Reopen the picker to refresh its list.",
                ),
                Err(error) => status.set_text(&format!("{error:#}")),
            }
        });
        stack.add_titled(
            &container,
            Some(match kind {
                PresetKind::BuiltIn => "built-in",
                PresetKind::ExternalOnly => "external",
                PresetKind::MirrorBuiltIn => "mirror",
                PresetKind::Horizontal => "horizontal",
                PresetKind::Vertical => "vertical",
            }),
            kind.name(),
        );
    }
    window.present();
}
