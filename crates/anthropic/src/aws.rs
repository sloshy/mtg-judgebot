//! `SigV4` for the two AWS doors ([`Endpoint::ClaudePlatformOnAws`] and
//! [`Endpoint::Bedrock`]): where the credentials come from and how a request
//! is signed with them.
//!
//! Credentials come from AWS's standard chain (environment, shared profile,
//! SSO, assumed role, container or instance role) through `aws-config`,
//! never from `judge.toml`. The chain sits behind [`AwsCredentials`] so a
//! test can sign with static keys against wiremock and assert the
//! `Authorization` shape without an AWS account. The signature is computed
//! per attempt (it carries its own timestamp); the credentials are fetched
//! once per call, which `aws-config`'s own cache makes cheap.
//!
//! [`Endpoint::ClaudePlatformOnAws`]: crate::Endpoint::ClaudePlatformOnAws
//! [`Endpoint::Bedrock`]: crate::Endpoint::Bedrock

use std::{fmt, time::SystemTime};

use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::{
    Credentials,
    provider::{ProvideCredentials as _, SharedCredentialsProvider},
};
use aws_sigv4::{
    http_request::{SignableBody, SignableRequest, SigningSettings, sign},
    sign::v4,
};
use judge_llm::LlmError;
use reqwest::header::{HeaderName, HeaderValue};

/// The `door` an [`LlmError::Auth`] from the credential chain names.
const PLATFORM: &str = "aws";

/// The two doors `SigV4` signs, each with its service name (the third field
/// of the credential scope, `.../{region}/{service}/aws4_request`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AwsDoor {
    /// Claude Platform on AWS: service `aws-external-anthropic`.
    ClaudePlatform,
    /// Claude in Amazon Bedrock (the Messages-shaped endpoint): service
    /// `bedrock-mantle` — not `bedrock`, which is the legacy `InvokeModel`
    /// API's name.
    Bedrock,
}

impl AwsDoor {
    /// The `SigV4` service name.
    #[must_use]
    pub const fn service(self) -> &'static str {
        match self {
            AwsDoor::ClaudePlatform => "aws-external-anthropic",
            AwsDoor::Bedrock => "bedrock-mantle",
        }
    }

    /// The door's name in errors and logs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            AwsDoor::ClaudePlatform => "claude-platform-on-aws",
            AwsDoor::Bedrock => "bedrock",
        }
    }
}

/// Where `SigV4` credentials come from.
#[async_trait]
pub trait AwsCredentials: Send + Sync + fmt::Debug {
    /// Current credentials (a provider refreshes temporary ones itself).
    ///
    /// # Errors
    /// [`LlmError::Auth`] when the source has none.
    async fn credentials(&self) -> Result<Credentials, LlmError>;
}

/// AWS's default credential chain, resolved lazily on first use so that
/// building an [`crate::Anthropic`] never touches the network or the disk.
pub struct DefaultChain {
    region: String,
    provider: tokio::sync::OnceCell<SharedCredentialsProvider>,
}

impl DefaultChain {
    /// The chain for `region` (an assumed role talks to that region's STS).
    #[must_use]
    pub fn new(region: impl Into<String>) -> Self {
        Self {
            region: region.into(),
            provider: tokio::sync::OnceCell::new(),
        }
    }
}

impl fmt::Debug for DefaultChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultChain")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AwsCredentials for DefaultChain {
    async fn credentials(&self) -> Result<Credentials, LlmError> {
        let provider = self
            .provider
            .get_or_try_init(|| async {
                let config = aws_config::defaults(BehaviorVersion::latest())
                    .region(Region::new(self.region.clone()))
                    .load()
                    .await;
                config.credentials_provider().ok_or_else(|| LlmError::Auth {
                    door: PLATFORM,
                    message: "the default credential chain has no provider".to_owned(),
                })
            })
            .await?;
        provider
            .provide_credentials()
            .await
            .map_err(|e| LlmError::Auth {
                door: PLATFORM,
                message: e.to_string(),
            })
    }
}

/// Fixed credentials, for tests against a mock server. Never read from
/// configuration: the operator's keys stay in the platform chain.
#[derive(Clone)]
pub struct StaticCredentials(Credentials);

impl StaticCredentials {
    /// Long-term keys (`session_token: None`) or temporary ones.
    #[must_use]
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
    ) -> Self {
        Self(Credentials::new(
            access_key_id,
            secret_access_key,
            session_token,
            None,
            "static",
        ))
    }
}

impl fmt::Debug for StaticCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticCredentials")
            .field("access_key_id", &self.0.access_key_id())
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[async_trait]
impl AwsCredentials for StaticCredentials {
    async fn credentials(&self) -> Result<Credentials, LlmError> {
        Ok(self.0.clone())
    }
}

/// Sign `request` in place for `door` in `region`: adds `x-amz-date`,
/// `authorization` and, for temporary credentials, `x-amz-security-token`.
/// Every header already on the request is signed (`host` is derived from
/// the URL), so the request must be complete — body included — before
/// signing; reqwest's own defaults (`accept`, `content-length`) are added
/// later and are not in the signed set, which is what `SignedHeaders`
/// declares.
///
/// # Errors
/// [`LlmError::Auth`] when the request cannot be canonicalised (a header
/// value that is not ASCII, a body that is not in memory — the signature
/// covers the payload, so a streaming body would sign as empty and be
/// rejected upstream with nothing to say why) or the signer rejects its
/// parameters.
pub fn sign_request(
    request: &mut reqwest::Request,
    door: AwsDoor,
    region: &str,
    credentials: &Credentials,
    now: SystemTime,
) -> Result<(), LlmError> {
    let auth = |message: String| LlmError::Auth {
        door: door.name(),
        message,
    };
    let headers = request
        .headers()
        .iter()
        .map(|(name, value)| value.to_str().map(|v| (name.as_str(), v)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| auth(format!("header is not ASCII: {e}")))?;
    let body = match request.body() {
        None => &[][..],
        Some(body) => body
            .as_bytes()
            .ok_or_else(|| auth("the body must be in memory to be signed".to_owned()))?,
    };
    let signable = SignableRequest::new(
        request.method().as_str(),
        request.url().as_str(),
        headers.into_iter(),
        SignableBody::Bytes(body),
    )
    .map_err(|e| auth(e.to_string()))?;
    let identity = credentials.clone().into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(door.service())
        .time(now)
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| auth(e.to_string()))?
        .into();
    let (instructions, _signature) = sign(signable, &params)
        .map_err(|e| auth(e.to_string()))?
        .into_parts();
    let (signed_headers, _query) = instructions.into_parts();
    for header in signed_headers {
        let name =
            HeaderName::from_bytes(header.name().as_bytes()).map_err(|e| auth(e.to_string()))?;
        let mut value = HeaderValue::from_str(header.value()).map_err(|e| auth(e.to_string()))?;
        // The signer marks the session token; the signature is a credential too.
        value.set_sensitive(header.sensitive() || name == reqwest::header::AUTHORIZATION);
        request.headers_mut().insert(name, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_names_are_the_documented_ones() {
        assert_eq!(AwsDoor::ClaudePlatform.service(), "aws-external-anthropic");
        assert_eq!(AwsDoor::Bedrock.service(), "bedrock-mantle");
    }

    #[test]
    fn debug_redacts_the_secret() {
        let c = StaticCredentials::new("AKIDTEST", "very-secret", Some("session-secret".into()));
        let s = format!("{c:?}");
        assert!(
            s.contains("AKIDTEST") && !s.contains("very-secret") && !s.contains("session-secret"),
            "{s}"
        );
        assert!(format!("{:?}", DefaultChain::new("us-east-1")).contains("us-east-1"));
    }

    #[test]
    fn signing_adds_date_token_and_authorization_over_every_header()
    -> Result<(), Box<dyn std::error::Error>> {
        let http = reqwest::Client::new();
        let mut request = http
            .post("http://127.0.0.1:1/anthropic/v1/messages")
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01")
            .body(br#"{"model":"anthropic.claude-opus-5"}"#.to_vec())
            .build()?;
        let creds = StaticCredentials::new("AKIDTEST", "secret", Some("tok".into())).0;
        sign_request(
            &mut request,
            AwsDoor::Bedrock,
            "us-east-1",
            &creds,
            SystemTime::UNIX_EPOCH,
        )?;
        let h = |n: &str| {
            request
                .headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        assert_eq!(h("x-amz-date"), "19700101T000000Z");
        assert_eq!(h("x-amz-security-token"), "tok");
        let auth = h("authorization");
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/19700101/us-east-1/bedrock-mantle/aws4_request, SignedHeaders="), "{auth}");
        let signed = auth
            .split("SignedHeaders=")
            .nth(1)
            .and_then(|s| s.split(',').next())
            .unwrap_or_default();
        assert_eq!(
            signed, "anthropic-version;content-type;host;x-amz-date;x-amz-security-token",
            "{auth}"
        );
        let signature = auth.split("Signature=").nth(1).unwrap_or_default();
        assert!(
            signature.len() == 64 && signature.chars().all(|c| c.is_ascii_hexdigit()),
            "{auth}"
        );
        assert!(
            request
                .headers()
                .get("authorization")
                .is_some_and(HeaderValue::is_sensitive)
        );
        assert!(
            request
                .headers()
                .get("x-amz-security-token")
                .is_some_and(HeaderValue::is_sensitive)
        );

        // Long-term keys: no session token header, none in the signed set.
        let mut request = http
            .post("http://127.0.0.1:1/v1/messages")
            .body(Vec::new())
            .build()?;
        let creds = StaticCredentials::new("AKIDTEST", "secret", None).0;
        sign_request(
            &mut request,
            AwsDoor::ClaudePlatform,
            "us-west-2",
            &creds,
            SystemTime::UNIX_EPOCH,
        )?;
        assert!(request.headers().get("x-amz-security-token").is_none());
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            auth.contains(
                "/us-west-2/aws-external-anthropic/aws4_request, SignedHeaders=host;x-amz-date,"
            ),
            "{auth}"
        );
        Ok(())
    }
}
