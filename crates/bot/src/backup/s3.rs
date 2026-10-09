//! The four S3 operations a backup needs (list, put, get, delete), signed
//! with `SigV4` and sent with the reqwest client every other adapter uses.
//!
//! Cloudflare R2 speaks the S3 API: region `auto`, path-style URLs
//! (`https://<account>.r2.cloudflarestorage.com/<bucket>/<key>`). Any other
//! S3-compatible store (`MinIO`, a local stand-in for tests) takes the same
//! requests. An upload is one `PUT` of the dump file, streamed from disk,
//! with its SHA-256 in `x-amz-content-sha256`: the store verifies the bytes
//! it received against the signed hash and refuses a corrupted upload.
//!
//! Each request is tried up to [`ATTEMPTS`] times when the store does not
//! answer or answers 5xx or 429; any other refusal is final. Nothing here
//! logs a credential: the signature header is marked sensitive and the
//! errors carry the store's code and message, never a header.

use std::{
    fmt,
    path::Path,
    time::{Duration, SystemTime},
};

use aws_credential_types::Credentials;
use aws_sigv4::{
    http_request::{
        PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest, SigningSettings,
        UriPathNormalizationMode, sign,
    },
    sign::v4,
};
use reqwest::{
    Method, StatusCode,
    header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue},
};
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use super::settings::{Prefix, Store};

/// The signing region R2 documents; S3-compatible stand-ins accept it too.
pub const REGION: &str = "auto";
/// The `SigV4` service name of the S3 API.
const SERVICE: &str = "s3";
/// The largest dump one `PUT` may carry. S3 and R2 take up to 5 GiB in a
/// single request (R2 a little less); a judgebot dump is tens of MB, so a
/// dump this size means something else is wrong.
pub const MAX_PUT_BYTES: u64 = 5_000_000_000;
/// Tries per request.
pub const ATTEMPTS: u32 = 3;
/// The wait before the second try; the third waits four times as long.
const RETRY_WAIT: Duration = Duration::from_secs(2);
/// A listing longer than this many pages (of up to 1000 keys) is refused
/// rather than followed forever.
const MAX_PAGES: usize = 1000;
/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the store may go silent while answering.
const READ_TIMEOUT: Duration = Duration::from_mins(2);
/// The whole of a list or delete request.
const SHORT_TIMEOUT: Duration = Duration::from_mins(1);
/// The whole of an upload or download.
const TRANSFER_TIMEOUT: Duration = Duration::from_hours(1);

/// Which operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `ListObjectsV2`.
    List,
    /// `PutObject`.
    Put,
    /// `GetObject`.
    Get,
    /// `DeleteObject`.
    Delete,
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::List => "listing the backups",
            Self::Put => "uploading the dump",
            Self::Get => "downloading the backup",
            Self::Delete => "deleting an old backup",
        })
    }
}

/// A failed request.
#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    /// No answer: the connection failed or timed out.
    #[error("{op}: {source}")]
    Request {
        /// The operation.
        op: Op,
        /// What reqwest said, without the URL.
        source: reqwest::Error,
    },
    /// The store refused, with its error code and message when it sent them
    /// (`NoSuchKey`, `SignatureDoesNotMatch`, `AccessDenied`).
    #[error("{op}: HTTP {status}{}{}", code.as_ref().map(|c| format!(" {c}")).unwrap_or_default(), message.as_ref().map(|m| format!(": {m}")).unwrap_or_default())]
    Status {
        /// The operation.
        op: Op,
        /// The HTTP status.
        status: u16,
        /// The S3 error code.
        code: Option<String>,
        /// The S3 error message.
        message: Option<String>,
    },
    /// An answer this could not read.
    #[error("{op}: {why}")]
    Response {
        /// The operation.
        op: Op,
        /// What was wrong with it.
        why: String,
    },
    /// The request could not be signed.
    #[error("signing the request ({op}): {why}")]
    Sign {
        /// The operation.
        op: Op,
        /// The signer's reason.
        why: String,
    },
    /// A local file could not be read or the output written.
    #[error("{op}: {source}")]
    Io {
        /// The operation.
        op: Op,
        /// The I/O error.
        source: std::io::Error,
    },
}

impl S3Error {
    /// Whether another try may succeed: no answer, or 5xx or 429.
    fn transient(&self) -> bool {
        match self {
            Self::Request { .. } => true,
            Self::Status { status, .. } => *status >= 500 || *status == 429,
            Self::Response { .. } | Self::Sign { .. } | Self::Io { .. } => false,
        }
    }

    fn request(op: Op, e: reqwest::Error) -> Self {
        Self::Request {
            op,
            source: e.without_url(),
        }
    }
}

/// One object directly under the prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// Its name: the key without the prefix and its slash.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
}

/// What a listing found directly under the prefix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The objects.
    pub objects: Vec<Object>,
    /// The "directories" (key prefixes one level down), each ending in `/`.
    pub folders: Vec<String>,
}

/// One page of a `ListObjectsV2` answer.
#[derive(Debug, Default, PartialEq, Eq)]
struct Page {
    listing: Listing,
    /// `NextContinuationToken` when `IsTruncated` is true.
    next: Option<String>,
    truncated: bool,
}

/// The body to sign.
enum Payload {
    /// No body.
    Empty,
    /// A body whose lowercase hex SHA-256 is this.
    Sha256(String),
}

/// A file to upload as a request's body.
#[derive(Clone, Copy)]
struct FileBody<'a> {
    path: &'a Path,
    len: u64,
    /// Its lowercase hex SHA-256.
    sha256: &'a str,
}

/// One request, as each try builds it afresh.
struct Want<'a> {
    op: Op,
    method: Method,
    url: url::Url,
    timeout: Duration,
    body: Option<FileBody<'a>>,
}

impl Want<'_> {
    const fn bare(op: Op, method: Method, url: url::Url, timeout: Duration) -> Self {
        Self {
            op,
            method,
            url,
            timeout,
            body: None,
        }
    }
}

/// A client for one bucket.
pub struct Client {
    http: reqwest::Client,
    /// `<endpoint>/<bucket>`, without a trailing slash.
    bucket_url: url::Url,
    credentials: Credentials,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("bucket_url", &self.bucket_url.as_str())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for the store's bucket.
    ///
    /// # Errors
    /// The HTTP client cannot be built (no TLS backend).
    pub fn new(store: &Store) -> Result<Self, S3Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| S3Error::request(Op::List, e))?;
        let mut bucket_url = store.endpoint.url().clone();
        bucket_url.set_path(store.bucket.as_str());
        Ok(Self {
            http,
            bucket_url,
            credentials: Credentials::new(
                store.access_key_id.expose(),
                store.secret_access_key.expose(),
                None,
                None,
                "judgebot-backup",
            ),
        })
    }

    /// The URL of `key`. Keys are built from plain segments
    /// ([`super::settings::is_plain_segment`]), which need no encoding.
    fn object_url(&self, key: &str) -> url::Url {
        let mut url = self.bucket_url.clone();
        url.set_path(&format!("{}/{key}", self.bucket_url.path()));
        url
    }

    /// Sign `request` in place: `x-amz-date`, `x-amz-content-sha256` and
    /// `authorization`, over every header already on it.
    fn sign(
        &self,
        op: Op,
        request: &mut reqwest::Request,
        payload: &Payload,
    ) -> Result<(), S3Error> {
        let sign_err = |why: String| S3Error::Sign { op, why };
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| value.to_str().map(|v| (name.as_str(), v)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| sign_err(format!("header is not ASCII: {e}")))?;
        let body = match payload {
            Payload::Empty => SignableBody::Bytes(&[]),
            Payload::Sha256(hex) => SignableBody::Precomputed(hex.clone()),
        };
        let signable = SignableRequest::new(
            request.method().as_str(),
            request.url().as_str(),
            headers.into_iter(),
            body,
        )
        .map_err(|e| sign_err(e.to_string()))?;
        // S3 signs the path as sent (no second encoding, no normalisation)
        // and requires the payload hash header.
        let mut settings = SigningSettings::default();
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        let identity = self.credentials.clone().into();
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(REGION)
            .name(SERVICE)
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| sign_err(e.to_string()))?
            .into();
        let (instructions, _signature) = sign(signable, &params)
            .map_err(|e| sign_err(e.to_string()))?
            .into_parts();
        let (signed, _query) = instructions.into_parts();
        for header in signed {
            let name = HeaderName::from_bytes(header.name().as_bytes())
                .map_err(|e| sign_err(e.to_string()))?;
            let mut value =
                HeaderValue::from_str(header.value()).map_err(|e| sign_err(e.to_string()))?;
            value.set_sensitive(header.sensitive() || name == reqwest::header::AUTHORIZATION);
            request.headers_mut().insert(name, value);
        }
        Ok(())
    }

    /// Build and sign a fresh request for `want`: each try needs its own,
    /// because the signature carries the time and a file body is read once.
    async fn build(&self, want: &Want<'_>) -> Result<reqwest::Request, S3Error> {
        let op = want.op;
        let builder = self
            .http
            .request(want.method.clone(), want.url.clone())
            .timeout(want.timeout);
        let Some(FileBody { path, len, sha256 }) = want.body else {
            let mut request = builder.build().map_err(|e| S3Error::request(op, e))?;
            self.sign(op, &mut request, &Payload::Empty)?;
            return Ok(request);
        };
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|source| S3Error::Io { op, source })?;
        let mut request = builder
            .header(CONTENT_TYPE, "application/gzip")
            .body(reqwest::Body::from(file))
            .build()
            .map_err(|e| S3Error::request(op, e))?;
        self.sign(op, &mut request, &Payload::Sha256(sha256.to_owned()))?;
        // After signing: a streamed body has no length of its own, and S3
        // refuses a chunked upload, so the length goes in the header.
        request
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from(len));
        Ok(request)
    }

    /// Send `want`, trying again on a transient failure. A refusal is read
    /// into [`S3Error::Status`].
    async fn send(&self, want: &Want<'_>) -> Result<reqwest::Response, S3Error> {
        let op = want.op;
        let mut attempt = 1;
        loop {
            let e = match self.http.execute(self.build(want).await?).await {
                Ok(r) if r.status().is_success() => return Ok(r),
                Ok(r) => refusal(op, r).await,
                Err(e) => S3Error::request(op, e),
            };
            if !e.transient() || attempt >= ATTEMPTS {
                return Err(e);
            }
            let delay = RETRY_WAIT.saturating_mul(4_u32.saturating_pow(attempt - 1));
            tracing::warn!(error = %e, attempt, wait_secs = delay.as_secs(), "object store request failed; trying again");
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// Everything directly under `prefix/`, every page of it.
    ///
    /// # Errors
    /// A request that failed, or an answer that is not a listing.
    pub async fn list(&self, prefix: &Prefix) -> Result<Listing, S3Error> {
        let op = Op::List;
        let mut out = Listing::default();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = self.bucket_url.clone();
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("list-type", "2");
                q.append_pair("prefix", &format!("{prefix}/"));
                q.append_pair("delimiter", "/");
                if let Some(t) = &token {
                    q.append_pair("continuation-token", t);
                }
            }
            let response = self
                .send(&Want::bare(op, Method::GET, url, SHORT_TIMEOUT))
                .await?;
            let body = response.text().await.map_err(|e| S3Error::request(op, e))?;
            let page = parse_list(&body, prefix).map_err(|why| S3Error::Response { op, why })?;
            out.objects.extend(page.listing.objects);
            out.folders.extend(page.listing.folders);
            match (page.truncated, page.next) {
                (false, _) => return Ok(out),
                (true, Some(next)) if token.as_ref() != Some(&next) => token = Some(next),
                (true, _) => {
                    return Err(S3Error::Response {
                        op,
                        why: "a truncated listing without a new continuation token".to_owned(),
                    });
                }
            }
        }
        Err(S3Error::Response {
            op,
            why: format!("more than {MAX_PAGES} pages of objects under the prefix"),
        })
    }

    /// Upload the file at `path` (`len` bytes, whose lowercase hex SHA-256
    /// is `sha256`) as `key`.
    ///
    /// # Errors
    /// The file is larger than [`MAX_PUT_BYTES`] or cannot be opened, or the
    /// store refused it (a hash mismatch included).
    pub async fn put_file(
        &self,
        key: &str,
        path: &Path,
        len: u64,
        sha256: &str,
    ) -> Result<(), S3Error> {
        let op = Op::Put;
        if len > MAX_PUT_BYTES {
            return Err(S3Error::Response {
                op,
                why: format!(
                    "the dump is {len} bytes, more than one upload takes ({MAX_PUT_BYTES})"
                ),
            });
        }
        let want = Want {
            body: Some(FileBody { path, len, sha256 }),
            ..Want::bare(op, Method::PUT, self.object_url(key), TRANSFER_TIMEOUT)
        };
        self.send(&want).await.map(drop)
    }

    /// Download `key` into `out`; the number of bytes written.
    ///
    /// # Errors
    /// The store refused (`NoSuchKey` for a name that is not there), the
    /// download broke off, or `out` could not be written.
    pub async fn get_to<W>(&self, key: &str, out: &mut W) -> Result<u64, S3Error>
    where
        W: AsyncWrite + Unpin,
    {
        let op = Op::Get;
        let mut response = self
            .send(&Want::bare(
                op,
                Method::GET,
                self.object_url(key),
                TRANSFER_TIMEOUT,
            ))
            .await?;
        let expected = response.content_length();
        let mut written: u64 = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| S3Error::request(op, e))?
        {
            out.write_all(&chunk)
                .await
                .map_err(|source| S3Error::Io { op, source })?;
            written += u64::try_from(chunk.len()).unwrap_or(u64::MAX);
        }
        out.flush()
            .await
            .map_err(|source| S3Error::Io { op, source })?;
        match expected {
            Some(n) if n != written => Err(S3Error::Response {
                op,
                why: format!("received {written} bytes of {n}"),
            }),
            _ => Ok(written),
        }
    }

    /// Delete `key`.
    ///
    /// # Errors
    /// The store refused.
    pub async fn delete(&self, key: &str) -> Result<(), S3Error> {
        let op = Op::Delete;
        self.send(&Want::bare(
            op,
            Method::DELETE,
            self.object_url(key),
            SHORT_TIMEOUT,
        ))
        .await
        .map(drop)
    }
}

/// A non-2xx answer as an error, with the S3 `Code` and `Message` when the
/// body carries them.
async fn refusal(op: Op, response: reqwest::Response) -> S3Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let (code, message) = parse_error(&body);
    S3Error::Status {
        op,
        status: status.as_u16(),
        code: code.or_else(|| (status == StatusCode::NOT_FOUND).then(|| "NoSuchKey".to_owned())),
        message,
    }
}

/// The text of `node`'s child element `name`.
fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .and_then(|c| c.text())
}

/// `<Error><Code>…</Code><Message>…</Message></Error>`, either part absent
/// when the body is not that.
fn parse_error(body: &str) -> (Option<String>, Option<String>) {
    let Ok(doc) = roxmltree::Document::parse(body) else {
        return (None, None);
    };
    let root = doc.root_element();
    if root.tag_name().name() != "Error" {
        return (None, None);
    }
    let text = |name| child_text(root, name).map(|s| s.chars().take(300).collect::<String>());
    (text("Code"), text("Message"))
}

/// One page of `ListObjectsV2` (`ListBucketResult`), its keys made relative
/// to `prefix/`.
fn parse_list(body: &str, prefix: &Prefix) -> Result<Page, String> {
    let doc = roxmltree::Document::parse(body).map_err(|e| format!("not XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(format!(
            "expected ListBucketResult, got {}",
            root.tag_name().name()
        ));
    }
    let under = format!("{prefix}/");
    let relative = |key: &str| {
        key.strip_prefix(&under)
            .filter(|rest| !rest.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("a key outside the prefix: {key}"))
    };
    let mut page = Page::default();
    for node in root.children().filter(roxmltree::Node::is_element) {
        match node.tag_name().name() {
            "Contents" => {
                let key = child_text(node, "Key").ok_or("an object without a Key")?;
                // The zero-byte "folder" a console creates for the prefix
                // itself is not an object under it.
                if key == under {
                    continue;
                }
                let size = child_text(node, "Size")
                    .and_then(|s| s.trim().parse().ok())
                    .ok_or_else(|| format!("no Size for {key}"))?;
                page.listing.objects.push(Object {
                    name: relative(key)?,
                    size,
                });
            }
            "CommonPrefixes" => {
                if let Some(p) = child_text(node, "Prefix") {
                    page.listing.folders.push(relative(p)?);
                }
            }
            "IsTruncated" => page.truncated = node.text().is_some_and(|t| t.trim() == "true"),
            "NextContinuationToken" => page.next = node.text().map(str::to_owned),
            _ => {}
        }
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use judge_llm::ApiKey;
    use sha2::{Digest as _, Sha256};
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{header, method, path, query_param, query_param_is_missing},
    };

    use super::*;
    use crate::backup::settings::{Bucket, Endpoint};

    type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    fn store(uri: &str) -> Result<Store> {
        Ok(Store {
            endpoint: Endpoint::parse(uri)?,
            bucket: Bucket::parse("judgebot-backups")?,
            prefix: Prefix::parse(Some("db"))?,
            access_key_id: ApiKey::from("AKIDEXAMPLE"),
            secret_access_key: ApiKey::from("very-secret"),
        })
    }

    fn page(keys: &[(&str, u64)], next: Option<&str>) -> String {
        let contents = keys
            .iter()
            .map(|(k, s)| {
                format!(
                    "<Contents><Key>{k}</Key><LastModified>2026-10-09T04:15:00.000Z</LastModified>\
                     <ETag>\"x\"</ETag><Size>{s}</Size><StorageClass>STANDARD</StorageClass></Contents>"
                )
            })
            .collect::<Vec<_>>()
            .concat();
        let tail = next.map_or("<IsTruncated>false</IsTruncated>".to_owned(), |t| {
            format!(
                "<IsTruncated>true</IsTruncated><NextContinuationToken>{t}</NextContinuationToken>"
            )
        });
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>judgebot-backups</Name><Prefix>db/</Prefix><Delimiter>/</Delimiter>\
             {contents}<CommonPrefixes><Prefix>db/old/</Prefix></CommonPrefixes>{tail}</ListBucketResult>"
        )
    }

    fn signed_for_r2(r: &Request) -> bool {
        r.headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| {
                a.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
                    && a.contains("/auto/s3/aws4_request")
                    && a.contains("x-amz-content-sha256")
                    && !a.contains("very-secret")
            })
    }

    #[tokio::test]
    async fn a_listing_follows_every_page_under_the_prefix() -> Result {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/judgebot-backups"))
            .and(query_param("list-type", "2"))
            .and(query_param("prefix", "db/"))
            .and(query_param("delimiter", "/"))
            .and(query_param_is_missing("continuation-token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(page(
                &[
                    ("db/", 0),
                    ("db/judgebot-20261002T041500Z.dump.gz", 30_000_000),
                ],
                Some("tok+/="),
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/judgebot-backups"))
            .and(query_param("continuation-token", "tok+/="))
            .respond_with(ResponseTemplate::new(200).set_body_string(page(
                &[
                    ("db/judgebot-20261009T041500Z.dump.gz", 31_000_000),
                    ("db/a&amp;b.txt", 3),
                ],
                None,
            )))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(&store(&server.uri())?)?;
        let listing = client.list(&Prefix::parse(Some("db"))?).await?;
        let names: Vec<&str> = listing.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "judgebot-20261002T041500Z.dump.gz",
                "judgebot-20261009T041500Z.dump.gz",
                "a&b.txt"
            ]
        );
        assert_eq!(listing.folders, ["old/", "old/"]);
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests.iter().all(signed_for_r2), "{requests:?}");
        Ok(())
    }

    #[tokio::test]
    async fn an_upload_streams_the_file_with_its_signed_hash_and_length() -> Result {
        let dir = tempdir()?;
        let file = dir.join("judgebot-20261009T041500Z.dump.gz");
        let body = b"not really a dump".repeat(1000);
        std::fs::write(&file, &body)?;
        let sha = crate::backup::dump::hex(&Sha256::digest(&body));
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path(
                "/judgebot-backups/db/judgebot-20261009T041500Z.dump.gz",
            ))
            .and(header("x-amz-content-sha256", sha.as_str()))
            .and(header("content-type", "application/gzip"))
            .and(header("content-length", body.len().to_string().as_str()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(&store(&server.uri())?)?;
        let len = u64::try_from(body.len())?;
        client
            .put_file("db/judgebot-20261009T041500Z.dump.gz", &file, len, &sha)
            .await?;
        let requests = server.received_requests().await.unwrap_or_default();
        assert!(requests.iter().all(|r| signed_for_r2(r) && r.body == body));
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn a_refusal_names_the_s3_code_and_is_not_retried() -> Result {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                "<?xml version=\"1.0\"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
            ))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(&store(&server.uri())?)?;
        let err = client.delete("db/x.dump.gz").await.err().ok_or("deleted")?;
        let text = err.to_string();
        assert!(
            text.contains("403") && text.contains("AccessDenied") && text.contains("Access Denied"),
            "{text}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_server_error_is_tried_again() -> Result {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(&store(&server.uri())?)?;
        client.delete("db/x.dump.gz").await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_download_writes_the_body_and_a_missing_one_says_so() -> Result {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(
                "/judgebot-backups/db/judgebot-20261009T041500Z.dump.gz",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"gzip bytes".to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/judgebot-backups/db/missing.dump.gz"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = Client::new(&store(&server.uri())?)?;
        let mut out = Vec::new();
        let n = client
            .get_to("db/judgebot-20261009T041500Z.dump.gz", &mut out)
            .await?;
        assert_eq!((n, out.as_slice()), (10, &b"gzip bytes"[..]));
        let err = client
            .get_to("db/missing.dump.gz", &mut Vec::new())
            .await
            .err()
            .ok_or("found")?;
        assert!(err.to_string().contains("NoSuchKey"), "{err}");
        Ok(())
    }

    #[test]
    fn a_listing_that_is_not_one_is_refused() -> Result {
        let prefix = Prefix::parse(Some("db"))?;
        assert!(parse_list("<html>", &prefix).is_err());
        assert!(parse_list("<Error><Code>X</Code></Error>", &prefix).is_err());
        let marker = parse_list(&page(&[("db/", 0)], None), &prefix)?;
        assert!(
            marker.listing.objects.is_empty(),
            "the folder marker is skipped"
        );
        let outside = page(&[("other/judgebot-20261009T041500Z.dump.gz", 1)], None);
        assert!(parse_list(&outside, &prefix).is_err());
        assert_eq!(
            parse_error("<Error><Code>NoSuchBucket</Code></Error>"),
            (Some("NoSuchBucket".to_owned()), None)
        );
        Ok(())
    }

    /// A fresh directory under the system temp dir.
    fn tempdir() -> Result<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("judgebot-s3-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}
