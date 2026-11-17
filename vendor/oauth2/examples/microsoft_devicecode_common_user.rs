#[path = "support/terminal.rs"]
mod terminal;

use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, ClientId, DeviceAuthorizationUrl, Scope, StandardDeviceAuthorizationResponse, TokenUrl,
};

use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = BasicClient::new(ClientId::new("client_id".to_string()))
        .set_auth_uri(AuthUrl::new(
            "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".to_string(),
        )?)
        .set_token_uri(TokenUrl::new(
            "https://login.microsoftonline.com/common/oauth2/v2.0/token".to_string(),
        )?)
        .set_device_authorization_url(DeviceAuthorizationUrl::new(
            "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode".to_string(),
        )?);

    let http_client = reqwest::ClientBuilder::new()
        // Following redirects opens the client up to SSRF vulnerabilities.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("Client should build");

    let details: StandardDeviceAuthorizationResponse = client
        .exchange_device_code()
        .add_scope(Scope::new("read".to_string()))
        .request_async(&http_client)
        .await
        .map_err(|_| std::io::Error::other("Device authorization request failed"))?;

    terminal::display(
        details.verification_uri().as_str(),
        Some(details.user_code().secret()),
    )
    .expect("An interactive terminal is required for authorization");

    let token_result = client
        .exchange_device_access_token(&details)
        .request_async(&http_client, tokio::time::sleep, None)
        .await;

    println!("Token request succeeded: {}", token_result.is_ok());

    Ok(())
}
