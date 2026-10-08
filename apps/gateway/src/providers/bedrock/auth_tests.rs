//! Bedrock identity, endpoint and route policy. Loopback mocks only: no AWS account,
//! host credential chain or paid request is used.
use super::*;
use crate::providers::bedrock::auth::{
    self, AwsAuth, Plan, Policy, connection_valid, valid_upstream_model,
};
use aws_sdk_bedrockruntime::config::{Credentials, ProvideCredentials, SharedCredentialsProvider};

const ROLE: &str = "arn:aws:iam::123456789012:role/gateway/bedrock-invoke";
const VPCE: &str = "https://vpce-0abc123.bedrock-runtime.us-east-1.vpce.amazonaws.com";

#[test]
fn references_round_trip_and_reject_noncanonical_forms() {
    for reference in [
        "aws:default".to_owned(),
        "aws:profile:bedrock-prod".to_owned(),
        format!("aws:role:{ROLE}"),
        format!("aws:role:{ROLE};external_id=Tenant:42/a=b"),
        format!("aws:role:{ROLE};session_name=team.a@gw"),
        format!("aws:role:{ROLE};external_id=ext-1;session_name=gw"),
        "aws:role:arn:aws-us-gov:iam::123456789012:role/r".to_owned(),
        "aws:role:arn:aws-cn:iam::123456789012:role/a/b/c/name+=,.@-_".to_owned(),
    ] {
        let parsed = AwsAuth::parse(&reference).unwrap_or_else(|| panic!("{reference}"));
        assert!(reference == parsed.reference(), "{reference}");
    }
    for bad in [
        "aws:default ".to_owned(),
        "aws:".to_owned(),
        "aws:profile:".to_owned(),
        "aws:profile:-starts-with-dash".to_owned(),
        "aws:profile:has space".to_owned(),
        "aws:profile:../../etc/passwd".to_owned(),
        format!("aws:profile:{}", "p".repeat(65)),
        "aws:role:not-an-arn".to_owned(),
        "aws:role:arn:aws:iam::12345678901:role/short-account".to_owned(),
        "aws:role:arn:aws:iam:us-east-1:123456789012:role/regional".to_owned(),
        "aws:role:arn:aws:sts::123456789012:role/wrong-service".to_owned(),
        "aws:role:arn:aws:iam::123456789012:user/not-a-role".to_owned(),
        "aws:role:arn:aws:iam::123456789012:role/".to_owned(),
        "aws:role:arn:aws:iam::123456789012:role/a//b".to_owned(),
        "aws:role:arn:aws:iam::123456789012:role/trailing/".to_owned(),
        format!("aws:role:arn:aws:iam::123456789012:role/{}", "n".repeat(65)),
        "aws:role:arn:evil:iam::123456789012:role/r".to_owned(),
        format!("aws:role:{ROLE};session_name=gw;external_id=wrong-order"),
        format!("aws:role:{ROLE};external_id=a1;external_id=a2"),
        format!("aws:role:{ROLE};external_id=x"),
        format!("aws:role:{ROLE};external_id=has space"),
        format!("aws:role:{ROLE};session_name=has:colon"),
        format!("aws:role:{ROLE};session_name={}", "s".repeat(65)),
        format!("aws:role:{ROLE};unknown=value"),
        format!("aws:role:{ROLE};"),
        "env:AWS_SECRET_ACCESS_KEY".to_owned(),
        "AKIAIOSFODNN7EXAMPLE".to_owned(),
    ] {
        assert!(AwsAuth::parse(&bad).is_none(), "{bad}");
    }
    // Management input: options are separate fields, accepted only with a role.
    let role = format!("aws:role:{ROLE}");
    assert_eq!(
        AwsAuth::from_parts(&role, Some("ext-1"), Some("gw"))
            .unwrap()
            .reference(),
        format!("{role};external_id=ext-1;session_name=gw")
    );
    assert!(AwsAuth::from_parts(&format!("{role};external_id=ext-1"), None, None).is_none());
    assert!(AwsAuth::from_parts("aws:default", Some("ext-1"), None).is_none());
    assert!(AwsAuth::from_parts("aws:profile:p", None, Some("gw")).is_none());
    assert!(AwsAuth::from_parts(&role, Some(""), None).is_none());
    assert!(AwsAuth::from_parts(&role, Some("a;session_name=x"), None).is_none());
}

#[test]
fn server_allowlists_gate_profiles_endpoints_and_regions() {
    let policy = Policy::new(
        [" bedrock-prod ", "", "bad name"],
        [
            &format!("{VPCE}/") as &str,
            "http://plain.example",
            "https://user:pw@host.example",
            "not a url",
        ],
    );
    let ok = |r: &str, e: Option<&str>, region: &str| connection_valid(&policy, r, e, Some(region));
    assert!(ok("aws:default", None, "us-east-1"));
    assert!(ok("aws:default", None, "us-gov-west-1"));
    assert!(ok("aws:profile:bedrock-prod", None, "eu-central-1"));
    assert!(ok(
        &format!("aws:role:{ROLE};external_id=e1"),
        Some(VPCE),
        "us-east-1"
    ));
    assert!(
        !ok("aws:profile:other", None, "us-east-1"),
        "profile not allowlisted"
    );
    assert!(!ok("aws:profile:bad name", None, "us-east-1"));
    assert!(!connection_valid(&policy, "aws:default", None, None));
    for region in [
        "",
        "us-east",
        "useast1",
        "US-EAST-1",
        "us-east-1/evil",
        "us-east-123",
        "u-east-1",
        "us--1",
    ] {
        assert!(!ok("aws:default", None, region), "{region}");
    }
    for endpoint in [
        "https://vpce-other.bedrock-runtime.us-east-1.vpce.amazonaws.com",
        &format!("{VPCE}/"),
        &format!("{VPCE}/model"),
        &format!("{VPCE}?x=1"),
        &format!("{VPCE}#f"),
        "http://plain.example",
        "https://user:pw@host.example",
        "",
    ] {
        assert!(
            !ok("aws:default", Some(endpoint), "us-east-1"),
            "{endpoint}"
        );
    }
    // Empty allowlists: only the default identity, roles and regional endpoints.
    let empty = Policy::default();
    assert!(connection_valid(
        &empty,
        "aws:default",
        None,
        Some("us-east-1")
    ));
    assert!(!connection_valid(
        &empty,
        "aws:profile:bedrock-prod",
        None,
        Some("us-east-1")
    ));
    assert!(!connection_valid(
        &empty,
        "aws:default",
        Some(VPCE),
        Some("us-east-1")
    ));
}

#[test]
fn routes_accept_model_and_inference_profile_ids_and_same_region_arns() {
    for model in [
        "anthropic.claude-3-5-sonnet-20240620-v1:0",
        "us.anthropic.claude-3-7-sonnet-20250219-v1:0",
        "global.anthropic.claude-sonnet-4-20250514-v1:0",
        "apac.amazon.nova-pro-v1:0",
        "test-model",
        "arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-3-haiku-20240307-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-3-7-sonnet-20250219-v1:0",
        "arn:aws:bedrock:us-east-1:123456789012:application-inference-profile/a1b2c3d4e5",
        "arn:aws:bedrock:us-east-1:123456789012:provisioned-model/abc123",
        "arn:aws:bedrock:us-east-1:123456789012:prompt/PROMPT12345:1",
    ] {
        assert!(valid_upstream_model(model, "us-east-1"), "{model}");
    }
    for model in [
        "",
        " us.anthropic.claude",
        "model with space",
        "model\n",
        "-leading-dash",
        &"m".repeat(257),
        "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.anthropic.claude-3-7-sonnet-20250219-v1:0",
        "arn:aws:bedrock:us-east-1::inference-profile/us.anthropic.claude",
        "arn:aws:bedrock:us-east-1:123456789012:knowledge-base/kb1",
        "arn:aws:s3:us-east-1:123456789012:inference-profile/x",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/",
        "arn:aws:bedrock:us-east-1:123456789012",
    ] {
        assert!(!valid_upstream_model(model, "us-east-1"), "{model}");
    }
}

fn bedrock_target(credential_ref: &str, endpoint: Option<&str>) -> Deployment {
    let mut t = target();
    t.credential_ref = credential_ref.into();
    t.endpoint = endpoint.map(str::to_owned);
    t
}

#[tokio::test]
async fn plans_follow_the_adapter_policy_before_credentials_or_network() {
    let mut adapter = BedrockAdapter::new().unwrap();
    adapter.policy = Policy::new(["bedrock-prod"], [VPCE]);
    let plan = |t: &Deployment| Plan::new(t, &adapter.policy);
    let role = format!("aws:role:{ROLE};external_id=ext-1");
    let with_vpce = plan(&bedrock_target(&role, Some(VPCE))).unwrap();
    assert_eq!(with_vpce.endpoint.as_deref(), Some(VPCE));
    assert!(with_vpce.auth == AwsAuth::parse(&role).unwrap());
    // Every identity/endpoint/region combination has its own client and credential cache.
    let keys: std::collections::BTreeSet<String> = [
        bedrock_target("aws:default", None),
        bedrock_target("aws:profile:bedrock-prod", None),
        bedrock_target(&role, None),
        bedrock_target(&format!("aws:role:{ROLE};external_id=ext-2"), None),
        bedrock_target(&role, Some(VPCE)),
    ]
    .iter()
    .map(|t| plan(t).unwrap().key())
    .collect();
    assert_eq!(keys.len(), 5);
    let mut arn_route = bedrock_target("aws:default", None);
    arn_route.upstream_model =
        "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.anthropic.claude".into();
    for target in [
        bedrock_target("aws:profile:not-allowlisted", None),
        bedrock_target("aws:default", Some("https://vpce-unlisted.example")),
        bedrock_target("aws:default", Some("http://127.0.0.1:9")),
        bedrock_target("aws:role:not-an-arn", None),
        arn_route,
    ] {
        assert!(plan(&target).is_none());
        // Rejected before encoding, credential discovery or any connection attempt.
        assert!(matches!(
            adapter.execute(&target, request()).await,
            Err(InferenceError::Configuration)
        ));
    }
}

#[tokio::test]
#[allow(deprecated)]
async fn named_profile_resolves_only_that_profile() {
    use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
    let path = std::env::temp_dir().join(format!("omg-bedrock-{}.ini", uuid::Uuid::new_v4()));
    std::fs::write(
        &path,
        "[default]\naws_access_key_id = AKIDDEFAULT\naws_secret_access_key = default-not-a-secret\n\n[bedrock-prod]\naws_access_key_id = AKIDPROFILE\naws_secret_access_key = profile-not-a-secret\n",
    )
    .unwrap();
    let files = ProfileFiles::builder()
        .with_file(ProfileFileKind::Credentials, &path)
        .build();
    let provider = auth::profile_provider("bedrock-prod", "us-east-1", Some(files.clone()));
    let credentials = provider.provide_credentials().await.unwrap();
    assert_eq!(credentials.access_key_id(), "AKIDPROFILE");
    let missing = auth::profile_provider("absent", "us-east-1", Some(files));
    assert!(missing.provide_credentials().await.is_err());
    std::fs::remove_file(path).unwrap();
}

/// Minimal STS AssumeRole endpoint capturing the form body and authorization header.
async fn sts_mock() -> (
    String,
    Arc<StdMutex<Vec<(String, String)>>>,
    tokio::task::JoinHandle<()>,
) {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let capture = requests.clone();
    let app = Router::new().fallback(move |headers: HeaderMap, bytes: Bytes| {
        let capture = capture.clone();
        async move {
            capture.lock().unwrap().push((
                headers["authorization"].to_str().unwrap().to_owned(),
                String::from_utf8(bytes.to_vec()).unwrap(),
            ));
            Response::builder()
                .status(200)
                .header("content-type", "text/xml")
                .body(Body::from(
                    r#"<AssumeRoleResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/"><AssumeRoleResult><AssumedRoleUser><AssumedRoleId>AROATEST:gw</AssumedRoleId><Arn>arn:aws:sts::123456789012:assumed-role/bedrock-invoke/gw</Arn></AssumedRoleUser><Credentials><AccessKeyId>ASIAASSUMEDTEST</AccessKeyId><SecretAccessKey>assumed-not-a-secret</SecretAccessKey><SessionToken>assumed-session-token</SessionToken><Expiration>2099-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult><ResponseMetadata><RequestId>req-1</RequestId></ResponseMetadata></AssumeRoleResponse>"#,
                ))
                .unwrap()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, requests, task)
}

#[tokio::test]
async fn assumed_role_uses_server_identity_external_id_and_signs_bedrock_with_role_credentials() {
    let (sts_base, sts_requests, sts_task) = sts_mock().await;
    let server_identity = SharedCredentialsProvider::new(Credentials::new(
        "AKIDSERVER",
        "server-not-a-secret",
        None,
        None,
        "server-identity",
    ));
    // Production builds the same STS configuration; only the test points it at loopback.
    let sts = auth::sts_config("us-east-1", server_identity)
        .into_builder()
        .endpoint_url(&sts_base)
        .build();
    let role = auth::assume_role_provider(ROLE, Some("tenant-ext-42"), None, &sts).await;
    let mock = Mock::serve(complete(false), Vec::new(), 200).await;
    let mut adapter = BedrockAdapter::new().unwrap();
    let plan = Plan {
        region: "us-east-1".into(),
        auth: AwsAuth::parse(&format!("aws:role:{ROLE};external_id=tenant-ext-42")).unwrap(),
        // Endpoint override path (allowlisted HTTPS in production; loopback here).
        endpoint: Some(mock.base.clone()),
    };
    adapter.test_client = Some(Client::from_conf(
        adapter.service_config(&plan, SharedCredentialsProvider::new(role)),
    ));
    let output = adapter.execute(&target(), request()).await.unwrap();
    assert!(matches!(output, ProviderOutput::Complete(_)));
    {
        let sts_requests = sts_requests.lock().unwrap();
        assert_eq!(sts_requests.len(), 1);
        let (authorization, form) = &sts_requests[0];
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDSERVER/"));
        assert!(authorization.contains("/us-east-1/sts/aws4_request"));
        assert!(form.contains("Action=AssumeRole"));
        assert!(form.contains(
            "RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fgateway%2Fbedrock-invoke"
        ));
        assert!(form.contains("ExternalId=tenant-ext-42"));
        assert!(form.contains(&format!("RoleSessionName={}", auth::DEFAULT_SESSION_NAME)));
    }
    let requests = mock.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let authorization = requests[0].headers["authorization"].to_str().unwrap();
    assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=ASIAASSUMEDTEST/"));
    assert!(authorization.contains("/us-east-1/bedrock/aws4_request"));
    assert_eq!(
        requests[0].headers["x-amz-security-token"],
        "assumed-session-token"
    );
    sts_task.abort();
}

#[tokio::test]
async fn assumed_role_session_name_is_configurable() {
    let (sts_base, sts_requests, sts_task) = sts_mock().await;
    let sts = auth::sts_config(
        "eu-central-1",
        SharedCredentialsProvider::new(Credentials::new("AKIDSERVER", "s", None, None, "t")),
    )
    .into_builder()
    .endpoint_url(&sts_base)
    .build();
    let role = auth::assume_role_provider(ROLE, None, Some("team-a.gw"), &sts).await;
    assert_eq!(
        role.provide_credentials().await.unwrap().access_key_id(),
        "ASIAASSUMEDTEST"
    );
    let (authorization, form) = sts_requests.lock().unwrap()[0].clone();
    assert!(authorization.contains("/eu-central-1/sts/aws4_request"));
    assert!(form.contains("RoleSessionName=team-a.gw"));
    assert!(!form.contains("ExternalId"));
    sts_task.abort();
}
