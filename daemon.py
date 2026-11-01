"""Opt-in systemd user service for restoring confirmed monitor layouts."""
from __future__ import annotations

import argparse
import hashlib
import json
import logging
import os
from pathlib import Path
import subprocess
import sys
import time

from backend import NiriClient, snapshot
from services import ConfigStore


SERVICE = "niri-display-daemon.service"


def config_home() -> Path:
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))


class LayoutMemory:
    """Keep one confirmed layout for each exact set of output connector names."""

    def __init__(self, directory: Path | None = None):
        self.directory = directory if directory is not None else config_home() / "niri-display" / "layouts"
        self.store = ConfigStore()

    def path(self, names) -> Path:
        key = json.dumps(sorted(names), ensure_ascii=True).encode("utf-8")
        return self.directory / (hashlib.sha256(key).hexdigest() + ".json")

    def remember(self, layout: list[dict]) -> None:
        self.store.save(self.path(row["name"] for row in layout), layout)

    def recall(self, names) -> list[dict] | None:
        names = set(names)
        path = self.path(names)
        if not path.exists():
            return None
        layout = self.store.load(path)
        if {row["name"] for row in layout} != names:
            raise RuntimeError(f"Saved layout does not match connected outputs: {path}")
        return layout


class RestoreDaemon:
    """Restore only on startup and connector changes, never on ordinary edits."""

    def __init__(self, client=None, memory=None):
        self.client = client if client is not None else NiriClient()
        self.memory = memory if memory is not None else LayoutMemory()
        self.connected: frozenset[str] | None = None

    def poll(self) -> None:
        outputs = self.client.outputs()
        names = frozenset(outputs)
        if names == self.connected:
            return
        self.connected = names
        desired = self.memory.recall(names)
        previous = snapshot(outputs)
        if desired is None:
            if (
                len(previous) == 1
                and previous[0]["name"].startswith(("eDP-", "LVDS-", "DSI-"))
                and not previous[0]["enabled"]
            ):
                # No saved single-panel layout exists: recover a usable screen.
                previous[0]["enabled"] = True
                self.client.apply(previous)
                logging.info("Enabled sole built-in display %s", previous[0]["name"])
            return
        if sorted(previous, key=lambda row: row["name"]) == sorted(desired, key=lambda row: row["name"]):
            return
        try:
            self.client.apply(desired)
        except Exception as error:
            try:
                self.client.apply(previous)
            except Exception as rollback_error:
                raise RuntimeError(f"Restore failed: {error}. Rollback failed: {rollback_error}") from error
            raise RuntimeError(f"Restore failed; previous layout restored: {error}") from error
        logging.info("Restored layout for %s", ", ".join(sorted(names)))


def systemctl(*arguments: str) -> subprocess.CompletedProcess:
    try:
        result = subprocess.run(
            ["systemctl", "--user", *arguments], capture_output=True, text=True, timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError(f"Cannot control user daemon: {error}") from error
    return result


def enabled() -> bool:
    return systemctl("is-enabled", SERVICE).stdout.strip() == "enabled"


def _checked(*arguments: str) -> None:
    result = systemctl(*arguments)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip() or "User daemon command failed.")


def _unit_argument(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%") + '"'


def set_enabled(active: bool) -> None:
    if not active:
        _checked("disable", "--now", SERVICE)
        return
    unit = config_home() / "systemd" / "user" / SERVICE
    unit.parent.mkdir(parents=True, exist_ok=True)
    command = " ".join(_unit_argument(value) for value in (sys.executable, str(Path(__file__).resolve())))
    unit.write_text(
        "[Unit]\nDescription=Restore saved Niri display layouts\n"
        "PartOf=graphical-session.target\nAfter=graphical-session.target\n\n"
        "[Service]\nType=simple\nExecStart=" + command + "\n\n"
        "[Install]\nWantedBy=graphical-session.target\n",
        encoding="utf-8",
    )
    _checked("daemon-reload")
    # Import the live compositor address before starting the user service.
    variables = [name for name in ("NIRI_SOCKET", "WAYLAND_DISPLAY", "DISPLAY") if name in os.environ]
    if variables:
        _checked("import-environment", *variables)
    _checked("enable", "--now", SERVICE)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    options = parser.add_mutually_exclusive_group()
    options.add_argument("--enable", action="store_true", help="enable and start the user service")
    options.add_argument("--disable", action="store_true", help="disable and stop the user service")
    options.add_argument("--status", action="store_true", help="show whether the user service is enabled")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")
    try:
        if args.enable or args.disable:
            set_enabled(args.enable)
            return 0
        if args.status:
            print("enabled" if enabled() else "disabled")
            return 0
        daemon = RestoreDaemon()
        while True:
            try:
                daemon.poll()
            except (RuntimeError, ValueError) as error:
                logging.error("%s", error)
            time.sleep(2)
    except (RuntimeError, OSError) as error:
        logging.error("%s", error)
        return 1
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
