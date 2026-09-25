//! S3-compatible object storage for the job cache: path-style URLs, SigV4
//! header signing and the three calls the cache needs.

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use super::config::S3CacheConfig;

/// SHA-256 of an empty payload (GET and HEAD requests)
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// S3 accepts at most 5 GiB in one PUT
const MAX_SINGLE_PUT: u64 = 5 << 30;

const ATTEMPTS: u32 = 3;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn hmac_sha256(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode everything but unreserved characters and `/`, as SigV4's
/// canonical URI for S3 (encoded once, not twice)
fn encode_path(path: &str) -> String {
    path.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

/// AWS Signature Version 4 for the `s3` service
pub struct Signer<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
}

impl Signer<'_> {
    /// `Authorization` header for a request without a query string. `headers`
    /// are the signed headers: lowercase names, trimmed values, sorted by name.
    pub fn authorization(
        &self,
        method: &str,
        canonical_uri: &str,
        headers: &[(&str, &str)],
        payload_hash: &str,
        amz_date: &str,
    ) -> String {
        let date = &amz_date[..8];
        let scope = format!("{}/{}/s3/aws4_request", date, self.region);
        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{}:{}\n", name, value))
            .collect();
        let signed_headers = headers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(";");
        let canonical_request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method, canonical_uri, canonical_headers, signed_headers, payload_hash
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            hex(&Sha256::digest(canonical_request.as_bytes()))
        );
        let key = [date, self.region, "s3", "aws4_request"].iter().fold(
            format!("AWS4{}", self.secret_key).into_bytes(),
            |key, part| hmac_sha256(&key, part),
        );
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.access_key,
            scope,
            signed_headers,
            hex(&hmac_sha256(&key, &string_to_sign))
        )
    }
}

/// Where the cache lives: `http(s)://host[:port]/bucket[/prefix]`
#[derive(Debug, Clone)]
pub struct S3Location {
    /// Scheme, host and port only
    pub endpoint: reqwest::Url,
    pub bucket: String,
    /// Without leading or trailing `/`; empty for the bucket's root
    pub prefix: String,
    pub region: String,
}

impl S3Location {
    pub fn parse(url: &str, region: Option<&str>) -> Result<Self> {
        let form = "cache_s3.url must look like https://host[:port]/bucket[/prefix] (path-style)";
        let parsed = reqwest::Url::parse(url).with_context(|| form.to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            anyhow::bail!("{}, got {:?}", form, url);
        }
        let path = parsed.path().trim_matches('/');
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty() {
            anyhow::bail!("{}: the bucket is missing, got {:?}", form, url);
        }
        let host = parsed.host_str().unwrap_or_default().to_string();
        let region = region.map(str::to_string).unwrap_or_else(|| {
            host.strip_suffix(".amazonaws.com")
                .and_then(|rest| {
                    rest.strip_prefix("s3.")
                        .or_else(|| rest.strip_prefix("s3-"))
                })
                .filter(|region| !region.is_empty() && !region.contains('.'))
                .unwrap_or("us-east-1")
                .to_string()
        });
        let mut endpoint = parsed.clone();
        endpoint.set_path("/");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        Ok(Self {
            endpoint,
            bucket: percent_decode(bucket),
            prefix: percent_decode(prefix.trim_matches('/')),
            region,
        })
    }
}

/// Undo the URL parser's percent-encoding of the path (e.g. a space in a prefix)
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| text.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// When an upload body last handed a chunk to the connection. Without a
/// body it stays at the start of the request.
#[derive(Clone)]
struct Progress(Arc<Mutex<Instant>>);

impl Progress {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn bump(&self) {
        if let Ok(mut last) = self.0.lock() {
            *last = Instant::now();
        }
    }

    fn idle(&self) -> Duration {
        self.0.lock().map(|last| last.elapsed()).unwrap_or_default()
    }
}

fn stalled(limit: Duration) -> anyhow::Error {
    anyhow::anyhow!("S3 stalled: no data for {:?}", limit)
}

/// Run `fut` until it finishes, or fail once `progress` has not moved for
/// `limit`. There is no total timeout: a large upload may take as long as it
/// keeps moving.
async fn until_stalled<F: Future>(
    fut: F,
    progress: &Progress,
    limit: Duration,
) -> Result<F::Output> {
    let mut check =
        tokio::time::interval((limit / 4).clamp(Duration::from_millis(1), Duration::from_secs(1)));
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            output = &mut fut => return Ok(output),
            _ = check.tick() => {
                if progress.idle() > limit {
                    return Err(stalled(limit));
                }
            }
        }
    }
}

/// Result of a conditional download
#[derive(Debug)]
pub enum Download {
    /// The local copy with the given ETag is current (304)
    NotModified,
    /// No such object (404)
    NotFound,
    /// The object, in a temporary file inside the requested directory
    Downloaded {
        file: tempfile::NamedTempFile,
        etag: Option<String>,
        bytes: u64,
    },
}

/// A bucket (and prefix) holding cache archives. `Debug` is written by hand
/// so the secret key is never printed.
#[derive(Clone)]
pub struct Bucket {
    client: reqwest::Client,
    location: S3Location,
    access_key: String,
    secret_key: String,
    retry_delay: Duration,
    /// A transfer fails after this long without data moving
    stall_timeout: Duration,
    /// How long calls are skipped after S3 was unreachable
    cooldown: Duration,
    /// Until when calls are skipped, and why; shared by clones, so one job's
    /// failure spares the others the same timeouts
    breaker: Arc<Mutex<Option<(Instant, String)>>>,
}

impl std::fmt::Debug for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bucket")
            .field("location", &self.location)
            .field("access_key", &self.access_key)
            .finish_non_exhaustive()
    }
}

impl Bucket {
    /// `http` carries the runner's proxy and CA; a connect timeout is added
    /// here. Transfers have no total timeout, only a stall timeout.
    pub fn new(config: &S3CacheConfig, http: reqwest::ClientBuilder) -> Result<Self> {
        let location = S3Location::parse(&config.url, config.region.as_deref())?;
        let client = http
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("Failed to build the S3 HTTP client")?;
        Ok(Self {
            client,
            location,
            access_key: config.access_key.clone(),
            secret_key: config.secret_key.clone(),
            retry_delay: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(60),
            cooldown: Duration::from_secs(60),
            breaker: Arc::default(),
        })
    }

    /// Shorter waits between attempts (tests)
    #[cfg(test)]
    pub fn with_retry_delay(mut self, delay: Duration) -> Self {
        self.retry_delay = delay;
        self
    }

    /// Shorter stall timeout (tests)
    #[cfg(test)]
    pub fn with_stall_timeout(mut self, limit: Duration) -> Self {
        self.stall_timeout = limit;
        self
    }

    /// Shorter pause after an outage (tests)
    #[cfg(test)]
    pub fn with_cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// Fail at once while S3 is known to be down, instead of waiting through
    /// the same timeouts and retries on every call
    fn check_breaker(&self) -> Result<()> {
        let Ok(breaker) = self.breaker.lock() else {
            return Ok(());
        };
        match &*breaker {
            Some((until, error)) if *until > Instant::now() => {
                let left = until.saturating_duration_since(Instant::now());
                anyhow::bail!(
                    "S3 skipped for the next {}s after: {}",
                    left.as_secs_f64().ceil() as u64,
                    error
                )
            }
            _ => Ok(()),
        }
    }

    /// Skip calls for the cooldown after a network-level failure
    fn trip(&self, error: anyhow::Error) -> anyhow::Error {
        if let Ok(mut breaker) = self.breaker.lock() {
            *breaker = Some((Instant::now() + self.cooldown, format!("{:#}", error)));
        }
        error
    }

    /// `<prefix>/project-<id>/<file_name>`
    pub fn object_key(&self, project_id: u64, file_name: &str) -> String {
        let key = format!("project-{}/{}", project_id, file_name);
        if self.location.prefix.is_empty() {
            key
        } else {
            format!("{}/{}", self.location.prefix, key)
        }
    }

    /// URL path of an object (or of the bucket for an empty key), encoded once
    fn object_path(&self, key: &str) -> String {
        let raw = if key.is_empty() {
            format!("/{}", self.location.bucket)
        } else {
            format!("/{}/{}", self.location.bucket, key)
        };
        encode_path(&raw)
    }

    fn host_header(&self) -> String {
        let url = &self.location.endpoint;
        let host = url.host_str().unwrap_or_default();
        match url.port() {
            Some(port) => format!("{}:{}", host, port),
            None => host.to_string(),
        }
    }

    /// A signed request; `body` is a file for PUT
    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        payload_hash: &str,
        extra: &[(&str, String)],
        body: Option<&Path>,
    ) -> Result<reqwest::Response> {
        let path = self.object_path(key);
        let url = format!(
            "{}{}",
            self.location.endpoint.as_str().trim_end_matches('/'),
            path
        );
        self.check_breaker()?;
        let mut last_error = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay * attempt).await;
            }
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let host = self.host_header();
            let authorization = Signer {
                access_key: &self.access_key,
                secret_key: &self.secret_key,
                region: &self.location.region,
            }
            .authorization(
                method.as_str(),
                &path,
                &[
                    ("host", host.as_str()),
                    ("x-amz-content-sha256", payload_hash),
                    ("x-amz-date", amz_date.as_str()),
                ],
                payload_hash,
                &amz_date,
            );
            let mut request = self
                .client
                .request(method.clone(), &url)
                .header("x-amz-content-sha256", payload_hash)
                .header("x-amz-date", &amz_date)
                .header("authorization", authorization);
            for (name, value) in extra {
                request = request.header(*name, value);
            }
            let progress = Progress::new();
            if let Some(file) = body {
                let opened = tokio::fs::File::open(file)
                    .await
                    .with_context(|| format!("Failed to open {}", file.display()))?;
                let len = opened.metadata().await?.len();
                // Streamed in 64 KiB pieces; S3 needs the length up front
                // (it rejects chunked uploads)
                let stream = futures_util::stream::unfold(
                    (opened, progress.clone()),
                    |(mut file, progress)| async move {
                        use tokio::io::AsyncReadExt;
                        let mut buf = vec![0u8; 64 * 1024];
                        match file.read(&mut buf).await {
                            Ok(0) => None,
                            Ok(n) => {
                                buf.truncate(n);
                                progress.bump();
                                Some((Ok::<Vec<u8>, std::io::Error>(buf), (file, progress)))
                            }
                            Err(e) => Some((Err(e), (file, progress))),
                        }
                    },
                );
                request = request
                    .header("content-length", len)
                    .body(reqwest::Body::wrap_stream(stream));
            }
            // The reply must start within the stall timeout of the last
            // chunk sent (or of the request, without a body)
            let sent = until_stalled(request.send(), &progress, self.stall_timeout)
                .await
                .and_then(|sent| sent.context("cannot reach S3"));
            match sent {
                Ok(response) if response.status().is_server_error() => {
                    last_error = Some(anyhow::anyhow!("S3 answered {}", response.status()));
                }
                Ok(response) => {
                    // S3 answered: it is reachable again (4xx included)
                    if let Ok(mut breaker) = self.breaker.lock() {
                        *breaker = None;
                    }
                    return Ok(response);
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(self.trip(last_error.unwrap_or_else(|| anyhow::anyhow!("S3 request failed"))))
    }

    /// Download `key` into a temporary file in `dir`, unless `etag` is current
    pub async fn get_to_file(&self, key: &str, etag: Option<&str>, dir: &Path) -> Result<Download> {
        let extra: Vec<(&str, String)> = etag
            .map(|etag| vec![("if-none-match", etag.to_string())])
            .unwrap_or_default();
        let mut response = self
            .send(reqwest::Method::GET, key, EMPTY_SHA256, &extra, None)
            .await?;
        match response.status().as_u16() {
            304 => return Ok(Download::NotModified),
            404 => return Ok(Download::NotFound),
            200 => {}
            _ => return Err(status_error(response).await),
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let file = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create a file in {}", dir.display()))?;
        let mut out = tokio::fs::File::from_std(file.reopen()?);
        let mut bytes = 0u64;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = tokio::time::timeout(self.stall_timeout, response.chunk())
            .await
            .map_err(|_| stalled(self.stall_timeout))
            .and_then(|chunk| chunk.context("S3 download interrupted"))
            .map_err(|e| self.trip(e))?
        {
            out.write_all(&chunk).await?;
            bytes += chunk.len() as u64;
        }
        out.flush().await?;
        Ok(Download::Downloaded { file, etag, bytes })
    }

    /// Upload the file at `path` as `key`; returns the new object's ETag
    pub async fn put_file(&self, key: &str, path: &Path) -> Result<Option<String>> {
        // Before hashing: a large archive takes a while
        self.check_breaker()?;
        let len = tokio::fs::metadata(path).await?.len();
        if len > MAX_SINGLE_PUT {
            anyhow::bail!(
                "the archive is {} MB, above S3's 5 GiB single-upload limit",
                len >> 20
            );
        }
        let owned = path.to_path_buf();
        let hash = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut file = std::fs::File::open(&owned)?;
            let mut hasher = Sha256::new();
            std::io::copy(&mut file, &mut hasher)?;
            Ok(hex(&hasher.finalize()))
        })
        .await??;
        let response = self
            .send(reqwest::Method::PUT, key, &hash, &[], Some(path))
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        Ok(response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string))
    }

    /// Whether the bucket can be used, as a message for the runner log
    pub async fn probe(&self) -> std::result::Result<(), String> {
        let bucket = &self.location.bucket;
        let endpoint = self.location.endpoint.as_str();
        match self
            .send(reqwest::Method::HEAD, "", EMPTY_SHA256, &[], None)
            .await
        {
            Ok(response) => match response.status().as_u16() {
                200..=299 => Ok(()),
                404 => Err(format!("bucket {} not found at {}", bucket, endpoint)),
                403 => Err(format!(
                    "access to bucket {} denied (check access_key and secret_key)",
                    bucket
                )),
                301 => Err(format!(
                    "bucket {} is in another region: set cache_s3.region",
                    bucket
                )),
                status => Err(format!("bucket {} answered {}", bucket, status)),
            },
            Err(e) => Err(format!("cannot reach {}: {:#}", endpoint, e)),
        }
    }

    /// Create the bucket (tests against a real server)
    #[cfg(test)]
    pub async fn create_bucket(&self) -> Result<()> {
        let response = self
            .send(reqwest::Method::PUT, "", EMPTY_SHA256, &[], None)
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response).await);
        }
        Ok(())
    }
}

/// An error naming the status and S3's error code (`<Code>...</Code>`)
async fn status_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let code = body
        .split_once("<Code>")
        .and_then(|(_, rest)| rest.split_once("</Code>"))
        .map(|(code, _)| format!(" ({})", code))
        .unwrap_or_default();
    anyhow::anyhow!("S3 answered {}{}", status, code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The "GET Object" example of AWS's SigV4 documentation
    /// (docs.aws.amazon.com/AmazonS3/latest/API/sig-v4-header-based-auth.html)
    #[test]
    fn signs_the_aws_documentation_example() {
        let signer = Signer {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
        };
        let authorization = signer.authorization(
            "GET",
            "/test.txt",
            &[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY_SHA256),
                ("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY_SHA256,
            "20130524T000000Z",
        );

        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn parses_path_style_urls_and_regions() {
        let minio = S3Location::parse("http://minio.corp:9000/ci-cache/team-a/", None).unwrap();
        assert_eq!(minio.endpoint.as_str(), "http://minio.corp:9000/");
        assert_eq!(
            (minio.bucket.as_str(), minio.prefix.as_str()),
            ("ci-cache", "team-a")
        );
        assert_eq!(minio.region, "us-east-1");

        let aws =
            S3Location::parse("https://s3.eu-central-1.amazonaws.com/ci-cache", None).unwrap();
        assert_eq!(aws.region, "eu-central-1");
        assert_eq!(aws.prefix, "");
        let old_style = S3Location::parse("https://s3-eu-west-1.amazonaws.com/b", None).unwrap();
        assert_eq!(old_style.region, "eu-west-1");
        let r2 = S3Location::parse("https://acc.r2.cloudflarestorage.com/b", Some("auto")).unwrap();
        assert_eq!(r2.region, "auto");
    }

    #[test]
    fn rejects_urls_without_bucket() {
        for bad in [
            "https://my-bucket.s3.eu-central-1.amazonaws.com/",
            "https://minio.corp:9000",
            "minio.corp:9000/bucket",
            "ftp://minio.corp/bucket",
        ] {
            let error = S3Location::parse(bad, None).unwrap_err().to_string();
            assert!(
                error.contains("https://host[:port]/bucket"),
                "{}: {}",
                bad,
                error
            );
        }
    }

    fn bucket(server: &MockServer, prefix: &str) -> Bucket {
        Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: format!("{}/ci-cache{}", server.uri(), prefix),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::builder(),
        )
        .unwrap()
        .with_retry_delay(std::time::Duration::from_millis(10))
    }

    #[test]
    fn object_paths_are_percent_encoded_once() {
        let server_uri = "http://127.0.0.1:9";
        let b = Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: format!("{}/ci-cache/team a", server_uri),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::builder(),
        )
        .unwrap();
        let key = b.object_key(7, "feature%2Fx%20y%25.zip");
        assert_eq!(key, "team a/project-7/feature%2Fx%20y%25.zip");
        assert_eq!(
            b.object_path(&key),
            "/ci-cache/team%20a/project-7/feature%252Fx%2520y%2525.zip"
        );
    }

    #[tokio::test]
    async fn conditional_get_and_download() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/main.zip"))
            .and(header("if-none-match", "\"abc\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/main.zip"))
            .and(header_exists("authorization"))
            .and(header_exists("x-amz-date"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"def\"")
                    .set_body_bytes(b"zip-bytes".to_vec()),
            )
            .mount(&server)
            .await;
        let b = bucket(&server, "");
        let dir = tempfile::tempdir().unwrap();
        let key = b.object_key(1, "main.zip");

        assert!(matches!(
            b.get_to_file(&key, Some("\"abc\""), dir.path())
                .await
                .unwrap(),
            Download::NotModified
        ));
        match b.get_to_file(&key, None, dir.path()).await.unwrap() {
            Download::Downloaded { file, etag, bytes } => {
                assert_eq!(std::fs::read(file.path()).unwrap(), b"zip-bytes");
                assert_eq!(etag.as_deref(), Some("\"def\""));
                assert_eq!(bytes, 9);
            }
            other => panic!("{:?}", other),
        }
    }

    #[tokio::test]
    async fn missing_objects_and_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/gone.zip"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/denied.zip"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code></Error>"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/flaky.zip"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        let b = bucket(&server, "");
        let dir = tempfile::tempdir().unwrap();

        assert!(matches!(
            b.get_to_file(&b.object_key(1, "gone.zip"), None, dir.path())
                .await
                .unwrap(),
            Download::NotFound
        ));
        let denied = b
            .get_to_file(&b.object_key(1, "denied.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(
            format!("{:#}", denied).contains("AccessDenied"),
            "{:#}",
            denied
        );
        assert!(b
            .get_to_file(&b.object_key(1, "flaky.zip"), None, dir.path())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn put_sends_the_file_with_its_hash() {
        let server = MockServer::start().await;
        let body = b"archive".to_vec();
        let hash = hex(&Sha256::digest(&body));
        Mock::given(method("PUT"))
            .and(path("/ci-cache/pre/project-3/k.zip"))
            .and(header("x-amz-content-sha256", hash.as_str()))
            .and(header("content-length", "7"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"e1\""))
            .expect(1)
            .mount(&server)
            .await;
        let b = bucket(&server, "/pre");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("k.zip");
        std::fs::write(&file, &body).unwrap();

        let etag = b.put_file(&b.object_key(3, "k.zip"), &file).await.unwrap();

        assert_eq!(etag.as_deref(), Some("\"e1\""));
        let received = &server.received_requests().await.unwrap()[0];
        assert_eq!(received.body, body);
    }

    #[tokio::test]
    async fn probe_explains_the_bucket_state() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/ci-cache"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let message = bucket(&server, "").probe().await.unwrap_err();

        assert!(message.contains("not found"), "{}", message);
        let unreachable = Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: "http://127.0.0.1:9/ci-cache".to_string(),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::builder(),
        )
        .unwrap()
        .with_retry_delay(std::time::Duration::from_millis(10));
        assert!(unreachable
            .probe()
            .await
            .unwrap_err()
            .contains("cannot reach"));
    }

    #[tokio::test]
    async fn watchdog_fails_only_without_progress() {
        let limit = Duration::from_millis(300);
        let progress = Progress::new();
        let working = async {
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                progress.bump();
            }
        };
        assert!(until_stalled(working, &progress, limit).await.is_ok());

        let started = std::time::Instant::now();
        let silent = tokio::time::sleep(Duration::from_secs(1));
        let error = until_stalled(silent, &Progress::new(), limit)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stalled"), "{}", error);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn stalled_replies_fail_and_slow_ones_do_not() {
        let server = MockServer::start().await;
        for (name, delay) in [("slow.zip", 50), ("stuck.zip", 1000)] {
            Mock::given(method("GET"))
                .and(path(format!("/ci-cache/project-1/{}", name)))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(b"zip".to_vec())
                        .set_delay(Duration::from_millis(delay)),
                )
                .mount(&server)
                .await;
        }
        Mock::given(method("PUT"))
            .and(path("/ci-cache/project-1/stuck.zip"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .mount(&server)
            .await;
        let b = bucket(&server, "").with_stall_timeout(Duration::from_millis(300));
        let dir = tempfile::tempdir().unwrap();

        assert!(matches!(
            b.get_to_file(&b.object_key(1, "slow.zip"), None, dir.path())
                .await
                .unwrap(),
            Download::Downloaded { bytes: 3, .. }
        ));
        let stuck = b
            .get_to_file(&b.object_key(1, "stuck.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(stuck.to_string().contains("stalled"), "{:#}", stuck);
        let file = dir.path().join("stuck.zip");
        std::fs::write(&file, b"zip").unwrap();
        // A new bucket: the stalled GET opened this one's breaker
        let b = bucket(&server, "").with_stall_timeout(Duration::from_millis(300));
        let upload = b
            .put_file(&b.object_key(1, "stuck.zip"), &file)
            .await
            .unwrap_err();
        assert!(upload.to_string().contains("S3 stalled"), "{:#}", upload);
    }

    #[tokio::test]
    async fn an_outage_stops_further_calls_until_the_cooldown_ends() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/down.zip"))
            .respond_with(ResponseTemplate::new(503))
            .expect(3)
            .mount(&server)
            .await;
        let b = bucket(&server, "");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("k.zip");
        std::fs::write(&file, b"zip").unwrap();

        let outage = b
            .get_to_file(&b.object_key(1, "down.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(outage.to_string().contains("503"), "{:#}", outage);

        // Clones share the breaker: nothing reaches S3 during the cooldown
        let other = b.clone();
        let skipped = other
            .get_to_file(&other.object_key(1, "other.zip"), None, dir.path())
            .await
            .unwrap_err()
            .to_string();
        assert!(
            skipped.starts_with("S3 skipped for the next") && skipped.contains("503"),
            "{}",
            skipped
        );
        let upload = other
            .put_file(&other.object_key(1, "k.zip"), &file)
            .await
            .unwrap_err();
        assert!(upload.to_string().contains("S3 skipped"), "{:#}", upload);
        let probe = other.probe().await.unwrap_err();
        assert!(probe.contains("S3 skipped"), "{}", probe);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn calls_reach_s3_again_after_the_cooldown() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/down.zip"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/back.zip"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let b = bucket(&server, "").with_cooldown(Duration::from_millis(50));
        let dir = tempfile::tempdir().unwrap();

        assert!(b
            .get_to_file(&b.object_key(1, "down.zip"), None, dir.path())
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert!(matches!(
            b.get_to_file(&b.object_key(1, "back.zip"), None, dir.path())
                .await
                .unwrap(),
            Download::NotFound
        ));
    }

    #[tokio::test]
    async fn client_errors_do_not_open_the_breaker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/denied.zip"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code></Error>"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/gone.zip"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let b = bucket(&server, "");
        let dir = tempfile::tempdir().unwrap();

        let denied = b
            .get_to_file(&b.object_key(1, "denied.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(denied.to_string().contains("AccessDenied"), "{:#}", denied);

        assert!(matches!(
            b.get_to_file(&b.object_key(1, "gone.zip"), None, dir.path())
                .await
                .unwrap(),
            Download::NotFound
        ));
    }

    #[tokio::test]
    async fn a_download_that_stalls_midway_opens_the_breaker() {
        use tokio::io::AsyncWriteExt;
        // Sends the headers and part of the body, then goes silent
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npart")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let b = Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: format!("http://{}/ci-cache", address),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::builder(),
        )
        .unwrap()
        .with_stall_timeout(Duration::from_millis(200));
        let dir = tempfile::tempdir().unwrap();

        let stalled = b
            .get_to_file(&b.object_key(1, "big.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(stalled.to_string().contains("S3 stalled"), "{:#}", stalled);

        let skipped = b
            .get_to_file(&b.object_key(1, "next.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(skipped.to_string().contains("S3 skipped"), "{:#}", skipped);
        server.abort();
    }
}
