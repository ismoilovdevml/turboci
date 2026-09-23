//! HTTP client construction shared by the runner and `turboci upgrade`.
//!
//! Name resolution asks for IPv4 addresses first and only then for IPv6. With a
//! single AF_UNSPEC getaddrinfo call, musl (the static release build) sends A and
//! AAAA queries together and fails the whole lookup after a 5s timeout when the
//! DNS server answers AAAA with SERVFAIL, as some corporate DNS servers do, even
//! though the A answer arrived. glibc returns the A answer in that case.

use dns_lookup::{getaddrinfo, AddrFamily, AddrInfoHints, SockType};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::SocketAddr;
use std::sync::Arc;

/// Resolve `host`, trying IPv4 before IPv6
pub fn lookup(host: &str) -> std::io::Result<Vec<SocketAddr>> {
    for family in [AddrFamily::Inet, AddrFamily::Inet6] {
        let hints = AddrInfoHints {
            address: family.into(),
            socktype: SockType::Stream.into(),
            ..AddrInfoHints::default()
        };
        if let Ok(results) = getaddrinfo(Some(host), None, Some(hints)) {
            let addrs: Vec<SocketAddr> = results
                .filter_map(Result::ok)
                .map(|info| info.sockaddr)
                .collect();
            if !addrs.is_empty() {
                return Ok(addrs);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("could not resolve {}", host),
    ))
}

struct Ipv4FirstResolver;

impl Resolve for Ipv4FirstResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = tokio::task::spawn_blocking(move || lookup(&host)).await??;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// A client builder using the IPv4-first resolver
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().dns_resolver(Arc::new(Ipv4FirstResolver))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_localhost_to_ipv4_first() {
        let addrs = lookup("localhost").unwrap();
        assert!(addrs[0].is_ipv4(), "{:?}", addrs);
    }

    #[test]
    fn unknown_host_is_an_error() {
        assert!(lookup("does-not-exist.invalid").is_err());
    }

    #[tokio::test]
    async fn client_connects_by_host_name() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let url = format!("http://localhost:{}/", server.address().port());

        let status = client_builder()
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap()
            .status();

        assert_eq!(status, 200);
    }
}
