"""Behavior checks for layout safety and configuration boundaries."""
import copy
import json
from pathlib import Path
import tempfile
import unittest

from backend import NiriClient, snapshot
from services import ConfigStore


def output(name, active=True, transform="Normal"):
    return {
        "name": name,
        "modes": [{"width": 1920, "height": 1080, "refresh_rate": 60000}],
        "current_mode": 0 if active else None,
        "logical": {"x": 0, "y": 0, "scale": 1.0, "transform": transform} if active else None,
    }


class RecordingClient(NiriClient):
    """Expose compositor commands without changing the desktop."""
    def __init__(self, outputs):
        super().__init__()
        self.connected = outputs
        self.commands = []

    def outputs(self):
        return copy.deepcopy(self.connected)

    def _run(self, arguments):
        self.commands.append(arguments)
        return ""


class LayoutSafetyTests(unittest.TestCase):
    def test_disabled_output_can_be_enabled_from_snapshot(self):
        client = RecordingClient({"DP-1": output("DP-1"), "DP-2": output("DP-2", False)})
        layout = snapshot(client.outputs())
        self.assertFalse(layout[1]["enabled"])
        self.assertIsNone(layout[1]["mode"])
        layout[0]["enabled"] = False
        layout[1]["enabled"] = True
        client.apply(layout)
        self.assertLess(client.commands.index(["output", "DP-2", "on"]), client.commands.index(["output", "DP-1", "off"]))

    def test_all_disabled_layout_never_changes_compositor(self):
        client = RecordingClient({"DP-1": output("DP-1")})
        layout = snapshot(client.outputs())
        layout[0]["enabled"] = False
        with self.assertRaisesRegex(ValueError, "At least one"):
            client.apply(layout)
        self.assertEqual(client.commands, [])

    def test_invalid_second_monitor_prevents_partial_apply(self):
        client = RecordingClient({"DP-1": output("DP-1"), "DP-2": output("DP-2")})
        layout = snapshot(client.outputs())
        layout[1]["mode"] = "0x600@60"
        with self.assertRaisesRegex(ValueError, "positive"):
            client.apply(layout)
        self.assertEqual(client.commands, [])

    def test_flipped_rotation_round_trips_to_cli_transform(self):
        client = RecordingClient({"DP-1": output("DP-1", transform="Flipped90")})
        client.apply(snapshot(client.outputs()))
        self.assertIn(["output", "DP-1", "transform", "flipped-90"], client.commands)


class CanvasGeometryTests(unittest.TestCase):
    def test_fractional_output_edges_include_partial_logical_pixels(self):
        from app import DisplayWindow

        for transform, expected in (
            ("normal", (1552, 873)),
            ("90", (873, 1552)),
            ("flipped-270", (873, 1552)),
        ):
            with self.subTest(transform=transform):
                row = {"mode": "2560x1440@59.998", "scale": 1.65, "transform": transform}
                self.assertEqual(DisplayWindow.dimensions(None, row), expected)


class ConfigBoundaryTests(unittest.TestCase):
    def test_saved_layout_keeps_disabled_monitors_and_negative_positions(self):
        layout = snapshot({"DP-1": output("DP-1"), "DP-2": output("DP-2", False)})
        layout[0].update(x=-1920, y=-100, scale=1.25)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "chosen" / "display.json"
            store = ConfigStore()
            store.save(path, layout)
            self.assertEqual(store.load(path), layout)

    def test_corrupt_json_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "broken.json"
            path.write_text("{")
            with self.assertRaisesRegex(RuntimeError, "Cannot read config"):
                ConfigStore().load(path)

    def test_duplicate_outputs_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "duplicate.json"
            layout = snapshot({"DP-1": output("DP-1")})
            path.write_text(json.dumps([layout[0], layout[0]]))
            with self.assertRaisesRegex(RuntimeError, "duplicate output"):
                ConfigStore().load(path)

    def test_all_disabled_config_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "disabled.json"
            layout = snapshot({"DP-1": output("DP-1")})
            layout[0]["enabled"] = False
            path.write_text(json.dumps(layout))
            with self.assertRaisesRegex(RuntimeError, "enabled output"):
                ConfigStore().load(path)



    def test_imported_config_rejects_position_and_mode_bounds(self):
        cases = (
            ("x", 100001),
            ("x", -100001),
            ("y", 100001),
            ("y", -(10 ** 309)),
        )
        for field, value in cases:
            with self.subTest(field=field, value=value), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "out-of-bounds.json"
                layout = snapshot({"DP-1": output("DP-1")})
                layout[0][field] = value
                path.write_text(json.dumps(layout))
                with self.assertRaisesRegex(RuntimeError, "Corrupt config"):
                    ConfigStore().load(path)

        for mode in ("100001x1080@60", "1920x100001@60", "1920x1080@1000.001", "1920x1080@0.0009"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "out-of-bounds-mode.json"
                layout = snapshot({"DP-1": output("DP-1")})
                layout[0]["mode"] = mode
                path.write_text(json.dumps(layout))
                with self.assertRaisesRegex(RuntimeError, "Corrupt config"):
                    ConfigStore().load(path)

    def test_imported_config_rejects_huge_mode_dimensions(self):
        for width, height in ((10 ** 309, 1080), (1920, 10 ** 309)):
            with self.subTest(width=width, height=height), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "huge-mode.json"
                layout = snapshot({"DP-1": output("DP-1")})
                layout[0]["mode"] = f"{width}x{height}@60"
                path.write_text(json.dumps(layout))
                with self.assertRaisesRegex(RuntimeError, "Corrupt config"):
                    ConfigStore().load(path)

    def test_imported_config_accepts_position_and_mode_boundaries(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "boundary.json"
            layout = snapshot({"DP-1": output("DP-1")})
            layout[0].update(x=-100000, y=100000, mode="100000x100000@1000")
            path.write_text(json.dumps(layout))
            self.assertEqual(ConfigStore().load(path), layout)
    def test_malformed_refresh_modes_are_rejected_before_apply(self):
        for mode in ("1920x1080@.5", "1920x1080@60.", "1920x1080@", "1920x1080@abc"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "malformed.json"
                layout = snapshot({"DP-1": output("DP-1")})
                layout[0]["mode"] = mode
                path.write_text(json.dumps(layout))

                with self.assertRaisesRegex(RuntimeError, "Corrupt config"):
                    ConfigStore().load(path)
    def test_nonfinite_scale_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "nonfinite.json"
            layout = snapshot({"DP-1": output("DP-1")})
            path.write_text(json.dumps(layout).replace('"scale": 1.0', '"scale": NaN'))
            with self.assertRaisesRegex(RuntimeError, "non-standard JSON"):
                ConfigStore().load(path)

    def test_save_validation_preserves_existing_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "existing.json"
            path.write_text("original")
            layout = snapshot({"DP-1": output("DP-1")})
            layout[0]["scale"] = 0
            with self.assertRaisesRegex(ValueError, "scale"):
                ConfigStore().save(path, layout)
            self.assertEqual(path.read_text(), "original")


if __name__ == "__main__":
    unittest.main()
