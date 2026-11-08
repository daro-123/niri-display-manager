"""Compact GTK profile switcher for Niri display layouts."""
from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import threading

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk, Pango

from backend import NiriClient, snapshot
from daemon import LayoutMemory
from services import ProfileStore, merge_profile


class ProfileSwitcherWindow(Gtk.ApplicationWindow):
    def __init__(self, application, client=None, store=None, memory=None):
        super().__init__(application=application, title="Display profiles")
        self.set_default_size(380, -1)
        self.set_resizable(False)
        self.client = client or NiriClient()
        self.store = store or ProfileStore()
        self.memory = memory or LayoutMemory()
        self.busy = False
        self.closing = False
        self.previous = None
        self.profiles = []
        self.drop_position = None

        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10,
                       margin_top=14, margin_bottom=14, margin_start=14, margin_end=14)
        self.set_child(root)
        self.list = Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE)
        self.drop_line = Gtk.DrawingArea(can_target=False)
        self.drop_line.set_draw_func(self.draw_drop_indicator)
        self.list.set_activate_on_single_click(True)
        self.list.connect("row-activated", self.row_activated)
        scroller = Gtk.ScrolledWindow(
            propagate_natural_height=True, max_content_height=420,
            hscrollbar_policy=Gtk.PolicyType.NEVER)
        overlay = Gtk.Overlay()
        overlay.set_child(self.list)
        overlay.add_overlay(self.drop_line)
        scroller.set_child(overlay)
        root.append(scroller)
        root.append(Gtk.Label(
            label="1–9 Switch · E Editor · ⠿ Reorder",
            xalign=0, wrap=False, css_classes=["dim-label"],
            tooltip_text="Drag a profile row to reorder."))
        self.status = Gtk.Label(xalign=0, wrap=True, max_width_chars=42)
        self.status.set_visible(False)
        self.status.connect("notify::label", lambda label, *_: label.set_visible(bool(label.get_text())))
        root.append(self.status)
        self.restore_button = Gtk.Button(label="Retry restore")
        self.restore_button.connect("clicked", lambda *_: self.restore())
        self.restore_button.set_visible(False)
        root.append(self.restore_button)
        keys = Gtk.EventControllerKey()
        keys.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        keys.connect("key-pressed", self.key_pressed)
        self.add_controller(keys)
        self.connect("close-request", self.close_requested)
        self.load_profiles()

    def set_busy(self, busy):
        self.busy = busy
        self.list.set_sensitive(not busy and self.previous is None)
        self.restore_button.set_sensitive(not busy)

    def run(self, action, success):
        if self.busy:
            return
        self.set_busy(True)

        def finish(value, error):
            self.set_busy(False)
            if error:
                self.status.set_text(str(error))
                if self.closing and self.previous is None:
                    self.destroy()
            else:
                success(value)
            return GLib.SOURCE_REMOVE

        def worker():
            try:
                result = action()
            except Exception as error:
                GLib.idle_add(finish, None, error)
            else:
                GLib.idle_add(finish, result, None)

        threading.Thread(target=worker, daemon=True).start()

    def load_profiles(self):
        self.status.set_text("Loading profiles…")

        def loaded(names):
            if self.closing:
                self.destroy()
                return
            self.profiles = names
            self.build_rows()
            self.status.set_text("" if names else "No saved profiles.")

        self.run(self.store.list_names, loaded)

    def build_rows(self, selected=None):
        self.clear_drop_indicator()
        while self.list.get_first_child() is not None:
            self.list.remove(self.list.get_first_child())
        for index, name in enumerate(self.profiles):
            row = Gtk.ListBoxRow()
            row.set_cursor_from_name("grab")
            row.profile_name = name
            box = Gtk.Box(spacing=10, margin_top=8, margin_bottom=8,
                          margin_start=10, margin_end=10)
            handle = Gtk.Label(label="⠿", tooltip_text="Drag to reorder this profile")
            box.append(handle)
            box.append(Gtk.Label(label=str(index + 1) if index < 9 else "",
                                 width_chars=1, css_classes=["dim-label"]))
            box.append(Gtk.Label(label=name, xalign=0, hexpand=True,
                                 ellipsize=Pango.EllipsizeMode.END,
                                 max_width_chars=32, tooltip_text=name))
            row.set_child(box)
            source = Gtk.DragSource(actions=Gdk.DragAction.MOVE)
            source.connect("prepare", lambda _source, _x, _y, value=name:
                           Gdk.ContentProvider.new_for_value(value))
            source.connect("drag-end", lambda *_: self.clear_drop_indicator())
            row.add_controller(source)
            target = Gtk.DropTarget.new(str, Gdk.DragAction.MOVE)
            target.connect("enter", self.show_drop_indicator, row)
            target.connect("motion", self.show_drop_indicator, row)
            target.connect("leave", lambda *_: self.clear_drop_indicator())
            target.connect("drop", self.drop_profile, row)
            row.add_controller(target)
            self.list.append(row)
            if name == selected or (selected is None and index == 0):
                self.list.select_row(row)
                row.grab_focus()

    def clear_drop_indicator(self):
        self.drop_position = None
        self.drop_line.queue_draw()

    def draw_drop_indicator(self, _area, cr, width, height):
        if self.drop_position is None:
            return
        found, color = self.list.get_style_context().lookup_color("theme_fg_color")
        if not found:
            color = self.list.get_color()
        cr.set_source_rgba(color.red, color.green, color.blue, color.alpha)
        cr.set_line_width(2)
        y = max(1, min(self.drop_position, height - 1))
        cr.move_to(0, y)
        cr.line_to(width, y)
        cr.stroke()

    def show_drop_indicator(self, _target, _x, y, row):
        self.clear_drop_indicator()
        if self.busy or self.previous is not None:
            return Gdk.DragAction(0)
        allocation = row.get_allocation()
        self.drop_position = allocation.y
        if y >= row.get_height() / 2:
            self.drop_position += allocation.height
        self.drop_line.queue_draw()
        return Gdk.DragAction.MOVE

    def drop_profile(self, _target, name, _x, y, row):
        self.clear_drop_indicator()
        if self.busy or self.previous is not None or name not in self.profiles:
            return False
        target_name = row.profile_name
        if name == target_name:
            return True
        names = [value for value in self.profiles if value != name]
        index = names.index(target_name) + (y >= row.get_height() / 2)
        names.insert(index, name)

        def saved(_):
            self.profiles = names
            self.build_rows(selected=name)
            self.status.set_text("Profile order saved.")
            if self.closing:
                self.destroy()

        self.run(lambda: self.store.save_order(names), saved)
        return True

    def selected_name(self):
        row = self.list.get_selected_row()
        return row.profile_name if row else None

    def key_pressed(self, _controller, keyval, _keycode, state):
        if keyval == Gdk.KEY_Escape:
            self.close()
            return True
        if self.busy or self.previous is not None:
            return False
        if state & (Gdk.ModifierType.CONTROL_MASK | Gdk.ModifierType.ALT_MASK |
                    Gdk.ModifierType.SUPER_MASK):
            return False
        if keyval in (Gdk.KEY_e, Gdk.KEY_E):
            self.open_editor()
            return True
        if Gdk.KEY_1 <= keyval <= Gdk.KEY_9:
            index = keyval - Gdk.KEY_1
        elif Gdk.KEY_KP_1 <= keyval <= Gdk.KEY_KP_9:
            index = keyval - Gdk.KEY_KP_1
        else:
            index = None
        if index is not None:
            if index < len(self.profiles):
                self.apply(self.profiles[index])
            return True
        if keyval in (Gdk.KEY_Return, Gdk.KEY_KP_Enter):
            name = self.selected_name()
            if name is not None:
                self.apply(name)
            return True
        return False

    def open_editor(self):
        try:
            subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve().with_name("app.py"))],
                start_new_session=True,
            )
        except OSError as error:
            self.status.set_text(f"Cannot open editor: {error}")
        else:
            self.destroy()

    def row_activated(self, _list, row):
        if not self.busy and self.previous is None:
            self.apply(row.profile_name)

    def apply(self, name):
        if self.busy or self.previous is not None:
            return
        self.status.set_text(f"Applying {name}…")

        def action():
            outputs = self.client.outputs()
            previous = snapshot(outputs)
            desired = merge_profile(self.store.load(name), outputs)
            try:
                self.client.apply(desired)
            except Exception as error:
                try:
                    self.client.apply(previous)
                except Exception as rollback_error:
                    return previous, f"Apply failed: {error}. Restore failed: {rollback_error}"
                return None, f"Apply failed; previous layout restored: {error}"
            try:
                self.memory.remember(desired)
            except Exception as error:
                return None, f"Profile applied, but automatic restore could not remember it: {error}"
            return None, None

        def applied(result):
            self.previous, error = result
            self.set_busy(False)
            self.restore_button.set_visible(self.previous is not None)
            if error:
                self.status.set_text(error)
                if self.closing and self.previous is not None:
                    self.restore()
                elif self.closing:
                    self.destroy()
            else:
                self.destroy()

        self.run(action, applied)

    def restore(self):
        if self.busy or self.previous is None:
            return
        previous = self.previous

        def restored(_):
            self.previous = None
            self.restore_button.set_visible(False)
            self.set_busy(False)
            self.status.set_text("Previous display layout restored.")
            if self.closing:
                self.destroy()

        self.run(lambda: self.client.apply(previous), restored)

    def close_requested(self, *_):
        if self.busy:
            self.closing = True
            self.status.set_text("Finishing operation before closing…")
            return True
        if self.previous is not None:
            self.closing = True
            self.restore()
            return True
        return False


class ProfileSwitcherApplication(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="io.github.niri_display.profiles",
                         flags=Gio.ApplicationFlags.NON_UNIQUE)

    def do_activate(self):
        window = self.get_active_window()
        if window is None:
            window = ProfileSwitcherWindow(self)
        window.present()
