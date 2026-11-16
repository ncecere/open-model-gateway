"""Every journey module. Importing the package registers them all."""
from . import (  # noqa: F401
    access_requests,
    access_reviews,
    auditor,
    credentials,
    csrf_origin,
    health_spa,
    inference,
    keys,
    me,
    members,
    platform_admin,
    reports,
    signin,
    workspaces,
)
