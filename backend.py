"""Small synchronous client for Niri output configuration."""

from __future__ import annotations

import json
import math
import re
import subprocess
from typing import Any


_MODE_RE = re.compile(r"^(\d+)x(\d+)@([0-9]+(?:\.[0-9]+)?)$")
_TRANSFORMS = {
    "normal": "normal",
    "90": "90",
    "180": "180",
    "270": "270",
    "flipped": "flipped",
    "flipped-90": "flipped-90",
    "flipped-180": "flipped-180",
    "flipped-270": "flipped-270",
}


def _mode_string(mode: dict[str, Any]) -> str:
    try:
        width = mode["width"]
        height = mode["height"]
        refresh_rate = mode["refresh_rate"]
        if isinstance(width, bool) or isinstance(height, bool) or isinstance(refresh_rate, bool):
            raise TypeError
        return f"{int(width)}x{int(height)}@{int(refresh_rate) / 1000:g}"
    except (KeyError, TypeError, ValueError, OverflowError) as exc:
        raise ValueError(f"Invalid mode reported by Niri: {mode!r}") from exc


def _current_mode(output: dict[str, Any]) -> str | None:
    modes = output.get("modes") or []
    current = output.get("current_mode")
    if current is None:
        return None
    if isinstance(current, int) and not isinstance(current, bool):
        if 0 <= current < len(modes):
            return _mode_string(modes[current])
        raise ValueError(f"Niri reported invalid current_mode index {current} for {output.get('name')!r}")
    if isinstance(current, dict):
        return _mode_string(current)
    raise ValueError(f"Niri reported invalid current_mode for {output.get('name')!r}")


def snapshot(outputs: dict[str, Any]) -> list[dict[str, Any]]:
    """Convert Niri's outputs object into editable layout rows."""
    if not isinstance(outputs, dict):
        raise ValueError("Niri outputs must be a JSON object")
    layout = []
    for name, output in outputs.items():
        if not isinstance(name, str) or not isinstance(output, dict):
            raise ValueError("Niri outputs contains an invalid output entry")
        logical = output.get("logical")
        enabled = logical is not None and output.get("current_mode") is not None
        mode = _current_mode(output)
        if enabled:
            x, y = logical.get("x"), logical.get("y")
            scale = logical.get("scale")
            transform = logical.get("transform", "Normal")
            if not isinstance(x, int) or isinstance(x, bool) or not isinstance(y, int) or isinstance(y, bool):
                raise ValueError(f"Niri reported invalid logical position for {name!r}")
            if not isinstance(scale, (int, float)) or isinstance(scale, bool) or not math.isfinite(scale):
                raise ValueError(f"Niri reported invalid scale for {name!r}")
            if not isinstance(transform, str):
                raise ValueError(f"Niri reported invalid transform for {name!r}")
            transform = transform.lower().replace("flipped90", "flipped-90").replace("flipped180", "flipped-180").replace("flipped270", "flipped-270")
        else:
            x, y, scale = 0, 0, 1.0
            # Keep a disabled output's last-known transform if Niri supplies it.
            transform = "normal"
            if isinstance(logical, dict) and isinstance(logical.get("transform"), str):
                transform = logical["transform"].lower().replace("flipped90", "flipped-90").replace("flipped180", "flipped-180").replace("flipped270", "flipped-270")
        layout.append({
            "name": name,
            "enabled": enabled,
            "mode": mode,
            "scale": float(scale),
            "x": x,
            "y": y,
            "transform": transform,
        })
    return layout


class NiriClient:
    def __init__(self, timeout: float = 10.0):
        self.timeout = timeout

    def _run(self, args: list[str]) -> str:
        try:
            result = subprocess.run(
                ["niri", "msg", *args],
                capture_output=True,
                text=True,
                timeout=self.timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise RuntimeError(f"Niri command timed out: {' '.join(args)}") from exc
        except OSError as exc:
            raise RuntimeError(f"Could not run Niri command: {exc}") from exc
        if result.returncode != 0:
            detail = result.stderr.strip() or result.stdout.strip() or f"exit status {result.returncode}"
            raise RuntimeError(f"Niri command failed ({' '.join(args)}): {detail}")
        return result.stdout

    def outputs(self) -> dict[str, Any]:
        """Return raw output data from `niri msg --json outputs`."""
        try:
            data = json.loads(self._run(["--json", "outputs"]))
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"Niri returned invalid outputs JSON: {exc}") from exc
        if not isinstance(data, dict):
            raise RuntimeError("Niri outputs response was not a JSON object")
        return data

    def apply(self, layout: list[dict[str, Any]]) -> None:
        """Validate complete layout, then apply enabled outputs before disabling any."""
        if not isinstance(layout, list) or not layout:
            raise ValueError("Layout must be a non-empty list")

        connected = self.outputs()
        prepared: list[tuple[dict[str, Any], str | None, str]] = []
        seen: set[str] = set()
        enabled_count = 0
        required = {name for name in connected if isinstance(name, str)}
        for row in layout:
            if not isinstance(row, dict):
                raise ValueError("Each layout entry must be an object")
            name = row.get("name")
            if not isinstance(name, str) or not name:
                raise ValueError("Each layout entry needs a non-empty output name")
            if name in seen:
                raise ValueError(f"Duplicate output in layout: {name}")
            seen.add(name)
            if name not in connected:
                raise ValueError(f"Output is not connected: {name}")
            enabled = row.get("enabled")
            if not isinstance(enabled, bool):
                raise ValueError(f"Output {name!r} needs a boolean enabled value")
            if not enabled:
                prepared.append((row, None, name))
                continue
            enabled_count += 1
            mode = row.get("mode")
            if mode is not None:
                if not isinstance(mode, str) or not _MODE_RE.fullmatch(mode):
                    raise ValueError(f"Invalid mode for {name!r}: {mode!r}; expected WIDTHxHEIGHT@HZ")
                width, height, refresh = _MODE_RE.fullmatch(mode).groups()
                if int(width) <= 0 or int(height) <= 0 or not math.isfinite(float(refresh)) or float(refresh) <= 0:
                    raise ValueError(f"Invalid mode for {name!r}: dimensions and refresh rate must be positive and finite")
            scale = row.get("scale")
            if isinstance(scale, bool) or not isinstance(scale, (int, float)) or not math.isfinite(scale) or not 0.1 <= scale <= 10:
                raise ValueError(f"Scale for {name!r} must be finite and between 0.1 and 10")
            x, y = row.get("x"), row.get("y")
            if not isinstance(x, int) or isinstance(x, bool) or not isinstance(y, int) or isinstance(y, bool):
                raise ValueError(f"Position for {name!r} must use integer coordinates")
            transform = row.get("transform")
            if not isinstance(transform, str) or transform.lower() not in _TRANSFORMS:
                raise ValueError(f"Invalid transform for {name!r}: {transform!r}")
            prepared.append((row, mode, name))

        missing = required - seen
        if missing:
            raise ValueError(f"Layout must include every connected output: {', '.join(sorted(missing))}")
        if enabled_count == 0:
            raise ValueError("At least one output must remain enabled")

        for row, mode, name in prepared:
            if not row["enabled"]:
                continue
            self._run(["output", name, "on"])
            if mode is not None:
                available = {_mode_string(m) for m in (connected[name].get("modes") or [])}
                custom = mode not in available or (
                    connected[name].get("is_custom_mode") is True and _current_mode(connected[name]) == mode
                )
                self._run(["output", name, "custom-mode" if custom else "mode", mode])
            self._run(["output", name, "scale", str(row["scale"])])
            self._run(["output", name, "transform", row["transform"].lower()])
            self._run(["output", name, "position", "set", "--", str(row["x"]), str(row["y"])])
        for row, _mode, name in prepared:
            if not row["enabled"]:
                self._run(["output", name, "off"])
