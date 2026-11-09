use crate::backend::{snapshot, Layout, NiriClient};
use crate::daemon::LayoutMemory;
use crate::services::{merge_profile, ProfileStore};
use anyhow::Result;
use glib::translate::IntoGlib;
use gtk::{gdk, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Default)]
struct State {
    profiles: Vec<String>,
    busy: bool,
    closing: bool,
    destroyed: bool,
    previous: Option<Layout>,
}

struct Picker {
    window: gtk::ApplicationWindow,
    list: gtk::ListBox,
    indicator: gtk::DrawingArea,
    drop_position: Cell<Option<f64>>,
    status: gtk::Label,
    retry: gtk::Button,
    state: RefCell<State>,
    editor_renderer: Option<OsString>,
}

impl Picker {
    fn message(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    fn update_controls(&self) {
        let state = self.state.borrow();
        self.list
            .set_sensitive(!state.busy && state.previous.is_none());
        self.retry.set_visible(state.previous.is_some());
        self.retry.set_sensitive(!state.busy);
    }

    fn clear_indicator(&self) {
        self.drop_position.set(None);
        self.indicator.queue_draw();
    }

    fn run<T: Send + 'static>(
        self: &Rc<Self>,
        action: impl FnOnce() -> Result<T> + Send + 'static,
        done: impl FnOnce(&Rc<Self>, T) + 'static,
    ) {
        if self.state.borrow().busy {
            return;
        }
        self.state.borrow_mut().busy = true;
        self.update_controls();
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(action());
        });
        let this = self.clone();
        let mut done = Some(done);
        glib::timeout_add_local(Duration::from_millis(15), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("Background operation stopped unexpectedly"))
                }
            };
            this.state.borrow_mut().busy = false;
            match result {
                Ok(value) => {
                    if let Some(done) = done.take() {
                        done(&this, value);
                    }
                }
                Err(error) => {
                    this.message(&format!("{error:#}"));
                    if this.state.borrow().closing && this.state.borrow().previous.is_none() {
                        this.window.destroy();
                    }
                }
            }
            this.update_controls();
            glib::ControlFlow::Break
        });
    }

    fn finish_close(self: &Rc<Self>) {
        if self.state.borrow().closing && !self.state.borrow().busy {
            if self.state.borrow().previous.is_some() {
                self.restore();
            } else {
                self.window.destroy();
            }
        }
    }

    fn build_rows(self: &Rc<Self>, selected: Option<&str>) {
        self.clear_indicator();
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let names = self.state.borrow().profiles.clone();
        for (index, name) in names.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            row.set_cursor_from_name(Some("grab"));
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            content.set_margin_top(8);
            content.set_margin_bottom(8);
            content.set_margin_start(10);
            content.set_margin_end(10);
            let handle = gtk::Label::new(Some("⠿"));
            handle.set_tooltip_text(Some("Drag to reorder this profile"));
            content.append(&handle);
            let number = if index < 9 {
                (index + 1).to_string()
            } else {
                String::new()
            };
            let shortcut = gtk::Label::new(Some(&number));
            shortcut.set_width_chars(1);
            shortcut.add_css_class("dim-label");
            content.append(&shortcut);
            let label = gtk::Label::new(Some(name));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(32);
            label.set_tooltip_text(Some(name));
            content.append(&label);
            row.set_child(Some(&content));

            let source = gtk::DragSource::new();
            source.set_actions(gdk::DragAction::MOVE);
            let value = name.clone();
            source.connect_prepare(move |_, _, _| {
                Some(gdk::ContentProvider::for_value(&value.to_value()))
            });
            let weak = Rc::downgrade(self);
            source.connect_drag_end(move |_, _, _| {
                if let Some(this) = weak.upgrade() {
                    this.clear_indicator();
                }
            });
            row.add_controller(source);
            let target = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
            let weak = Rc::downgrade(self);
            let weak_row = row.downgrade();
            target.connect_enter(move |_, _, y| match (weak.upgrade(), weak_row.upgrade()) {
                (Some(this), Some(row)) => this.show_indicator(&row, y),
                _ => gdk::DragAction::empty(),
            });
            let weak = Rc::downgrade(self);
            let weak_row = row.downgrade();
            target.connect_motion(move |_, _, y| match (weak.upgrade(), weak_row.upgrade()) {
                (Some(this), Some(row)) => this.show_indicator(&row, y),
                _ => gdk::DragAction::empty(),
            });
            let weak = Rc::downgrade(self);
            target.connect_leave(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.clear_indicator();
                }
            });
            let weak = Rc::downgrade(self);
            let weak_row = row.downgrade();
            let target_name = name.clone();
            target.connect_drop(move |_, value, _, y| {
                let (Some(this), Some(row), Ok(source)) =
                    (weak.upgrade(), weak_row.upgrade(), value.get::<String>())
                else {
                    return false;
                };
                this.clear_indicator();
                let state = this.state.borrow();
                if state.busy || state.previous.is_some() || !state.profiles.contains(&source) {
                    return false;
                }
                if source == target_name {
                    return true;
                }
                let mut order: Vec<String> = state
                    .profiles
                    .iter()
                    .filter(|name| **name != source)
                    .cloned()
                    .collect();
                let Some(mut index) = order.iter().position(|name| *name == target_name) else {
                    return false;
                };
                if y >= f64::from(row.height()) / 2.0 {
                    index += 1;
                }
                order.insert(index, source.clone());
                drop(state);
                let saved = order.clone();
                this.run(
                    move || ProfileStore::default().save_order(&saved),
                    move |this, ()| {
                        this.state.borrow_mut().profiles = order;
                        this.build_rows(Some(&source));
                        this.message("Profile order saved.");
                        this.finish_close();
                    },
                );
                true
            });
            row.add_controller(target);
            self.list.append(&row);
            if selected == Some(name.as_str()) || (selected.is_none() && index == 0) {
                self.list.select_row(Some(&row));
                row.grab_focus();
            }
        }
    }

    fn show_indicator(&self, row: &gtk::ListBoxRow, y: f64) -> gdk::DragAction {
        self.clear_indicator();
        let state = self.state.borrow();
        if state.busy || state.previous.is_some() {
            return gdk::DragAction::empty();
        }
        let Some(bounds) = row.compute_bounds(&self.list) else {
            return gdk::DragAction::empty();
        };
        self.drop_position.set(Some(
            f64::from(bounds.y())
                + if y >= f64::from(row.height()) / 2.0 {
                    f64::from(bounds.height())
                } else {
                    0.0
                },
        ));
        self.indicator.queue_draw();
        gdk::DragAction::MOVE
    }

    fn apply(self: &Rc<Self>, name: String) {
        if self.state.borrow().busy || self.state.borrow().previous.is_some() {
            return;
        }
        self.message(&format!("Applying {name}…"));
        self.run(move || {
            let client = NiriClient;
            let outputs = client.outputs()?;
            let previous = snapshot(&outputs)?;
            let desired = merge_profile(&ProfileStore::default().load(&name)?, &outputs)?;
            if let Err(error) = client.apply(&desired) {
                return Ok(match client.apply(&previous) {
                    Ok(()) => (None, Some(format!("Apply failed; previous layout restored: {error:#}"))),
                    Err(restore) => (Some(previous), Some(format!("Apply failed: {error:#}. Restore failed: {restore:#}"))),
                });
            }
            if let Err(error) = LayoutMemory::default().remember(&desired) {
                return Ok((None, Some(format!("Profile applied, but automatic restore could not remember it: {error:#}"))));
            }
            Ok((None, None))
        }, |this, (previous, error)| {
            this.state.borrow_mut().previous = previous;
            if let Some(error) = error {
                this.message(&error);
                this.finish_close();
            } else {
                this.window.destroy();
            }
        });
    }

    fn restore(self: &Rc<Self>) {
        if self.state.borrow().busy {
            return;
        }
        let Some(previous) = self.state.borrow().previous.clone() else {
            return;
        };
        self.run(
            move || NiriClient.apply(&previous),
            |this, ()| {
                this.state.borrow_mut().previous = None;
                this.message("Previous display layout restored.");
                this.finish_close();
            },
        );
    }

    fn open_editor(&self) {
        let result = (|| -> Result<()> {
            let mut command = Command::new(std::env::current_exe()?);
            if let Some(renderer) = &self.editor_renderer {
                command.env("GSK_RENDERER", renderer);
            } else {
                command.env_remove("GSK_RENDERER");
            }
            use std::os::unix::process::CommandExt;
            // Match the detached editor launch used by the original picker.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            command.spawn()?;
            Ok(())
        })();
        match result {
            Ok(()) => self.window.destroy(),
            Err(error) => self.message(&format!("Cannot open editor: {error:#}")),
        }
    }

    fn key(self: &Rc<Self>, key: gdk::Key, modifiers: gdk::ModifierType) -> glib::Propagation {
        if key == gdk::Key::Escape {
            self.window.close();
            return glib::Propagation::Stop;
        }
        if self.state.borrow().busy || self.state.borrow().previous.is_some() {
            return glib::Propagation::Proceed;
        }
        if modifiers.intersects(
            gdk::ModifierType::CONTROL_MASK
                | gdk::ModifierType::ALT_MASK
                | gdk::ModifierType::SUPER_MASK,
        ) {
            return glib::Propagation::Proceed;
        }
        if key == gdk::Key::e || key == gdk::Key::E {
            self.open_editor();
            return glib::Propagation::Stop;
        }
        let index = if (gdk::Key::_1..=gdk::Key::_9).contains(&key) {
            Some(key.into_glib() - gdk::Key::_1.into_glib())
        } else if (gdk::Key::KP_1..=gdk::Key::KP_9).contains(&key) {
            Some(key.into_glib() - gdk::Key::KP_1.into_glib())
        } else if key == gdk::Key::Return || key == gdk::Key::KP_Enter {
            self.list.selected_row().map(|row| row.index() as u32)
        } else {
            return glib::Propagation::Proceed;
        };
        let name =
            index.and_then(|index| self.state.borrow().profiles.get(index as usize).cloned());
        if let Some(name) = name {
            self.apply(name);
        }
        glib::Propagation::Stop
    }
}

pub fn present(app: &gtk::Application, editor_renderer: Option<OsString>) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Display profiles")
        .default_width(380)
        .resizable(false)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.set_margin_top(14);
    root.set_margin_bottom(14);
    root.set_margin_start(14);
    root.set_margin_end(14);
    window.set_child(Some(&root));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_activate_on_single_click(true);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&list));
    let indicator = gtk::DrawingArea::new();
    indicator.set_can_target(false);
    overlay.add_overlay(&indicator);
    let scroller = gtk::ScrolledWindow::builder()
        .propagate_natural_height(true)
        .max_content_height(420)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&overlay)
        .build();
    root.append(&scroller);
    let hint = gtk::Label::new(Some("1–9 Switch · E Editor · ⠿ Reorder"));
    hint.set_xalign(0.0);
    hint.add_css_class("dim-label");
    hint.set_tooltip_text(Some("Drag a profile row to reorder."));
    root.append(&hint);
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_max_width_chars(42);
    root.append(&status);
    let retry = gtk::Button::with_label("Retry restore");
    retry.set_visible(false);
    root.append(&retry);
    let this = Rc::new(Picker {
        window,
        list,
        indicator,
        drop_position: Cell::new(None),
        status,
        retry,
        state: RefCell::new(State::default()),
        editor_renderer,
    });
    let weak = Rc::downgrade(&this);
    this.indicator.set_draw_func(move |_, cr, width, height| {
        let Some(this) = weak.upgrade() else {
            return;
        };
        if let Some(y) = this.drop_position.get() {
            let color = this.list.color();
            cr.set_source_rgba(
                f64::from(color.red()),
                f64::from(color.green()),
                f64::from(color.blue()),
                f64::from(color.alpha()),
            );
            cr.set_line_width(2.0);
            let y = y.clamp(1.0, f64::from(height.max(2) - 1));
            cr.move_to(0.0, y);
            cr.line_to(f64::from(width), y);
            let _ = cr.stroke();
        }
    });
    let weak = Rc::downgrade(&this);
    this.list.connect_row_activated(move |_, row| {
        let Some(this) = weak.upgrade() else {
            return;
        };
        let name = this
            .state
            .borrow()
            .profiles
            .get(row.index() as usize)
            .cloned();
        if let Some(name) = name {
            this.apply(name);
        }
    });
    let weak = Rc::downgrade(&this);
    this.retry.connect_clicked(move |_| {
        if let Some(this) = weak.upgrade() {
            this.restore();
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&this);
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        weak.upgrade()
            .map_or(glib::Propagation::Proceed, |this| this.key(key, modifiers))
    });
    this.window.add_controller(keys);
    let weak = Rc::downgrade(&this);
    this.window.connect_close_request(move |_| {
        let Some(this) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if this.state.borrow().busy {
            this.state.borrow_mut().closing = true;
            this.message("Finishing operation before closing…");
            return glib::Propagation::Stop;
        }
        if this.state.borrow().previous.is_some() {
            this.state.borrow_mut().closing = true;
            this.restore();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&this);
    this.window.connect_destroy(move |_| {
        if let Some(this) = weak.upgrade() {
            this.state.borrow_mut().destroyed = true;
        }
    });
    // Own the controller until the native window is destroyed; signal handlers use weak references.
    let owner = this.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if owner.state.borrow().destroyed {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
    this.message("Loading profiles…");
    this.run(
        || ProfileStore::default().list_names(),
        |this, names| {
            let empty = names.is_empty();
            this.state.borrow_mut().profiles = names;
            this.build_rows(None);
            this.message(if empty { "No saved profiles." } else { "" });
            this.finish_close();
        },
    );
    this.window.present();
}
