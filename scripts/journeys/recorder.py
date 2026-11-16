"""The check recorder: the only thing that reaches the summary artifact or stdout.

Structural redaction (#592): a check is a name, an expected and an actual HTTP status
and a note from a fixed vocabulary. The recorder never receives a response, a header, a
cookie, a key or a URL, so none of them can be written. Nothing is scrubbed afterwards.
"""
import re

# Closed alphabet: no underscore (inference keys are omg_...), no '@', '?', '/', '=' or ':'.
NAME = re.compile(r"[a-z][a-z0-9.\- ]{0,70}")
LONG_HEX = re.compile(r"[0-9a-f]{12,}")
# Fixed vocabulary for the optional note. Anything else is refused.
NOTES = frozenset({
    "",
    "assertion",          # a pure condition, no HTTP status involved
    "unexpected_status",  # the gateway answered with a status outside the expectation
    "journey_error",      # a journey raised before finishing
    "cleanup_failed",     # an undo step failed; shared state may need a look
    "setup_failed",       # a precondition could not be created, dependent checks skipped
    "skipped",            # not applicable to this environment (reason is in the feature map)
})
# Exception types that may be named in the artifact. Anything else is reported as "Exception".
EXCEPTION_TYPES = frozenset({
    "AssertionError", "KeyError", "IndexError", "TypeError", "ValueError", "AttributeError",
    "ConnectionError", "ConnectionRefusedError", "ConnectionResetError", "TimeoutError", "OSError",
    "HTTPException", "JourneyError", "SetupError", "RemoteDisconnected", "IncompleteRead", "JSONDecodeError",
})


def exception_type(exc):
    name = type(exc).__name__
    return name if name in EXCEPTION_TYPES else "Exception"


def _statuses(value, label):
    """A status, or a set of acceptable statuses, as a sorted tuple; None means 'no status'."""
    if value is None:
        return None
    if isinstance(value, bool):
        raise TypeError(f"{label} must be an HTTP status")
    items = [value] if isinstance(value, int) else list(value)
    if not items or any(isinstance(i, bool) or not isinstance(i, int) or not 100 <= i <= 599 for i in items):
        raise TypeError(f"{label} must be an HTTP status or a collection of them")
    return tuple(sorted(set(items)))


class CheckRecord:
    __slots__ = ("name", "passed", "expected", "actual", "note")

    def __init__(self, name, passed, expected, actual, note):
        self.name, self.passed, self.expected, self.actual, self.note = name, passed, expected, actual, note

    def as_dict(self):
        expected = list(self.expected) if self.expected is not None else None
        if expected is not None and len(expected) == 1:
            expected = expected[0]
        return {"name": self.name, "passed": self.passed, "expected": expected, "actual": self.actual, "note": self.note}


class Recorder:
    def __init__(self):
        self.checks = []

    @staticmethod
    def _name(name):
        if not isinstance(name, str) or not NAME.fullmatch(name) or LONG_HEX.search(name):
            raise ValueError("check name outside the closed alphabet")
        return name

    @staticmethod
    def _note(note):
        if note not in NOTES:
            raise ValueError("check note outside the fixed vocabulary")
        return note

    def check(self, name, expected, actual, note=""):
        """Record a status expectation. `actual` is an int status (never a response)."""
        self._name(name)
        want = _statuses(expected, "expected")
        if want is None:
            raise TypeError("expected must be a status; use assertion() for a plain condition")
        if isinstance(actual, bool) or not isinstance(actual, int) or not 100 <= actual <= 599:
            raise TypeError("actual must be an HTTP status")
        passed = actual in want
        self.checks.append(CheckRecord(name, passed, want, actual, self._note(note) or ("" if passed else "unexpected_status")))
        return passed

    def assertion(self, name, condition, note="assertion"):
        self._name(name)
        passed = condition is True
        self.checks.append(CheckRecord(name, passed, None, None, self._note(note)))
        return passed

    def fail(self, name, note):
        """A check that cannot pass: a journey error, a failed setup or cleanup."""
        self._name(name)
        self.checks.append(CheckRecord(name, False, None, None, self._note(note)))
        return False

    def skip(self, name):
        """Record that a check does not apply in this environment (the reason is in the feature map)."""
        self._name(name)
        self.checks.append(CheckRecord(name, True, None, None, "skipped"))

    @property
    def passed(self):
        return sum(1 for c in self.checks if c.passed and c.note != "skipped")

    @property
    def failed(self):
        return sum(1 for c in self.checks if not c.passed)

    @property
    def skipped(self):
        return sum(1 for c in self.checks if c.note == "skipped")


class JourneyResult:
    def __init__(self, name, recorder, duration, error=None):
        self.name, self.recorder, self.duration, self.error = name, recorder, duration, error

    @property
    def ok(self):
        return self.error is None and self.recorder.failed == 0

    def as_dict(self):
        return {
            "journey": self.name,
            "ok": self.ok,
            "passed": self.recorder.passed,
            "failed": self.recorder.failed,
            "skipped": self.recorder.skipped,
            "error": self.error,
            "duration_seconds": round(self.duration, 2),
            "checks": [c.as_dict() for c in self.recorder.checks],
        }


def summary_document(environment, results):
    """The JSON summary. `environment` is the loopback/host label only, never a URL with a query."""
    return {
        "suite": "scripts/journeys",
        "environment": environment,
        "ok": all(r.ok for r in results),
        "passed": sum(r.recorder.passed for r in results),
        "failed": sum(r.recorder.failed for r in results),
        "skipped": sum(r.recorder.skipped for r in results),
        "journeys": [r.as_dict() for r in results],
    }


def render_table(results):
    width = max([len(r.name) for r in results] + [7])
    lines = [f"{'journey':<{width}}  {'passed':>6}  {'failed':>6}  {'seconds':>7}  result"]
    for r in results:
        lines.append(f"{r.name:<{width}}  {r.recorder.passed:>6}  {r.recorder.failed:>6}  {r.duration:>7.1f}  {'ok' if r.ok else 'FAILED' + (' (' + r.error + ')' if r.error else '')}")
        for c in r.recorder.checks:
            if not c.passed:
                want = "-" if c.expected is None else "/".join(str(s) for s in c.expected)
                lines.append(f"    FAIL {c.name}: expected {want}, got {c.actual if c.actual is not None else '-'}{' [' + c.note + ']' if c.note else ''}")
    total_p = sum(r.recorder.passed for r in results)
    total_f = sum(r.recorder.failed for r in results)
    total_s = sum(r.recorder.skipped for r in results)
    lines.append(f"{total_p} checks passed, {total_f} failed{f', {total_s} skipped' if total_s else ''} across {len(results)} journeys")
    return "\n".join(lines)
