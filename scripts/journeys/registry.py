"""Journey registry. A journey is a function taking a Ctx; its name is its identity in the feature map."""
import re

NAME = re.compile(r"[a-z][a-z0-9-]{1,40}")
JOURNEYS = {}


def journey(name):
    if not NAME.fullmatch(name):
        raise ValueError("journey names are lower-case words joined by dashes")

    def register(fn):
        if name in JOURNEYS:
            raise ValueError("duplicate journey")
        JOURNEYS[name] = fn
        return fn

    return register


def load_all():
    """Import every journey module so the registry is complete."""
    from . import suite  # noqa: F401

    return JOURNEYS
