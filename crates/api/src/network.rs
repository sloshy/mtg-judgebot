//! The network roles (`--api`, `--web`, `--mcp`) as one launch: the
//! checked [`Network`], [`Network::bind`], which builds the routes over a
//! process's shared composition ([`judge_bot::serving::Serving`]) and binds
//! `API_ADDR`, and [`Listening::serve`].

use std::sync::Arc;

use anyhow::Result;
use judge_agent::{Client, Options, Quota, Toolbox, Transport};
use judge_bot::{
    discord::endpoint::Interactions, ingest::runs::FreshnessReader, serving::Serving,
    shutdown::Shutdown,
};
use judge_core::NetworkOperator;
use judge_llm::ApiKey;

use crate::{
    ApiConfig, App, Interfaces, Refused, bind,
    discord::{All, Open},
    http::{self, Maintenance, Probe},
    router, serve_on,
};

/// The longest a listener that serves Discord's interactions endpoint waits
/// for it to open before it listens anyway, so the other routes are not held
/// up by a Discord API that does not answer.
pub const LISTEN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The address, listening or only reserved.
enum Socket {
    Listening(tokio::net::TcpListener),
    /// Reserved: a dial is refused, and goes on to another container, until
    /// the interactions endpoint opens.
    Reserved(tokio::net::TcpSocket),
}

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
                 add --mcp to the roles (JUDGE_ROLES) or unset the token",
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
    socket: Socket,
    routes: axum::Router,
    shutdown: Shutdown,
    /// Discord's interactions endpoint, when it is mounted: told when the
    /// listener has stopped, so the Discord role drains only then.
    interactions: Option<Interactions>,
}

impl Listening {
    /// Serve until the accept loop fails, or until the process's shutdown is
    /// requested and the requests in flight have been answered (at most
    /// until the drain's limit; see [`serve_on`]).
    ///
    /// # Errors
    /// A fatal accept-loop error.
    ///
    /// A listener that serves Discord's interactions endpoint listens only
    /// once the endpoint opens (or after [`LISTEN_WAIT`]): until then the
    /// address refuses connections, so during a deploy the proxy's dial goes
    /// on to the old container, which can verify them, rather than here.
    pub async fn serve(self) -> Result<()> {
        let Self {
            socket,
            routes,
            shutdown,
            interactions,
        } = self;
        let served = async {
            let listener = match socket {
                Socket::Listening(l) => l,
                Socket::Reserved(socket) => {
                    if let Some(i) = &interactions {
                        tokio::select! {
                            () = i.opened() => {}
                            () = tokio::time::sleep(LISTEN_WAIT) => tracing::warn!(
                                wait_secs = LISTEN_WAIT.as_secs(),
                                "Discord's interactions endpoint has not opened; listening anyway"
                            ),
                            // Nothing was accepted, so nothing to drain.
                            () = shutdown.requested() => return Ok(()),
                        }
                    }
                    http::listen(socket)?
                }
            };
            serve_on(listener, routes, &shutdown).await
        }
        .await;
        if let Some(i) = &interactions {
            i.close();
        }
        served
    }

    /// The listener of a process whose only HTTP role is Discord's
    /// interactions endpoint (`--discord` alone with
    /// `DISCORD_INTERACTIONS=http`): the endpoint and `GET /api/health` on
    /// `cfg`'s `API_ADDR`, nothing else.
    ///
    /// # Errors
    /// Binding `API_ADDR`.
    pub fn interactions_only(
        cfg: &ApiConfig,
        serving: &Serving,
        interactions: &Interactions,
    ) -> Result<Self> {
        // The endpoint before maintenance: `scripts/deploy.sh` reads a
        // `maintenance:` answer as "up and answering with the maintenance
        // reply", which needs the endpoint open.
        let probe = All(vec![
            Arc::new(serving.pool().clone()),
            Arc::new(Open(interactions.clone())),
            Arc::new(Maintenance(serving.availability().clone())),
        ]);
        let routes = crate::discord::alone(interactions.clone(), Arc::new(probe));
        tracing::info!(
            path = judge_bot::discord::endpoint::PATH,
            "starting HTTP adapter for Discord's interactions endpoint alone"
        );
        let socket = Socket::Reserved(http::reserve(cfg)?);
        Ok(Self {
            socket,
            routes,
            shutdown: serving.shutdown().clone(),
            interactions: Some(interactions.clone()),
        })
    }
}

impl Network {
    /// Build the routes over `serving` and bind `API_ADDR`. A process binds
    /// before it starts its other roles, so a taken address fails before the
    /// Discord gateway is ever contacted (with `discord`, an address another
    /// process listens on; one only reserved by another fails at listen). With `discord`, Discord's
    /// interactions endpoint is mounted beside them, and `/api/health` fails
    /// until it is open.
    ///
    /// # Errors
    /// Binding `API_ADDR`.
    pub async fn bind(
        self,
        serving: &Serving,
        discord: Option<&Interactions>,
    ) -> Result<Listening> {
        let Self {
            cfg,
            interfaces,
            operator,
        } = self;
        let routes = routes(serving, &cfg, &interfaces, &operator, discord);
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
            discord_interactions = discord.map(|_| judge_bot::discord::endpoint::PATH),
            "starting HTTP adapter"
        );
        let socket = match discord {
            None => Socket::Listening(bind(&cfg).await?),
            Some(_) => Socket::Reserved(http::reserve(&cfg)?),
        };
        Ok(Listening {
            socket,
            routes,
            shutdown: serving.shutdown().clone(),
            interactions: discord.cloned(),
        })
    }
}

/// The router for `interfaces`, the MCP transport mounted when asked for,
/// and Discord's interactions endpoint with `discord`.
fn routes(
    serving: &Serving,
    cfg: &ApiConfig,
    interfaces: &Interfaces,
    operator: &NetworkOperator,
    discord: Option<&Interactions>,
) -> axum::Router {
    let judge = serving.judge();
    let pool = serving.pool();
    // /api/health answers 503 when Postgres does not: the compose healthcheck
    // and the tunnel's readiness key off it. MCP's `judge` bills to the same
    // meter as the page (the process's one), so it is under the same budget.
    let app = Arc::new(
        App::new(
            serving.deps(),
            serving.store(),
            cfg,
            judge.source_offer().clone(),
            operator.clone(),
        )
        .with_probe(match discord {
            None => Arc::new(All(vec![
                Arc::new(pool.clone()) as Arc<dyn Probe>,
                Arc::new(Maintenance(serving.availability().clone())),
            ])),
            // The endpoint before maintenance, as in `interactions_only`.
            Some(i) => Arc::new(All(vec![
                Arc::new(pool.clone()) as Arc<dyn Probe>,
                Arc::new(Open(i.clone())),
                Arc::new(Maintenance(serving.availability().clone())),
            ])),
        })
        .with_availability(serving.availability().clone())
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
                transport: Transport::Mcp,
                client: Client::Instance(serving.availability().clone()),
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
    if let Some(i) = discord {
        routes = routes.merge(crate::discord::router(i.clone()));
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

    /// A reserved address refuses a dial (so a proxy goes on to another
    /// container) until it listens, and holds the port meanwhile.
    #[tokio::test]
    async fn a_reserved_address_refuses_until_it_listens() -> anyhow::Result<()> {
        let cfg = ApiConfig::from_vars(|k| (k == "API_ADDR").then(|| "127.0.0.1:0".to_owned()))?;
        let socket = http::reserve(&cfg)?;
        let addr = socket.local_addr()?;
        assert!(tokio::net::TcpStream::connect(addr).await.is_err());
        let listener = http::listen(socket)?;
        assert!(tokio::net::TcpStream::connect(addr).await.is_ok());
        drop(listener);
        Ok(())
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
