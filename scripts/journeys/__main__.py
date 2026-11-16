"""Entry point for `python3 -I scripts/journeys ...` (a directory) and `python3 -m journeys` from scripts/.

Plain syntax only up to the version check, so an old Python gets a clear message instead of a SyntaxError."""
import os
import sys

if sys.version_info < (3, 12):
    sys.stderr.write("scripts/journeys needs Python 3.12 or newer.\n")
    sys.exit(2)

if __package__ in (None, ""):
    # Executed as a directory: make the parent importable so `journeys` is a package. Appended, so it never
    # shadows the standard library.
    sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from journeys.cli import main  # noqa: E402

if __name__ == "__main__":
    sys.exit(main())
