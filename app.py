#!/usr/bin/env python3
"""Launch the Niri display editor or compact profile picker."""
import argparse


def main(argv=None):
    parser = argparse.ArgumentParser(description="Configure Niri displays and saved profiles.")
    parser.add_argument(
        "--profiles", action="store_true", help="Open the minimal saved-profile switcher")
    args = parser.parse_args(argv)
    if args.profiles:
        from switcher import ProfileSwitcherApplication
        application = ProfileSwitcherApplication()
    else:
        from editor import DisplayApplication
        application = DisplayApplication()
    return application.run([])


if __name__ == "__main__":
    raise SystemExit(main())
