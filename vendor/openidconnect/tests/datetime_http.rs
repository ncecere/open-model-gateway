//! Synthetic user-info over the unchanged blocking HTTP adapter; no external issuer.
use openidconnect::core::{CoreClient, CoreJsonWebKeySet, CoreUserInfoClaims};
use openidconnect::{
    AccessToken, ClientId, IssuerUrl, SubjectIdentifier, UserInfoError, UserInfoUrl,
};
use serde_json::json;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

#[test]
fn user_info_datetime_and_subject_contract_over_real_http() {
    let mut cases = vec![
        (
            json!({"sub":"subject", "updated_at":-1e-20}),
            Some((-1, 1_000_000_000)),
        ),
        (
            json!({"sub":"subject", "updated_at":-1e-100}),
            Some((-1, 1_000_000_000)),
        ),
        (
            json!({"sub":"subject", "updated_at":-1.25}),
            Some((-2, 750_000_000)),
        ),
        (
            json!({"sub":"subject", "updated_at":8210266876799_i64}),
            Some((8_210_266_876_799, 0)),
        ),
        (
            json!({"sub":"subject", "updated_at":-8334601228800_i64}),
            Some((-8_334_601_228_800, 0)),
        ),
        (
            json!({"sub":"subject", "updated_at":8210266876800_i64}),
            None,
        ),
        (
            json!({"sub":"subject", "updated_at":-8334601228801_i64}),
            None,
        ),
        (json!({"sub":"other", "updated_at":0}), None),
    ];
    if cfg!(feature = "accept-rfc3339-timestamps") {
        cases.extend([
            (
                json!({"sub":"subject", "updated_at":"2017-01-01T00:59:60.25+01:00"}),
                Some((1_483_228_799, 1_250_000_000)),
            ),
            (
                json!({"sub":"subject", "updated_at":"1970-01-01 01:00:00.1234567899+01:00"}),
                Some((0, 123_456_789)),
            ),
        ]);
    }
    for invalid in [
        "1970-01-01X00:00:00Z",
        "1970-01-01\t00:00:00Z",
        "1970-01-01T00:00:00+25:00",
        "not a timestamp",
    ] {
        cases.push((json!({"sub":"subject", "updated_at":invalid}), None));
    }
    for (body, expected) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/userinfo", listener.local_addr().unwrap());
        let bytes = serde_json::to_vec(&body).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
            }
            assert!(request.starts_with(b"GET /userinfo HTTP/1.1\r\n"));
            assert!(String::from_utf8_lossy(&request)
                .to_ascii_lowercase()
                .contains("authorization: bearer synthetic-access\r\n"));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
            stream.write_all(&bytes).unwrap();
        });
        let transport = openidconnect::reqwest::blocking::Client::builder()
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let client = CoreClient::new(
            ClientId::new("client".to_owned()),
            IssuerUrl::new("https://issuer.example".to_owned()).unwrap(),
            CoreJsonWebKeySet::new(vec![]),
        )
        .set_user_info_url(UserInfoUrl::new(url).unwrap());
        let result: Result<CoreUserInfoClaims, _> = client
            .user_info(
                AccessToken::new("synthetic-access".to_owned()),
                Some(SubjectIdentifier::new("subject".to_owned())),
            )
            .request(&transport);
        server.join().unwrap();
        if let Some((seconds, nanos)) = expected {
            let observed = result.unwrap().updated_at().unwrap();
            assert_eq!(
                (observed.unix_seconds(), observed.subsecond_nanos()),
                (seconds, nanos)
            );
        } else if body["sub"] == "other" {
            assert!(matches!(result, Err(UserInfoError::ClaimsVerification(_))));
        } else {
            assert!(matches!(result, Err(UserInfoError::Parse(_))), "{body}");
        }
    }
}
