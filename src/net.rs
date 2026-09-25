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

/// Proxy and CA settings shared by every HTTP client of the runner
// Only the runner (feature `runner`) builds clients from it
#[cfg_attr(not(feature = "runner"), allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub struct Network {
    /// Proxy for HTTP and HTTPS; without it reqwest reads HTTP(S)_PROXY/NO_PROXY
    pub proxy: Option<String>,
    /// Hosts, domains (.corp.local), IPs and CIDRs reached directly, comma separated
    pub no_proxy: Option<String>,
    /// PEM bundle trusted in addition to the system roots
    pub ca_pem: Option<String>,
}

#[cfg_attr(not(feature = "runner"), allow(dead_code))]
impl Network {
    /// A client builder with the IPv4-first resolver, the proxy and the CA
    pub fn client_builder(&self) -> anyhow::Result<reqwest::ClientBuilder> {
        use anyhow::Context;

        let mut builder = client_builder();
        if let Some(proxy) = &self.proxy {
            let no_proxy = self
                .no_proxy
                .as_deref()
                .and_then(reqwest::NoProxy::from_string);
            builder = builder.proxy(
                reqwest::Proxy::all(proxy.as_str())
                    .context("Invalid proxy URL")?
                    .no_proxy(no_proxy),
            );
        }
        if let Some(pem) = &self.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
                .context("Invalid CA certificate (PEM)")?;
            if certificates.is_empty() {
                anyhow::bail!("The CA file contains no PEM certificate");
            }
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        Ok(builder)
    }
}

/// NO_PROXY for jobs: the configured entries, loopback, and `extra` (the job's
/// service aliases, which only exist on the job's network), without repeats
// Only the runner (feature `runner`) passes it to jobs
#[cfg_attr(not(feature = "runner"), allow(dead_code))]
pub fn effective_no_proxy(configured: Option<&str>, extra: &[String]) -> String {
    let mut entries: Vec<String> = Vec::new();
    let given = configured.unwrap_or("").split(',').map(str::trim);
    let loopback = ["localhost", "127.0.0.1", "::1"].into_iter();
    for entry in given
        .chain(loopback)
        .chain(extra.iter().map(String::as_str))
    {
        if !entry.is_empty() && !entries.iter().any(|e| e == entry) {
            entries.push(entry.to_string());
        }
    }
    entries.join(",")
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

    /// A proxy that answers one request with 200 and returns its request line
    async fn one_shot_proxy() -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..n]);
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&head)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn requests_go_through_the_configured_proxy() {
        let (proxy, request_line) = one_shot_proxy().await;
        let network = Network {
            proxy: Some(proxy),
            ..Network::default()
        };

        let status = network
            .client_builder()
            .unwrap()
            .build()
            .unwrap()
            .get("http://turboci-proxy-test.invalid/ping")
            .send()
            .await
            .unwrap()
            .status();

        assert_eq!(status, 200);
        assert_eq!(
            request_line.await.unwrap(),
            "GET http://turboci-proxy-test.invalid/ping HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn no_proxy_hosts_are_reached_directly() {
        let (proxy, request_line) = one_shot_proxy().await;
        let network = Network {
            proxy: Some(proxy),
            no_proxy: Some("localhost,.invalid".to_string()),
            ..Network::default()
        };

        let result = network
            .client_builder()
            .unwrap()
            .build()
            .unwrap()
            .get("http://turboci-proxy-test.invalid/ping")
            .send()
            .await;

        assert!(
            result.is_err(),
            "a .invalid host reached directly cannot resolve"
        );
        assert!(!request_line.is_finished(), "the proxy must not be used");
        request_line.abort();
    }

    #[test]
    fn effective_no_proxy_adds_loopback_and_services_once() {
        assert_eq!(
            effective_no_proxy(
                Some(" .corp.local, localhost ,10.0.0.0/8"),
                &[
                    "docker".to_string(),
                    "postgres".to_string(),
                    "docker".to_string()
                ]
            ),
            ".corp.local,localhost,10.0.0.0/8,127.0.0.1,::1,docker,postgres"
        );
        assert_eq!(effective_no_proxy(None, &[]), "localhost,127.0.0.1,::1");
    }

    #[test]
    fn rejects_bad_proxy_and_ca() {
        let bad_proxy = Network {
            proxy: Some("not a url".to_string()),
            ..Network::default()
        };
        let bad_ca = Network {
            ca_pem: Some("no pem here".to_string()),
            ..Network::default()
        };
        assert!(bad_proxy.client_builder().is_err());
        assert!(bad_ca.client_builder().is_err());
    }
}
