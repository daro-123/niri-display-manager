# Niri Displays

A small GTK 4 application for managing displays in a running Niri session.

## Run

Requires Python 3.10+, PyGObject, GTK 4, Pycairo with GI Cairo integration, and `niri` on `PATH`. Install `wl-mirror` for mirroring. Run from a terminal inside your Niri session:

```sh
python3 app.py
```

On Fedora, the Python/GTK dependencies are available as:

```sh
sudo dnf install python3-gobject gtk4 python3-cairo
```

Niri and wl-mirror can come from your existing system packages or Nix profile. Use the system Python that provides `gi`; a plain virtual environment will not normally include system PyGObject.

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

## Checks

Run the regression suite and syntax checks:

```sh
python3 -m unittest discover -s tests -v
python3 -m py_compile app.py backend.py services.py
```

The 15 unit tests cover layout safety, configuration boundaries, and fractional-scale canvas geometry, including rotated outputs. The geometry check requires the application's GTK dependencies but does not open a window.
