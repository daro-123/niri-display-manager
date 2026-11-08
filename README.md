# Niri Displays

A small GTK 4 application for managing displays in a running Niri session.


![Screenshot](Screenshot.png)

> [!WARNING]
> This extension was written with the help of a coding agent.

## Run

Requires Python 3.10+, PyGObject, GTK 4, Pycairo with GI Cairo integration, and `niri` on `PATH`. Install `wl-mirror` for mirroring. Run from a terminal inside your Niri session:

```sh
python3 app.py
```

On Fedora, the Python/GTK dependencies are available as:

```sh
sudo dnf install python3-gobject gtk4 python3-cairo
```

Niri and wl-mirror can come from your existing system packages or Nix profile. On non-NixOS systems, install `nixGL` if `wl-mirror` comes from Nix; the app automatically launches that Nix-store binary through `nixGL` when available. Use the system Python that provides `gi`; a plain virtual environment will not normally include system PyGObject.

## Named profiles

Open **Profiles** in the editor. Enter a new name and choose **Save current edits** to save the current monitor settings, including unapplied edits. Save different names for different layouts. Existing names cannot be overwritten; delete a profile before reusing its name.

Select a saved profile and choose **Apply profile** to switch layouts. Choose **Keep changes** within 20 seconds or the previous layout is restored. Use **Delete** and confirm to remove the selected profile without changing the displays.

Launch the compact profile picker from a terminal or key binding:

```sh
python3 app.py --profiles
# After installation:
niri-display --profiles
```

Press **1–9** to apply the corresponding numbered profile immediately and close the picker. Number-pad digits also work. Alternatively, use the arrow keys and Enter, or click a profile. This mode does not ask for confirmation or automatically revert successful changes; the full editor still does. Escape closes the picker without switching. If a switch is already running, it finishes before the picker closes. Failed applies attempt to restore the previous layout.

Drag a profile's **⠿** handle onto the upper or lower half of another row to place it before or after that profile. The order is saved across launches and also used in the editor's profile list. Shortcuts always follow the current order; profiles after the ninth remain accessible by clicking or using the arrow keys. Newly created profiles appear after the saved order.

The picker fits its height to the list, scrolling when the list exceeds 420 pixels.

Press **E** in the picker to open the full display editor and close the picker.

Profiles live under `$XDG_CONFIG_HOME/niri-display/profiles` (default `~/.config/niri-display/profiles`), separately from automatic daemon layouts. They store connector names, enabled state, mode, scale, position, and rotation, not mirroring. All monitors saved in a profile must be connected before applying it. Additional connected monitors keep their current settings.


## Optional background daemon

The daemon is **off by default**. Open **Settings → Restore layouts automatically** to enable it. Disable the same option to stop the daemon and turn off its login startup.

Each time you apply a layout and choose **Keep**, or successfully switch with `--profiles`, the app saves that layout under `$XDG_CONFIG_HOME/niri-display/layouts` (default `~/.config/niri-display/layouts`). Unconfirmed editor changes and reverted changes are not saved. Enabling the daemon also remembers the current layout if no layout exists for those connected outputs. To reuse an older exported configuration, import it and confirm it first.

The daemon restores the last confirmed layout for an exact set of output connector names at startup and when that set changes. It checks connections every two seconds. Unknown monitor combinations stay unchanged. It does not continually enforce settings, so it does not undo ordinary edits. Different monitors using the same connectors share a saved layout. Mirroring is not restored.

If disconnecting external monitors leaves only a disabled built-in display, the daemon turns it on even without a saved layout. Built-in displays are identified by `eDP-`, `LVDS-`, or `DSI-` connector names. This fallback does not enable a lone external display or change an already enabled built-in display.

Enabling creates and starts `niri-display-daemon.service` in your systemd user session. The service starts with the user manager's `default.target`, so login startup does not require `niri --session` or `graphical-session.target`. It keeps running after the app closes and waits for Niri to become available. If user lingering is enabled, it may also run outside a graphical session. Re-run the install task after updating an installed copy, then enable the option again (or run `python3 daemon.py --enable`) to migrate an existing service's startup links.

The daemon can also be controlled from the source directory:

```sh
python3 daemon.py --enable
python3 daemon.py --status
python3 daemon.py --disable
```

Run `python3 daemon.py` in a terminal for foreground operation without enabling a service. View service errors with `journalctl --user -u niri-display-daemon.service`. A failed restore attempts to roll back the previous layout; it does not retry that monitor combination until connections change.


## Mise tasks

Run tests and syntax checks:

```sh
mise run test
```

Install the application for your user, without root access:

```sh
mise run install
niri-display
```

Installation copies the Python files into `$XDG_DATA_HOME/niri-display` (default `~/.local/share/niri-display`) and creates `~/.local/bin/niri-display`. Add `~/.local/bin` to `PATH` if needed. Re-run the install task after changing the source. The task refuses to overwrite an unrelated launcher.

The install task does not install system dependencies. Install the dependencies listed above before launching the application.

## Desktop diagnostics

If GTK reports `Unable to connect to the accessibility bus` with `Connection refused`, restart the user accessibility service:

```sh
systemctl --user restart at-spi-dbus-bus.service
```

GTK `Theme parser error` messages identify invalid CSS in the desktop theme, not the display layout. Check the indicated line in `~/.config/gtk-4.0/gtk.css` or the active theme. A selector ending with `>` immediately before a comma is incomplete; remove that trailing combinator or supply its missing selector. Theme changes affect other GTK applications too.

`MESA-INTEL` diagnostics come from the graphics driver. They do not by themselves mean a profile failed to apply; application errors appear in the picker or editor.

## Checks

Run the regression suite and syntax checks:

```sh
python3 -m unittest discover -s tests -v
python3 -m py_compile app.py backend.py services.py daemon.py switcher.py
```

The 15 unit tests cover layout safety, configuration boundaries, and fractional-scale canvas geometry, including rotated outputs. The geometry check requires the application's GTK dependencies but does not open a window.
