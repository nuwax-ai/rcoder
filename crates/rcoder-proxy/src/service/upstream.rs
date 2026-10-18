//! Resolve upstreams before constructing Pingora peers. Pingora's generic
//! constructor unwraps `ToSocketAddrs`, so it must receive a concrete address.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use pingora_core::upstreams::peer::HttpPeer;
use pingora_core::{Error, ErrorType, Result};
use tokio::time::Instant;

pub(super) const STREAMING_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const STREAMING_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
pub(super) enum AddressSelection {
    FirstResolved,
    PreferIpv4,
}

/// DNS and the ensuing TCP/TLS connection share the original callsite budget.
/// `None` preserves an originally unconfigured timeout rather than inventing one.
pub(super) struct ConnectBudget {
    started: Instant,
    connection: Option<Duration>,
    total: Option<Duration>,
}

impl ConnectBudget {
    pub(super) fn streaming() -> Self {
        Self::new(
            Some(STREAMING_CONNECT_TIMEOUT),
            Some(STREAMING_TOTAL_TIMEOUT),
        )
    }

    pub(super) fn model_api() -> Self {
        Self::new(Some(Duration::from_secs(10)), Some(Duration::from_secs(30)))
    }

    pub(super) fn unbounded() -> Self {
        Self::new(None, None)
    }

    fn new(connection: Option<Duration>, total: Option<Duration>) -> Self {
        Self {
            started: Instant::now(),
            connection,
            total,
        }
    }

    fn resolution_deadline(&self) -> Option<Instant> {
        self.connection
            .into_iter()
            .chain(self.total)
            .min()
            .map(|duration| self.started + duration)
    }

    pub(super) async fn resolve_host(
        &self,
        host: &str,
        port: u16,
        selection: AddressSelection,
    ) -> Result<SocketAddr> {
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(SocketAddr::new(ip, port));
        }
        self.resolve_with(host, selection, async {
            tokio::net::lookup_host((host, port))
                .await
                .map(Iterator::collect)
        })
        .await
    }

    async fn resolve_with(
        &self,
        target: &str,
        selection: AddressSelection,
        lookup: impl Future<Output = std::io::Result<Vec<SocketAddr>>>,
    ) -> Result<SocketAddr> {
        let result = match self.resolution_deadline() {
            Some(deadline) => tokio::time::timeout_at(deadline, lookup)
                .await
                .map_err(|_| {
                    resolution_error(target, "connection budget exhausted during DNS lookup")
                })?,
            None => lookup.await,
        };
        let addresses = result.map_err(|error| {
            Error::because(
                ErrorType::HTTPStatus(502),
                format!("upstream address resolution failed for {target}"),
                error,
            )
            .into_up()
        })?;
        let selected = match selection {
            AddressSelection::FirstResolved => addresses.first(),
            AddressSelection::PreferIpv4 => addresses
                .iter()
                .find(|address| address.is_ipv4())
                .or_else(|| addresses.first()),
        };
        selected
            .copied()
            .ok_or_else(|| resolution_error(target, "DNS returned no addresses"))
    }

    pub(super) async fn registered_address(
        &self,
        host: &str,
        port: u16,
        selection: AddressSelection,
    ) -> Result<SocketAddr> {
        #[cfg(feature = "deploy-host")]
        if shared_types::is_deploy_host() {
            return shared_types::published::resolve_published_addr(host, port).map_err(|error| {
                Error::new_up(ErrorType::HTTPStatus(503)).more_context(error.to_string())
            });
        }
        self.resolve_host(host, port, selection).await
    }

    pub(super) async fn direct_peer(
        &self,
        host: &str,
        port: u16,
        tls: bool,
        sni: String,
    ) -> Result<HttpPeer> {
        let address = self
            .resolve_host(host, port, AddressSelection::FirstResolved)
            .await?;
        self.peer(address, tls, sni)
    }

    pub(super) async fn peer_from_addr(&self, target: &str) -> Result<HttpPeer> {
        let address = match target.parse::<SocketAddr>() {
            Ok(address) => address,
            Err(_) => {
                self.resolve_with(target, AddressSelection::FirstResolved, async {
                    tokio::net::lookup_host(target).await.map(Iterator::collect)
                })
                .await?
            }
        };
        self.peer(address, false, String::new())
    }

    pub(super) fn peer(&self, address: SocketAddr, tls: bool, sni: String) -> Result<HttpPeer> {
        let elapsed = self.started.elapsed();
        let remaining = |budget: Option<Duration>| -> Result<Option<Duration>> {
            budget
                .map(|duration| {
                    duration
                        .checked_sub(elapsed)
                        .filter(|duration| !duration.is_zero())
                        .ok_or_else(|| {
                            resolution_error(
                                &address.to_string(),
                                "connection budget exhausted before connect",
                            )
                        })
                })
                .transpose()
        };
        let connection = remaining(self.connection)?;
        let total = remaining(self.total)?;
        // SocketAddr has an infallible, non-empty ToSocketAddrs implementation.
        let mut peer = HttpPeer::new(address, tls, sni);
        peer.options.connection_timeout = connection;
        peer.options.total_connection_timeout = total;
        Ok(peer)
    }
}

fn resolution_error(target: &str, reason: &str) -> Box<Error> {
    Error::new_up(ErrorType::HTTPStatus(502)).more_context(format!(
        "upstream address resolution failed for {target}: {reason}"
    ))
}

/// Registered container targets use the published-port map only in deploy-host.
pub async fn dial_peer(host: &str, port: u16) -> Result<HttpPeer> {
    let budget = ConnectBudget::streaming();
    let address = budget
        .registered_address(host, port, AddressSelection::FirstResolved)
        .await?;
    budget.peer(address, false, String::new())
}

/// Context/log address representation; this function never performs DNS.
pub fn dial_addr(host: &str, port: u16) -> Result<String> {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        return shared_types::published::resolve_published_addr(host, port)
            .map(|address| address.to_string())
            .map_err(|error| {
                Error::new_up(ErrorType::HTTPStatus(503)).more_context(error.to_string())
            });
    }
    Ok(match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{host}:{port}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses() -> Vec<SocketAddr> {
        vec![
            "[::1]:4321".parse().unwrap(),
            "127.0.0.1:4321".parse().unwrap(),
        ]
    }

    #[tokio::test]
    async fn resolution_preserves_order_unless_ipv4_was_explicitly_preferred() {
        let budget = ConnectBudget::streaming();
        let first = budget
            .resolve_with("fixture", AddressSelection::FirstResolved, async {
                Ok(addresses())
            })
            .await
            .unwrap();
        let preferred = budget
            .resolve_with("fixture", AddressSelection::PreferIpv4, async {
                Ok(addresses())
            })
            .await
            .unwrap();
        assert!(first.is_ipv6());
        assert!(preferred.is_ipv4());
        let v6_only = budget
            .resolve_with("fixture", AddressSelection::PreferIpv4, async {
                Ok(vec![addresses()[0]])
            })
            .await
            .unwrap();
        assert!(v6_only.is_ipv6());
    }

    #[tokio::test]
    async fn dns_error_and_empty_result_are_502_and_do_not_poison_later_peers() {
        let budget = ConnectBudget::streaming();
        for failure in [
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "controlled resolver failure",
            )),
            Ok(Vec::new()),
        ] {
            let error = budget
                .resolve_with("fixture", AddressSelection::FirstResolved, async {
                    failure
                })
                .await
                .unwrap_err();
            assert_eq!(error.etype(), &ErrorType::HTTPStatus(502));
            assert_eq!(error.esource(), &pingora_core::ErrorSource::Upstream);
            assert!(error.to_string().contains("upstream address resolution"));
        }
        let peer = ConnectBudget::streaming()
            .direct_peer("127.0.0.1", 4321, false, String::new())
            .await
            .unwrap();
        assert_eq!(
            peer._address.as_inet(),
            Some(&"127.0.0.1:4321".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn dns_uses_and_deducts_the_original_connection_and_total_budgets() {
        let budget = ConnectBudget::new(Some(Duration::from_secs(1)), Some(Duration::from_secs(2)));
        let address = budget
            .resolve_with("fixture", AddressSelection::FirstResolved, async {
                tokio::time::sleep(Duration::from_millis(25)).await;
                Ok(addresses())
            })
            .await
            .unwrap();
        let mut peer = budget
            .peer(address, true, "original.example".into())
            .unwrap();
        let connection = peer.options.connection_timeout.unwrap();
        let total = peer.options.total_connection_timeout.unwrap();
        assert!(connection < Duration::from_millis(990));
        assert!(total < Duration::from_millis(1990));
        super::super::handlers::streaming_peer_options(&mut peer, Duration::from_secs(3600));
        assert_eq!(peer.options.connection_timeout, Some(connection));
        assert_eq!(peer.options.total_connection_timeout, Some(total));
        assert_eq!(peer.sni, "original.example");
        assert!(peer.is_tls());
        assert!(peer.options.read_timeout.is_none());
        assert!(peer.options.write_timeout.is_none());
    }

    #[tokio::test]
    async fn hung_dns_ends_inside_the_existing_budget_without_blocking_the_next_request() {
        let budget = ConnectBudget::new(
            Some(Duration::from_millis(20)),
            Some(Duration::from_millis(40)),
        );
        let error = budget
            .resolve_with(
                "fixture",
                AddressSelection::FirstResolved,
                std::future::pending::<std::io::Result<Vec<SocketAddr>>>(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.etype(), &ErrorType::HTTPStatus(502));
        assert!(error.to_string().contains("connection budget exhausted"));
        assert!(budget.peer(addresses()[0], false, String::new()).is_err());
        assert!(
            ConnectBudget::streaming()
                .direct_peer("::1", 4321, false, String::new())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn concrete_ipv4_ipv6_and_context_addresses_need_no_dns() {
        let budget = ConnectBudget::streaming();
        for target in ["127.0.0.1:4321", "[::1]:4321"] {
            let peer = budget.peer_from_addr(target).await.unwrap();
            assert_eq!(
                peer._address.as_inet(),
                Some(&target.parse::<SocketAddr>().unwrap())
            );
        }
        let peer = budget.peer_from_addr("localhost:4321").await.unwrap();
        assert!(peer._address.as_inet().unwrap().ip().is_loopback());
    }

    #[tokio::test]
    async fn health_peer_keeps_its_original_unconfigured_timeouts() {
        let peer = ConnectBudget::unbounded()
            .direct_peer("127.0.0.1", 4321, false, String::new())
            .await
            .unwrap();
        assert_eq!(peer.options.connection_timeout, None);
        assert_eq!(peer.options.total_connection_timeout, None);
    }

    #[cfg(feature = "deploy-host")]
    #[tokio::test]
    async fn registered_peer_uses_published_ports_but_direct_peer_never_does() {
        let key = format!("dns-peer-{}", uuid::Uuid::new_v4());
        shared_types::published::register_port(&key, 4321, 5432);
        let peer = dial_peer(&key, 4321).await.unwrap();
        shared_types::published::unregister(&key);
        assert_eq!(
            peer._address.as_inet(),
            Some(&"127.0.0.1:5432".parse().unwrap())
        );
        let direct = ConnectBudget::streaming()
            .direct_peer("127.0.0.1", 4321, false, String::new())
            .await
            .unwrap();
        assert_eq!(
            direct._address.as_inet(),
            Some(&"127.0.0.1:4321".parse().unwrap())
        );
    }
}
