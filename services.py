"""Configuration persistence and wl-mirror process management."""

from __future__ import annotations

import base64
import copy
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unicodedata
from typing import Any, BinaryIO

from backend import snapshot


_MODE_PATTERN = re.compile(r"([0-9]+)x([0-9]+)@([0-9]+(?:\.[0-9]+)?)\Z")
_TRANSFORMS = {
    "normal", "90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270",
}

_REQUIRED_FIELDS = {
    "name": str,
    "enabled": bool,
    "mode": (str, type(None)),
    "scale": (int, float),
    "x": int,
    "y": int,
    "transform": str,
}


def _validate_layout(layout: Any, *, config: str) -> list[dict[str, Any]]:
    if not isinstance(layout, list) or not layout:
        raise ValueError(f"Config '{config}' must contain at least one output.")
    names: set[str] = set()
    enabled = False
    for index, output in enumerate(layout):
        location = f"Config '{config}', output {index}"
        if not isinstance(output, dict):
            raise ValueError(f"{location} must be an object.")
        for field, expected in _REQUIRED_FIELDS.items():
            if field not in output:
                raise ValueError(f"{location} is missing required field '{field}'.")
            value = output[field]
            if field in {"x", "y"}:
                valid = isinstance(value, int) and not isinstance(value, bool)
                if valid and not -100000 <= value <= 100000:
                    raise ValueError(f"{location} field '{field}' must be between -100000 and 100000.")
            elif field == "scale":
                valid = (
                    isinstance(value, (int, float))
                    and not isinstance(value, bool)
                    and (isinstance(value, int) or math.isfinite(value))
                )
            else:
                valid = isinstance(value, expected)
            if not valid:
                raise ValueError(f"{location} field '{field}' has invalid type or value.")

        output_name = output["name"]
        if not output_name.strip():
            raise ValueError(f"{location} field 'name' must not be empty.")
        if output_name in names:
            raise ValueError(f"{location} has duplicate output name '{output_name}'.")
        names.add(output_name)

        scale = output["scale"]
        if not 0.1 <= scale <= 10:
            raise ValueError(f"{location} field 'scale' must be between 0.1 and 10.")

        mode = output["mode"]
        if mode is not None:
            match = _MODE_PATTERN.fullmatch(mode)
            if match is None:
                raise ValueError(f"{location} field 'mode' must be positive WIDTHxHEIGHT@HZ or null.")
            width, height = (int(match.group(part)) for part in (1, 2))
            refresh = float(match.group(3))
            if (
                width <= 0
                or height <= 0
                or width > 100000
                or height > 100000
                or not math.isfinite(refresh)
                or not 0.001 <= refresh <= 1000
            ):
                raise ValueError(f"{location} field 'mode' must be within supported dimensions and refresh rate.")

        if output["transform"] not in _TRANSFORMS:
            raise ValueError(f"{location} field 'transform' must be one of: {', '.join(sorted(_TRANSFORMS))}.")
        enabled |= output["enabled"]
    if not enabled:
        raise ValueError(f"Config '{config}' must have at least one enabled output.")
    return layout


def _reject_json_constant(value: str) -> None:
    raise ValueError(f"Invalid non-standard JSON value '{value}'.")


class ConfigStore:
    """Read and write validated monitor configuration JSON at chosen paths."""

    def save(self, path: Path, layout: list[dict]) -> None:
        path = Path(path)
        _validate_layout(layout, config=str(path))
        try:
            payload = json.dumps(layout, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
        except (TypeError, ValueError) as exc:
            raise ValueError(f"Cannot encode config at '{path}' as JSON: {exc}") from exc

        temporary: str | None = None
        try:
            path.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w", encoding="utf-8", dir=path.parent,
                prefix=f".{path.name}.", suffix=".tmp", delete=False,
            ) as handle:
                temporary = handle.name
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, path)
            temporary = None
        except OSError as exc:
            raise RuntimeError(f"Cannot save config at '{path}': {exc}") from exc
        finally:
            if temporary is not None:
                try:
                    os.unlink(temporary)
                except OSError:
                    pass

    def load(self, path: Path) -> list[dict]:
        path = Path(path)
        try:
            with path.open("r", encoding="utf-8") as handle:
                layout = json.load(handle, parse_constant=_reject_json_constant)
        except FileNotFoundError as exc:
            raise RuntimeError(f"Config does not exist at '{path}'.") from exc
        except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as exc:
            raise RuntimeError(f"Cannot read config at '{path}': {exc}") from exc
        try:
            return _validate_layout(layout, config=str(path))
        except ValueError as exc:
            raise RuntimeError(f"Corrupt config at '{path}': {exc}") from exc


class ProfileStore:
    """Persist named monitor layouts under the user configuration directory."""

    def __init__(self, directory: Path | None = None):
        if directory is None:
            config_home = os.environ.get("XDG_CONFIG_HOME")
            directory = Path(config_home) if config_home else Path.home() / ".config"
            directory = directory / "niri-display" / "profiles"
        self.directory = Path(directory)
        self.config = ConfigStore()

    @staticmethod
    def _normalize_name(name: str) -> str:
        if not isinstance(name, str):
            raise ValueError("Profile name must be text.")
        name = name.strip()
        if not name:
            raise ValueError("Profile name must not be empty.")
        if any(unicodedata.category(character) == "Cc" for character in name):
            raise ValueError("Profile name must not contain control characters.")
        return name

    @classmethod
    def _filename(cls, name: str) -> str:
        encoded = base64.urlsafe_b64encode(cls._normalize_name(name).encode("utf-8")).decode("ascii")
        return f"{encoded}.json"

    def list_names(self) -> list[str]:
        names = []
        try:
            for path in self.directory.glob("*.json"):
                try:
                    raw = base64.urlsafe_b64decode(path.stem.encode("ascii"))
                    name = raw.decode("utf-8")
                    if self._filename(name) == path.name and path.is_file():
                        names.append(name)
                except (UnicodeError, ValueError):
                    continue
        except OSError:
            return []
        names.sort(key=str.casefold)
        try:
            with (self.directory / ".order.json").open(encoding="utf-8") as handle:
                order = json.load(handle)
        except FileNotFoundError:
            return names
        except (OSError, UnicodeError, ValueError) as error:
            raise RuntimeError(f"Cannot read profile order: {error}") from error
        if not isinstance(order, list) or any(not isinstance(name, str) for name in order):
            raise RuntimeError("Profile order must be a list of names.")
        available = set(names)
        ordered = list(dict.fromkeys(name for name in order if name in available))
        included = set(ordered)
        return ordered + [name for name in names if name not in included]

    def save_order(self, names: list[str]) -> None:
        """Persist the displayed order without changing any profile layouts."""
        if len(names) != len(set(names)) or set(names) != set(self.list_names()):
            raise ValueError("Profiles changed. Reopen the picker before reordering.")
        self.directory.mkdir(parents=True, exist_ok=True)
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w", encoding="utf-8", dir=self.directory,
                prefix=".order.", suffix=".tmp", delete=False,
            ) as handle:
                temporary = handle.name
                json.dump(names, handle, ensure_ascii=False)
                handle.write("\n")
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, self.directory / ".order.json")
            temporary = None
        finally:
            if temporary is not None:
                os.unlink(temporary)

    def save(self, name: str, layout: list[dict]) -> None:
        filename = self._filename(name)
        path = self.directory / filename
        _validate_layout(layout, config=f"profile '{name}'")
        self.directory.mkdir(parents=True, exist_ok=True)
        if path.exists():
            raise FileExistsError(f"Profile '{name}' already exists.")

        fd, temporary = tempfile.mkstemp(prefix=".profile.", suffix=".tmp", dir=self.directory)
        os.close(fd)
        try:
            self.config.save(Path(temporary), layout)
            try:
                os.link(temporary, path)
            except FileExistsError as exc:
                raise FileExistsError(f"Profile '{name}' already exists.") from exc
        finally:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass

    def load(self, name: str) -> list[dict]:
        name = self._normalize_name(name)
        path = self.directory / self._filename(name)
        try:
            return self.config.load(path)
        except RuntimeError as exc:
            if not path.exists():
                raise RuntimeError(f"Profile '{name}' does not exist.") from exc
            raise

    def delete(self, name: str) -> None:
        name = self._normalize_name(name)
        path = self.directory / self._filename(name)
        try:
            path.unlink()
        except FileNotFoundError as exc:
            raise RuntimeError(f"Profile '{name}' does not exist.") from exc


def merge_profile(layout: list[dict], outputs: dict[str, Any]) -> list[dict]:
    """Merge saved profile rows into current outputs without dropping connected displays."""
    _validate_layout(layout, config="profile")
    saved_names = {output["name"] for output in layout}
    missing = sorted(saved_names - outputs.keys())
    if missing:
        raise ValueError("Profile has missing monitors: " + ", ".join(missing))

    current = snapshot(outputs)
    by_name = {output["name"]: output for output in layout}
    merged = [copy.deepcopy(by_name.get(output["name"], output)) for output in current]
    return _validate_layout(merged, config="merged profile")



class MirrorManager:
    """Own at most one wl-mirror process for this application."""

    def __init__(self) -> None:
        self._process: subprocess.Popen[bytes] | None = None
        self._stderr_file: BinaryIO | None = None
        self._pending_error: str | None = None

    def start(self, source: str, target: str) -> None:
        if not isinstance(source, str) or not source.strip():
            raise ValueError("Mirror source output must not be empty.")
        if not isinstance(target, str) or not target.strip():
            raise ValueError("Mirror target output must not be empty.")
        if source == target:
            raise ValueError("Mirror source and target outputs must be different.")

        if self._process is not None and self._process.poll() is None:
            raise RuntimeError("A mirror process is already running.")
        self._collect_exit()
        try:
            stderr_file = tempfile.TemporaryFile(mode="w+b")
        except OSError as exc:
            raise RuntimeError(f"Cannot capture wl-mirror errors: {exc}") from exc
        try:
            command = ["wl-mirror", "--fullscreen-output", target, source]
            binary = shutil.which("wl-mirror")
            nix_gl = shutil.which("nixGL")
            if (
                binary
                and nix_gl
                and Path(binary).resolve().is_relative_to(Path("/nix/store"))
            ):
                command.insert(0, nix_gl)
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=stderr_file,
                close_fds=True,
            )
        except FileNotFoundError as exc:
            stderr_file.close()
            raise RuntimeError("Cannot start mirroring: 'wl-mirror' was not found. Install wl-mirror and ensure it is on PATH.") from exc
        except OSError as exc:
            stderr_file.close()
            raise RuntimeError(f"Cannot start wl-mirror: {exc}") from exc
        except Exception as exc:
            stderr_file.close()
            raise RuntimeError(f"Cannot start wl-mirror: {exc}") from exc
        self._process = process
        self._stderr_file = stderr_file

    def stop(self) -> None:
        process = self._process
        if process is None:
            return
        if process.poll() is not None:
            self._collect_exit()
            return
        try:
            try:
                process.terminate()
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        finally:
            self._process = None
            self._close_stderr()

    def running(self) -> bool:
        self._collect_exit()
        return self._process is not None

    def poll_error(self) -> str | None:
        if self._pending_error is not None:
            error, self._pending_error = self._pending_error, None
            return error
        self._collect_exit()
        error, self._pending_error = self._pending_error, None
        return error

    def _collect_exit(self) -> None:
        process = self._process
        if process is None or process.poll() is None:
            return
        returncode = process.returncode
        detail = self._read_stderr()
        self._process = None
        self._close_stderr()
        if returncode:
            message = f"wl-mirror exited with status {returncode}. Check wl-mirror and display configuration."
            if detail:
                message += f"\n{detail}"
            self._pending_error = message

    def _read_stderr(self) -> str:
        if self._stderr_file is None:
            return ""
        try:
            self._stderr_file.seek(0, os.SEEK_END)
            end = self._stderr_file.tell()
            self._stderr_file.seek(max(0, end - 4096))
            data = self._stderr_file.read(4096)
        except OSError:
            return ""
        text = data.decode("utf-8", errors="replace").strip()
        if end > 4096:
            return "...(earlier stderr omitted)...\n" + text
        return text

    def _close_stderr(self) -> None:
        if self._stderr_file is not None:
            self._stderr_file.close()
            self._stderr_file = None
