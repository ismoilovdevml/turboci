//! A small client for the Docker Engine API calls the Docker executor makes,
//! over dockerd's unix socket (HTTP/1.1, one connection per request).
//!
//! It speaks to dockerd exactly as bollard 0.19 did before it:
//! - the socket is `DOCKER_HOST`, which must be a `unix://` address when
//!   set (TCP and TLS are not supported), else /var/run/docker.sock, and it
//!   must exist when the client is created;
//! - paths carry no `/v1.xx` prefix, so dockerd answers with its own current
//!   API version (bollard's prefix was dropped by `Url::join`);
//! - every request has `Content-Type: application/json`, and waiting for a
//!   response's head is limited to 2 minutes (bodies are not limited);
//! - a status other than 2xx, 304 or 101 is an error carrying the status and
//!   dockerd's `message`, and errors read the same as bollard's did.

mod decode;
mod models;

pub use decode::LogOutput;
pub use models::*;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryFutureExt, TryStreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, CONNECTION, CONTENT_TYPE, UPGRADE};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyperlocal::UnixConnector;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::codec::FramedRead;

use decode::{JsonLineDecoder, LogDecoder};

/// The socket used when DOCKER_HOST names none
const DEFAULT_SOCKET: &str = "unix:///var/run/docker.sock";
/// Limit of waiting for a response's head
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Body of an exec start that stays attached to the exec's output
const ATTACHED_EXEC_START: &str = r#"{"Detach":false,"Tty":false,"OutputCapacity":null}"#;
/// Read buffer of an exec's output stream
const EXEC_OUTPUT_CAPACITY: usize = 8 * 1024;

/// Why a Docker API call failed
#[derive(Debug)]
pub enum Error {
    /// dockerd answered with an error status
    Status {
        status_code: u16,
        message: String,
    },
    /// A response that does not have the expected shape
    JsonData {
        message: String,
    },
    /// An image pull reported an error
    Stream {
        error: String,
    },
    /// The container waited for exited with a non-zero code (which the
    /// message does not name)
    ContainerWait {
        error: String,
    },
    /// dockerd did not answer in time
    Timeout,
    /// The socket does not exist
    SocketNotFound(String),
    /// DOCKER_HOST names something other than a unix socket
    UnsupportedHost(String),
    Json(serde_json::Error),
    Io(std::io::Error),
    Http(hyper::http::Error),
    Hyper(hyper::Error),
    Client(hyper_util::client::legacy::Error),
    Url(url::ParseError),
    Uri(hyper::http::uri::InvalidUri),
}

impl Error {
    /// HTTP status of an error answer from dockerd
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Error::Status { status_code, .. } => Some(*status_code),
            _ => None,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Status {
                status_code,
                message,
            } => write!(
                f,
                "Docker responded with status code {}: {}",
                status_code, message
            ),
            Error::JsonData { message } => write!(f, "Failed to deserialize JSON: {}", message),
            Error::Stream { error } => write!(f, "Docker stream error: {}", error),
            Error::ContainerWait { error } => {
                write!(f, "Docker container wait error: {}", error)
            }
            Error::Timeout => write!(f, "Timeout error"),
            Error::SocketNotFound(path) => write!(f, "Socket not found: {}", path),
            Error::UnsupportedHost(host) => write!(
                f,
                "DOCKER_HOST={} is not supported; use a unix socket (unix:///path/to/docker.sock)",
                host
            ),
            Error::Json(e) => e.fmt(f),
            Error::Io(e) => e.fmt(f),
            Error::Http(e) => e.fmt(f),
            Error::Hyper(e) => e.fmt(f),
            Error::Client(e) => write!(f, "Error in the hyper legacy client: {}", e),
            Error::Url(e) => write!(f, "Unable to parse URL: {}", e),
            Error::Uri(e) => write!(f, "Unable to parse URI: {}", e),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            // Shown as the error itself, so their source is the inner one's
            Error::Json(e) => e.source(),
            Error::Io(e) => e.source(),
            Error::Http(e) => e.source(),
            Error::Hyper(e) => e.source(),
            // Named in the message and kept as the source too
            Error::Client(e) => Some(e),
            Error::Url(e) => Some(e),
            Error::Uri(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Label (or other) filters of a list call, e.g. `label` → [`key=value`]
pub type Filters = HashMap<String, Vec<String>>;

/// Docker Engine API client over a unix socket
pub struct Docker {
    client: Client<UnixConnector, Full<Bytes>>,
    /// Host part of request URIs: the socket path, hex-encoded
    host: String,
}

impl Docker {
    /// Connect to the `unix://` socket `DOCKER_HOST` names, or to
    /// /var/run/docker.sock when it is unset or empty; fails when the socket
    /// does not exist or DOCKER_HOST is not a unix socket
    pub fn connect_with_local_defaults() -> Result<Self> {
        let path = socket_path(std::env::var("DOCKER_HOST").ok().as_deref())?;
        if !std::path::Path::new(&path).exists() {
            return Err(Error::SocketNotFound(path));
        }
        Ok(Self::with_socket(&path))
    }

    /// A client of the socket at `path`, which is not checked
    pub fn with_socket(path: &str) -> Self {
        let mut builder = Client::builder(TokioExecutor::new());
        builder.pool_max_idle_per_host(0);
        Self {
            client: builder.build(UnixConnector),
            host: path.bytes().map(|b| format!("{:02x}", b)).collect(),
        }
    }

    fn uri(&self, path: &str, query: Option<&[(&str, &str)]>) -> Result<hyper::Uri> {
        let base = url::Url::parse(&format!("unix://{}/", self.host)).map_err(Error::Url)?;
        let mut url = base.join(path).map_err(Error::Url)?;
        if let Some(pairs) = query {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(pairs)
                .finish();
            url.set_query(Some(&query));
        }
        url.as_str().parse().map_err(Error::Uri)
    }

    /// Send a request and return the response, once its head arrived with
    /// a status that is not an error
    async fn request(
        &self,
        method: Method,
        path: &str,
        query: Option<&[(&str, &str)]>,
        headers: Vec<(HeaderName, HeaderValue)>,
        body: Bytes,
    ) -> Result<Response<Incoming>> {
        let mut builder = Request::builder().method(method);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let request = builder
            .uri(self.uri(path, query)?)
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(body))
            .map_err(Error::Http)?;
        let response =
            match tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request)).await {
                Ok(response) => response.map_err(Error::Client)?,
                Err(_) => return Err(Error::Timeout),
            };
        let status = response.status();
        if status.is_success()
            || status == StatusCode::NOT_MODIFIED
            || status == StatusCode::SWITCHING_PROTOCOLS
        {
            return Ok(response);
        }
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(Error::Hyper)?
            .to_bytes();
        Err(status_error(status.as_u16(), &body)?)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: Option<&[(&str, &str)]>,
    ) -> Result<T> {
        let response = self
            .request(Method::GET, path, query, Vec::new(), Bytes::new())
            .await?;
        json_body(response).await
    }

    async fn send_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: Option<&[(&str, &str)]>,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        let body = serde_json::to_vec(body).map_err(Error::Json)?;
        let response = self
            .request(Method::POST, path, query, Vec::new(), body.into())
            .await?;
        json_body(response).await
    }

    /// A call whose response body is not needed
    async fn call(&self, method: Method, path: &str, query: Option<&[(&str, &str)]>) -> Result<()> {
        self.request(method, path, query, Vec::new(), Bytes::new())
            .await
            .map(drop)
    }

    /// The newline-delimited JSON messages of a response, sent when the
    /// stream is first polled
    fn json_stream<T: DeserializeOwned>(
        &self,
        method: Method,
        path: String,
        query: Option<Vec<(&'static str, String)>>,
        headers: Vec<(HeaderName, HeaderValue)>,
    ) -> impl Stream<Item = Result<T>> + Unpin + Send + '_ {
        Box::pin(
            async move {
                let pairs: Option<Vec<(&str, &str)>> = query
                    .as_ref()
                    .map(|query| query.iter().map(|(k, v)| (*k, v.as_str())).collect());
                self.request(method, &path, pairs.as_deref(), headers, Bytes::new())
                    .await
            }
            .map_ok(|response| {
                FramedRead::new(body_reader(response), JsonLineDecoder::<T>::default())
            })
            .try_flatten_stream(),
        )
    }

    // --- System

    /// `GET /info`
    pub async fn info(&self) -> Result<SystemInfo> {
        self.get("/info", None).await
    }

    // --- Images

    /// `POST /images/create`: pull `from_image` with `credentials`; the
    /// stream ends with an error when dockerd reports one
    pub fn create_image<'a>(
        &'a self,
        from_image: &str,
        credentials: Option<&DockerCredentials>,
    ) -> impl Stream<Item = Result<CreateImageInfo>> + Unpin + Send + 'a {
        let auth = credentials.map(registry_auth).transpose();
        let query = vec![
            ("fromImage", from_image.to_string()),
            ("fromSrc", String::new()),
            ("repo", String::new()),
            ("tag", String::new()),
            ("platform", String::new()),
        ];
        let pull = match auth {
            Ok(auth) => self
                .json_stream::<CreateImageInfo>(
                    Method::POST,
                    "/images/create".to_string(),
                    Some(query),
                    auth.into_iter().collect(),
                )
                .left_stream(),
            Err(e) => futures_util::stream::once(async move { Err(e) }).right_stream(),
        };
        Box::pin(pull.map(|progress| match progress {
            Ok(CreateImageInfo {
                error: Some(error), ..
            }) => Err(Error::Stream { error }),
            progress => progress,
        }))
    }

    /// `GET /images/{name}/json`, for whether the image is present
    pub async fn inspect_image(&self, name: &str) -> Result<()> {
        self.get::<serde::de::IgnoredAny>(&format!("/images/{}/json", name), None)
            .await
            .map(drop)
    }

    /// `DELETE /images/{name}`
    #[cfg(test)]
    pub async fn remove_image(&self, name: &str) -> Result<()> {
        let response = self
            .request(
                Method::DELETE,
                &format!("/images/{}", name),
                None,
                Vec::new(),
                Bytes::new(),
            )
            .await?;
        json_body::<serde::de::IgnoredAny>(response).await.map(drop)
    }

    // --- Containers

    /// `GET /containers/json`
    pub async fn list_containers(
        &self,
        all: bool,
        filters: &Filters,
    ) -> Result<Vec<ContainerSummary>> {
        let filters = serde_json::to_string(filters).map_err(Error::Json)?;
        let query = [
            ("all", if all { "true" } else { "false" }),
            ("size", "false"),
            ("filters", filters.as_str()),
        ];
        self.get("/containers/json", Some(&query)).await
    }

    /// `POST /containers/create?name=`
    pub async fn create_container(
        &self,
        name: &str,
        config: &ContainerCreateBody,
    ) -> Result<ContainerCreateResponse> {
        self.send_json(
            "/containers/create",
            Some(&[("name", name), ("platform", "")]),
            config,
        )
        .await
    }

    /// `POST /containers/{id}/start`
    pub async fn start_container(&self, id: &str) -> Result<()> {
        self.call(Method::POST, &format!("/containers/{}/start", id), None)
            .await
    }

    /// `GET /containers/{id}/json`
    pub async fn inspect_container(&self, id: &str) -> Result<ContainerInspectResponse> {
        self.get(&format!("/containers/{}/json", id), None).await
    }

    /// `DELETE /containers/{id}`: kill it when running (`force`) and remove
    /// its anonymous volumes (`volumes`)
    pub async fn remove_container(&self, id: &str, force: bool, volumes: bool) -> Result<()> {
        let flag = |on: bool| if on { "true" } else { "false" };
        let query = [
            ("v", flag(volumes)),
            ("force", flag(force)),
            ("link", "false"),
        ];
        self.call(Method::DELETE, &format!("/containers/{}", id), Some(&query))
            .await
    }

    /// `POST /containers/{id}/wait`: one message once the container stopped;
    /// a non-zero exit code is an error
    pub fn wait_container(
        &self,
        id: &str,
    ) -> impl Stream<Item = Result<ContainerWaitResponse>> + Unpin + Send + '_ {
        self.json_stream::<ContainerWaitResponse>(
            Method::POST,
            format!("/containers/{}/wait", id),
            None,
            Vec::new(),
        )
        .map(|waited| match waited {
            Ok(ContainerWaitResponse {
                status_code: code,
                error:
                    Some(ContainerWaitExitError {
                        message: Some(error),
                    }),
            }) if code > 0 => Err(Error::ContainerWait { error }),
            Ok(ContainerWaitResponse {
                status_code: code,
                error: None,
            }) if code > 0 => Err(Error::ContainerWait {
                error: String::new(),
            }),
            // Like bollard: an `Error` object without a message passes
            waited => waited,
        })
    }

    /// `GET /containers/{id}/logs`: what a container wrote so far
    pub fn logs(
        &self,
        id: &str,
        stdout: bool,
        stderr: bool,
    ) -> impl Stream<Item = Result<LogOutput>> + Unpin + Send + '_ {
        let flag = |on: bool| if on { "true" } else { "false" };
        let path = format!("/containers/{}/logs", id);
        Box::pin(
            async move {
                let query = [
                    ("follow", "false"),
                    ("stdout", flag(stdout)),
                    ("stderr", flag(stderr)),
                    ("since", "0"),
                    ("until", "0"),
                    ("timestamps", "false"),
                    ("tail", ""),
                ];
                self.request(Method::GET, &path, Some(&query), Vec::new(), Bytes::new())
                    .await
            }
            .map_ok(|response| {
                FramedRead::new(body_reader(response), LogDecoder::logs()).map_err(Error::Io)
            })
            .try_flatten_stream(),
        )
    }

    // --- Exec

    /// `POST /containers/{id}/exec`
    pub async fn create_exec(
        &self,
        id: &str,
        config: &ExecConfig<'_>,
    ) -> Result<CreateExecResults> {
        self.send_json(&format!("/containers/{}/exec", id), None, config)
            .await
    }

    /// `POST /exec/{id}/start`, attached: the exec's output until it ends
    pub async fn start_exec(
        &self,
        id: &str,
    ) -> Result<impl Stream<Item = Result<LogOutput>> + Unpin + Send> {
        let response = self
            .request(
                Method::POST,
                &format!("/exec/{}/start", id),
                None,
                vec![
                    (CONNECTION, HeaderValue::from_static("Upgrade")),
                    (UPGRADE, HeaderValue::from_static("tcp")),
                ],
                Bytes::from_static(ATTACHED_EXEC_START.as_bytes()),
            )
            .await?;
        let upgraded = hyper::upgrade::on(response).await.map_err(Error::Hyper)?;
        Ok(FramedRead::with_capacity(
            TokioIo::new(upgraded),
            LogDecoder::hijacked(),
            EXEC_OUTPUT_CAPACITY,
        )
        .map_err(Error::Io))
    }

    /// `GET /exec/{id}/json`
    pub async fn inspect_exec(&self, id: &str) -> Result<ExecInspectResponse> {
        self.get(&format!("/exec/{}/json", id), None).await
    }

    // --- Networks

    /// `GET /networks`
    pub async fn list_networks(&self, filters: &Filters) -> Result<Vec<NetworkSummary>> {
        let filters = serde_json::to_string(filters).map_err(Error::Json)?;
        self.get("/networks", Some(&[("filters", filters.as_str())]))
            .await
    }

    /// `POST /networks/create`
    pub async fn create_network(&self, config: &NetworkCreateRequest) -> Result<()> {
        self.send_json::<serde::de::IgnoredAny>("/networks/create", None, config)
            .await
            .map(drop)
    }

    /// `GET /networks/{name}`
    #[cfg(test)]
    pub async fn inspect_network(&self, name: &str) -> Result<()> {
        self.get::<serde::de::IgnoredAny>(&format!("/networks/{}", name), None)
            .await
            .map(drop)
    }

    /// `DELETE /networks/{name}`
    pub async fn remove_network(&self, name: &str) -> Result<()> {
        self.call(Method::DELETE, &format!("/networks/{}", name), None)
            .await
    }

    // --- Volumes

    /// `POST /volumes/create`
    pub async fn create_volume(&self, config: &VolumeCreateOptions) -> Result<()> {
        self.send_json::<serde::de::IgnoredAny>("/volumes/create", None, config)
            .await
            .map(drop)
    }

    /// `GET /volumes/{name}`, for whether the volume exists
    pub async fn inspect_volume(&self, name: &str) -> Result<()> {
        self.get::<serde::de::IgnoredAny>(&format!("/volumes/{}", name), None)
            .await
            .map(drop)
    }

    /// `GET /volumes`
    #[cfg(test)]
    pub async fn list_volumes(&self, filters: &Filters) -> Result<VolumeListResponse> {
        let filters = serde_json::to_string(filters).map_err(Error::Json)?;
        self.get("/volumes", Some(&[("filters", filters.as_str())]))
            .await
    }

    /// `DELETE /volumes/{name}`
    #[cfg(test)]
    pub async fn remove_volume(&self, name: &str) -> Result<()> {
        self.call(Method::DELETE, &format!("/volumes/{}", name), None)
            .await
    }
}

/// The X-Registry-Auth header for `credentials`: their JSON, base64-encoded
/// Path of the socket `docker_host` (DOCKER_HOST's value) names. Anything
/// but a `unix://` address is an error rather than silently using the
/// default socket: jobs would run on another daemon than intended.
fn socket_path(docker_host: Option<&str>) -> Result<String> {
    let host = docker_host
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .unwrap_or(DEFAULT_SOCKET);
    match host.strip_prefix("unix://") {
        Some(path) if !path.is_empty() => Ok(path.to_string()),
        _ => Err(Error::UnsupportedHost(host.to_string())),
    }
}

fn registry_auth(credentials: &DockerCredentials) -> Result<(HeaderName, HeaderValue)> {
    use base64::Engine;

    let json = serde_json::to_string(credentials).map_err(Error::Json)?;
    let value = base64::engine::general_purpose::STANDARD.encode(json);
    let value = HeaderValue::try_from(value).map_err(|e| Error::Http(e.into()))?;
    Ok((HeaderName::from_static("x-registry-auth"), value))
}

/// The error for an error answer: dockerd's `message` when the body is the
/// usual JSON, else the body as it is
fn status_error(status_code: u16, body: &[u8]) -> Result<Error> {
    #[derive(serde::Deserialize)]
    struct ErrorMessage {
        message: String,
    }

    let contents = String::from_utf8_lossy(body).to_string();
    let message = if contents.is_empty() {
        contents
    } else {
        match serde_json::from_str::<ErrorMessage>(&contents) {
            Ok(error) => error.message,
            Err(e) if e.is_data() || e.is_syntax() => contents,
            Err(e) => return Err(Error::Json(e)),
        }
    };
    Ok(Error::Status {
        status_code,
        message,
    })
}

/// A whole response body as `T`
async fn json_body<T: DeserializeOwned>(response: Response<Incoming>) -> Result<T> {
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(Error::Hyper)?
        .to_bytes();
    parse_json(&body)
}

fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| {
        if e.is_data() || e.is_syntax() {
            Error::JsonData {
                message: e.to_string(),
            }
        } else {
            Error::Json(e)
        }
    })
}

/// A response body as a byte reader
fn body_reader(response: Response<Incoming>) -> impl tokio::io::AsyncRead {
    let chunks = response
        .into_body()
        .into_data_stream()
        .map_err(|e| std::io::Error::other(e.to_string()));
    tokio_util::io::StreamReader::new(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_carry_no_api_version_and_queries_are_form_encoded() {
        let docker = Docker::with_socket("/var/run/docker.sock");

        let uri = docker
            .uri(
                "/containers/json",
                Some(&[
                    ("all", "true"),
                    ("filters", r#"{"label":["turboci.runner=a b"]}"#),
                ]),
            )
            .unwrap();

        assert_eq!(
            uri.to_string(),
            "unix://2f7661722f72756e2f646f636b65722e736f636b/containers/json\
             ?all=true&filters=%7B%22label%22%3A%5B%22turboci.runner%3Da+b%22%5D%7D"
        );
        let image = docker
            .uri("/images/registry:5000/app:1/json", None)
            .unwrap();
        assert_eq!(image.path(), "/images/registry:5000/app:1/json");
        assert_eq!(image.query(), None);
    }

    #[test]
    fn error_answers_carry_the_status_and_dockerd_s_message() {
        let error = status_error(
            409,
            br#"{"message":"Conflict. The container name is in use"}"#,
        )
        .unwrap();
        assert_eq!(error.status_code(), Some(409));
        assert_eq!(
            error.to_string(),
            "Docker responded with status code 409: Conflict. The container name is in use"
        );

        let plain = status_error(500, b"page not found").unwrap();
        assert_eq!(
            plain.to_string(),
            "Docker responded with status code 500: page not found"
        );
        let empty = status_error(404, b"").unwrap();
        assert_eq!(empty.to_string(), "Docker responded with status code 404: ");
        assert_eq!(Error::Timeout.status_code(), None);
    }

    #[test]
    fn unexpected_responses_are_json_errors() {
        let error = parse_json::<ContainerCreateResponse>(b"{}").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Failed to deserialize JSON: missing field `Id` at line 1 column 2"
        );
        assert!(parse_json::<serde::de::IgnoredAny>(b"{\"Id\":").is_err());
    }

    #[test]
    fn registry_logins_are_sent_as_base64_json_with_every_field() {
        use base64::Engine;

        let credentials = DockerCredentials {
            username: Some("ci".to_string()),
            password: Some("p@ss".to_string()),
            serveraddress: Some("registry.corp".to_string()),
            ..Default::default()
        };

        let (name, value) = registry_auth(&credentials).unwrap();

        assert_eq!(name, "x-registry-auth");
        let json = base64::engine::general_purpose::STANDARD
            .decode(value.as_bytes())
            .unwrap();
        assert_eq!(
            String::from_utf8(json).unwrap(),
            r#"{"username":"ci","password":"p@ss","auth":null,"email":null,"serveraddress":"registry.corp","identitytoken":null,"registrytoken":null}"#
        );
    }

    #[test]
    fn container_bodies_leave_out_what_is_not_set() {
        let body = ContainerCreateBody {
            image: Some("alpine:3.20".to_string()),
            cmd: Some(vec!["sh".to_string()]),
            labels: Some(HashMap::from([(
                "turboci.runner".to_string(),
                "r1".to_string(),
            )])),
            host_config: Some(HostConfig {
                binds: Some(vec!["/w:/builds".to_string()]),
                network_mode: Some("bridge".to_string()),
                privileged: Some(false),
                memory: Some(268_435_456),
                mounts: Some(vec![Mount {
                    target: Some("/etc/docker/certs.d/r/ca.crt".to_string()),
                    source: Some("/ca.pem".to_string()),
                    typ: Some(MountType::Bind),
                    read_only: Some(true),
                }]),
                ..Default::default()
            }),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(HashMap::from([(
                    "net".to_string(),
                    EndpointConfig {
                        aliases: Some(vec!["db".to_string()]),
                    },
                )])),
            }),
            ..Default::default()
        };

        assert_eq!(
            serde_json::to_string(&body).unwrap(),
            r#"{"Cmd":["sh"],"Image":"alpine:3.20","Labels":{"turboci.runner":"r1"},"HostConfig":{"Memory":268435456,"Binds":["/w:/builds"],"NetworkMode":"bridge","Mounts":[{"Target":"/etc/docker/certs.d/r/ca.crt","Source":"/ca.pem","Type":"bind","ReadOnly":true}],"Privileged":false},"NetworkingConfig":{"EndpointsConfig":{"net":{"Aliases":["db"]}}}}"#
        );
        assert_eq!(
            serde_json::to_string(&ContainerCreateBody::default()).unwrap(),
            "{}"
        );
    }

    #[test]
    fn exec_network_and_volume_bodies_leave_out_what_is_not_set() {
        let exec = ExecConfig {
            cmd: Some(vec!["kill", "-KILL", "-1"]),
            user: Some("0"),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&exec).unwrap(),
            r#"{"AttachStdout":true,"AttachStderr":true,"Cmd":["kill","-KILL","-1"],"User":"0"}"#
        );
        let network = NetworkCreateRequest {
            name: "turboci-job-1".to_string(),
            driver: Some("bridge".to_string()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&network).unwrap(),
            r#"{"Name":"turboci-job-1","Driver":"bridge"}"#
        );
        let volume = VolumeCreateOptions {
            name: Some("turboci-cache".to_string()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&volume).unwrap(),
            r#"{"Name":"turboci-cache"}"#
        );
        assert!(serde_json::from_str::<serde_json::Value>(ATTACHED_EXEC_START).is_ok());
    }

    #[test]
    fn inspected_containers_read_only_what_the_runner_needs() {
        let info: ContainerInspectResponse = parse_json(
            br#"{"Id":"abc","Name":"/svc","State":{"Status":"running"},
                "Config":{"Env":["A=1"],"ExposedPorts":{"6379/tcp":{}},"Labels":{"turboci.runner":"r1"}},
                "NetworkSettings":{"Networks":{"net":{"Gateway":"172.18.0.1","IPAddress":"172.18.0.2"}}},
                "HostConfig":{"CpuShares":512,"Mounts":[{"Type":"unknown-kind"}]}}"#,
        )
        .unwrap();

        assert_eq!(info.name.as_deref(), Some("/svc"));
        let config = info.config.unwrap();
        assert!(config.exposed_ports.unwrap().contains_key("6379/tcp"));
        assert_eq!(config.labels.unwrap()["turboci.runner"], "r1");
        let endpoint = &info.network_settings.unwrap().networks.unwrap()["net"];
        assert_eq!(endpoint.gateway.as_deref(), Some("172.18.0.1"));
        assert_eq!(endpoint.ip_address.as_deref(), Some("172.18.0.2"));
        assert_eq!(info.host_config.unwrap().cpu_shares, Some(512));
    }

    #[test]
    fn docker_host_must_be_a_unix_socket() {
        for unset in [None, Some(""), Some("  ")] {
            assert_eq!(socket_path(unset).unwrap(), "/var/run/docker.sock");
        }
        assert_eq!(
            socket_path(Some("unix:///run/user/1000/docker.sock")).unwrap(),
            "/run/user/1000/docker.sock"
        );
        for unsupported in [
            "tcp://127.0.0.1:2375",
            "ssh://user@host",
            "npipe:////./pipe/docker_engine",
            "/var/run/docker.sock",
            "unix://",
        ] {
            let error = socket_path(Some(unsupported)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "DOCKER_HOST={} is not supported; use a unix socket \
                     (unix:///path/to/docker.sock)",
                    unsupported
                )
            );
        }
    }

    #[tokio::test]
    async fn a_missing_socket_fails_the_call() {
        let docker = Docker::with_socket("/nonexistent/docker.sock");

        let error = docker.info().await.unwrap_err();

        assert!(matches!(error, Error::Client(_)), "{:?}", error);
        assert!(
            error
                .to_string()
                .starts_with("Error in the hyper legacy client: "),
            "{}",
            error
        );
    }
}
