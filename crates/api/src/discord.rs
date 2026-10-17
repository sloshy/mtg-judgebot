//! The route for Discord's interactions endpoint (D30), mounted when the
//! Discord role runs with `DISCORD_INTERACTIONS=http`.
//! [`judge_bot::discord::endpoint`] verifies and dispatches; this only moves
//! the request's headers and raw body in and the answer out. It sits outside
//! the per-IP rate limit: every request it acts on is signed by Discord, and
//! the per-user `/judge` window applies as over the gateway.

use std::sync::Arc;

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use judge_bot::discord::endpoint::{self, Interactions};

use crate::http::Probe;

/// `POST` [`endpoint::PATH`], answered by `interactions`.
pub fn router(interactions: Interactions) -> Router {
    Router::new()
        .route(endpoint::PATH, post(receive))
        .with_state(interactions)
}

async fn receive(
    State(interactions): State<Interactions>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let got = interactions
        .receive(
            text(endpoint::SIGNATURE_HEADER),
            text(endpoint::TIMESTAMP_HEADER),
            &body,
        )
        .await;
    let status = StatusCode::from_u16(got.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    match got.body() {
        Some(json) => (status, [(header::CONTENT_TYPE, "application/json")], json).into_response(),
        None => status.into_response(),
    }
}

/// Ready once the endpoint is open: until then a request to it gets `503`,
/// so `/api/health` says so too, and a deploy does not stop the old
/// container for one that cannot verify requests yet.
pub struct Open(pub Interactions);

#[async_trait::async_trait]
impl Probe for Open {
    async fn probe(&self) -> Result<(), String> {
        if self.0.is_open() {
            Ok(())
        } else {
            Err("discord: the interactions endpoint is not open yet (reading the application's details)".to_owned())
        }
    }
}

/// Every probe, in order; the first failure is the answer.
pub struct All(pub Vec<Arc<dyn Probe>>);

#[async_trait::async_trait]
impl Probe for All {
    async fn probe(&self) -> Result<(), String> {
        for p in &self.0 {
            p.probe().await?;
        }
        Ok(())
    }
}

/// The routes of a process whose only HTTP role is the interactions
/// endpoint: it, and `GET /api/health` running `probe`.
pub fn alone(interactions: Interactions, probe: Arc<dyn Probe>) -> Router {
    router(interactions).merge(
        Router::new()
            .route("/api/health", get(health))
            .with_state(probe),
    )
}

async fn health(State(probe): State<Arc<dyn Probe>>) -> (StatusCode, String) {
    match probe.probe().await {
        Ok(()) => (StatusCode::OK, "ok".to_owned()),
        Err(why) => {
            tracing::warn!(%why, "health probe failed");
            (StatusCode::SERVICE_UNAVAILABLE, why)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt as _;

    type Res = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn a_closed_endpoint_answers_503_and_fails_the_health_check() -> Res {
        let interactions = Interactions::new();
        let app = alone(
            interactions.clone(),
            Arc::new(All(vec![Arc::new(Open(interactions))])),
        );
        let post = Request::post(endpoint::PATH).body(Body::from("{}"))?;
        let got = app.clone().oneshot(post).await?;
        assert_eq!(got.status(), StatusCode::SERVICE_UNAVAILABLE);
        let health = Request::get("/api/health").body(Body::empty())?;
        let got = app.oneshot(health).await?;
        assert_eq!(got.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(got.into_body(), 1024).await?;
        assert!(String::from_utf8_lossy(&body).contains("interactions endpoint"));
        Ok(())
    }
}
