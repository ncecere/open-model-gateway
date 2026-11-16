"""Run journeys: each in its own context, always cleaned up, one failure never stops the rest."""
import os
import time
import traceback

from .context import Ctx
from .recorder import JourneyResult, Recorder, exception_type

HERE = os.path.dirname(os.path.abspath(__file__))


def failing_line(exc):
    """The source line (an int) of the innermost frame inside this suite; never the message."""
    line = None
    for frame in traceback.extract_tb(exc.__traceback__):
        if os.path.abspath(frame.filename).startswith(HERE):
            line = frame.lineno
    return line


def run_one(env, name, fn):
    ctx = Ctx(env, name)
    started = time.monotonic()
    error = after = line = None
    try:
        fn(ctx)
    except Exception as exc:  # noqa: BLE001 - recorded by type only; the message may carry anything
        error, after, line = exception_type(exc), ctx.rec.last_name, failing_line(exc)
        ctx.rec.fail("journey ended early", "setup_failed" if error == "SetupError" else "journey_error")
    finally:
        ctx.run_cleanups()
    if not error and not ctx.rec.checks:
        ctx.rec.fail("journey recorded no checks", "journey_error")
    return JourneyResult(name, ctx.rec, time.monotonic() - started, error, after, line)


def not_run(name):
    rec = Recorder()
    rec.fail("journey not run before the deadline", "not_run")
    return JourneyResult(name, rec, 0.0, "TimeoutError")


def run_all(env, journeys, names, deadline=None):
    """Run the named journeys in order. `deadline` is a time.monotonic() value: journeys that have not
    started by then are reported as not run, and a running journey's requests stop being sent."""
    env.http.deadline = deadline
    results = []
    try:
        for name in names:
            if deadline is not None and time.monotonic() > deadline:
                results.append(not_run(name))
            else:
                results.append(run_one(env, name, journeys[name]))
    finally:
        env.close()
    return results
