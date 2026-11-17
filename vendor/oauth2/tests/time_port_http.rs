//! Synthetic loopback HTTP acceptance; never contacts an identity provider.
#![cfg(feature = "reqwest-blocking")]

use oauth2::basic::BasicClient;
use oauth2::{AccessToken, ClientId, IntrospectionUrl, TokenIntrospectionResponse};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

#[test]
fn introspection_dates_over_real_http_accept_and_refuse() {
    for (body, accepted) in [
        (
            r#"{"active":true,"exp":8210266876799,"iat":-8334601228800,"nbf":0}"#,
            true,
        ),
        (r#"{"active":false,"exp":null}"#, true),
        (r#"{"active":true,"exp":0.25}"#, false),
        (r#"{"active":true,"exp":8210266876800}"#, false),
        (r#"{"active":true,"nbf":-8334601228801}"#, false),
        (r#"{"active":true,"iat":"1970-01-01T00:00:00Z"}"#, false),
        (r#"{"active":true,"exp":18446744073709551615}"#, false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/introspect", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let length = stream.read(&mut buffer).unwrap();
                assert!(length > 0);
                request.extend_from_slice(&buffer[..length]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let content_length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if request.len() >= end + 4 + content_length {
                        break;
                    }
                }
            }
            assert!(request.starts_with(b"POST /introspect HTTP/1.1\r\n"));
            assert!(String::from_utf8_lossy(&request)
                .ends_with("\r\n\r\ntoken=synthetic-token&client_id=synthetic-client"));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let transport = oauth2::reqwest::blocking::Client::builder()
            .redirect(oauth2::reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let client = BasicClient::new(ClientId::new("synthetic-client".to_owned()))
            .set_introspection_url(IntrospectionUrl::new(url).unwrap());
        let result = client
            .introspect(&AccessToken::new("synthetic-token".to_owned()))
            .request(&transport);
        server.join().unwrap();
        assert_eq!(result.is_ok(), accepted, "{body}");
        if let Ok(response) = result {
            if response.active() {
                assert_eq!(response.exp().unwrap().unix_seconds(), 8_210_266_876_799);
                assert_eq!(response.iat().unwrap().unix_seconds(), -8_334_601_228_800);
                assert_eq!(response.nbf().unwrap().unix_seconds(), 0);
            }
        }
    }
}
