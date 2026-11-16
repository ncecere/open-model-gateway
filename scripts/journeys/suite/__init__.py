"""Every journey module. Importing the package registers them all."""
from . import (  # noqa: F401
    access_requests,
    access_reviews,
    auditor,
    catalogs,
    credentials,
    csrf_origin,
    health_spa,
    inference,
    keys,
    me,
    members,
    platform_admin,
    reports,
    schedules,
    signin,
    workspaces,
)
