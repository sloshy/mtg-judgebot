//! The MCP transport at `/mcp`, behind a bearer token.
//!
//! `judge-agent`'s Streamable HTTP service is mounted as a sub-router under
//! one middleware: every request must carry `Authorization: Bearer
//! <MCP_TOKEN>`, compared in constant time, or it gets `401` before the
//! protocol sees it. The tools behind it include `judge`, which spends the
//! operator's Anthropic budget, and sessions, which are otherwise open to
//! anyone who can reach the socket — so the endpoint only exists when a
//! token is configured, and there is no anonymous mode.
//!
//! The per-IP rate limiter of `/api/judge` is not applied here: the token
//! *is* the identity, and the judge concurrency semaphore and the spend cap
//! are shared with the web route through the same [`crate::App`].

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use tower_service::Service;

/// The token, kept as bytes for the comparison.
#[derive(Clone)]
struct Token(std::sync::Arc<[u8]>);

/// A router serving `service` under `/mcp`, gated by `token`.
///
/// Generic over the service so the gate can be tested with a stub; in
/// production it is `judge_agent::mcp::http_service`.
pub fn router<S>(service: S, token: &str) -> Router
where
    S: Service<Request<Body>, Error = std::convert::Infallible> + Clone + Send + Sync + 'static,
    S::Response: IntoResponse,
    S::Future: Send + 'static,
{
    let token = Token(token.as_bytes().into());
    Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(token, require_bearer))
}

async fn require_bearer(State(token): State<Token>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(p) if constant_time_eq(p.as_bytes(), &token.0) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"mcp\"")],
            "missing or invalid bearer token",
        )
            .into_response(),
    }
}

/// Equal without an early exit on the first differing byte (`subtle`, so the
/// compiler cannot vectorise an exit back in). The length is not hidden,
/// which is fine: it is fixed by the operator's token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt as _;

    /// Stands in for the MCP service: answers 200 with the path it saw.
    #[derive(Clone)]
    pub(crate) struct Echo;

    impl Service<Request<Body>> for Echo {
        type Response = Response;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Response, Self::Error>>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Request<Body>) -> Self::Future {
            std::future::ready(Ok(
                (StatusCode::OK, req.uri().path().to_owned()).into_response()
            ))
        }
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    async fn status(
        auth: Option<&str>,
        path: &str,
    ) -> Result<StatusCode, Box<dyn std::error::Error>> {
        let mut req = HttpRequest::post(path).body(Body::empty())?;
        if let Some(a) = auth {
            req.headers_mut().insert(header::AUTHORIZATION, a.parse()?);
        }
        Ok(router(Echo, TOKEN).oneshot(req).await?.status())
    }

    #[tokio::test]
    async fn only_the_exact_bearer_token_passes() -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(status(None, "/mcp").await?, StatusCode::UNAUTHORIZED);
        assert_eq!(
            status(Some("Bearer nope"), "/mcp").await?,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(Some(&format!("Bearer {TOKEN}x")), "/mcp").await?,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(Some(&format!("Basic {TOKEN}")), "/mcp").await?,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status(Some(&format!("Bearer {TOKEN}")), "/mcp").await?,
            StatusCode::OK
        );
        assert_eq!(
            status(Some(&format!("Bearer {TOKEN}")), "/mcp/anything").await?,
            StatusCode::OK
        );
        Ok(())
    }

    #[test]
    fn constant_time_eq_is_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }
}
