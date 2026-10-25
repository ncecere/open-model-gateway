//! Hand-written expectations, independent of the shared decision implementation.
use super::*;

#[test]
fn workspace_authority_matches_independent_expected_table() {
    // owner, admin, all activity, member, platform reader, platform writer, disabled
    const PERSONAL: (bool, bool, bool, bool, bool, bool, bool) =
        (true, true, true, true, false, false, false);
    const MEMBER: (bool, bool, bool, bool, bool, bool, bool) =
        (false, false, false, true, false, false, false);
    const ADMIN_MEMBER: (bool, bool, bool, bool, bool, bool, bool) =
        (false, true, true, true, false, false, false);
    const OWNER: (bool, bool, bool, bool, bool, bool, bool) =
        (true, true, true, true, false, false, false);
    const AUDITOR: (bool, bool, bool, bool, bool, bool, bool) =
        (false, false, false, false, true, false, false);
    const PLATFORM_ADMIN: (bool, bool, bool, bool, bool, bool, bool) =
        (false, false, false, false, true, true, false);
    const DISABLED_AUDITOR: (bool, bool, bool, bool, bool, bool, bool) =
        (false, false, false, false, true, false, true);
    const DISABLED_ADMIN: (bool, bool, bool, bool, bool, bool, bool) =
        (false, false, false, false, true, true, true);
    let caller = Uuid::from_u128(1);
    let other = Uuid::from_u128(2);
    let cases = [
        (
            "foreign personal/user",
            "user",
            "personal",
            Some(other),
            false,
            None,
            false,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "foreign personal/auditor",
            "auditor",
            "personal",
            Some(other),
            false,
            None,
            false,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "foreign personal/admin and owner membership",
            "admin",
            "personal",
            Some(other),
            false,
            Some("owner"),
            false,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "own personal without membership",
            "user",
            "personal",
            Some(caller),
            false,
            None,
            false,
            Ok(PERSONAL),
        ),
        (
            "own personal member role ignored",
            "user",
            "personal",
            Some(caller),
            false,
            Some("member"),
            false,
            Ok(PERSONAL),
        ),
        (
            "own personal admin role ignored",
            "user",
            "personal",
            Some(caller),
            false,
            Some("admin"),
            false,
            Ok(PERSONAL),
        ),
        (
            "disabled personal owner",
            "user",
            "personal",
            Some(caller),
            true,
            Some("owner"),
            true,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled personal platform admin owner",
            "admin",
            "personal",
            Some(caller),
            true,
            Some("owner"),
            true,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled personal foreign auditor",
            "auditor",
            "personal",
            Some(other),
            true,
            None,
            true,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled shared hidden from user owner",
            "user",
            "team",
            None,
            true,
            Some("owner"),
            false,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled shared hidden from auditor",
            "auditor",
            "team",
            None,
            true,
            None,
            false,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled shared hidden from admin",
            "admin",
            "team",
            None,
            true,
            None,
            false,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled shared user owner has no access",
            "user",
            "project",
            None,
            true,
            Some("owner"),
            true,
            Err(StatusCode::NOT_FOUND),
        ),
        (
            "disabled shared auditor loses membership powers",
            "auditor",
            "team",
            None,
            true,
            Some("owner"),
            true,
            Ok(DISABLED_AUDITOR),
        ),
        (
            "disabled shared admin loses membership powers",
            "admin",
            "project",
            None,
            true,
            Some("admin"),
            true,
            Ok(DISABLED_ADMIN),
        ),
        (
            "shared non-member user",
            "user",
            "team",
            None,
            false,
            None,
            false,
            Err(StatusCode::FORBIDDEN),
        ),
        (
            "shared non-member auditor",
            "auditor",
            "team",
            None,
            false,
            None,
            false,
            Ok(AUDITOR),
        ),
        (
            "shared non-member platform admin",
            "admin",
            "project",
            None,
            false,
            None,
            false,
            Ok(PLATFORM_ADMIN),
        ),
        (
            "shared member",
            "user",
            "team",
            None,
            false,
            Some("member"),
            false,
            Ok(MEMBER),
        ),
        (
            "shared admin member",
            "user",
            "project",
            None,
            false,
            Some("admin"),
            false,
            Ok(ADMIN_MEMBER),
        ),
        (
            "shared owner",
            "user",
            "team",
            None,
            false,
            Some("owner"),
            false,
            Ok(OWNER),
        ),
    ];
    for (name, role, kind, owner, disabled, membership, allow_disabled, expected) in cases {
        let actual = workspace_authority(
            caller,
            role,
            kind.to_owned(),
            owner,
            disabled,
            membership.map(str::to_owned),
            allow_disabled,
        )
        .map(|a| {
            (
                a.owner,
                a.admin,
                a.view_all_activity,
                a.member,
                a.platform_reader,
                a.platform_writer,
                a.disabled,
            )
        })
        .map_err(|e| e.0);
        assert_eq!(actual, expected, "{name}");
    }
}
