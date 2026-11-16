"""Entry point for `python3 -I scripts/journeys ...` (a directory) and `python3 -m journeys` from scripts/."""
import os
import sys

if __package__ in (None, ""):
    # Executed as a directory: make the parent importable so `journeys` is a package. Nothing else is added.
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from journeys.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
