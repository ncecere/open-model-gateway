#[path = "support/terminal.rs"]
mod terminal;

use oauth2::basic::BasicClient;
use oauth2::reqwest;
use oauth2::StandardDeviceAuthorizationResponse;
use oauth2::{AuthUrl, ClientId, DeviceAuthorizationUrl, Scope, TokenUrl};

use std::error::Error;

// Reference: https://learn.microsoft.com/en-us/azure/active-directory/develop/v2-oauth2-device-code
// Please use your tenant id when using this example
const TENANT_ID: &str = "{tenant}";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let client = BasicClient::new(ClientId::new("client_id".to_string()))
        .set_auth_uri(AuthUrl::new(format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
            TENANT_ID
        ))?)
        .set_token_uri(TokenUrl::new(format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            TENANT_ID
        ))?)
        .set_device_authorization_url(DeviceAuthorizationUrl::new(format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode",
            TENANT_ID
        ))?);

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
