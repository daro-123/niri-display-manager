use crate::backend::{dimensions, snapshot, Layout, NiriClient, OutputConfig, Outputs};
use crate::daemon::{self, LayoutMemory};
use crate::mirror::{MirrorConfig, MirrorManager};
use crate::profile_sync;
use crate::services::{merge_profile, ConfigStore, Profile, ProfileStore};
use anyhow::{anyhow, bail, Result};
use gtk::cairo;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

struct CanvasRect {
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    ratio: f64,
}

struct MonitorControls<'a> {
    selector: &'a gtk::DropDown,
    enabled: &'a gtk::Switch,
    resolution: &'a gtk::DropDown,
    refresh: &'a gtk::DropDown,
    scale: &'a gtk::SpinButton,
    x: &'a gtk::SpinButton,
    y: &'a gtk::SpinButton,
    width: &'a gtk::SpinButton,
    height: &'a gtk::SpinButton,
    hz: &'a gtk::SpinButton,
    advanced: &'a gtk::ToggleButton,
    custom: &'a gtk::Button,
    modes: &'a [String],
    sizes: &'a [String],
    rates: &'a [String],
}

struct Editor {
    owner: Weak<RefCell<Editor>>,
    window: ApplicationWindow,
    client: NiriClient,
    mirrors: MirrorManager,
    memory: LayoutMemory,
    profiles: ProfileStore,
    outputs: Outputs,
    layout: Layout,
    baseline: Option<Layout>,
    previous: Option<Profile>,
    selected: Option<String>,
    mirror_pair: Option<MirrorConfig>,
    mirror_source_first: bool,
    busy: bool,
    closing: bool,
    canvas_zoom: f64,
    rectangles: Vec<CanvasRect>,
    dragged: Option<(String, i32, i32, f64)>,
    remaining: i32,
    confirm_timer: Option<glib::SourceId>,
    mirror_timer: Option<glib::SourceId>,
    identification: Vec<gtk::Window>,
    file_chooser: Option<gtk::gio::Cancellable>,
    root: gtk::Box,
    toolbar: gtk::Box,
    editor_area: gtk::Box,
    layout_actions: gtk::Box,
    status: gtk::Label,
    summary: gtk::Label,
    canvas: gtk::DrawingArea,
    drawer: gtk::Revealer,
    dock: gtk::Box,
    advanced: gtk::Box,
    mirror_menu: gtk::MenuButton,
    mirror_peer: gtk::DropDown,
    mirror_destination: gtk::Label,
    mirror_choice_label: gtk::Label,
    mirror_status: gtk::Label,
    mirror_start: gtk::Button,
    mirror_stop: gtk::Button,
    mirror_orientation: gtk::CheckButton,
    daemon_toggle: gtk::CheckButton,
    profile_choice: gtk::DropDown,
    profile_name: gtk::Entry,
    profile_status: gtk::Label,
    profile_apply: gtk::Button,
    profile_delete: gtk::Button,
    identify_button: gtk::Button,
    discard_button: gtk::Button,
    apply_button: gtk::Button,
    confirm: gtk::Window,
    confirm_label: gtk::Label,
    confirm_progress: gtk::ProgressBar,
    confirm_keep: gtk::Button,
    confirm_revert: gtk::Button,
    updating: Rc<Cell<bool>>,
    _profile_sync: Option<gio::SignalSubscription>,
    sync_pending: bool,
}

thread_local! {
    static OPEN_EDITORS: RefCell<Vec<Rc<RefCell<Editor>>>> = const { RefCell::new(Vec::new()) };
}
fn weak(editor: &Rc<RefCell<Editor>>) -> Weak<RefCell<Editor>> {
    Rc::downgrade(editor)
}

pub fn present(app: &Application) {
    let editor = Editor::new(app);
    let weak = Rc::downgrade(&editor);
    let subscription = profile_sync::on_switched(move || {
        if let Some(editor) = weak.upgrade() {
            Editor::synchronize(&editor);
        }
    });
    editor.borrow_mut()._profile_sync = subscription;
    OPEN_EDITORS.with(|editors| editors.borrow_mut().push(Rc::clone(&editor)));
    editor.borrow().window.present();
    Editor::refresh(&editor);
}

impl Editor {
    fn new(app: &Application) -> Rc<RefCell<Self>> {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Niri Displays")
            .default_width(1100)
            .default_height(720)
            .build();
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".display-workspace, .display-dock, .display-drawer { background: @theme_base_color; border: 1px solid alpha(@theme_fg_color, 0.14); border-radius: 12px; }
             .display-dock { padding: 12px; }
             .display-canvas { color: @theme_selected_bg_color; }
             .display-drawer { padding: 16px; }
             .confirmation-pending { opacity: 0.45; }
             .display-confirmation { border-radius: 18px; }
             .confirmation-title { font-size: 22px; font-weight: 600; }
             .confirmation-countdown { font-feature-settings: \"tnum\"; }
             .confirmation-actions button { padding: 10px 20px; }
             .confirmation-progress trough, .confirmation-progress progress { min-height: 4px; }",
        );
        gtk::style_context_add_provider_for_display(
            &WidgetExt::display(&window),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        // Resolve the two menu arrows directly, without indexing the desktop icon theme.
        let arrows = gtk::CssProvider::new();
        arrows.load_from_string(
            "#display-root menubutton arrow { -gtk-icon-source: -gtk-recolor(url(\"resource:///org/gtk/libgtk/icons/scalable/actions/pan-down-symbolic.svg\")); }",
        );
        gtk::style_context_add_provider_for_display(
            &WidgetExt::display(&window),
            &arrows,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
        );
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        for side in ["top", "bottom", "start", "end"] {
            root.set_widget_name("display-root");
            match side {
                "top" => root.set_margin_top(8),
                "bottom" => root.set_margin_bottom(16),
                "start" => root.set_margin_start(16),
                _ => root.set_margin_end(16),
            }
        }
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let handle = gtk::WindowHandle::new();
        handle.set_child(Some(&toolbar));
        root.append(&handle);
        let title = gtk::Label::new(Some("Displays"));
        title.add_css_class("title-1");
        title.set_hexpand(true);
        title.set_xalign(0.0);
        toolbar.append(&title);
        let identify_button = gtk::Button::with_label("Identify");
        toolbar.append(&identify_button);

        let profile_menu = gtk::MenuButton::builder().label("Profiles").build();
        let profile_popover = gtk::Popover::new();
        profile_menu.set_popover(Some(&profile_popover));
        let profile_box = gtk::Box::new(gtk::Orientation::Vertical, 10);
        profile_box.set_margin_top(16);
        profile_box.set_margin_bottom(16);
        profile_box.set_margin_start(16);
        profile_box.set_margin_end(16);
        profile_box.append(&heading("Saved profiles"));
        let profile_choice = dropdown(&[]);
        profile_box.append(&profile_choice);
        let profile_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let profile_apply = gtk::Button::with_label("Apply profile");
        let profile_delete = gtk::Button::with_label("Delete");
        profile_actions.append(&profile_apply);
        profile_actions.append(&profile_delete);
        profile_box.append(&profile_actions);
        profile_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let profile_name = gtk::Entry::new();
        profile_name.set_placeholder_text(Some("New profile name"));
        profile_box.append(&profile_name);
        let save_profile = gtk::Button::with_label("Save current edits");
        profile_box.append(&save_profile);
        profile_box.append(&dim_label(
            "Saves display settings, including unapplied edits, and the active mirror.",
        ));
        let profile_status = gtk::Label::new(None);
        profile_status.set_xalign(0.0);
        profile_status.set_wrap(true);
        profile_box.append(&profile_status);
        profile_popover.set_child(Some(&profile_box));
        toolbar.append(&profile_menu);
        let import_button = gtk::Button::with_label("Import");
        let export_button = gtk::Button::with_label("Export");
        import_button.add_css_class("flat");
        export_button.add_css_class("flat");
        toolbar.append(&import_button);
        toolbar.append(&export_button);

        let settings = gtk::MenuButton::new();
        settings.set_child(Some(&gtk::Label::new(Some("⚙"))));
        settings.set_tooltip_text(Some("Settings"));
        let settings_popover = gtk::Popover::new();
        let settings_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        for (method, value) in [(0, 16), (1, 16), (2, 16), (3, 16)] {
            match method {
                0 => settings_box.set_margin_top(value),
                1 => settings_box.set_margin_bottom(value),
                2 => settings_box.set_margin_start(value),
                _ => settings_box.set_margin_end(value),
            }
        }
        settings_box.append(&heading("Mirroring settings"));
        let mirror_orientation = gtk::CheckButton::with_label("Selected display is the source");
        mirror_orientation.set_active(true);
        mirror_orientation.set_tooltip_text(Some("When enabled, choose a target to mirror the selected display. When disabled, choose a source for the selected destination."));
        settings_box.append(&mirror_orientation);
        settings_box.append(&heading("Saved layouts"));
        let daemon_toggle = gtk::CheckButton::with_label("Restore layouts automatically");
        daemon_toggle.set_tooltip_text(Some("Run a background user service, including after closing this window. Remember layouts on disconnect and restore them at login and reconnect. Requires a systemd user session."));
        daemon_toggle.set_sensitive(false);
        settings_box.append(&daemon_toggle);
        settings_popover.set_child(Some(&settings_box));
        settings.set_popover(Some(&settings_popover));
        toolbar.append(&settings);

        let summary = gtk::Label::new(None);
        summary.set_xalign(0.0);
        summary.add_css_class("dim-label");
        root.append(&summary);
        let editor_area = gtk::Box::new(gtk::Orientation::Vertical, 12);
        editor_area.set_vexpand(true);
        root.append(&editor_area);
        let panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
        panel.add_css_class("display-workspace");
        panel.set_hexpand(true);
        panel.set_vexpand(true);
        editor_area.append(&panel);
        panel.append(&heading("Arrangement"));
        let workspace = gtk::Overlay::new();
        workspace.set_vexpand(true);
        panel.append(&workspace);
        let canvas = gtk::DrawingArea::new();
        canvas.add_css_class("display-canvas");
        canvas.set_content_width(240);
        canvas.set_hexpand(true);
        canvas.set_vexpand(true);
        canvas.set_focusable(true);
        canvas.set_overflow(gtk::Overflow::Hidden);
        canvas.set_tooltip_text(Some(
            "Select a display. Drag or use arrow keys to arrange it.",
        ));
        workspace.set_child(Some(&canvas));
        let drawer = gtk::Revealer::new();
        drawer.set_halign(gtk::Align::End);
        drawer.set_transition_type(gtk::RevealerTransitionType::SlideLeft);
        drawer.set_width_request(320);
        drawer.set_can_target(false);
        let adv_scroll = gtk::ScrolledWindow::new();
        adv_scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
        let advanced = gtk::Box::new(gtk::Orientation::Vertical, 12);
        advanced.add_css_class("display-drawer");
        adv_scroll.set_child(Some(&advanced));
        drawer.set_child(Some(&adv_scroll));
        workspace.add_overlay(&drawer);
        let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let hint = dim_label("Drag to arrange · Edges snap together");
        hint.set_hexpand(true);
        footer.append(&hint);
        let fit = gtk::Button::with_label("Fit to view");
        footer.append(&fit);
        panel.append(&footer);
        let dock_scroll = gtk::ScrolledWindow::new();
        dock_scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        dock_scroll.set_vscrollbar_policy(gtk::PolicyType::Never);
        dock_scroll.set_min_content_height(108);
        let dock = gtk::Box::new(gtk::Orientation::Horizontal, 20);
        dock.add_css_class("display-dock");
        dock_scroll.set_child(Some(&dock));
        editor_area.append(&dock_scroll);

        let mirror_menu = gtk::MenuButton::builder().label("Independent").build();
        let mirror_popover = gtk::Popover::new();
        let mirror_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        mirror_box.set_margin_top(16);
        mirror_box.set_margin_bottom(16);
        mirror_box.set_margin_start(16);
        mirror_box.set_margin_end(16);
        mirror_box.append(&heading("Mirroring"));
        let mirror_destination = gtk::Label::new(None);
        mirror_destination.set_xalign(0.0);
        mirror_destination.set_wrap(true);
        mirror_box.append(&mirror_destination);
        let mirror_choice_label = gtk::Label::new(None);
        mirror_box.append(&mirror_choice_label);
        let mirror_peer = dropdown(&[]);
        mirror_box.append(&mirror_peer);
        let mirror_status = gtk::Label::new(None);
        mirror_status.set_xalign(0.0);
        mirror_status.set_wrap(true);
        mirror_box.append(&mirror_status);
        mirror_box.append(&dim_label(
            "Mirroring starts immediately. Display changes suspend and resume mirroring.",
        ));
        let mirror_start = gtk::Button::with_label("Start mirroring now");
        mirror_start.add_css_class("suggested-action");
        mirror_box.append(&mirror_start);
        let mirror_stop = gtk::Button::with_label("Stop mirroring");
        mirror_popover.set_child(Some(&mirror_box));
        mirror_menu.set_popover(Some(&mirror_popover));

        let footer_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_selectable(true);
        status.set_hexpand(true);
        footer_actions.append(&status);
        let layout_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let discard_button = gtk::Button::with_label("Revert");
        discard_button.set_tooltip_text(Some("Discard staged edits and reread connected displays"));
        let apply_button = gtk::Button::with_label("Apply changes");
        apply_button.add_css_class("suggested-action");
        layout_actions.append(&discard_button);
        layout_actions.append(&apply_button);
        footer_actions.append(&layout_actions);
        root.append(&footer_actions);

        let confirm = gtk::Window::builder()
            .title("Keep these display settings?")
            .transient_for(&window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(460)
            .resizable(false)
            .build();
        let confirm_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        confirm_box.set_margin_top(28);
        confirm_box.set_margin_bottom(28);
        confirm_box.set_margin_start(28);
        confirm_box.set_margin_end(28);
        confirm_box.append(&heading("Keep these display settings?"));
        confirm_box.append(&dim_label("Check that every display looks right."));
        let confirm_label = gtk::Label::new(None);
        confirm_box.append(&confirm_label);
        let confirm_progress = gtk::ProgressBar::new();
        confirm_box.append(&confirm_progress);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        actions.set_halign(gtk::Align::End);
        let confirm_revert = gtk::Button::with_label("Revert now");
        let confirm_keep = gtk::Button::with_label("Keep changes");
        confirm_keep.add_css_class("suggested-action");
        actions.append(&confirm_revert);
        actions.append(&confirm_keep);
        confirm_box.append(&actions);
        confirm.set_child(Some(&confirm_box));
        confirm.set_default_widget(Some(&confirm_revert));

        window.set_child(Some(&root));
        let editor = Rc::new(RefCell::new(Self {
            owner: Weak::new(),
            window,
            client: NiriClient,
            mirrors: MirrorManager::default(),
            memory: LayoutMemory::default(),
            profiles: ProfileStore::default(),
            outputs: Outputs::new(),
            layout: Vec::new(),
            baseline: None,
            previous: None,
            selected: None,
            mirror_pair: None,
            mirror_source_first: true,
            busy: false,
            closing: false,
            canvas_zoom: 1.0,
            rectangles: Vec::new(),
            dragged: None,
            remaining: 0,
            confirm_timer: None,
            mirror_timer: None,
            identification: Vec::new(),
            file_chooser: None,
            root,
            toolbar,
            editor_area,
            layout_actions,
            status,
            summary,
            canvas,
            drawer,
            dock,
            advanced,
            mirror_menu,
            mirror_peer,
            mirror_destination,
            mirror_choice_label,
            mirror_status,
            mirror_start,
            mirror_stop,
            mirror_orientation,
            daemon_toggle,
            profile_choice,
            profile_name,
            profile_status,
            profile_apply,
            profile_delete,
            identify_button,
            discard_button,
            apply_button,
            confirm,
            confirm_label,
            confirm_progress,
            confirm_keep,
            confirm_revert,
            updating: Rc::new(Cell::new(false)),
            _profile_sync: None,
            sync_pending: false,
        }));
        editor.borrow_mut().owner = Rc::downgrade(&editor);
        Self::connect(
            &editor,
            save_profile,
            profile_popover,
            fit,
            import_button,
            export_button,
        );
        editor
    }

    fn connect(
        e: &Rc<RefCell<Self>>,
        save: gtk::Button,
        profile_popover: gtk::Popover,
        fit: gtk::Button,
        import: gtk::Button,
        export: gtk::Button,
    ) {
        let w = weak(e);
        e.borrow().discard_button.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::refresh(&e);
            }
        });
        let w = weak(e);
        e.borrow().apply_button.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::apply(&e);
            }
        });
        let w = weak(e);
        e.borrow().confirm_keep.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::keep(&e);
            }
        });
        let w = weak(e);
        e.borrow().confirm_revert.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::revert(&e);
            }
        });
        let w = weak(e);
        e.borrow().identify_button.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::identify(&e);
            }
        });
        let w = weak(e);
        save.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::save_profile(&e);
            }
        });
        let w = weak(e);
        e.borrow().profile_name.connect_activate(move |_| {
            if let Some(e) = w.upgrade() {
                Self::save_profile(&e);
            }
        });
        let w = weak(e);
        e.borrow().profile_apply.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::apply_profile(&e);
            }
        });
        let w = weak(e);
        e.borrow().profile_delete.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::delete_profile(&e);
            }
        });
        let w = weak(e);
        e.borrow().mirror_start.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::start_mirror(&e);
            }
        });
        let w = weak(e);
        e.borrow().mirror_stop.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::stop_mirror(&e);
            }
        });
        let w = weak(e);
        e.borrow().mirror_orientation.connect_toggled(move |x| {
            if let Some(e) = w.upgrade() {
                let mut s = e.borrow_mut();
                s.mirror_source_first = x.is_active();
                s.update_mirror_sources();
            }
        });
        let w = weak(e);
        let updating = e.borrow().updating.clone();
        e.borrow().daemon_toggle.connect_toggled(move |x| {
            if !updating.get() {
                if let Some(e) = w.upgrade() {
                    Self::change_daemon(&e, x.is_active());
                }
            }
        });
        let w = weak(e);
        e.borrow()
            .canvas
            .set_draw_func(move |area, cr, width, height| {
                if let Some(e) = w.upgrade() {
                    e.borrow_mut().draw(area, cr, width, height);
                }
            });
        let drag = gtk::GestureDrag::new();
        let w = weak(e);
        drag.connect_drag_begin(move |_, x, y| {
            if let Some(e) = w.upgrade() {
                Self::drag_begin(&e, x, y)
            }
        });
        let w = weak(e);
        drag.connect_drag_update(move |_, x, y| {
            if let Some(e) = w.upgrade() {
                Self::drag_update(&e, x, y)
            }
        });
        let w = weak(e);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(e) = w.upgrade() {
                Self::drag_end(&e)
            }
        });
        e.borrow().canvas.add_controller(drag);
        let keys = gtk::EventControllerKey::new();
        let w = weak(e);
        keys.connect_key_pressed(move |_, key, _, state| {
            if let Some(e) = w.upgrade() {
                Self::key_pressed(&e, key, state)
            } else {
                glib::Propagation::Proceed
            }
        });
        e.borrow().canvas.add_controller(keys);
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let w = weak(e);
        scroll.connect_scroll(move |_, _, dy| {
            if let Some(e) = w.upgrade() {
                let mut s = e.borrow_mut();
                s.canvas_zoom = (s.canvas_zoom * 1.15_f64.powf(-dy)).clamp(0.25, 4.0);
                s.canvas.queue_draw();
            }
            glib::Propagation::Stop
        });
        e.borrow().canvas.add_controller(scroll);
        let w = weak(e);
        fit.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                let mut s = e.borrow_mut();
                s.canvas_zoom = 1.0;
                s.canvas.queue_draw();
                s.message("View fitted to all enabled displays.");
            }
        });
        let w = weak(e);
        import.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::choose_file(&e, false);
            }
        });
        let w = weak(e);
        export.connect_clicked(move |_| {
            if let Some(e) = w.upgrade() {
                Self::choose_file(&e, true);
            }
        });
        let w = weak(e);
        profile_popover.connect_show(move |_| {
            if let Some(e) = w.upgrade() {
                Self::reload_profiles(&e, None);
            }
        });
        let w = weak(e);
        e.borrow()
            .mirror_peer
            .connect_notify_local(Some("selected"), move |_, _| {
                let weak = w.clone();
                glib::idle_add_local_once(move || {
                    if let Some(e) = weak.upgrade() {
                        e.borrow_mut().update_mirror_actions();
                    }
                });
            });
        let w = weak(e);
        e.borrow()
            .drawer
            .connect_notify_local(Some("reveal-child"), move |drawer, _| {
                drawer.set_can_target(drawer.reveals_child());
                let weak = w.clone();
                glib::idle_add_local_once(move || {
                    if let Some(e) = weak.upgrade() {
                        e.borrow().canvas.queue_draw();
                    }
                });
            });
        let w = weak(e);
        e.borrow().confirm.connect_close_request(move |_| {
            if let Some(e) = w.upgrade() {
                Self::revert(&e);
            }
            glib::Propagation::Stop
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let confirm_owner = weak(e);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                if let Some(e) = confirm_owner.upgrade() {
                    let confirm = e.borrow().confirm.clone();
                    confirm.close();
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        e.borrow().confirm.add_controller(keys);
        let w = weak(e);
        e.borrow().window.connect_close_request(move |_| {
            let Some(e) = w.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let result = Self::close(&e);
            if result == glib::Propagation::Proceed {
                OPEN_EDITORS
                    .with(|editors| editors.borrow_mut().retain(|owner| !Rc::ptr_eq(owner, &e)));
            }
            result
        });
        let w = weak(e);
        e.borrow_mut().mirror_timer = Some(glib::timeout_add_seconds_local(1, move || {
            if let Some(e) = w.upgrade() {
                Self::check_mirror(&e)
            } else {
                glib::ControlFlow::Break
            }
        }));
    }

    fn run<T, F, S>(e: &Rc<RefCell<Self>>, action: F, success: S)
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
        S: FnOnce(&Rc<RefCell<Self>>, T) + 'static,
    {
        Self::run_with_ui_lock(e, true, action, success);
    }
    fn apply_profile_transaction(
        e: &Rc<RefCell<Self>>,
        profile: Profile,
        confirm: bool,
        message: &'static str,
    ) {
        let (client, mut mirrors) = {
            let mut s = e.borrow_mut();
            if s.busy {
                return;
            }
            (s.client.clone(), std::mem::take(&mut s.mirrors))
        };
        let desired = profile.clone();
        Self::run(
            e,
            move || {
                let (previous, result) = if confirm {
                    match Profile::capture(&client, &mut mirrors) {
                        Ok(previous) => match profile.apply(&client, &mut mirrors) {
                            Ok(()) => (Some(previous), Ok(())),
                            Err(error) => match previous.apply(&client, &mut mirrors) {
                                Ok(()) => (None, Err(format!("{error:#}; previous settings and mirror restored"))),
                                Err(restore) => (Some(previous), Err(format!("{error:#}; restoring previous settings and mirror failed: {restore:#}"))),
                            },
                        },
                        Err(error) => (None, Err(format!("{error:#}"))),
                    }
                } else {
                    match profile.apply(&client, &mut mirrors) {
                        Ok(()) => (None, Ok(())),
                        Err(error) => (
                            Some(profile),
                            Err(format!(
                                "Could not restore previous settings and mirror: {error:#}"
                            )),
                        ),
                    }
                };
                Ok((mirrors, previous, result))
            },
            move |e, (mirrors, previous, result)| {
                let mut s = e.borrow_mut();
                s.mirrors = mirrors;
                s.sync_mirror_state();
                match result {
                    Ok(()) if confirm => {
                        s.previous = previous;
                        s.remaining = 20;
                        s.set_busy(false);
                        s.confirm_label
                            .set_text("Previous settings restore in 20 seconds");
                        s.confirm_progress.set_fraction(1.0);
                        s.confirm_keep.set_sensitive(true);
                        s.confirm_revert.set_label("Revert now");
                        s.confirm.present();
                        s.confirm_revert.grab_focus();
                        s.root.add_css_class("confirmation-pending");
                        s.message(message);
                        drop(s);
                        let weak = weak(e);
                        e.borrow_mut().confirm_timer =
                            Some(glib::timeout_add_seconds_local(1, move || {
                                if let Some(e) = weak.upgrade() {
                                    Self::tick(&e)
                                } else {
                                    glib::ControlFlow::Break
                                }
                            }));
                    }
                    Ok(()) => {
                        s.previous = None;
                        s.confirm.set_visible(false);
                        s.root.remove_css_class("confirmation-pending");
                        s.editor_area.set_sensitive(true);
                        let closing = s.closing;
                        if closing {
                            if let Some(timer) = s.mirror_timer.take() {
                                timer.remove();
                            }
                        } else {
                            s.layout = desired.outputs.clone();
                            s.baseline = Some(desired.outputs);
                            s.rebuild();
                        }
                        let window = s.window.clone();
                        s.message(message);
                        drop(s);
                        if closing {
                            window.close();
                        }
                    }
                    Err(error) => {
                        if let Some(previous) = previous {
                            s.previous = Some(previous);
                            s.set_busy(false);
                            s.confirm_label.set_text(&error);
                            s.confirm_progress.set_fraction(0.0);
                            s.confirm_keep.set_sensitive(false);
                            s.confirm_revert.set_label("Retry restore");
                            s.confirm.present();
                            s.root.add_css_class("confirmation-pending");
                        }
                        s.message(&error);
                        s.profile_status.set_text(&error);
                    }
                }
            },
        );
    }

    fn run_with_ui_lock<T, F, S>(e: &Rc<RefCell<Self>>, lock_ui: bool, action: F, success: S)
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
        S: FnOnce(&Rc<RefCell<Self>>, T) + 'static,
    {
        {
            let mut s = e.borrow_mut();
            if s.busy {
                return;
            }
            s.busy = true;
            if lock_ui {
                s.set_busy(true);
            } else {
                s.update_dirty();
            }
        }
        let (tx, rx) = mpsc::channel::<Result<T, String>>();
        thread::spawn(move || {
            let result = action().map_err(|x| format!("{x:#}"));
            let _ = tx.send(result);
        });
        let weak = Rc::downgrade(e);
        let mut success = Some(success);
        glib::timeout_add_local(Duration::from_millis(40), move || match rx.try_recv() {
            Ok(result) => {
                if let Some(e) = weak.upgrade() {
                    let mut s = e.borrow_mut();
                    s.busy = false;
                    s.set_busy(false);
                    drop(s);
                    match result {
                        Ok(value) => {
                            if let Some(done) = success.take() {
                                done(&e, value)
                            }
                        }
                        Err(error) => {
                            let s = e.borrow();
                            s.message(&error);
                            if !lock_ui {
                                s.profile_status.set_text(&error);
                            }
                        }
                    }
                    e.borrow_mut().update_dirty();
                    Self::flush_pending_sync(&e);
                }
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => {
                if let Some(e) = weak.upgrade() {
                    let mut s = e.borrow_mut();
                    s.busy = false;
                    s.set_busy(false);
                    s.message("Display operation ended unexpectedly.");
                    s.update_dirty();
                    drop(s);
                    Self::flush_pending_sync(&e);
                }
                glib::ControlFlow::Break
            }
        });
    }

    fn set_busy(&self, busy: bool) {
        self.editor_area
            .set_sensitive(!busy && self.previous.is_none());
        self.toolbar.set_sensitive(!busy && self.previous.is_none());
        self.confirm.set_sensitive(!busy);
        self.layout_actions
            .set_sensitive(!busy && self.previous.is_none());
    }
    fn message(&self, text: &str) {
        self.status.set_text(text);
    }
    fn refresh(e: &Rc<RefCell<Self>>) {
        Self::refresh_with(e, "All changes applied.");
    }
    fn refresh_with(e: &Rc<RefCell<Self>>, applied: &'static str) {
        e.borrow().message("Reading connected displays…");
        let client = e.borrow().client.clone();
        Self::run(
            e,
            move || Ok((client.outputs()?, daemon::enabled().ok())),
            move |e, (outputs, daemon_active)| {
                let mut s = e.borrow_mut();
                s.outputs = outputs;
                if let Some(active) = daemon_active {
                    s.updating.set(true);
                    s.daemon_toggle.set_active(active);
                    s.daemon_toggle.set_sensitive(true);
                    s.updating.set(false);
                }
                match snapshot(&s.outputs) {
                    Ok(layout) => {
                        s.layout = layout.clone();
                        s.baseline = Some(layout);
                        s.rebuild();
                        s.message(applied);
                    }
                    Err(error) => s.message(&format!("{error:#}")),
                }
                s.sync_mirror_state();
            },
        );
    }
    fn flush_pending_sync(e: &Rc<RefCell<Self>>) {
        let pending = {
            let mut s = e.borrow_mut();
            std::mem::take(&mut s.sync_pending)
        };
        if pending {
            Self::synchronize(e);
        }
    }
    fn synchronize(e: &Rc<RefCell<Self>>) {
        {
            let mut s = e.borrow_mut();
            if s.closing {
                return;
            }
            if s.busy {
                s.sync_pending = true;
                return;
            }
            if s.previous.is_some() {
                // A picker switch supersedes this editor's pending confirmation.
                s.cancel_timer();
                s.previous = None;
                s.confirm.set_visible(false);
                s.root.remove_css_class("confirmation-pending");
                s.editor_area.set_sensitive(true);
                s.toolbar.set_sensitive(true);
                s.layout_actions.set_sensitive(true);
            }
        }
        Self::refresh_with(e, "Display layout synchronized to the profile switch.");
    }

    fn update_dirty(&mut self) {
        let changed = self
            .baseline
            .as_ref()
            .is_some_and(|base| !same_layout(base, &self.layout));
        self.discard_button
            .set_sensitive(changed && !self.busy && self.previous.is_none());
        self.apply_button
            .set_sensitive(changed && !self.busy && self.previous.is_none());
        self.apply_button.set_tooltip_text(Some(if changed {
            "Unapplied changes"
        } else {
            "All changes applied"
        }));
        if ["All changes applied.", "Unapplied changes"].contains(&self.status.text().as_str()) {
            self.message(if changed {
                "Unapplied changes"
            } else {
                "All changes applied."
            });
        }
    }
    fn rebuild(&mut self) {
        let names: Vec<String> = self.layout.iter().map(|row| row.name.clone()).collect();
        if !self
            .selected
            .as_ref()
            .is_some_and(|name| names.contains(name))
        {
            self.selected = self
                .layout
                .iter()
                .find(|row| row.enabled)
                .or_else(|| self.layout.first())
                .map(|row| row.name.clone());
        }
        if let Some(popover) = self.mirror_menu.popover() {
            popover.popdown();
        }
        if let Some(parent) = self.mirror_menu.parent().and_downcast::<gtk::Box>() {
            parent.remove(&self.mirror_menu);
        }
        if let Some(parent) = self.mirror_stop.parent().and_downcast::<gtk::Box>() {
            parent.remove(&self.mirror_stop);
        }
        clear_box(&self.dock);
        clear_box(&self.advanced);
        self.summary.set_text(&format!(
            "{} connected · Select a display to configure",
            names.len()
        ));
        self.identify_button.set_sensitive(!names.is_empty());
        if names.is_empty() {
            self.dock
                .append(&gtk::Label::new(Some("No connected displays")));
            self.drawer.set_reveal_child(false);
            self.update_mirror_sources();
            self.canvas.queue_draw();
            self.update_dirty();
            return;
        }
        let Some(selected) = self.selected.clone() else {
            return;
        };
        let Some(row) = self.layout.iter().find(|row| row.name == selected).cloned() else {
            return;
        };
        let titles = names
            .iter()
            .enumerate()
            .map(|(index, name)| format!("{} · {} ({name})", index + 1, self.display_title(name)))
            .collect::<Vec<_>>();
        let selector = dropdown(&titles);
        selector.set_selected(names.iter().position(|name| name == &selected).unwrap_or(0) as u32);
        self.add_control("Selected display", &selector);

        let enabled = gtk::Switch::new();
        enabled.set_active(row.enabled);
        enabled.set_valign(gtk::Align::Center);
        self.add_control("Enabled", &enabled);
        let mut modes = self
            .outputs
            .get(&row.name)
            .and_then(|output| output.get("modes"))
            .and_then(|value| value.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|mode| crate::backend::mode_string(mode).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(current) = &row.mode {
            if !modes.contains(current) {
                modes.insert(0, current.clone());
            }
        }
        let mut sizes = Vec::new();
        for mode in &modes {
            let size = mode.split('@').next().unwrap_or(mode).to_owned();
            if !sizes.contains(&size) {
                sizes.push(size);
            }
        }
        let resolution = dropdown(
            &sizes
                .iter()
                .map(|size| size.replace('x', " × "))
                .collect::<Vec<_>>(),
        );
        let current = row.mode.clone().or_else(|| modes.first().cloned());
        if let Some(size) = current.as_ref().and_then(|value| value.split('@').next()) {
            resolution.set_selected(
                sizes
                    .iter()
                    .position(|candidate| candidate == size)
                    .unwrap_or(0) as u32,
            );
        }
        self.add_control("Resolution", &resolution);
        let rates = modes
            .iter()
            .filter(|mode| {
                current
                    .as_ref()
                    .is_some_and(|value| mode.split('@').next() == value.split('@').next())
            })
            .cloned()
            .collect::<Vec<_>>();
        let refresh = dropdown(
            &rates
                .iter()
                .map(|mode| format!("{} Hz", mode.split('@').nth(1).unwrap_or("60")))
                .collect::<Vec<_>>(),
        );
        if let Some(index) = current
            .as_ref()
            .and_then(|value| rates.iter().position(|mode| mode == value))
        {
            refresh.set_selected(index as u32);
        }
        self.add_control("Refresh rate", &refresh);
        let scale = spin(row.scale * 100.0, 10.0, 1000.0, 5.0, 1);
        self.add_control("Scale (%)", &scale);
        resolution.set_sensitive(row.enabled);
        refresh.set_sensitive(row.enabled);
        scale.set_sensitive(row.enabled);
        let mirror_control = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        mirror_control.append(&self.mirror_menu);
        mirror_control.append(&self.mirror_stop);
        self.add_control("Display mode", &mirror_control);
        let advanced_toggle = gtk::ToggleButton::with_label("Advanced");
        advanced_toggle.set_active(self.drawer.reveals_child());
        self.dock.append(&advanced_toggle);
        let advanced_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let advanced_title = heading("Advanced settings");
        advanced_title.set_hexpand(true);
        advanced_header.append(&advanced_title);
        let close_advanced = gtk::Button::with_label("Close");
        let toggle = advanced_toggle.clone();
        close_advanced.connect_clicked(move |_| toggle.set_active(false));
        advanced_header.append(&close_advanced);
        self.advanced.append(&advanced_header);
        self.advanced
            .append(&gtk::Label::new(Some(&self.display_title(&row.name))));
        self.advanced.append(&heading("Position · logical pixels"));
        let x = spin(row.x as f64, -100000.0, 100000.0, 1.0, 0);
        let y = spin(row.y as f64, -100000.0, 100000.0, 1.0, 0);
        self.add_advanced("X", &x);
        self.add_advanced("Y", &y);
        self.advanced
            .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        self.advanced.append(&heading("Custom resolution"));
        let (mode_width, mode_height) = row
            .mode
            .as_deref()
            .unwrap_or("1920x1080@60")
            .split('@')
            .next()
            .unwrap_or("1920x1080")
            .split_once('x')
            .and_then(|(width, height)| {
                Some((width.parse::<f64>().ok()?, height.parse::<f64>().ok()?))
            })
            .unwrap_or((1920.0, 1080.0));
        let width = spin(mode_width, 1.0, 100000.0, 1.0, 0);
        let height = spin(mode_height, 1.0, 100000.0, 1.0, 0);
        let hz = spin(
            row.mode
                .as_deref()
                .and_then(|mode| mode.split('@').nth(1))
                .and_then(|value| value.parse().ok())
                .unwrap_or(60.0),
            0.001,
            1000.0,
            1.0,
            3,
        );
        for (label, widget) in [
            ("Width (pixels)", &width),
            ("Height (pixels)", &height),
            ("Refresh rate (Hz)", &hz),
        ] {
            self.add_advanced(label, widget);
        }
        let custom = gtk::Button::with_label("Use custom resolution");
        self.advanced.append(&custom);
        self.advanced.append(&dim_label("Custom modes may be rejected by your display. Changes stay staged until Apply changes."));
        for widget in [&x, &y, &width, &height, &hz] {
            widget.set_sensitive(row.enabled);
        }
        custom.set_sensitive(row.enabled);
        if let Some(owner) = self.owner.upgrade() {
            Self::hook_controls(
                &owner,
                &row,
                MonitorControls {
                    selector: &selector,
                    enabled: &enabled,
                    resolution: &resolution,
                    refresh: &refresh,
                    scale: &scale,
                    x: &x,
                    y: &y,
                    width: &width,
                    height: &height,
                    hz: &hz,
                    advanced: &advanced_toggle,
                    custom: &custom,
                    modes: &modes,
                    sizes: &sizes,
                    rates: &rates,
                },
            );
        }
        self.update_mirror_sources();
        self.canvas.queue_draw();
        self.update_dirty();
    }
    fn add_control(&self, label: &str, widget: &impl IsA<gtk::Widget>) {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let cap = dim_label(label);
        col.append(&cap);
        col.append(widget);
        widget.set_tooltip_text(Some(label));
        self.dock.append(&col);
    }
    fn add_advanced(&self, label: &str, widget: &impl IsA<gtk::Widget>) {
        let col = gtk::Box::new(gtk::Orientation::Vertical, 6);
        col.append(&dim_label(label));
        col.append(widget);
        widget.set_tooltip_text(Some(label));
        self.advanced.append(&col);
    }
    fn hook_controls(e: &Rc<RefCell<Self>>, row: &OutputConfig, controls: MonitorControls<'_>) {
        let MonitorControls {
            selector,
            enabled,
            resolution,
            refresh,
            scale,
            x,
            y,
            width,
            height,
            hz,
            advanced,
            custom,
            modes,
            sizes,
            rates,
        } = controls;
        let name = row.name.clone();
        let w = weak(e);
        let name2 = name.clone();
        enabled.connect_state_set(move |_, v| {
            if let Some(e) = w.upgrade() {
                let mut s = e.borrow_mut();
                if v && s
                    .layout
                    .iter()
                    .find(|i| i.name == name2)
                    .is_some_and(|i| i.mode.is_none())
                {
                    if let Some(mode) = s
                        .outputs
                        .get(&name2)
                        .and_then(|o| o.get("modes"))
                        .and_then(|v| v.as_array())
                        .and_then(|v| v.first())
                        .and_then(|v| crate::backend::mode_string(v).ok())
                    {
                        if let Some(i) = s.layout.iter_mut().find(|x| x.name == name2) {
                            i.mode = Some(mode);
                        }
                    }
                }
                if let Some(i) = s.layout.iter_mut().find(|x| x.name == name2) {
                    i.enabled = v;
                }
                s.rebuild();
            }
            glib::Propagation::Stop
        });
        let w = weak(e);
        let modes = modes.to_vec();
        let sizes = sizes.to_vec();
        resolution.connect_notify_local(Some("selected"), move |d, _| {
            if let Some(size) = sizes.get(d.selected() as usize) {
                let options: Vec<_> = modes
                    .iter()
                    .filter(|m| m.split('@').next() == Some(size.as_str()))
                    .collect();
                if let Some(e) = w.upgrade() {
                    let old = e
                        .borrow()
                        .layout
                        .iter()
                        .find(|i| i.name == name)
                        .and_then(|i| i.mode.as_ref())
                        .and_then(|m| m.split('@').nth(1))
                        .map(str::to_owned);
                    let chosen = options
                        .iter()
                        .find(|m| m.split('@').nth(1) == old.as_deref())
                        .unwrap_or(&options[0])
                        .to_string();
                    if let Some(i) = e.borrow_mut().layout.iter_mut().find(|i| i.name == name) {
                        i.mode = Some(chosen);
                    }
                    e.borrow_mut().rebuild();
                }
            }
        });
        let w = weak(e);
        let rates = rates.to_vec();
        let name2 = row.name.clone();
        refresh.connect_notify_local(Some("selected"), move |d, _| {
            if let Some(mode) = rates.get(d.selected() as usize) {
                if let Some(e) = w.upgrade() {
                    if let Some(i) = e.borrow_mut().layout.iter_mut().find(|i| i.name == name2) {
                        i.mode = Some(mode.clone());
                    }
                    e.borrow_mut().rebuild();
                }
            }
        });
        let w = weak(e);
        let n = row.name.clone();
        scale.connect_value_changed(move |v| {
            if let Some(e) = w.upgrade() {
                if let Some(i) = e.borrow_mut().layout.iter_mut().find(|i| i.name == n) {
                    i.scale = v.value() / 100.0;
                }
                e.borrow().canvas.queue_draw();
                e.borrow_mut().update_dirty();
            }
        });
        for (widget, is_x) in [(x.clone(), true), (y.clone(), false)] {
            let w = weak(e);
            let n = row.name.clone();
            widget.connect_value_changed(move |v| {
                if let Some(e) = w.upgrade() {
                    if let Some(i) = e.borrow_mut().layout.iter_mut().find(|i| i.name == n) {
                        if is_x {
                            i.x = v.value_as_int()
                        } else {
                            i.y = v.value_as_int()
                        }
                    }
                    e.borrow().canvas.queue_draw();
                    e.borrow_mut().update_dirty();
                }
            });
        }
        let w = weak(e);
        let n = row.name.clone();
        let width = width.clone();
        let height = height.clone();
        let hz = hz.clone();
        custom.connect_clicked(move |_|if let Some(e)=w.upgrade(){let mode=format!("{}x{}@{}",width.value_as_int(),height.value_as_int(),hz.value());if let Some(i)=e.borrow_mut().layout.iter_mut().find(|i|i.name==n){i.mode=Some(mode);}e.borrow_mut().rebuild();e.borrow().message("Custom resolution staged. Apply changes to test it; unsupported modes may fail.");});
        let w = weak(e);
        advanced.connect_toggled(move |b| {
            if let Some(e) = w.upgrade() {
                e.borrow().drawer.set_reveal_child(b.is_active());
            }
        });
        let w = weak(e);
        let name = row.name.clone();
        selector.connect_notify_local(Some("selected"), move |d, _| {
            if let Some(e) = w.upgrade() {
                let names: Vec<String> = e.borrow().layout.iter().map(|i| i.name.clone()).collect();
                if let Some(n) = names.get(d.selected() as usize) {
                    let mut s = e.borrow_mut();
                    s.selected = Some(n.clone());
                    s.rebuild();
                }
            }
            let _ = &name;
        });
    }

    fn display_title(&self, name: &str) -> String {
        let output = self.outputs.get(name);
        let title = output
            .map(|o| {
                ["make", "model"]
                    .iter()
                    .filter_map(|k| o.get(*k).and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if !title.is_empty() {
            title
        } else if name.starts_with("eDP") {
            "Built-in display".into()
        } else {
            name.into()
        }
    }
    fn update_mirror_sources(&mut self) {
        let names = self
            .layout
            .iter()
            .filter(|item| {
                Some(&item.name) != self.selected.as_ref()
                    && self.outputs.contains_key(&item.name)
                    && (self.mirror_source_first
                        || item.enabled
                            && self.outputs[&item.name]
                                .get("logical")
                                .is_some_and(|value| !value.is_null()))
            })
            .map(|item| item.name.clone())
            .collect::<Vec<_>>();
        let old_name = self
            .mirror_peer
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .map(|item| item.string().to_string());
        self.mirror_peer.set_model(Some(&gtk::StringList::new(
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        )));
        if let Some(index) = old_name.and_then(|old| names.iter().position(|name| name == &old)) {
            self.mirror_peer.set_selected(index as u32);
        }
        self.mirror_peer.set_sensitive(!names.is_empty());
        self.update_mirror_actions();
    }
    fn sync_mirror_state(&mut self) {
        match self.mirrors.current() {
            Ok(mirror) => self.mirror_pair = mirror,
            Err(error) => {
                self.mirror_pair = None;
                self.message(&format!("Cannot read active mirror: {error:#}"));
            }
        }
        self.update_mirror_actions();
    }
    fn update_mirror_actions(&mut self) {
        let running = self.mirror_pair.is_some();
        let selected = self
            .layout
            .iter()
            .find(|i| Some(&i.name) == self.selected.as_ref());
        let active = selected.is_some_and(|i| {
            self.outputs.contains_key(&i.name)
                && (!self.mirror_source_first
                    || i.enabled
                        && self.outputs[&i.name]
                            .get("logical")
                            .is_some_and(|value| !value.is_null()))
        });
        let role = if self.mirror_source_first {
            "Source"
        } else {
            "Destination"
        };
        let peer_role = if self.mirror_source_first {
            "target"
        } else {
            "source"
        };
        self.mirror_destination.set_text(
            &selected
                .map(|i| format!("{role}: {} · {}", self.display_title(&i.name), i.name))
                .unwrap_or_else(|| format!("No {} selected", role.to_lowercase())),
        );
        self.mirror_choice_label
            .set_text(if self.mirror_source_first {
                "Mirror to display"
            } else {
                "Mirror content from"
            });
        self.mirror_peer.set_tooltip_text(Some(&format!(
            "Choose a {peer_role} display; selected display is the {}",
            role.to_lowercase()
        )));
        if running {
            if let Some(popover) = self.mirror_menu.popover() {
                popover.popdown();
            }
        }
        self.mirror_menu.set_visible(!running);
        let multiple = self.outputs.len() > 1;
        let status = if let Some(pair) = &self.mirror_pair {
            format!(
                "Active · {} → {}. Stop this mirror before starting another.",
                pair.source, pair.target
            )
        } else if !multiple {
            "Mirroring requires at least two connected displays.".into()
        } else if !active {
            "Enable this source and apply its display settings before mirroring.".into()
        } else if self.mirror_peer.selected_item().is_none() {
            if self.mirror_source_first {
                "No other connected display. Connect a target first.".into()
            } else {
                "No active source display. Enable a source and apply its display settings first."
                    .into()
            }
        } else {
            "Not active · The target will be enabled automatically when mirroring starts.".into()
        };
        self.mirror_status.set_text(&status);
        self.mirror_start.set_sensitive(
            multiple && active && self.mirror_peer.selected_item().is_some() && !running,
        );
        self.mirror_stop.set_visible(running);
        self.mirror_start.set_visible(!running);
    }

    fn draw(&mut self, _area: &gtk::DrawingArea, cr: &cairo::Context, width: i32, height: i32) {
        let foreground = self.window.color();
        let accent = self.canvas.color();
        cr.set_source_rgba(
            foreground.red() as f64,
            foreground.green() as f64,
            foreground.blue() as f64,
            0.06,
        );
        cr.set_line_width(1.0);
        for x in (24..width).step_by(40) {
            cr.move_to(x as f64, 0.0);
            cr.line_to(x as f64, height as f64);
        }
        for y in (24..height).step_by(40) {
            cr.move_to(0.0, y as f64);
            cr.line_to(width as f64, y as f64);
        }
        let _ = cr.stroke();
        let active: Vec<_> = self.layout.iter().filter(|item| item.enabled).collect();
        self.rectangles.clear();
        if active.is_empty() {
            cr.set_source_rgba(
                foreground.red() as f64,
                foreground.green() as f64,
                foreground.blue() as f64,
                0.7,
            );
            cr.set_font_size(14.0);
            cr.move_to(24.0, height as f64 / 2.0);
            let _ = cr.show_text("Enable a display using the dock below.");
            return;
        }
        let left = active.iter().map(|item| item.x).min().unwrap_or(0);
        let top = active.iter().map(|item| item.y).min().unwrap_or(0);
        let right = active
            .iter()
            .map(|item| item.x + dimensions(item).0)
            .max()
            .unwrap_or(1);
        let bottom = active
            .iter()
            .map(|item| item.y + dimensions(item).1)
            .max()
            .unwrap_or(1);
        let available = (width
            - if self.drawer.reveals_child() {
                self.drawer.width()
            } else {
                0
            })
        .max(1) as f64;
        let horizontal = (available - 80.0).max(1.0) / (right - left).max(1) as f64;
        let vertical = (height as f64 - 80.0).max(1.0) / (bottom - top).max(1) as f64;
        let ratio = horizontal.min(vertical) * self.canvas_zoom;
        let origin_x = (available - (right - left) as f64 * ratio) / 2.0;
        let origin_y = (height as f64 - (bottom - top) as f64 * ratio) / 2.0;
        for (index, item) in self.layout.iter().enumerate() {
            if !item.enabled {
                continue;
            }
            let (logical_width, logical_height) = dimensions(item);
            let x = origin_x + (item.x - left) as f64 * ratio;
            let y = origin_y + (item.y - top) as f64 * ratio;
            let w = logical_width as f64 * ratio;
            let h = logical_height as f64 * ratio;
            let selected = self.selected.as_deref() == Some(&item.name);
            let color = if selected { &accent } else { &foreground };
            let radius = 8.0_f64.min(w / 2.0).min(h / 2.0);
            rounded_rectangle(cr, x, y, w, h, radius);
            cr.set_source_rgba(
                color.red() as f64,
                color.green() as f64,
                color.blue() as f64,
                if selected { 0.14 } else { 0.06 },
            );
            let _ = cr.fill_preserve();
            cr.set_source_rgba(
                color.red() as f64,
                color.green() as f64,
                color.blue() as f64,
                if selected { 1.0 } else { 0.5 },
            );
            cr.set_line_width(if selected { 3.0 } else { 1.5 });
            let _ = cr.stroke();
            cr.save().ok();
            cr.rectangle(x + 4.0, y + 4.0, (w - 8.0).max(0.0), (h - 8.0).max(0.0));
            cr.clip();
            cr.set_source_rgba(
                foreground.red() as f64,
                foreground.green() as f64,
                foreground.blue() as f64,
                1.0,
            );
            cr.set_font_size(13.0);
            cr.move_to(x + 12.0, y + 24.0);
            let _ = cr.show_text(&format!("{} · {}", index + 1, item.name));
            cr.set_font_size(16.0);
            cr.move_to(x + 12.0, y + 53.0);
            let _ = cr.show_text(&self.display_title(&item.name));
            cr.set_font_size(12.0);
            cr.move_to(x + 12.0, y + 77.0);
            let _ = cr.show_text(&format!(
                "{} · Scale {}%",
                item.mode.as_deref().unwrap_or("No mode"),
                item.scale * 100.0
            ));
            cr.restore().ok();
            self.rectangles.push(CanvasRect {
                name: item.name.clone(),
                x,
                y,
                width: w,
                height: h,
                ratio,
            });
        }
    }
    fn key_pressed(
        e: &Rc<RefCell<Self>>,
        key: gdk::Key,
        state: gdk::ModifierType,
    ) -> glib::Propagation {
        let mut s = e.borrow_mut();
        if s.layout.is_empty() {
            return glib::Propagation::Proceed;
        }
        if key == gdk::Key::Page_Up || key == gdk::Key::Page_Down {
            let names = s
                .layout
                .iter()
                .map(|item| item.name.clone())
                .collect::<Vec<_>>();
            let index = s
                .selected
                .as_ref()
                .and_then(|name| names.iter().position(|item| item == name))
                .unwrap_or(0);
            let delta = if key == gdk::Key::Page_Down { 1 } else { -1 };
            s.selected = Some(
                names[(index as isize + delta).rem_euclid(names.len() as isize) as usize].clone(),
            );
            s.rebuild();
            return glib::Propagation::Stop;
        }
        let (dx, dy) = match key {
            gdk::Key::Left => (-1, 0),
            gdk::Key::Right => (1, 0),
            gdk::Key::Up => (0, -1),
            gdk::Key::Down => (0, 1),
            _ => return glib::Propagation::Proceed,
        };
        let step = if state.contains(gdk::ModifierType::SHIFT_MASK) {
            10
        } else {
            1
        };
        let selected = s.selected.clone();
        if let Some(item) = s
            .layout
            .iter_mut()
            .find(|item| selected.as_ref() == Some(&item.name))
        {
            if item.enabled {
                item.x = (item.x + dx * step).clamp(-100000, 100000);
                item.y = (item.y + dy * step).clamp(-100000, 100000);
            }
        }
        s.canvas.queue_draw();
        s.update_dirty();
        glib::Propagation::Stop
    }
    fn drag_begin(e: &Rc<RefCell<Self>>, x: f64, y: f64) {
        let mut s = e.borrow_mut();
        s.dragged = None;
        s.canvas.grab_focus();
        if let Some(rect) = s
            .rectangles
            .iter()
            .rev()
            .find(|r| x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height)
        {
            let name = rect.name.clone();
            let ratio = rect.ratio;
            if let Some(row) = s.layout.iter().find(|i| i.name == name) {
                s.dragged = Some((name.clone(), row.x, row.y, ratio));
                s.selected = Some(name);
            }
            s.rebuild();
        }
    }
    fn drag_update(e: &Rc<RefCell<Self>>, dx: f64, dy: f64) {
        let mut s = e.borrow_mut();
        if let Some((name, x, y, ratio)) = s.dragged.clone() {
            if let Some(item) = s.layout.iter_mut().find(|i| i.name == name) {
                item.x = (x as f64 + dx / ratio).round().clamp(-100000.0, 100000.0) as i32;
                item.y = (y as f64 + dy / ratio).round().clamp(-100000.0, 100000.0) as i32;
            }
            s.snap_drag(&name, ratio);
            s.canvas.queue_draw();
            s.update_dirty();
        }
    }
    fn drag_end(e: &Rc<RefCell<Self>>) {
        let mut s = e.borrow_mut();
        if s.dragged.take().is_some() {
            s.rebuild();
        }
    }
    fn snap_drag(&mut self, name: &str, ratio: f64) {
        // Snap only the dragged output within 12 preview pixels of a shared edge.
        let Some(index) = self.layout.iter().position(|row| row.name == name) else {
            return;
        };
        let item = self.layout[index].clone();
        let (width, height) = dimensions(&item);
        let mut best = (12.0 / ratio).powi(2);
        let mut target = None;
        for neighbor in self
            .layout
            .iter()
            .filter(|row| row.enabled && row.name != name)
        {
            let (neighbor_width, neighbor_height) = dimensions(neighbor);
            // Require at least one logical pixel of shared edge, not just a corner.
            let shared_y =
                (neighbor.y - height + 1).max(item.y.min(neighbor.y + neighbor_height - 1));
            let shared_x =
                (neighbor.x - width + 1).max(item.x.min(neighbor.x + neighbor_width - 1));
            for (x, y) in [
                (neighbor.x - width, shared_y),
                (neighbor.x + neighbor_width, shared_y),
                (shared_x, neighbor.y - height),
                (shared_x, neighbor.y + neighbor_height),
            ] {
                let distance = ((x - item.x) as f64).powi(2) + ((y - item.y) as f64).powi(2);
                if distance > best {
                    continue;
                }
                let overlaps = self
                    .layout
                    .iter()
                    .filter(|row| row.enabled && row.name != name)
                    .any(|row| {
                        let (other_width, other_height) = dimensions(row);
                        x < row.x + other_width
                            && x + width > row.x
                            && y < row.y + other_height
                            && y + height > row.y
                    });
                if !overlaps {
                    best = distance;
                    target = Some((x, y));
                }
            }
        }
        if let Some((x, y)) = target {
            self.layout[index].x = x;
            self.layout[index].y = y;
        }
    }

    fn apply(e: &Rc<RefCell<Self>>) {
        let active_mirror = e.borrow_mut().mirrors.current();
        let pair = match active_mirror {
            Ok(pair) => pair,
            Err(error) => {
                e.borrow()
                    .message(&format!("Cannot read active mirror: {error:#}"));
                return;
            }
        };
        let desired = {
            let mut s = e.borrow_mut();
            s.mirror_pair = pair.clone();
            s.layout.clone()
        };
        if !desired.iter().any(|item| item.enabled) {
            e.borrow()
                .message("At least one display must remain enabled.");
            return;
        }
        if pair.as_ref().is_some_and(|p| {
            desired
                .iter()
                .any(|item| !item.enabled && (item.name == p.source || item.name == p.target))
        }) {
            e.borrow()
                .message("Stop mirroring before disabling its source or destination.");
            return;
        }
        e.borrow().message("Applying layout…");
        Self::apply_profile_transaction(
            e,
            Profile {
                outputs: desired,
                mirror: pair,
            },
            true,
            "Layout applied. Awaiting confirmation.",
        );
    }
    fn tick(e: &Rc<RefCell<Self>>) -> glib::ControlFlow {
        let mut s = e.borrow_mut();
        s.remaining -= 1;
        let n = s.remaining;
        s.confirm_label.set_text(&format!(
            "Previous settings restore in {n} {}",
            if n == 1 { "second" } else { "seconds" }
        ));
        s.confirm_progress
            .set_fraction((n as f64 / 20.0).clamp(0.0, 1.0));
        if n <= 0 {
            s.confirm_timer = None;
            drop(s);
            Self::revert(e);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }
    fn cancel_timer(&mut self) {
        if let Some(id) = self.confirm_timer.take() {
            id.remove();
        }
    }
    fn keep(e: &Rc<RefCell<Self>>) {
        let (layout, memory, client) = {
            let mut s = e.borrow_mut();
            if s.previous.is_none() {
                return;
            }
            s.cancel_timer();
            s.previous = None;
            s.baseline = Some(s.layout.clone());
            s.confirm.set_visible(false);
            s.root.remove_css_class("confirmation-pending");
            s.editor_area.set_sensitive(true);
            (s.layout.clone(), s.memory.clone(), s.client.clone())
        };
        Self::run(
            e,
            move || {
                memory.remember(&layout)?;
                let outputs = client.outputs()?;
                let layout = snapshot(&outputs)?;
                Ok((outputs, layout))
            },
            |e, (outputs, layout)| {
                let mut s = e.borrow_mut();
                s.outputs = outputs;
                s.layout = layout.clone();
                s.baseline = Some(layout);
                s.rebuild();
                s.message("All changes applied.");
            },
        );
    }
    fn revert(e: &Rc<RefCell<Self>>) {
        let previous = {
            let mut s = e.borrow_mut();
            if s.busy {
                return;
            }
            let Some(previous) = s.previous.clone() else {
                return;
            };
            s.cancel_timer();
            s.message("Restoring previous display settings and mirror…");
            previous
        };
        Self::apply_profile_transaction(e, previous, false, "Previous settings restored.");
    }

    fn reload_profiles(e: &Rc<RefCell<Self>>, selected: Option<String>) {
        {
            let s = e.borrow();
            if s.busy {
                return;
            }
            s.profile_choice.set_sensitive(false);
            s.profile_apply.set_sensitive(false);
            s.profile_delete.set_sensitive(false);
        }
        let store = e.borrow().profiles.clone();
        Self::run_with_ui_lock(
            e,
            false,
            move || store.list_names(),
            move |e, names| {
                let s = e.borrow();
                let old = selected.or_else(|| {
                    s.profile_choice
                        .selected_item()
                        .and_downcast::<gtk::StringObject>()
                        .map(|v| v.string().to_string())
                });
                s.profile_choice.set_model(Some(&gtk::StringList::new(
                    &names.iter().map(String::as_str).collect::<Vec<_>>(),
                )));
                if let Some(index) =
                    old.and_then(|name| names.iter().position(|candidate| candidate == &name))
                {
                    s.profile_choice.set_selected(index as u32);
                }
                s.profile_choice.set_sensitive(!names.is_empty());
                s.profile_apply.set_sensitive(!names.is_empty());
                s.profile_delete.set_sensitive(!names.is_empty());
                s.profile_status.set_text(if names.is_empty() {
                    "No saved profiles yet."
                } else {
                    ""
                });
            },
        );
    }
    fn save_profile(e: &Rc<RefCell<Self>>) {
        let (name, layout, store, mut mirrors) = {
            let mut s = e.borrow_mut();
            if s.busy || s.previous.is_some() {
                return;
            }
            (
                s.profile_name.text().trim().to_owned(),
                s.layout.clone(),
                s.profiles.clone(),
                std::mem::take(&mut s.mirrors),
            )
        };
        Self::run_with_ui_lock(
            e,
            false,
            move || {
                let result = (|| {
                    let profile = Profile {
                        outputs: layout,
                        mirror: mirrors.current()?,
                    };
                    profile
                        .validate()
                        .map_err(|error| anyhow!("Invalid profile: {error:#}"))?;
                    store.save(&name, &profile)?;
                    Ok::<_, anyhow::Error>(name)
                })();
                Ok((mirrors, result))
            },
            |e, (mirrors, result)| {
                e.borrow_mut().mirrors = mirrors;
                e.borrow_mut().sync_mirror_state();
                match result {
                    Ok(name) => {
                        e.borrow().profile_name.set_text("");
                        Self::reload_profiles(e, Some(name.clone()));
                        e.borrow()
                            .profile_status
                            .set_text(&format!("Saved profile “{name}”."));
                        e.borrow().message(&format!("Saved profile “{name}”."));
                    }
                    Err(error) => {
                        let error = format!("{error:#}");
                        e.borrow().profile_status.set_text(&error);
                        e.borrow().message(&error);
                    }
                }
            },
        );
    }
    fn apply_profile(e: &Rc<RefCell<Self>>) {
        let name = {
            let s = e.borrow();
            if s.busy || s.previous.is_some() {
                return;
            }
            selected_string(&s.profile_choice)
        };
        let Ok(name) = name else {
            e.borrow().profile_status.set_text("Select an item first.");
            return;
        };
        let (store, client) = {
            let s = e.borrow();
            (s.profiles.clone(), s.client.clone())
        };
        e.borrow().message(&format!("Loading profile “{name}”…"));
        Self::run(
            e,
            move || {
                let profile = store.load(&name)?;
                let outputs = client.outputs()?;
                let profile = merge_profile(&profile, &outputs)?;
                profile
                    .validate()
                    .map_err(|error| anyhow!("Invalid profile: {error:#}"))?;
                Ok((outputs, profile))
            },
            |e, (outputs, profile)| {
                let mut s = e.borrow_mut();
                s.outputs = outputs;
                s.layout = profile.outputs.clone();
                s.rebuild();
                drop(s);
                Self::apply_profile_transaction(
                    e,
                    profile,
                    true,
                    "Profile applied. Awaiting confirmation.",
                );
            },
        );
    }
    fn delete_profile(e: &Rc<RefCell<Self>>) {
        let name = {
            let s = e.borrow();
            if s.busy || s.previous.is_some() {
                return;
            }
            selected_string(&s.profile_choice)
        };
        let Ok(name) = name else {
            e.borrow().profile_status.set_text("Select an item first.");
            return;
        };
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message("Delete profile?")
            .detail(format!("Delete profile “{name}”? This cannot be undone."))
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let owner = weak(e);
        let window = e.borrow().window.clone();
        dialog.choose(
            Some(&window),
            None::<&gtk::gio::Cancellable>,
            move |response| {
                if response != Ok(1) {
                    return;
                }
                if let Some(e) = owner.upgrade() {
                    let store = e.borrow().profiles.clone();
                    Self::run(
                        &e,
                        move || {
                            store.delete(&name)?;
                            Ok(name)
                        },
                        |e, name| {
                            Self::reload_profiles(e, None);
                            e.borrow().message(&format!("Deleted profile “{name}”."));
                        },
                    );
                }
            },
        );
    }

    fn choose_file(e: &Rc<RefCell<Self>>, save: bool) {
        let window = {
            let s = e.borrow();
            if s.file_chooser.is_some() || s.busy || s.previous.is_some() {
                return;
            }
            s.window.clone()
        };
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("JSON files"));
        filter.add_mime_type("application/json");
        filter.add_pattern("*.json");
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(if save {
                "Export display configuration"
            } else {
                "Import display configuration"
            })
            .modal(true)
            .filters(&filters)
            .build();
        if save {
            dialog.set_initial_name(Some("niri-display.json"));
        }
        let cancellable = gtk::gio::Cancellable::new();
        e.borrow_mut().file_chooser = Some(cancellable.clone());
        let owner = weak(e);
        let finished = move |result: std::result::Result<gtk::gio::File, glib::Error>| {
            let Some(e) = owner.upgrade() else {
                return;
            };
            e.borrow_mut().file_chooser = None;
            let file = match result {
                Ok(file) => file,
                Err(error)
                    if error.matches(gtk::DialogError::Dismissed)
                        || error.matches(gtk::DialogError::Cancelled) =>
                {
                    return
                }
                Err(error) => {
                    e.borrow().message(&error.to_string());
                    return;
                }
            };
            let Some(path) = file.path() else {
                e.borrow().message("Choose a local JSON file.");
                return;
            };
            if save {
                let layout = e.borrow().layout.clone();
                Self::run(
                    &e,
                    move || {
                        ConfigStore::save(&path, &layout)?;
                        Ok(path)
                    },
                    |e, path| {
                        e.borrow().message(&format!(
                            "Exported display configuration to {}.",
                            path.display()
                        ))
                    },
                );
            } else {
                Self::import_file(&e, path);
            }
        };
        if save {
            dialog.save(Some(&window), Some(&cancellable), finished);
        } else {
            dialog.open(Some(&window), Some(&cancellable), finished);
        }
    }
    fn import_file(e: &Rc<RefCell<Self>>, path: PathBuf) {
        let client = e.borrow().client.clone();
        e.borrow().message("Loading display configuration…");
        Self::run(
            e,
            move || {
                let loaded = ConfigStore::load(&path)?;
                let outputs = client.outputs()?;
                let missing = loaded
                    .iter()
                    .filter(|item| !outputs.contains_key(&item.name))
                    .map(|item| item.name.clone())
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    bail!(
                        "Connect the configuration's missing monitors first: {}",
                        missing.join(", ")
                    );
                }
                let actual = snapshot(&outputs)?;
                let by_name = loaded
                    .into_iter()
                    .map(|item| (item.name.clone(), item))
                    .collect::<std::collections::HashMap<_, _>>();
                let merged = actual
                    .into_iter()
                    .map(|item| by_name.get(&item.name).cloned().unwrap_or(item))
                    .collect::<Layout>();
                if !merged.iter().any(|item| item.enabled) {
                    bail!("At least one display must remain enabled.");
                }
                Ok((outputs, merged))
            },
            |e, (outputs, layout)| {
                let mut s = e.borrow_mut();
                s.outputs = outputs;
                s.layout = layout;
                s.rebuild();
                s.message("Imported configuration. Applying layout…");
                drop(s);
                Self::apply(e);
            },
        );
    }

    fn start_mirror(e: &Rc<RefCell<Self>>) {
        let (source, target, client, mut mirrors) = {
            let mut s = e.borrow_mut();
            if s.busy {
                return;
            }
            let Some(selected) = s.selected.clone() else {
                s.message("Select a display first.");
                return;
            };
            let Some(peer) = s
                .mirror_peer
                .selected_item()
                .and_downcast::<gtk::StringObject>()
            else {
                s.message("Select another display first.");
                return;
            };
            let peer = peer.string().to_string();
            let (source, target) = if s.mirror_source_first {
                (selected, peer)
            } else {
                (peer, selected)
            };
            (
                source,
                target,
                s.client.clone(),
                std::mem::take(&mut s.mirrors),
            )
        };
        let status_text = format!(
            "Mirroring {source} to {target}. Stop the mirror before disabling either display."
        );
        let target_for_ui = target.clone();
        Self::run(
            e,
            move || {
                let (outputs, result) = match client.prepare_mirror(&source, &target) {
                    Ok(outputs) => (Some(outputs), mirrors.start(&source, &target)),
                    Err(error) => (None, Err(error)),
                };
                Ok((
                    mirrors,
                    outputs,
                    result.map_err(|error| format!("{error:#}")),
                ))
            },
            move |e, (mirrors, outputs, result)| {
                let mut s = e.borrow_mut();
                s.mirrors = mirrors;
                if let Some(outputs) = outputs {
                    if let Ok(actual) = snapshot(&outputs) {
                        if let Some(target_row) =
                            actual.iter().find(|row| row.name == target_for_ui)
                        {
                            let baseline = s
                                .baseline
                                .as_ref()
                                .and_then(|rows| rows.iter().find(|row| row.name == target_for_ui));
                            if baseline.is_some_and(|row| !row.enabled) {
                                let unchanged =
                                    s.layout.iter().find(|row| row.name == target_for_ui)
                                        == baseline;
                                if let Some(row) =
                                    s.layout.iter_mut().find(|row| row.name == target_for_ui)
                                {
                                    if unchanged {
                                        *row = target_row.clone();
                                    } else {
                                        row.enabled = true;
                                    }
                                }
                                if let Some(row) = s.baseline.as_mut().and_then(|rows| {
                                    rows.iter_mut().find(|row| row.name == target_for_ui)
                                }) {
                                    *row = target_row.clone();
                                }
                            }
                        }
                    }
                    s.outputs = outputs;
                    s.rebuild();
                }
                match result {
                    Ok(()) => {
                        s.sync_mirror_state();
                        s.message(&status_text);
                    }
                    Err(error) => {
                        s.sync_mirror_state();
                        s.message(&error);
                    }
                }
            },
        );
    }
    fn stop_mirror(e: &Rc<RefCell<Self>>) {
        let mut mirrors = {
            let mut s = e.borrow_mut();
            if s.busy {
                return;
            }
            std::mem::take(&mut s.mirrors)
        };
        Self::run(
            e,
            move || {
                let result = mirrors.stop().map_err(|error| format!("{error:#}"));
                Ok((mirrors, result))
            },
            |e, (mirrors, result)| {
                let mut s = e.borrow_mut();
                s.mirrors = mirrors;
                s.sync_mirror_state();
                match result {
                    Ok(()) => s.message("Mirror stopped."),
                    Err(error) => s.message(&error),
                }
            },
        );
    }
    fn check_mirror(e: &Rc<RefCell<Self>>) -> glib::ControlFlow {
        let mut s = e.borrow_mut();
        if s.busy {
            return glib::ControlFlow::Continue;
        }
        let error = s.mirrors.poll_error();
        let before = s.mirror_pair.clone();
        s.sync_mirror_state();
        if before != s.mirror_pair && error.is_none() {
            s.message(if s.mirror_pair.is_some() {
                "Mirror started in another window."
            } else {
                "Mirror ended."
            });
        }
        if let Some(error) = error {
            s.message(&error);
        }
        glib::ControlFlow::Continue
    }
    fn change_daemon(e: &Rc<RefCell<Self>>, active: bool) {
        let (baseline, memory) = {
            let s = e.borrow();
            if s.updating.get() || s.busy {
                return;
            }
            (s.baseline.clone(), s.memory.clone())
        };
        Self::run(
            e,
            move || {
                let result = (|| -> Result<()> {
                    if active {
                        if let Some(layout) = baseline.as_ref() {
                            if memory
                                .recall(layout.iter().map(|item| item.name.clone()))?
                                .is_none()
                            {
                                memory.remember(layout)?;
                            }
                        }
                    }
                    daemon::set_enabled(active)?;
                    Ok(())
                })()
                .map_err(|error| format!("{error:#}"));
                let actual = daemon::enabled().unwrap_or(!active);
                Ok((actual, result))
            },
            |e, (actual, result)| {
                let s = e.borrow();
                s.updating.set(true);
                s.daemon_toggle.set_active(actual);
                s.daemon_toggle.set_sensitive(true);
                s.updating.set(false);
                match result {
                    Ok(()) => s.message(if actual {
                        "Automatic layout restore enabled."
                    } else {
                        "Automatic layout restore disabled."
                    }),
                    Err(error) => s.message(&error),
                }
            },
        );
    }
    fn identify(e: &Rc<RefCell<Self>>) {
        let mut s = e.borrow_mut();
        for window in s.identification.drain(..) {
            window.close();
        }
        let Some(application) = s.window.application() else {
            return;
        };
        let monitors = WidgetExt::display(&s.window).monitors();
        let numbers = s
            .layout
            .iter()
            .enumerate()
            .map(|(index, row)| (row.name.clone(), index + 1))
            .collect::<std::collections::HashMap<_, _>>();
        for index in 0..monitors.n_items() {
            let Some(monitor) = monitors.item(index).and_downcast::<gdk::Monitor>() else {
                continue;
            };
            let Some(name) = monitor.connector() else {
                continue;
            };
            let Some(number) = numbers.get(name.as_str()) else {
                continue;
            };
            let overlay = gtk::Window::builder()
                .application(&application)
                .title(format!("Identify {name}"))
                .decorated(false)
                .resizable(false)
                .build();
            let label = gtk::Label::new(Some(&format!(
                "{number}\n{}\n{name}",
                s.display_title(&name)
            )));
            label.add_css_class("title-1");
            overlay.set_child(Some(&label));
            overlay.fullscreen_on_monitor(&monitor);
            overlay.present();
            s.identification.push(overlay);
        }
        let weak = weak(e);
        glib::timeout_add_seconds_local(3, move || {
            if let Some(e) = weak.upgrade() {
                for window in e.borrow_mut().identification.drain(..) {
                    window.close();
                }
            }
            glib::ControlFlow::Break
        });
    }
    fn close(e: &Rc<RefCell<Self>>) -> glib::Propagation {
        let mut s = e.borrow_mut();
        if s.busy {
            s.message("Wait for the current display operation before closing.");
            return glib::Propagation::Stop;
        }
        let chooser = s.file_chooser.take();
        if let Some(chooser) = chooser {
            glib::idle_add_local_once(move || chooser.cancel());
        }
        for window in s.identification.drain(..) {
            window.close();
        }
        if s.previous.is_some() {
            s.closing = true;
            drop(s);
            Self::revert(e);
            return glib::Propagation::Stop;
        }
        if let Some(id) = s.mirror_timer.take() {
            id.remove();
        }
        glib::Propagation::Proceed
    }
}

fn rounded_rectangle(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -PI / 2.0, 0.0);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        PI / 2.0,
    );
    cr.arc(x + radius, y + height - radius, radius, PI / 2.0, PI);
    cr.arc(x + radius, y + radius, radius, PI, 3.0 * PI / 2.0);
    cr.close_path();
}
fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}
fn dim_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.add_css_class("dim-label");
    label
}
fn dropdown(labels: &[String]) -> gtk::DropDown {
    let strings = labels.iter().map(String::as_str).collect::<Vec<_>>();
    gtk::DropDown::new(
        Some(gtk::StringList::new(&strings)),
        None::<gtk::Expression>,
    )
}
fn spin(value: f64, low: f64, high: f64, step: f64, digits: u32) -> gtk::SpinButton {
    let button = gtk::SpinButton::with_range(low, high, step);
    button.set_digits(digits);
    button.set_value(value);
    button
}
fn clear_box(widget: &gtk::Box) {
    while let Some(child) = widget.first_child() {
        widget.remove(&child);
    }
}
fn same_layout(a: &Layout, b: &Layout) -> bool {
    let left = a
        .iter()
        .map(|x| (&x.name, x))
        .collect::<std::collections::BTreeMap<_, _>>();
    let right = b
        .iter()
        .map(|x| (&x.name, x))
        .collect::<std::collections::BTreeMap<_, _>>();
    left == right
}
fn selected_string(dropdown: &gtk::DropDown) -> Result<String> {
    dropdown
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|i| i.string().to_string())
        .ok_or_else(|| anyhow!("Select an item first."))
}
