//! The MCP transport at `/mcp`, behind a bearer token.
//!
//! `judge-agent`'s Streamable HTTP service is mounted behind one gate: every
//! request must carry `Authorization: Bearer <MCP_TOKEN>`, compared in
//! constant time, or it gets `401` before the protocol sees it. With
//! `MCP_ADMIN_TOKEN` set, a request carrying that token instead reaches a
//! second service whose handler also has the admin tools (D37); the one
//! `MCP_TOKEN` reaches has none, by type (`judge_agent::mcp::Public`). The tools behind it include `judge`, which spends the
//! operator's Anthropic budget, and sessions, which are otherwise open to
//! anyone who can reach the socket — so the endpoint only exists when a
//! token is configured, and there is no anonymous mode.
//!
//! The per-IP rate limiter of `/api/judge` is not applied here: the token
//! *is* the identity, and the judge concurrency semaphore and the spend cap
//! are shared with the web route through the same [`crate::App`].

use std::{convert::Infallible, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::Request,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use judge_agent::mcp::{AdminService, PublicHttp};
use tower::ServiceExt as _;
use tower_service::Service;

use crate::auth::{constant_time_eq, presented_bearer};

/// What a request's bearer token opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grant {
    /// `MCP_TOKEN`: the judge, session and lookup tools.
    Judge,
    /// `MCP_ADMIN_TOKEN`: those and the admin tools.
    Admin,
}

/// The two services and the tokens that open them.
#[derive(Clone)]
struct Gate<S> {
    judge_token: Arc<[u8]>,
    public: S,
    /// The admin service, behind its own token; `None` without
    /// `MCP_ADMIN_TOKEN`.
    admin: Option<(Arc<[u8]>, judge_agent::mcp::AdminHttp)>,
}

impl<S> Gate<S> {
    /// Which service `presented` opens, if any. Both comparisons run in
    /// constant time; the two tokens are checked to differ at load, so at
    /// most one matches.
    fn grant(&self, presented: Option<&str>) -> Option<Grant> {
        let p = presented?.as_bytes();
        if self
            .admin
            .as_ref()
            .is_some_and(|(token, _)| constant_time_eq(p, token))
        {
            Some(Grant::Admin)
        } else if constant_time_eq(p, &self.judge_token) {
            Some(Grant::Judge)
        } else {
            None
        }
    }
}

/// A router serving `service` (the public tools, `JudgeMcp<Public>`, by
/// type) under `/mcp` to requests that carry `token` (`MCP_TOKEN`), and
/// `admin`'s service (the admin tools too) to those that carry its own token,
/// `MCP_ADMIN_TOKEN`. Any other request gets `401` before the protocol sees
/// it.
pub fn router(service: PublicHttp, token: &str, admin: Option<&AdminService>) -> Router {
    gated(service, token, admin)
}

/// [`router`] over any public service: crate-private, so only this crate's
/// tests put a stub there, and every other caller hands [`router`] the
/// public handler's service.
pub(crate) fn gated<S>(service: S, token: &str, admin: Option<&AdminService>) -> Router
where
    S: Service<Request<Body>, Error = Infallible> + Clone + Send + Sync + 'static,
    S::Response: IntoResponse,
    S::Future: Send + 'static,
{
    let gate = Gate {
        judge_token: token.as_bytes().into(),
        public: service,
        admin: admin.map(|a| (a.token().as_bytes().into(), a.service())),
    };
    // One service for `/mcp` and everything under it, so the gate never
    // becomes this router's fallback: merged into a router with no web
    // page, an unrouted path stays a 404 rather than a 401.
    Router::new().nest_service(
        "/mcp",
        tower::service_fn(move |req: Request| {
            let gate = gate.clone();
            async move { Ok::<_, Infallible>(serve(gate, req).await) }
        }),
    )
}

async fn serve<S>(gate: Gate<S>, req: Request) -> Response
where
    S: Service<Request<Body>, Error = Infallible> + Clone + Send + Sync + 'static,
    S::Response: IntoResponse,
    S::Future: Send + 'static,
{
    match gate.grant(presented_bearer(req.headers())) {
        Some(Grant::Judge) => match gate.public.oneshot(req).await {
            Ok(r) => r.into_response(),
            Err(never) => match never {},
        },
        Some(Grant::Admin) => match gate.admin {
            Some((_, admin)) => match admin.oneshot(req).await {
                Ok(r) => r.into_response(),
                Err(never) => match never {},
            },
            // `grant` names the admin service only when there is one.
            None => unauthorized(),
        },
        None => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"mcp\"")],
        "missing or invalid bearer token",
    )
        .into_response()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;

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
        Ok(gated(Echo, TOKEN, None).oneshot(req).await?.status())
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
        assert_eq!(
            status(Some(&format!("bearer {TOKEN}")), "/mcp").await?,
            StatusCode::OK,
            "the scheme is case-insensitive"
        );
        Ok(())
    }

    /// Merging this router must not hand its 401 to every unrouted path: with
    /// no web page there is no other fallback, and `layer` would have made the
    /// gate the catch-all, so `GET /` on a `--api --mcp` process answered 401
    /// (advertising `WWW-Authenticate` to any scanner) instead of 404.
    #[tokio::test]
    async fn the_gate_does_not_become_the_fallback() -> Result<(), Box<dyn std::error::Error>> {
        let merged = Router::new().merge(gated(Echo, TOKEN, None));
        for path in ["/", "/anything", "/api/judge"] {
            let res = merged
                .clone()
                .oneshot(HttpRequest::get(path).body(Body::empty())?)
                .await?;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{path}");
            assert!(
                !res.headers().contains_key(header::WWW_AUTHENTICATE),
                "{path} advertised the MCP gate"
            );
        }
        Ok(())
    }
}

/// The real services behind the gate: what each token lists and may call.
#[cfg(test)]
mod gate_tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use judge_agent::{Client, Options, Toolbox, Transport};
    use judge_bot::admin::McpAdminToken;

    const JUDGE: &str = "0123456789abcdef0123456789abcdef";
    const ADMIN: &str = "fedcba9876543210fedcba9876543210";
    const META: &str = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"},"io.modelcontextprotocol/clientCapabilities":{}}"#;

    fn mcp_router() -> Result<Router, Box<dyn std::error::Error>> {
        // Listing tools touches no database; a call that reaches one fails
        // to connect, which is still past the gate.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")?;
        let toolbox = Arc::new(Toolbox::new(
            pool,
            Options {
                transport: Transport::Mcp,
                client: Client::instance(judge_bot::maintenance::Availability::open()),
                models: None,
                deps_config: judge_bot::DepsConfig::default(),
                vectors: None,
                permits: Arc::new(tokio::sync::Semaphore::new(1)),
                judge_quota: None,
                history_len: 5,
                offer: judge_core::SourceOffer::upstream(judge_core::Commit::Unknown),
                operator: judge_core::Operator::default(),
            },
        ));
        let token = McpAdminToken::from_vars(|k| {
            match k {
                "MCP_ADMIN_TOKEN" => Some(ADMIN),
                "MCP_TOKEN" => Some(JUDGE),
                _ => None,
            }
            .map(str::to_owned)
        })?
        .ok_or("token")?;
        let public = judge_agent::mcp::http_service(Arc::clone(&toolbox), vec![]);
        let admin = judge_agent::mcp::admin_http_service(toolbox, vec![], token);
        Ok(router(public, JUDGE, Some(&admin)))
    }

    async fn rpc(
        router: &Router,
        token: &str,
        method: &str,
        tool: Option<&str>,
    ) -> Result<(StatusCode, serde_json::Value), Box<dyn std::error::Error>> {
        let params = tool.map_or_else(String::new, |t| {
            format!(r#""name":"{t}","arguments":{{}},"#)
        });
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{{{params}{META}}}}}"#
        );
        let mut req = HttpRequest::post("/mcp");
        if let Some(t) = tool {
            req = req.header("Mcp-Name", t);
        }
        let req = req
            .header(header::HOST, "localhost")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method)
            .body(Body::from(body))?;
        let res = router.clone().oneshot(req).await?;
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await?;
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        ))
    }

    fn tool_names(reply: &serde_json::Value) -> Vec<String> {
        reply
            .pointer("/result/tools")
            .and_then(serde_json::Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| t.get("name").and_then(serde_json::Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `MCP_TOKEN` neither lists nor calls an admin tool; `MCP_ADMIN_TOKEN`
    /// lists them with the public ones; any other token is a 401.
    #[tokio::test]
    async fn only_the_admin_token_reaches_the_admin_tools() -> Result<(), Box<dyn std::error::Error>>
    {
        let router = mcp_router()?;
        let (status, judge) = rpc(&router, JUDGE, "tools/list", None).await?;
        assert_eq!(status, StatusCode::OK, "{judge}");
        let judge = tool_names(&judge);
        assert!(judge.contains(&"judge".to_owned()), "{judge:?}");
        assert!(
            !judge
                .iter()
                .any(|t| t.starts_with("admin_") || t.starts_with("queue_")),
            "{judge:?}"
        );

        let (status, admin) = rpc(&router, ADMIN, "tools/list", None).await?;
        assert_eq!(status, StatusCode::OK, "{admin}");
        let admin = tool_names(&admin);
        for t in [
            "admin_calls",
            "admin_call",
            "admin_failures",
            "admin_usage",
            "queue_list",
            "queue_show",
            "queue_resolve",
            "queue_dismiss",
            "judge",
        ] {
            assert!(admin.contains(&t.to_owned()), "{t} missing: {admin:?}");
        }

        // Called with MCP_TOKEN, an admin tool is unknown to its handler.
        let (_, refused) = rpc(&router, JUDGE, "tools/call", Some("admin_usage")).await?;
        assert_eq!(
            refused.pointer("/error/message"),
            Some(&serde_json::json!("tool not found")),
            "{refused}"
        );
        // With MCP_ADMIN_TOKEN it runs (and fails only at the database).
        let (_, reached) = rpc(&router, ADMIN, "tools/call", Some("admin_usage")).await?;
        assert!(reached.get("error").is_none(), "{reached}");
        assert!(
            reached
                .pointer("/result/content/0/text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t.contains("begin a report")),
            "{reached}"
        );

        let (status, _) = rpc(&router, "not-a-token-of-this-instance", "tools/list", None).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        Ok(())
    }
}
