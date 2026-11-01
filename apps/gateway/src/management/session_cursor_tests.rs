use super::*;

#[test]
fn session_cursor_timestamp_bounds() {
    let ws = "00000000-0000-0000-0000-000000000001";
    for (case, micros, accepted) in [
        ("below PostgreSQL minimum", "-210866803200000001", false),
        ("PostgreSQL minimum", "-210866803200000000", true),
        ("chrono maximum", "8210266876799999999", true),
        ("above chrono maximum", "8210266876800000000", false),
        ("above PostgreSQL maximum", "9224318016000000000", false),
    ] {
        let result = session_cursor(&format!("{micros}_{ws}_61"));
        assert_eq!(result.is_ok(), accepted, "{case}: acceptance");
        if let Err(error) = result {
            assert_eq!(error.0, StatusCode::BAD_REQUEST, "{case}: status");
        }
    }
}
