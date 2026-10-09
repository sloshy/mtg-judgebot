//! The network roles (`--api`, `--web`, `--mcp`) as one launch: the
//! checked [`Network`], [`Network::bind`], which builds the routes over a
//! process's shared composition ([`judge_bot::serving::Serving`]) and binds
//! `API_ADDR`, and [`Listening::serve`].

use std::sync::Arc;

use anyhow::Result;
use judge_agent::{Options, Quota, Toolbox};
use judge_bot::{ingest::runs::FreshnessReader, serving::Serving, synth::Harness};
use judge_core::NetworkOperator;
use judge_llm::ApiKey;

use crate::{ApiConfig, App, Interfaces, Refused, bind, router, serve_on};

/// The HTTP interfaces a process opens, with everything they require. Made
/// only by [`Network::new`], which runs [`ApiConfig::check`], and holding the
/// [`NetworkOperator`] every interface names: a process holding one has
/// passed the network roles' requirements.
#[derive(Debug)]
pub struct Network {
    cfg: ApiConfig,
    interfaces: Interfaces,
    operator: NetworkOperator,
}

impl Network {
    /// Check `interfaces` against `cfg` and keep them together.
    ///
    /// # Errors
    /// As [`ApiConfig::check`]: every requirement unmet.
    pub fn new(
        cfg: ApiConfig,
        interfaces: Interfaces,
        operator: NetworkOperator,
    ) -> Result<Self, Refused> {
        cfg.check(&interfaces)?;
        Ok(Self {
            cfg,
            interfaces,
            operator,
        })
    }

    /// The interfaces it opens.
    #[must_use]
    pub fn interfaces(&self) -> &Interfaces {
        &self.interfaces
    }

    /// The contact every interface names.
    #[must_use]
    pub fn operator(&self) -> &NetworkOperator {
        &self.operator
    }

    /// The mismatches that cost nothing but a misconception: warnings, not
    /// refusals. Refusing would take the *page* down over an MCP mistake, and
    /// `MCP_TOKEN` is still a valid variable an operator may simply have left
    /// in place.
    #[must_use]
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut out = vec![];
        if self.cfg.mcp_token.is_some() && !self.interfaces.mcp() {
            out.push(
                "MCP_TOKEN is set but --mcp was not given, so /mcp is not served; \
                 add --mcp (JUDGE_ROLES, or API_INTERFACES for the compose api service) \
                 or unset the token",
            );
        }
        if self.interfaces.web() && !self.interfaces.api() {
            out.push(
                "--web without --api: the page is served but POST /api/judge is not, \
                 so questions asked on it fail (405 from the static file service) \
                 unless another process answers them",
            );
        }
        out
    }
}

/// The network roles with their routes built and `API_ADDR` bound, ready to
/// serve.
pub struct Listening {
    listener: tokio::net::TcpListener,
    routes: axum::Router,
}

impl Listening {
    /// Serve until the accept loop fails.
    ///
    /// # Errors
    /// A fatal accept-loop error.
    pub async fn serve(self) -> Result<()> {
        serve_on(self.listener, self.routes).await
    }
}

impl Network {
    /// Build the routes over `serving` and bind `API_ADDR`. A process binds
    /// before it starts its other roles, so a taken address fails before the
    /// Discord gateway is ever contacted.
    ///
    /// # Errors
    /// Binding `API_ADDR`.
    pub async fn bind(self, serving: &Serving) -> Result<Listening> {
        let Self {
            cfg,
            interfaces,
            operator,
        } = self;
        let routes = routes(serving, &cfg, &interfaces, &operator);
        // The enabled *and* the disabled interfaces: an operator hunting a
        // 404 reads the reason here rather than inferring it from the routes
        // that answer.
        tracing::info!(
            interfaces = %interfaces,
            off = %interfaces.disabled(),
            web_dist = interfaces.web().then(|| cfg.web_dist.display().to_string()),
            rate_limit = cfg.rate_limit,
            rate_window_secs = cfg.rate_window.as_secs(),
            concurrency = cfg.max_concurrent,
            "starting HTTP adapter"
        );
        let listener = bind(&cfg).await?;
        Ok(Listening { listener, routes })
    }
}

/// The router for `interfaces`, the MCP transport mounted when asked for.
fn routes(
    serving: &Serving,
    cfg: &ApiConfig,
    interfaces: &Interfaces,
    operator: &NetworkOperator,
) -> axum::Router {
    let judge = serving.judge();
    let pool = serving.pool();
    // /api/health answers 503 when Postgres does not: the compose healthcheck
    // and the tunnel's readiness key off it. MCP's `judge` bills to the same
    // meter as the page, so it is under the same budget.
    let app = Arc::new(
        App::new(
            serving.deps(),
            serving.store(),
            serving.models().meter().clone(),
            cfg,
            judge.source_offer().clone(),
            operator.clone(),
        )
        .with_probe(Arc::new(pool.clone()))
        // /api/about reads the run record for the data's freshness, cached
        // for a minute: the page asks on every load.
        .with_data_status(Arc::new(FreshnessReader::new(pool.clone()))),
    );
    let mut routes = router(Arc::clone(&app), interfaces, &cfg.web_dist);
    // The MCP transport shares the judge slots (one JUDGE_CONCURRENCY for
    // both interfaces) and the metered models (one cap). It takes the flag
    // *and* the token: `check` has already refused `--mcp` without one, and
    // the filter is what makes a token alone inert rather than a mount.
    if let Some(token) = cfg
        .mcp_token
        .as_ref()
        .filter(|_| interfaces.mcp())
        .map(ApiKey::expose)
    {
        let toolbox = Toolbox::new(
            pool.clone(),
            Options {
                harness: Harness::Mcp,
                models: Some(serving.models().clone()),
                deps_config: judge.deps_config(),
                vectors: serving.vectors().cloned(),
                permits: app.permits(),
                judge_quota: Some(Quota {
                    limit: cfg.mcp_judge_limit,
                    window: cfg.mcp_judge_window,
                }),
                history_len: cfg.history_len,
                offer: judge.source_offer().clone(),
                operator: operator.operator().clone(),
            },
        );
        let service = judge_agent::mcp::http_service(Arc::new(toolbox), cfg.mcp_hosts.clone());
        routes = routes.merge(crate::mcp::router(service, token));
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interface;
    use judge_core::{Operator, SupportEmail};
    use nonempty::nonempty;

    fn operator() -> Option<NetworkOperator> {
        let email = SupportEmail::try_new("help@example.com").ok()?;
        Operator::new(None, Some(email)).for_network().ok()
    }

    fn network(
        pairs: &[(&str, &str)],
        interfaces: &nonempty::NonEmpty<Interface>,
    ) -> Option<Network> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let cfg =
            ApiConfig::from_vars(|k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()))
                .ok()?;
        Network::new(cfg, Interfaces::of(interfaces), operator()?).ok()
    }

    #[test]
    fn the_check_runs_on_construction() {
        assert!(network(&[], &nonempty![Interface::Mcp]).is_none());
        assert!(network(&[], &nonempty![Interface::Api]).is_some());
    }

    #[test]
    fn a_stray_token_and_a_page_without_its_route_are_warnings() {
        let token = [("MCP_TOKEN", "0123456789abcdef0123456789abcdef")];
        let warned = network(&token, &nonempty![Interface::Api]).map(|n| n.warnings());
        assert!(
            warned
                .as_ref()
                .is_some_and(|w| w.len() == 1 && w.iter().all(|t| t.contains("MCP_TOKEN"))),
            "{warned:?}"
        );
        let quiet =
            network(&token, &nonempty![Interface::Api, Interface::Mcp]).map(|n| n.warnings());
        assert_eq!(quiet, Some(vec![]));
    }
}
