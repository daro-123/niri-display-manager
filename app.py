#!/usr/bin/env python3
"""GTK 4 display controls for the Niri compositor."""
import copy
import math
import threading
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Gdk", "4.0")
from gi.repository import Gdk, Gio, GLib, Gtk

from backend import NiriClient, snapshot
from services import ConfigStore, MirrorManager


class DisplayWindow(Gtk.ApplicationWindow):
    def __init__(self, application, client=None, store=None, mirrors=None):
        super().__init__(application=application, title="Niri Displays")
        self.set_default_size(1100, 720)
        self.client = client or NiriClient()
        self.store = store or ConfigStore()
        self.mirrors = mirrors or MirrorManager()
        self.outputs = {}
        self.layout = []
        self.baseline = None
        self.selected_monitor = None
        self.dock_controls = {}
        self.mirror_pair = None
        self.mirror_source_first = True
        self.identification_windows = []
        self.file_chooser = None
        self.previous = None
        self.remaining = 0
        self.timer = None
        self.busy = False
        self.closing = False
        self.rectangles = []
        self.dragged = None
        self.canvas_zoom = 1.0
        self.set_child(self.build())
        self.connect("close-request", self.close_requested)
        self.mirror_timer = GLib.timeout_add_seconds(1, self.check_mirror)
        self.refresh()

    def button(self, label, callback):
        button = Gtk.Button(label=label)
        button.connect("clicked", callback)
        return button

    def dropdown(self, labels, title=lambda value: value):
        dropdown = Gtk.DropDown(model=Gtk.StringList.new(labels))
        for selected in (True, False):
            factory = Gtk.SignalListItemFactory()
            factory.connect("setup", lambda _, row: row.set_child(Gtk.Label(xalign=0)))
            factory.connect("bind", lambda _, row, arrow=selected: row.get_child().set_text(
                title(row.get_item().get_string()) + ("   ▾" if arrow else "")))
            if selected:
                dropdown.set_factory(factory)
            else:
                dropdown.set_list_factory(factory)
        return dropdown

    def build(self):
        provider = Gtk.CssProvider()
        provider.load_from_data(b"""
            .display-workspace, .display-dock, .display-drawer {
                background: @theme_base_color;
                border: 1px solid alpha(@theme_fg_color, 0.14);
                border-radius: 12px;
            }
            .display-dock { padding: 12px; }
            .display-drawer { padding: 16px; }
            .confirmation-pending { opacity: 0.45; }
            .display-confirmation { border-radius: 18px; }
            .confirmation-title { font-size: 22px; font-weight: 600; }
            .confirmation-countdown { font-feature-settings: "tnum"; }
            .confirmation-actions button { padding: 10px 20px; }
            .confirmation-progress trough,
            .confirmation-progress progress { min-height: 4px; }
            dropdown > button > box > arrow,
            menubutton > button > box > arrow {
                -gtk-icon-source: none;
                min-width: 0;
                min-height: 0;
                margin: 0;
            }
        """)
        Gtk.StyleContext.add_provider_for_display(
            self.get_display(), provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        root = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        for side in ("top", "bottom", "start", "end"):
            getattr(root, "set_margin_" + side)(16)
        root.set_margin_top(8)
        self.toolbar = Gtk.Box(spacing=8)
        window_handle = Gtk.WindowHandle()
        window_handle.set_child(self.toolbar)
        root.append(window_handle)
        heading = Gtk.Label(label="Displays", xalign=0, hexpand=True)
        heading.add_css_class("title-1")
        self.toolbar.append(heading)
        self.identify_button = self.button("Identify", lambda *_: self.identify())
        self.toolbar.append(self.identify_button)
        for label, callback in (("Import", self.import_config), ("Export", self.export_config)):
            button = self.button(label, lambda *_, action=callback: action())
            button.add_css_class("flat")
            button.set_valign(Gtk.Align.CENTER)
            self.toolbar.append(button)
        settings = Gtk.MenuButton(valign=Gtk.Align.CENTER)
        settings.set_child(Gtk.Label(label="⚙", hexpand=True, halign=Gtk.Align.CENTER))
        settings.set_tooltip_text("Settings")
        settings_popover = Gtk.Popover()
        settings.set_popover(settings_popover)
        settings_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12,
                               margin_top=16, margin_bottom=16, margin_start=16, margin_end=16)
        settings_box.append(Gtk.Label(label="Mirroring settings", xalign=0, css_classes=["heading"]))
        self.mirror_orientation = Gtk.CheckButton(
            label="Selected display is the source", active=self.mirror_source_first)
        self.mirror_orientation.set_tooltip_text(
            "When enabled, choose a target to mirror the selected display. "
            "When disabled, choose a source for the selected destination.")
        self.mirror_orientation.connect("toggled", self.change_mirror_orientation)
        settings_box.append(self.mirror_orientation)
        settings_popover.set_child(settings_box)
        self.toolbar.append(settings)
        self.summary = Gtk.Label(xalign=0)
        self.summary.add_css_class("dim-label")
        root.append(self.summary)
        self.editor = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, vexpand=True)
        root.append(self.editor)
        canvas_panel = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8, hexpand=True, vexpand=True)
        canvas_panel.add_css_class("display-workspace")
        self.editor.append(canvas_panel)
        canvas_heading = Gtk.Label(label="Arrangement", xalign=0, margin_top=14, margin_start=16)
        canvas_heading.add_css_class("heading")
        canvas_panel.append(canvas_heading)
        workspace = Gtk.Overlay(vexpand=True)
        canvas_panel.append(workspace)
        self.canvas = Gtk.DrawingArea(content_width=240, hexpand=True, vexpand=True)
        self.canvas.set_draw_func(self.draw_layout)
        self.canvas.set_overflow(Gtk.Overflow.HIDDEN)
        self.canvas.set_focusable(True)
        self.canvas.set_tooltip_text("Select a display. Drag or use arrow keys to arrange it.")
        drag = Gtk.GestureDrag()
        drag.connect("drag-begin", self.drag_begin)
        drag.connect("drag-update", self.drag_update)
        drag.connect("drag-end", self.drag_end)
        self.canvas.add_controller(drag)
        canvas_keys = Gtk.EventControllerKey()
        canvas_keys.connect("key-pressed", self.canvas_key_pressed)
        self.canvas.add_controller(canvas_keys)
        zoom = Gtk.EventControllerScroll(flags=Gtk.EventControllerScrollFlags.VERTICAL)
        zoom.connect("scroll", self.zoom_canvas)
        self.canvas.add_controller(zoom)
        workspace.set_child(self.canvas)
        canvas_footer = Gtk.Box(spacing=8, margin_start=16, margin_end=12, margin_bottom=12)
        hint = Gtk.Label(label="Drag to arrange · Edges snap together", xalign=0, hexpand=True, wrap=True)
        hint.add_css_class("dim-label")
        canvas_footer.append(hint)
        self.fit_button = self.button("Fit to view", lambda *_: self.fit_canvas())
        canvas_footer.append(self.fit_button)
        canvas_panel.append(canvas_footer)
        self.drawer = Gtk.Revealer(transition_type=Gtk.RevealerTransitionType.SLIDE_LEFT,
                                  halign=Gtk.Align.END, width_request=320, can_target=False)
        self.drawer.connect("notify::reveal-child", self.drawer_visibility_changed)
        self.drawer.connect("notify::child-revealed", lambda *_: self.canvas.queue_draw())
        workspace.add_overlay(self.drawer)
        drawer_scroll = Gtk.ScrolledWindow(hscrollbar_policy=Gtk.PolicyType.NEVER)
        self.drawer.set_child(drawer_scroll)
        self.advanced_settings = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        self.advanced_settings.add_css_class("display-drawer")
        drawer_scroll.set_child(self.advanced_settings)
        dock_scroll = Gtk.ScrolledWindow(
            hscrollbar_policy=Gtk.PolicyType.AUTOMATIC,
            vscrollbar_policy=Gtk.PolicyType.NEVER, min_content_height=108)
        self.editor.append(dock_scroll)
        self.dock = Gtk.Box(spacing=20, valign=Gtk.Align.CENTER)
        self.dock.add_css_class("display-dock")
        dock_scroll.set_child(self.dock)

        self.mirror_menu = Gtk.MenuButton(label="Independent   ▾")
        self.mirror_popover = Gtk.Popover()
        self.mirror_menu.set_popover(self.mirror_popover)
        mirror = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        for side in ("top", "bottom", "start", "end"):
            getattr(mirror, "set_margin_" + side)(16)
        self.mirror_popover.set_child(mirror)
        mirror.append(Gtk.Label(label="Mirroring", xalign=0, css_classes=["heading"]))
        self.mirror_destination = Gtk.Label(xalign=0, wrap=True, max_width_chars=38)
        mirror.append(self.mirror_destination)
        self.mirror_choice_label = Gtk.Label(xalign=0)
        mirror.append(self.mirror_choice_label)
        self.mirror_peer = self.dropdown(
            [], lambda name: f"{self.display_title(name)} · {name}")
        mirror.append(self.mirror_peer)
        self.mirror_peer.connect("notify::selected", self.update_mirror_actions)
        self.mirror_status = Gtk.Label(xalign=0, wrap=True, max_width_chars=38)
        mirror.append(self.mirror_status)
        mirror.append(Gtk.Label(
            label="Mirroring starts immediately. Apply changes affects display settings only.",
            xalign=0, wrap=True, max_width_chars=38, css_classes=["dim-label"]))
        self.start_mirror_button = self.button("Start mirroring now", lambda *_: self.start_mirror())
        self.start_mirror_button.add_css_class("suggested-action")
        mirror.append(self.start_mirror_button)
        self.stop_mirror_button = self.button("Stop mirroring now", lambda *_: self.stop_mirror())
        mirror.append(self.stop_mirror_button)
        self.confirm = Gtk.Window(title="Keep these display settings?", transient_for=self,
                                  modal=True, destroy_with_parent=True, resizable=False)
        self.confirm.add_css_class("display-confirmation")
        self.confirm.connect("close-request", self.confirm_close_requested)
        keys = Gtk.EventControllerKey()
        keys.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        keys.connect("key-pressed", self.confirm_key_pressed)
        self.confirm.add_controller(keys)
        confirmation = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        for side in ("top", "bottom", "start", "end"):
            getattr(confirmation, "set_margin_" + side)(28)
        self.confirm.set_child(confirmation)
        confirmation.append(Gtk.Label(
            label="Keep these display settings?", xalign=0,
            css_classes=["confirmation-title"]))
        confirmation.append(Gtk.Label(
            label="Check that every display looks right.", xalign=0,
            css_classes=["dim-label"]))
        self.confirm_label = Gtk.Label(
            xalign=0, width_chars=43, margin_top=16,
            css_classes=["confirmation-countdown"])
        confirmation.append(self.confirm_label)
        self.confirm_progress = Gtk.ProgressBar(
            css_classes=["confirmation-progress"], margin_bottom=16)
        confirmation.append(self.confirm_progress)
        confirmation_actions = Gtk.Box(
            spacing=12, halign=Gtk.Align.END, css_classes=["confirmation-actions"])
        confirmation.append(confirmation_actions)
        revert_button = self.button("Revert now", lambda *_: self.revert())
        confirmation_actions.append(revert_button)
        keep_button = self.button("Keep changes", lambda *_: self.keep())
        keep_button.add_css_class("suggested-action")
        confirmation_actions.append(keep_button)
        self.confirm.set_default_widget(revert_button)
        self.confirm.set_focus(revert_button)
        footer = Gtk.Box(spacing=12)
        root.append(footer)
        self.status = Gtk.Label(xalign=0, wrap=True, selectable=True, hexpand=True)
        footer.append(self.status)
        self.layout_actions = Gtk.Box(spacing=8, valign=Gtk.Align.CENTER)
        footer.append(self.layout_actions)
        self.discard_button = self.button("Revert", lambda *_: self.refresh())
        self.discard_button.set_tooltip_text("Discard staged edits and reread connected displays")
        self.layout_actions.append(self.discard_button)
        self.apply_button = self.button("Apply changes", lambda *_: self.apply())
        self.apply_button.add_css_class("suggested-action")
        self.layout_actions.append(self.apply_button)
        return root

    def confirm_close_requested(self, *_):
        self.revert()
        return True

    def confirm_key_pressed(self, _, keyval, keycode, state):
        if keyval == Gdk.KEY_Escape:
            return self.confirm_close_requested()
        if keyval in (Gdk.KEY_Return, Gdk.KEY_KP_Enter):
            focused = self.confirm.get_focus()
            if isinstance(focused, Gtk.Button):
                focused.activate()
                return True
        return False

    def message(self, text):
        self.status.set_text(str(text))

    def run(self, action, success):
        if self.busy:
            return
        self.busy = True
        self.layout_actions.set_sensitive(False)
        self.editor.set_sensitive(False)
        self.toolbar.set_sensitive(False)
        self.confirm.set_sensitive(False)

        def finish(value, error):
            self.busy = False
            self.editor.set_sensitive(self.previous is None)
            self.toolbar.set_sensitive(self.previous is None)
            self.confirm.set_sensitive(True)
            self.layout_actions.set_sensitive(self.previous is None)
            if error:
                self.message(error)
            else:
                success(value)
            self.update_dirty()
            return GLib.SOURCE_REMOVE

        def worker():
            try:
                value = action()
            except Exception as error:
                GLib.idle_add(finish, None, str(error))
            else:
                GLib.idle_add(finish, value, None)
        threading.Thread(target=worker, daemon=True).start()

    def refresh(self):
        self.message("Reading connected displays…")
        self.run(self.client.outputs, self.received)

    def received(self, outputs):
        self.outputs = outputs
        self.layout = snapshot(outputs)
        self.rebuild()
        self.baseline = copy.deepcopy(self.layout)
        self.update_dirty()
        self.message("All changes applied.")

    def clear(self, box):
        while box.get_first_child():
            box.remove(box.get_first_child())

    def spin(self, value, low, high, step, changed, digits=0):
        spin = Gtk.SpinButton.new_with_range(low, high, step)
        spin.set_digits(digits)
        spin.set_value(value)
        spin.connect("value-changed", lambda widget: changed(widget.get_value() if digits else widget.get_value_as_int()))
        return spin

    def update_field(self, item, field, value):
        item[field] = value
        self.canvas.queue_draw()
        self.update_dirty()
        if field == "enabled":
            self.update_mirror_sources()

    def custom_resolution(self, item, width, height, refresh):
        self.update_field(item, "mode", f"{width.get_value_as_int()}x{height.get_value_as_int()}@{refresh.get_value():g}")
        self.rebuild()
        self.message("Custom resolution staged. Apply changes to test it; unsupported modes may fail.")

    def display_title(self, name):
        # GTK can rebind an old dropdown item while a disconnected output is removed.
        output = self.outputs.get(name, {})
        title = " ".join(str(output.get(key) or "") for key in ("make", "model")).strip()
        return title or ("Built-in display" if name.startswith("eDP") else name)

    def labeled_control(self, box, label, widget):
        column = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        caption = Gtk.Label(label=label, xalign=0)
        caption.add_css_class("dim-label")
        column.append(caption)
        column.append(widget)
        widget.set_tooltip_text(label)
        box.append(column)
        return widget

    def rebuild(self):
        names = [item["name"] for item in self.layout]
        if self.selected_monitor not in names:
            self.selected_monitor = names[0] if names else None
        self.mirror_popover.popdown()
        if self.mirror_menu.get_parent():
            self.mirror_menu.get_parent().remove(self.mirror_menu)
        self.clear(self.dock)
        self.clear(self.advanced_settings)
        self.dock_controls = {}
        self.summary.set_text(f"{len(names)} connected · Select a display to configure")
        self.identify_button.set_sensitive(bool(names))
        if not names:
            self.dock.append(Gtk.Label(label="No connected displays"))
            self.drawer.set_reveal_child(False)
            self.update_mirror_sources()
            self.canvas.queue_draw()
            self.update_dirty()
            return
        item = next(entry for entry in self.layout if entry["name"] == self.selected_monitor)
        selector = self.dropdown([
            f"{index + 1} · {self.display_title(name)} ({name})" for index, name in enumerate(names)])
        selector.set_selected(names.index(self.selected_monitor))
        selector.connect("notify::selected", lambda widget, _: self.select_monitor(names[widget.get_selected()])
                         if widget.get_selected() < len(names) else None)
        self.labeled_control(self.dock, "Selected display", selector)
        enabled = Gtk.Switch(active=item["enabled"], halign=Gtk.Align.START, valign=Gtk.Align.CENTER)
        self.labeled_control(self.dock, "Enabled", enabled)
        enabled.connect("notify::active", lambda widget, _: self.set_enabled(item, widget.get_active()))
        self.dock_controls["enabled"] = enabled
        controls = Gtk.Box(spacing=20, sensitive=item["enabled"])
        self.dock.append(controls)
        self.dock_controls["settings"] = controls
        modes = [f'{mode["width"]}x{mode["height"]}@{mode["refresh_rate"] / 1000:g}'
                 for mode in self.outputs[item["name"]]["modes"]]
        if item["mode"] and item["mode"] not in modes:
            modes.insert(0, item["mode"])
        resolutions = list(dict.fromkeys(mode.split("@")[0] for mode in modes))
        current = item["mode"] or (modes[0] if modes else None)
        resolution = self.dropdown([size.replace("x", " × ") for size in resolutions])
        if current:
            resolution.set_selected(resolutions.index(current.split("@")[0]))
        resolution.connect("notify::selected", lambda widget, _: self.choose_resolution(
            item, resolutions[widget.get_selected()], modes) if widget.get_selected() < len(resolutions) else None)
        self.labeled_control(controls, "Resolution", resolution)
        rates = [mode for mode in modes if current and mode.split("@")[0] == current.split("@")[0]]
        refresh = self.dropdown([mode.split("@")[1] + " Hz" for mode in rates])
        if current in rates:
            refresh.set_selected(rates.index(current))
        def refresh_changed(widget, _):
            if widget.get_selected() < len(rates):
                self.update_field(item, "mode", rates[widget.get_selected()])
                self.rebuild()
        refresh.connect("notify::selected", refresh_changed)
        self.labeled_control(controls, "Refresh rate", refresh)
        scale = self.spin(item["scale"] * 100, 10, 1000, 5,
                          lambda value: self.update_field(item, "scale", value / 100), 1)
        self.labeled_control(controls, "Scale (%)", scale)
        self.dock_controls.update(resolution=resolution, refresh=refresh, scale=scale)
        self.labeled_control(self.dock, "Display mode", self.mirror_menu)
        advanced_button = Gtk.ToggleButton(label="Advanced", active=self.drawer.get_reveal_child())
        advanced_button.connect("toggled", lambda widget: self.drawer.set_reveal_child(widget.get_active()))
        self.dock.append(advanced_button)
        self.dock_controls["advanced"] = advanced_button
        drawer_header = Gtk.Box(spacing=8)
        drawer_header.append(Gtk.Label(label="Advanced settings", xalign=0, hexpand=True, css_classes=["heading"]))
        drawer_header.append(self.button("Close", lambda *_: advanced_button.set_active(False)))
        self.advanced_settings.append(drawer_header)
        self.advanced_settings.append(Gtk.Label(label=self.display_title(item["name"]), xalign=0, wrap=True))
        advanced_controls = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12, sensitive=item["enabled"])
        self.advanced_settings.append(advanced_controls)
        self.dock_controls["advanced_settings"] = advanced_controls
        advanced_controls.append(Gtk.Label(label="Position · logical pixels", xalign=0, css_classes=["heading"]))
        for field in ("x", "y"):
            self.dock_controls[field] = self.labeled_control(
                advanced_controls, field.upper(), self.spin(item[field], -100000, 100000, 1,
                lambda value, key=field: self.update_field(item, key, value)))
        advanced_controls.append(Gtk.Separator())
        advanced_controls.append(Gtk.Label(label="Custom resolution", xalign=0, css_classes=["heading"]))
        current_width, current_height = self.dimensions(dict(item, scale=1, transform="normal"))
        current_refresh = float((current or "1920x1080@60").split("@")[1])
        width = self.spin(current_width, 1, 100000, 1, lambda _: None)
        height = self.spin(current_height, 1, 100000, 1, lambda _: None)
        hz = self.spin(current_refresh, 0.001, 1000, 1, lambda _: None, 3)
        for label, widget in (("Width (pixels)", width), ("Height (pixels)", height), ("Refresh rate (Hz)", hz)):
            self.labeled_control(advanced_controls, label, widget)
        advanced_controls.append(self.button("Use custom resolution", lambda *_: self.custom_resolution(item, width, height, hz)))
        advanced_controls.append(Gtk.Label(
            label="Custom modes may be rejected by your display. Changes stay staged until Apply changes.",
            xalign=0, wrap=True, max_width_chars=28, css_classes=["dim-label"]))
        self.update_mirror_sources()
        self.canvas.queue_draw()
        self.update_dirty()

    def set_enabled(self, item, enabled):
        if enabled and item["mode"] is None:
            modes = self.outputs[item["name"]]["modes"]
            if modes:
                mode = modes[0]
                self.update_field(item, "mode", f'{mode["width"]}x{mode["height"]}@{mode["refresh_rate"] / 1000:g}')
        self.update_field(item, "enabled", enabled)
        self.dock_controls["settings"].set_sensitive(enabled)
        self.dock_controls["advanced_settings"].set_sensitive(enabled)

    def choose_resolution(self, item, resolution, modes):
        options = [mode for mode in modes if mode.split("@")[0] == resolution]
        old_rate = (item["mode"] or "").split("@")[-1]
        mode = next((mode for mode in options if mode.split("@")[1] == old_rate), options[0])
        self.update_field(item, "mode", mode)
        self.rebuild()

    def select_monitor(self, name):
        if name != self.selected_monitor and any(item["name"] == name for item in self.layout):
            self.selected_monitor = name
            self.rebuild()

    def canvas_key_pressed(self, _, keyval, keycode, state):
        names = [item["name"] for item in self.layout]
        if not names:
            return False
        if keyval in (Gdk.KEY_Page_Up, Gdk.KEY_Page_Down):
            index = names.index(self.selected_monitor)
            self.select_monitor(names[(index + (1 if keyval == Gdk.KEY_Page_Down else -1)) % len(names)])
            return True
        directions = {Gdk.KEY_Left: (-1, 0), Gdk.KEY_Right: (1, 0),
                      Gdk.KEY_Up: (0, -1), Gdk.KEY_Down: (0, 1)}
        if keyval not in directions:
            return False
        item = next(entry for entry in self.layout if entry["name"] == self.selected_monitor)
        if item["enabled"]:
            dx, dy = directions[keyval]
            step = 10 if state & Gdk.ModifierType.SHIFT_MASK else 1
            for field, delta in (("x", dx), ("y", dy)):
                self.update_field(item, field, max(-100000, min(100000, item[field] + delta * step)))
                self.dock_controls[field].set_value(item[field])
        return True

    def identify(self):
        self.clear_identification()
        monitors = self.get_display().get_monitors()
        numbers = {item["name"]: index + 1 for index, item in enumerate(self.layout)}
        for index in range(monitors.get_n_items()):
            monitor = monitors.get_item(index)
            name = monitor.get_connector()
            if name not in numbers:
                continue
            overlay = Gtk.Window(application=self.get_application(), title=f"Identify {name}",
                                 decorated=False, resizable=False)
            label = Gtk.Label(label=f"{numbers[name]}\n{self.display_title(name)}\n{name}")
            label.add_css_class("title-1")
            overlay.set_child(label)
            overlay.fullscreen_on_monitor(monitor)
            overlay.present()
            self.identification_windows.append(overlay)
        GLib.timeout_add_seconds(3, self.clear_identification)

    def clear_identification(self):
        for window in self.identification_windows:
            window.destroy()
        self.identification_windows.clear()
        return GLib.SOURCE_REMOVE

    def dimensions(self, item):
        mode = item.get("mode") or "1920x1080"
        width, height = map(int, mode.split("@")[0].split("x"))
        if item.get("transform") in ("90", "270", "Flipped90", "Flipped270", "flipped-90", "flipped-270"):
            width, height = height, width
        # Niri's mapped output geometry rounds outward, unlike IPC logical sizes.
        return max(1, math.ceil(width / item["scale"])), max(1, math.ceil(height / item["scale"]))

    def snap_drag(self, item, ratio):
        """Snap only the dragged output within 12 preview pixels of a shared edge."""
        others = [other for other in self.layout if other["enabled"] and other is not item]
        width, height = self.dimensions(item)
        best_distance = (12 / ratio) ** 2
        best = None
        for neighbor in others:
            nx, ny = neighbor["x"], neighbor["y"]
            nw, nh = self.dimensions(neighbor)
            # Require at least one logical pixel of shared edge, not just a corner.
            y = max(ny - height + 1, min(item["y"], ny + nh - 1))
            x = max(nx - width + 1, min(item["x"], nx + nw - 1))
            for x, y in ((nx - width, y), (nx + nw, y),
                         (x, ny - height), (x, ny + nh)):
                distance = (x - item["x"]) ** 2 + (y - item["y"]) ** 2
                if distance > best_distance:
                    continue
                if any(
                    x < other["x"] + self.dimensions(other)[0]
                    and x + width > other["x"]
                    and y < other["y"] + self.dimensions(other)[1]
                    and y + height > other["y"]
                    for other in others
                ):
                    continue
                best_distance, best = distance, (x, y)
        if best is not None:
            item["x"], item["y"] = best

    def drawer_visibility_changed(self, drawer, _):
        drawer.set_can_target(drawer.get_reveal_child())
        self.canvas.queue_draw()

    def zoom_canvas(self, _, dx, dy):
        self.canvas_zoom = max(0.25, min(4.0, self.canvas_zoom * 1.15 ** -dy))
        self.canvas.queue_draw()
        return True

    def fit_canvas(self):
        self.canvas_zoom = 1.0
        self.canvas.queue_draw()
        self.message("View fitted to all enabled displays.")

    def draw_layout(self, area, cr, width, height):
        foreground = self.get_color()
        found, accent = self.get_style_context().lookup_color("accent_bg_color")
        if not found:
            _, accent = self.get_style_context().lookup_color("theme_selected_bg_color")
        cr.set_source_rgba(foreground.red, foreground.green, foreground.blue, 0.06)
        cr.set_line_width(1)
        for x in range(24, width, 40):
            cr.move_to(x, 0)
            cr.line_to(x, height)
        for y in range(24, height, 40):
            cr.move_to(0, y)
            cr.line_to(width, y)
        cr.stroke()
        active = [item for item in self.layout if item["enabled"]]
        self.rectangles = []
        if not active:
            cr.set_source_rgba(foreground.red, foreground.green, foreground.blue, 0.7)
            cr.set_font_size(14)
            cr.move_to(24, height / 2)
            cr.show_text("Enable a display using the dock below.")
            return
        left = min(item["x"] for item in active)
        top = min(item["y"] for item in active)
        right = max(item["x"] + self.dimensions(item)[0] for item in active)
        bottom = max(item["y"] + self.dimensions(item)[1] for item in active)
        available_width = max(1, width - (
            self.drawer.get_width() if self.drawer.get_reveal_child() else 0))
        ratio = min(max(1, available_width - 80) / max(right - left, 1),
                    max(1, height - 80) / max(bottom - top, 1)) * self.canvas_zoom
        origin_x = (available_width - (right - left) * ratio) / 2
        origin_y = (height - (bottom - top) * ratio) / 2
        for index, item in enumerate(self.layout):
            if not item["enabled"]:
                continue
            w, h = self.dimensions(item)
            x, y = origin_x + (item["x"] - left) * ratio, origin_y + (item["y"] - top) * ratio
            w, h = w * ratio, h * ratio
            selected = item["name"] == self.selected_monitor
            color = accent if selected else foreground
            radius = min(8, w / 2, h / 2)
            cr.new_sub_path()
            for cx, cy, angle in ((x + w - radius, y + radius, -math.pi / 2),
                                  (x + w - radius, y + h - radius, 0),
                                  (x + radius, y + h - radius, math.pi / 2),
                                  (x + radius, y + radius, math.pi)):
                cr.arc(cx, cy, radius, angle, angle + math.pi / 2)
            cr.close_path()
            cr.set_source_rgba(color.red, color.green, color.blue, 0.14 if selected else 0.06)
            cr.fill_preserve()
            cr.set_source_rgba(color.red, color.green, color.blue, 1 if selected else 0.5)
            cr.set_line_width(3 if selected else 1.5)
            cr.stroke()
            cr.save()
            cr.rectangle(x + 4, y + 4, max(0, w - 8), max(0, h - 8))
            cr.clip()
            cr.set_source_rgba(foreground.red, foreground.green, foreground.blue, 1)
            cr.set_font_size(13)
            cr.move_to(x + 12, y + 24)
            cr.show_text(f"{index + 1} · {item['name']}")
            cr.set_font_size(16)
            cr.move_to(x + 12, y + 53)
            cr.show_text(self.display_title(item["name"]))
            cr.set_font_size(12)
            cr.move_to(x + 12, y + 77)
            cr.show_text(f"{item.get('mode') or 'No mode'} · Scale {item['scale'] * 100:g}%")
            cr.restore()
            self.rectangles.append((item, x, y, w, h, ratio))

    def drag_begin(self, gesture, x, y):
        self.dragged = None
        self.canvas.grab_focus()
        for item, left, top, width, height, ratio in reversed(self.rectangles):
            if left <= x <= left + width and top <= y <= top + height:
                self.select_monitor(item["name"])
                self.dragged = (item, item["x"], item["y"], ratio)
                break

    def drag_update(self, gesture, dx, dy):
        if self.dragged:
            item, x, y, ratio = self.dragged
            item["x"] = max(-100000, min(100000, round(x + dx / ratio)))
            item["y"] = max(-100000, min(100000, round(y + dy / ratio)))
            self.snap_drag(item, ratio)
            self.canvas.queue_draw()
            self.update_dirty()

    def drag_end(self, *_):
        if self.dragged:
            item, x, y, _ = self.dragged
            self.dragged = None
            if (item["x"], item["y"]) != (x, y):
                self.rebuild()

    def apply(self):
        if not any(item["enabled"] for item in self.layout):
            self.message("At least one display must remain enabled.")
            return
        if self.mirror_pair and self.mirrors.running() and any(
                not item["enabled"] and item["name"] in self.mirror_pair for item in self.layout):
            self.message("Stop mirroring before disabling its source or destination.")
            return
        desired = copy.deepcopy(self.layout)
        self.message("Applying layout…")

        def change():
            previous = snapshot(self.client.outputs())
            try:
                self.client.apply(desired)
            except Exception as error:
                try:
                    self.client.apply(previous)
                except Exception as rollback_error:
                    raise RuntimeError(f"Apply failed: {error}. Restore failed: {rollback_error}") from error
                raise RuntimeError(f"Apply failed; previous layout restored: {error}") from error
            return previous

        def applied(previous):
            self.previous = previous
            self.toolbar.set_sensitive(False)
            self.layout_actions.set_sensitive(False)
            self.remaining = 20
            self.editor.set_sensitive(False)
            self.tick_label()
            self.get_child().add_css_class("confirmation-pending")
            self.confirm.present()
            self.confirm.get_default_widget().grab_focus()
            self.timer = GLib.timeout_add_seconds(1, self.tick)
            self.message("Layout applied. Awaiting confirmation.")
        self.run(change, applied)

    def tick_label(self):
        unit = "second" if self.remaining == 1 else "seconds"
        text = f"Previous settings restore in {self.remaining} {unit}"
        self.confirm_label.set_text(text)
        self.confirm_progress.set_fraction(max(0, min(1, self.remaining / 20)))
        if self.remaining in (10, 5) and hasattr(self.confirm, "announce"):
            self.confirm.announce(text, Gtk.AccessibleAnnouncementPriority.MEDIUM)

    def tick(self):
        self.remaining -= 1
        self.tick_label()
        if self.remaining <= 0:
            self.timer = None
            self.revert()
            return GLib.SOURCE_REMOVE
        return GLib.SOURCE_CONTINUE

    def cancel_timer(self):
        if self.timer:
            GLib.source_remove(self.timer)
            self.timer = None

    def keep(self):
        self.cancel_timer()
        self.previous = None
        self.baseline = copy.deepcopy(self.layout)
        self.update_dirty()
        self.confirm.set_visible(False)
        self.get_child().remove_css_class("confirmation-pending")
        self.editor.set_sensitive(True)
        self.refresh()

    def revert(self):
        if self.busy or self.previous is None:
            return
        self.cancel_timer()
        previous = self.previous
        self.message("Restoring previous display layout…")

        def restored(_):
            self.previous = None
            self.confirm.set_visible(False)
            self.get_child().remove_css_class("confirmation-pending")
            if hasattr(self, "announce"):
                self.announce("Previous display settings restored.",
                              Gtk.AccessibleAnnouncementPriority.MEDIUM)
            self.editor.set_sensitive(True)
            if self.closing:
                if self.mirror_timer:
                    GLib.source_remove(self.mirror_timer)
                    self.mirror_timer = None
                self.destroy()
            else:
                self.layout = copy.deepcopy(previous)
                self.baseline = copy.deepcopy(previous)
                self.rebuild()
                self.refresh()
        self.run(lambda: self.client.apply(previous), restored)

    def selected(self, dropdown):
        item = dropdown.get_selected_item()
        if item is None:
            raise ValueError("Select an item first.")
        return item.get_string()

    def layout_changed(self):
        if self.baseline is None:
            return False
        baseline = {item["name"]: item for item in self.baseline}
        current = {item["name"]: item for item in self.layout}
        return baseline != current

    def update_dirty(self):
        changed = self.layout_changed()
        self.discard_button.set_sensitive(changed)
        self.apply_button.set_sensitive(changed)
        self.apply_button.set_tooltip_text("Unapplied changes" if changed else "All changes applied")
        self.layout_actions.set_sensitive(not self.busy and self.previous is None)
        if self.status.get_text() in ("All changes applied.", "Unapplied changes"):
            self.message("Unapplied changes" if changed else "All changes applied.")

    def chooser(self, title, action, callback, default_name=None):
        if self.file_chooser is not None:
            self.file_chooser.present()
            return self.file_chooser
        dialog = Gtk.FileChooserDialog(title=title, transient_for=self, modal=True, action=action)
        accept_label = "Save" if action == Gtk.FileChooserAction.SAVE else "Open"
        dialog.add_buttons("Cancel", Gtk.ResponseType.CANCEL, accept_label, Gtk.ResponseType.ACCEPT)
        dialog.set_default_response(Gtk.ResponseType.ACCEPT)
        file_filter = Gtk.FileFilter()
        file_filter.set_name("JSON files")
        file_filter.add_mime_type("application/json")
        file_filter.add_pattern("*.json")
        dialog.add_filter(file_filter)
        if default_name:
            dialog.set_current_name(default_name)

        def finished(chooser, response):
            self.file_chooser = None
            callback(chooser, response)

        self.file_chooser = dialog
        dialog.connect("response", finished)
        dialog.present()
        return dialog

    def export_config(self):
        if self.busy or self.previous is not None:
            return
        self.chooser("Export display configuration", Gtk.FileChooserAction.SAVE, self.export_file_chooser_finished, "niri-display.json")

    def import_config(self):
        if self.busy or self.previous is not None:
            return
        self.chooser("Import display configuration", Gtk.FileChooserAction.OPEN, self.import_file_chooser_finished)

    def export_file_chooser_finished(self, chooser, response):
        try:
            if response != Gtk.ResponseType.ACCEPT:
                return
            if self.busy or self.previous is not None:
                return
            file = chooser.get_file()
            path = file.get_path() if file else None
            if path is None:
                raise ValueError("Choose a local JSON file.")
            self.store.save(Path(path), copy.deepcopy(self.layout))
            self.message(f"Exported display configuration to {path}.")
        except Exception as error:
            self.message(error)
        finally:
            chooser.destroy()

    def import_file_chooser_finished(self, chooser, response):
        try:
            if response != Gtk.ResponseType.ACCEPT:
                return
            if self.busy or self.previous is not None:
                return
            file = chooser.get_file()
            path = file.get_path() if file else None
            if path is None:
                raise ValueError("Choose a local JSON file.")

            def import_action():
                loaded = self.store.load(Path(path))
                outputs = self.client.outputs()
                missing = [item["name"] for item in loaded if item["name"] not in outputs]
                if missing:
                    raise ValueError("Connect the configuration's missing monitors first: " + ", ".join(missing))
                actual = snapshot(outputs)
                by_name = {item["name"]: item for item in loaded}
                merged = [copy.deepcopy(by_name.get(item["name"], item)) for item in actual]
                if not any(item["enabled"] for item in merged):
                    raise ValueError("At least one display must remain enabled.")
                return outputs, merged

            def imported(result):
                self.outputs, self.layout = result
                self.rebuild()
                self.message("Imported configuration. Applying layout…")
                self.apply()

            self.message("Loading display configuration…")
            self.run(import_action, imported)
        except Exception as error:
            self.message(error)
        finally:
            chooser.destroy()


    def change_mirror_orientation(self, widget):
        self.mirror_source_first = widget.get_active()
        self.update_mirror_sources()

    def update_mirror_sources(self):
        previous = self.mirror_peer.get_selected_item()
        previous_name = previous.get_string() if previous else None
        names = [item["name"] for item in self.layout
                 if item["enabled"] and item["name"] != self.selected_monitor
                 and self.outputs.get(item["name"], {}).get("logical") is not None]
        self.mirror_peer.set_model(Gtk.StringList.new(names))
        if previous_name in names:
            self.mirror_peer.set_selected(names.index(previous_name))
        self.mirror_peer.set_sensitive(bool(names))
        self.update_mirror_actions()

    def update_mirror_actions(self, *_):
        running = self.mirror_pair is not None
        selected = next((item for item in self.layout if item["name"] == self.selected_monitor), None)
        active = bool(selected and selected["enabled"]
                      and self.outputs.get(self.selected_monitor, {}).get("logical") is not None)
        role = "Source" if self.mirror_source_first else "Destination"
        peer_role = "target" if self.mirror_source_first else "source"
        self.mirror_destination.set_text(
            f"{role}: {self.display_title(self.selected_monitor)} · {self.selected_monitor}"
            if selected else f"No {role.lower()} selected")
        self.mirror_choice_label.set_text(
            "Mirror to display" if self.mirror_source_first else "Mirror content from")
        self.mirror_peer.set_tooltip_text(
            f"Choose a {peer_role} display; selected display is the {role.lower()}")
        self.mirror_menu.set_label(
            ("Mirroring" if running and self.selected_monitor in self.mirror_pair else "Independent") + "   ▾")
        if running:
            source, target = self.mirror_pair
            self.mirror_status.set_text(f"Active · {source} → {target}. Stop this mirror before starting another.")
        elif not active:
            self.mirror_status.set_text(
                f"Enable this {role.lower()} and apply its display settings before mirroring.")
        elif self.mirror_peer.get_selected_item() is None:
            self.mirror_status.set_text(
                f"No other active display. Connect and enable a {peer_role} first.")
        else:
            self.mirror_status.set_text("Not active · Source content will appear on the target display.")
        self.start_mirror_button.set_sensitive(
            active and self.mirror_peer.get_selected_item() is not None and not running)
        self.stop_mirror_button.set_visible(running)
        self.start_mirror_button.set_visible(not running)

    def start_mirror(self):
        try:
            peer = self.selected(self.mirror_peer)
            source, target = ((self.selected_monitor, peer) if self.mirror_source_first
                              else (peer, self.selected_monitor))
            if self.selected_monitor is None:
                raise ValueError("Select a display first.")
        except Exception as error:
            self.message(error)
            return

        def start():
            outputs = self.client.outputs()
            for name in (source, target):
                if name not in outputs or outputs[name].get("logical") is None:
                    raise ValueError(f"{name} is not active. Apply the layout before mirroring.")
            self.mirrors.start(source, target)

        def started(_):
            self.mirror_pair = (source, target)
            self.update_mirror_actions()
            self.message(f"Mirroring {source} to {target}. Stop the mirror before disabling either display.")
        self.run(start, started)

    def stop_mirror(self):
        def stopped(_):
            self.mirror_pair = None
            self.update_mirror_actions()
            self.message("Mirror stopped.")
        self.run(self.mirrors.stop, stopped)

    def check_mirror(self):
        if self.busy:
            return GLib.SOURCE_CONTINUE
        error = self.mirrors.poll_error()
        if self.mirror_pair and not self.mirrors.running():
            self.mirror_pair = None
            self.update_mirror_actions()
            if not error:
                self.message("Mirror ended.")
        if error:
            self.message(error)
        return GLib.SOURCE_CONTINUE

    def close_requested(self, *_):
        if self.busy:
            self.message("Wait for the current display operation before closing.")
            return True
        if self.file_chooser is not None:
            self.file_chooser.destroy()
            self.file_chooser = None
        self.mirrors.stop()
        self.clear_identification()
        if self.previous is not None:
            self.closing = True
            self.revert()
            return True
        if self.mirror_timer:
            GLib.source_remove(self.mirror_timer)
            self.mirror_timer = None
        return False


class DisplayApplication(Gtk.Application):
    def __init__(self):
        super().__init__(application_id="io.github.niri_display", flags=Gio.ApplicationFlags.DEFAULT_FLAGS)

    def do_activate(self):
        window = self.get_active_window()
        if window is None:
            window = DisplayWindow(self)
        window.present()


if __name__ == "__main__":
    raise SystemExit(DisplayApplication().run())
