"""Run journeys: each in its own context, always cleaned up, one failure never stops the rest."""
import time

from .context import Ctx
from .recorder import JourneyResult, exception_type


def run_one(env, name, fn):
    ctx = Ctx(env, name)
    started = time.monotonic()
    error = None
    try:
        fn(ctx)
    except Exception as exc:  # noqa: BLE001 - recorded by type only; the message may carry anything
        error = exception_type(exc)
        ctx.rec.fail("journey ended early", "setup_failed" if error == "SetupError" else "journey_error")
    finally:
        ctx.run_cleanups()
    return JourneyResult(name, ctx.rec, time.monotonic() - started, error)


def run_all(env, journeys, names):
    results = []
    try:
        for name in names:
            results.append(run_one(env, name, journeys[name]))
    finally:
        env.close()
    return results
