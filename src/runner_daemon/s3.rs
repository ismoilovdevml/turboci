//! S3-compatible object storage for the job cache: path-style URLs, SigV4
//! header signing and the calls the cache needs (GET, HEAD, PUT, and the
//! multipart upload of large archives).

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

use super::config::S3CacheConfig;

/// SHA-256 of an empty payload (GET and HEAD requests)
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// User metadata holding the SHA-256 of an uploaded archive: the same in a
/// single PUT and a multipart upload, whose ETags differ for one content
pub const META_SHA256: &str = "x-amz-meta-sha256";

/// Archives larger than this are uploaded in parts. A single PUT takes at
/// most 5 GiB and must start over after any failure.
const MULTIPART_THRESHOLD: u64 = 64 << 20;

/// Smallest part uploaded (S3 needs 5 MiB but for the last part); larger
/// for archives that would need more than `MAX_PARTS` parts
const MIN_PART_SIZE: u64 = 16 << 20;

/// S3's limits on a multipart upload
const MAX_PARTS: u64 = 10_000;
const MAX_OBJECT_SIZE: u64 = 5 << 40;

/// Parts uploaded at the same time
const PART_CONCURRENCY: usize = 4;

/// Longest XML reply read (CreateMultipartUpload, CompleteMultipartUpload)
const MAX_REPLY: usize = 1 << 20;

/// Size of the pieces an upload is read in, and of the download's write buffer
const IO_CHUNK: usize = 256 * 1024;

const ATTEMPTS: u32 = 3;

/// Replaces the shared client's total timeout (meant for GitLab API calls)
/// on every S3 request: a transfer may take as long as it keeps moving, and
/// the stall timeout ends one that does not. Not `Duration::MAX`, which
/// would rely on every layer saturating the deadline.
const NO_TOTAL_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);

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

/// A query string as sent and as signed: parameters sorted by name, names
/// and values percent-encoded but for unreserved characters. A parameter
/// with an empty value is sent bare (`?uploads`) and signed as `uploads=`.
fn encode_query(params: &[(&str, &str)]) -> (String, String) {
    let encode = |text: &str| -> String {
        text.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{:02X}", b),
            })
            .collect()
    };
    let mut encoded: Vec<(String, String)> = params
        .iter()
        .map(|(name, value)| (encode(name), encode(value)))
        .collect();
    encoded.sort();
    let sent = encoded
        .iter()
        .map(|(name, value)| match value.as_str() {
            "" => name.clone(),
            _ => format!("{}={}", name, value),
        })
        .collect::<Vec<_>>()
        .join("&");
    let signed = encoded
        .iter()
        .map(|(name, value)| format!("{}={}", name, value))
        .collect::<Vec<_>>()
        .join("&");
    (sent, signed)
}

/// AWS Signature Version 4 for the `s3` service
pub struct Signer<'a> {
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
}

impl Signer<'_> {
    /// `Authorization` header of a request. `canonical_query` comes from
    /// `encode_query`; `headers` are the signed headers: lowercase names,
    /// trimmed values, sorted by name.
    pub fn authorization(
        &self,
        method: &str,
        canonical_uri: &str,
        canonical_query: &str,
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
            "{}\n{}\n{}\n{}\n{}\n{}",
            method, canonical_uri, canonical_query, canonical_headers, signed_headers, payload_hash
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
        // Without echoing the URL: the runner logs it, and reqwest would send
        // the pair as a Basic Authorization header next to the signature
        if !parsed.username().is_empty() || parsed.password().is_some() {
            anyhow::bail!("cache_s3.url must not hold credentials: set access_key and secret_key");
        }
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            anyhow::bail!("{}, got {:?}", form, url);
        }
        let path = parsed.path().trim_matches('/');
        let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty() {
            anyhow::bail!("{}: the bucket is missing, got {:?}", form, url);
        }
        let host = parsed.host_str().unwrap_or_default().to_string();
        let region = match region.map(str::trim) {
            Some("") => anyhow::bail!("cache_s3.region must not be empty"),
            Some(region) => region.to_string(),
            None => host
                .strip_suffix(".amazonaws.com")
                .and_then(|rest| {
                    rest.strip_prefix("s3.")
                        .or_else(|| rest.strip_prefix("s3-"))
                })
                .filter(|region| !region.is_empty() && !region.contains('.'))
                .unwrap_or("us-east-1")
                .to_string(),
        };
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
/// body it stays at the start of the request. Bumped for every chunk, so
/// it is an atomic (nanoseconds after `start`) rather than a lock.
#[derive(Clone)]
struct Progress(Arc<(Instant, AtomicU64)>);

impl Progress {
    fn new() -> Self {
        Self(Arc::new((Instant::now(), AtomicU64::new(0))))
    }

    fn bump(&self) {
        let (start, last) = &*self.0;
        let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        last.store(elapsed, Ordering::Relaxed);
    }

    fn idle(&self) -> Duration {
        let (start, last) = &*self.0;
        start
            .elapsed()
            .saturating_sub(Duration::from_nanos(last.load(Ordering::Relaxed)))
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
        /// SHA-256 of the object (hex), computed while it was received
        sha256: String,
    },
}

/// What an object's HEAD tells about it
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObjectInfo {
    pub etag: Option<String>,
    /// `x-amz-meta-sha256`: the hash of the archive, set by the runners that
    /// upload it
    pub sha256: Option<String>,
}

/// The body of a request
#[derive(Clone, Copy)]
enum Payload<'a> {
    Empty,
    /// `len` bytes of `file` from `offset`, hashing to `sha256` (hex)
    File {
        file: &'a std::fs::File,
        offset: u64,
        len: u64,
        sha256: &'a str,
    },
    /// A short body (the XML of CompleteMultipartUpload)
    Bytes(&'a [u8]),
}

/// A part of a multipart upload: `len` bytes of the file from `offset`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Part {
    number: u32,
    offset: u64,
    len: u64,
}

/// Split `len` bytes into parts of `min_part` bytes (whole MiB), larger when
/// that would make more than `MAX_PARTS` parts; the last part takes the rest
fn plan_parts(len: u64, min_part: u64) -> Result<Vec<Part>> {
    if len > MAX_OBJECT_SIZE {
        anyhow::bail!(
            "the archive is {} GB, above S3's 5 TiB object limit",
            len >> 30
        );
    }
    const MIB: u64 = 1 << 20;
    let needed = len.div_ceil(MAX_PARTS);
    let size = if needed > min_part {
        needed.div_ceil(MIB) * MIB
    } else {
        min_part
    };
    let count = len.div_ceil(size).max(1);
    Ok((0..count)
        .map(|index| Part {
            number: index as u32 + 1,
            offset: index * size,
            len: size.min(len - index * size),
        })
        .collect())
}

/// SHA-256 (hex) of `len` bytes of `file` from `offset`, read at offsets
/// off the async runtime
async fn hash_range(file: &std::fs::File, offset: u64, len: u64) -> Result<String> {
    let source = file.try_clone().context("Failed to open the archive")?;
    tokio::task::spawn_blocking(move || -> Result<String> {
        use std::os::unix::fs::FileExt;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; IO_CHUNK];
        let mut done = 0u64;
        while done < len {
            let want = (len - done).min(IO_CHUNK as u64) as usize;
            let n = source.read_at(&mut buf[..want], offset + done)?;
            if n == 0 {
                anyhow::bail!("the archive shrank while it was uploaded");
            }
            hasher.update(&buf[..n]);
            done += n as u64;
        }
        Ok(hex(&hasher.finalize()))
    })
    .await
    .context("Hashing task panicked")?
}

/// The text of `<tag>` in an S3 XML reply, unescaped
fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let (_, rest) = xml.split_once(&format!("<{}>", tag))?;
    let (value, _) = rest.split_once(&format!("</{}>", tag))?;
    Some(
        value
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The body of CompleteMultipartUpload; `parts` are in order
fn complete_multipart_xml(parts: &[(u32, String)]) -> String {
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (number, etag) in parts {
        xml.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            number,
            xml_escape(etag)
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

/// A multipart upload that is aborted if it is dropped before it completed
/// or was aborted (the job was canceled while uploading): S3 would keep
/// its parts, and bill them, until a lifecycle rule removes them
struct PendingUpload {
    bucket: Bucket,
    key: String,
    upload_id: Option<String>,
}

impl PendingUpload {
    fn completed(mut self) {
        self.upload_id = None;
    }

    async fn abort(mut self) {
        if let Some(upload_id) = self.upload_id.take() {
            self.bucket.abort_multipart(&self.key, &upload_id).await;
        }
    }
}

impl Drop for PendingUpload {
    fn drop(&mut self) {
        let Some(upload_id) = self.upload_id.take() else {
            return;
        };
        let (bucket, key) = (self.bucket.clone(), std::mem::take(&mut self.key));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { bucket.abort_multipart(&key, &upload_id).await });
        }
    }
}

/// Result of a conditional GET
pub enum Get<'a> {
    /// The local copy with the given ETag is current (304)
    NotModified,
    /// No such object (404)
    NotFound,
    Found(Object<'a>),
}

/// An object being downloaded
pub struct Object<'a> {
    response: reqwest::Response,
    bucket: &'a Bucket,
    pub etag: Option<String>,
    /// The hash runners store with an archive (`x-amz-meta-sha256`)
    pub sha256: Option<String>,
}

impl Object<'_> {
    /// The next piece of the body, `None` at its end. A stall or a dropped
    /// connection opens the breaker.
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>> {
        let limit = self.bucket.stall_timeout;
        tokio::time::timeout(limit, self.response.chunk())
            .await
            .map_err(|_| stalled(limit))
            .and_then(|chunk| {
                chunk
                    .map_err(reqwest::Error::without_url)
                    .context("S3 download interrupted")
            })
            .map_err(|e| self.bucket.trip(e))
    }

    /// Fail when the body received, hashing to `sha256`, is not the archive
    /// that was uploaded (when the uploader stored its hash)
    pub fn check(&self, sha256: &str) -> Result<()> {
        match &self.sha256 {
            Some(expected) if expected != sha256 => {
                anyhow::bail!("the archive downloaded from S3 does not match its hash")
            }
            _ => Ok(()),
        }
    }
}

/// The body of an upload: `len` bytes of `file` from `offset`, read in
/// `IO_CHUNK` pieces at increasing offsets (never through the shared file
/// position, so an attempt that is still being torn down cannot move the
/// next one's, and parts can be read at the same time), bumping `progress`
/// for every piece
fn upload_stream(
    file: &std::fs::File,
    start: u64,
    len: u64,
    progress: Progress,
) -> Result<impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static> {
    let source = Arc::new(file.try_clone().context("Failed to open the archive")?);
    let end = start + len;
    let stream = futures_util::stream::unfold((start, progress), move |(offset, progress)| {
        let source = source.clone();
        async move {
            if offset >= end {
                return None;
            }
            let want = (end - offset).min(IO_CHUNK as u64) as usize;
            let read = tokio::task::spawn_blocking(move || {
                use std::os::unix::fs::FileExt;
                let mut buf = vec![0u8; want];
                let n = source.read_at(&mut buf, offset)?;
                buf.truncate(n);
                Ok(buf)
            })
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(e)));
            match read {
                Ok(buf) if buf.is_empty() => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the archive shrank while it was uploaded",
                    )),
                    (end, progress),
                )),
                Ok(buf) => {
                    progress.bump();
                    let next = offset + buf.len() as u64;
                    Some((Ok(buf), (next, progress)))
                }
                // Ends the stream after the error
                Err(e) => Some((Err(e), (end, progress))),
            }
        }
    });
    Ok(stream)
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
    /// Uploads larger than this go in parts of at least `part_size` bytes
    multipart_threshold: u64,
    part_size: u64,
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
    /// `client` is the runner's shared HTTP client (proxy, CA, connect
    /// timeout). Its total timeout, meant for API calls, is replaced on
    /// every request: transfers have only a stall timeout.
    pub fn new(config: &S3CacheConfig, client: reqwest::Client) -> Result<Self> {
        let location = S3Location::parse(&config.url, config.region.as_deref())?;
        Ok(Self {
            client,
            location,
            access_key: config.access_key.clone(),
            secret_key: config.secret_key.clone(),
            retry_delay: Duration::from_secs(1),
            stall_timeout: Duration::from_secs(60),
            cooldown: Duration::from_secs(60),
            breaker: Arc::default(),
            multipart_threshold: MULTIPART_THRESHOLD,
            part_size: MIN_PART_SIZE,
        })
    }

    /// Multipart uploads of smaller archives (tests)
    #[cfg(test)]
    pub fn with_multipart(mut self, threshold: u64, part_size: u64) -> Self {
        self.multipart_threshold = threshold;
        self.part_size = part_size;
        self
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
    /// the same timeouts and retries on every call. Once the cooldown is
    /// over, the first call with `claim` is a trial (`true`): it keeps the
    /// breaker open for another cooldown, so concurrent calls keep skipping
    /// until the trial closes it or trips it again.
    fn check_breaker(&self, claim: bool) -> Result<bool> {
        let Ok(mut breaker) = self.breaker.lock() else {
            return Ok(false);
        };
        let Some((until, error)) = breaker.as_mut() else {
            return Ok(false);
        };
        let now = Instant::now();
        if *until > now {
            anyhow::bail!(
                "S3 skipped for the next {}s after: {}",
                until.saturating_duration_since(now).as_secs_f64().ceil() as u64,
                error
            );
        }
        if claim {
            *until = now + self.cooldown;
        }
        Ok(claim)
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

    /// A signed request with `query` parameters and `headers` (those named
    /// `x-amz-*` are signed too), retried on 5xx answers and dropped
    /// connections
    async fn send(
        &self,
        method: reqwest::Method,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        payload: Payload<'_>,
    ) -> Result<reqwest::Response> {
        let path = self.object_path(key);
        let (query, canonical_query) = encode_query(query);
        let mut url = format!(
            "{}{}",
            self.location.endpoint.as_str().trim_end_matches('/'),
            path
        );
        if !query.is_empty() {
            url.push('?');
            url.push_str(&query);
        }
        let payload_hash = match payload {
            Payload::Empty => EMPTY_SHA256.to_string(),
            Payload::File { sha256, .. } => sha256.to_string(),
            Payload::Bytes(bytes) => hex(&Sha256::digest(bytes)),
        };
        let mut last_error = None;
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(self.retry_delay * attempt).await;
            }
            // Before every attempt: another call may have tripped the
            // breaker while this one was retrying
            let trial = self.check_breaker(true)?;
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let host = self.host_header();
            let mut signed: Vec<(&str, &str)> = vec![
                ("host", host.as_str()),
                ("x-amz-content-sha256", payload_hash.as_str()),
                ("x-amz-date", amz_date.as_str()),
            ];
            signed.extend(
                headers
                    .iter()
                    .filter(|(name, _)| name.starts_with("x-amz-"))
                    .map(|(name, value)| (*name, value.trim())),
            );
            signed.sort();
            let authorization = Signer {
                access_key: &self.access_key,
                secret_key: &self.secret_key,
                region: &self.location.region,
            }
            .authorization(
                method.as_str(),
                &path,
                &canonical_query,
                &signed,
                &payload_hash,
                &amz_date,
            );
            let mut request = self
                .client
                .request(method.clone(), &url)
                .timeout(NO_TOTAL_TIMEOUT)
                .header("x-amz-content-sha256", payload_hash.as_str())
                .header("x-amz-date", &amz_date)
                .header("authorization", authorization);
            for (name, value) in headers {
                request = request.header(*name, value.trim());
            }
            let progress = Progress::new();
            match payload {
                Payload::Empty => {}
                Payload::File {
                    file, offset, len, ..
                } => {
                    let stream = upload_stream(file, offset, len, progress.clone())?;
                    // S3 needs the length up front (it rejects chunked uploads)
                    request = request
                        .header("content-length", len)
                        .body(reqwest::Body::wrap_stream(stream));
                }
                Payload::Bytes(bytes) => request = request.body(bytes.to_vec()),
            }
            // The reply must start within the stall timeout of the last
            // chunk sent (or of the request, without a body). The URL is
            // dropped from errors: it names a project and its cache key,
            // and the breaker repeats the error to every job.
            let sent = until_stalled(request.send(), &progress, self.stall_timeout).await;
            match sent {
                Ok(Ok(response)) if response.status().is_server_error() => {
                    last_error = Some(anyhow::anyhow!("S3 answered {}", response.status()));
                }
                Ok(Ok(response)) => {
                    // S3 answered: it is reachable again (4xx included)
                    if let Ok(mut breaker) = self.breaker.lock() {
                        *breaker = None;
                    }
                    return Ok(response);
                }
                Ok(Err(e)) => {
                    // No connection (refused, timed out, no route): a retry
                    // would wait the same way, so the breaker opens at once
                    let unreachable = e.is_connect();
                    let error = anyhow::Error::new(e.without_url()).context("cannot reach S3");
                    if unreachable {
                        return Err(self.trip(error));
                    }
                    last_error = Some(error);
                }
                // A stalled attempt has already cost a whole stall timeout
                Err(stalled) => return Err(self.trip(stalled)),
            }
            // A trial gets one attempt: its failure opens the breaker again
            if trial {
                break;
            }
        }
        Err(self.trip(last_error.unwrap_or_else(|| anyhow::anyhow!("S3 request failed"))))
    }

    /// The body of a short reply (XML), read with the stall timeout between
    /// chunks: CompleteMultipartUpload may take minutes, sending spaces to
    /// keep the connection open
    async fn read_reply(&self, mut response: reqwest::Response) -> Result<String> {
        let mut body = Vec::new();
        while let Some(chunk) = tokio::time::timeout(self.stall_timeout, response.chunk())
            .await
            .map_err(|_| stalled(self.stall_timeout))
            .and_then(|chunk| {
                chunk
                    .map_err(reqwest::Error::without_url)
                    .context("S3 reply interrupted")
            })
            .map_err(|e| self.trip(e))?
        {
            if body.len() + chunk.len() > MAX_REPLY {
                anyhow::bail!("S3 reply longer than {} bytes", MAX_REPLY);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Ask for `key`, unless `etag` is current; a found object's body is
    /// then read with `Object::chunk`
    pub async fn get(&self, key: &str, etag: Option<&str>) -> Result<Get<'_>> {
        let headers: Vec<(&str, &str)> = etag
            .map(|etag| vec![("if-none-match", etag)])
            .unwrap_or_default();
        let response = self
            .send(reqwest::Method::GET, key, &[], &headers, Payload::Empty)
            .await?;
        match response.status().as_u16() {
            304 => Ok(Get::NotModified),
            404 => Ok(Get::NotFound),
            200 => Ok(Get::Found(Object {
                etag: header(&response, "etag"),
                sha256: header(&response, META_SHA256),
                response,
                bucket: self,
            })),
            _ => Err(status_error(response, self.stall_timeout).await),
        }
    }

    /// Download `key` into a temporary file in `dir`, unless `etag` is current
    pub async fn get_to_file(&self, key: &str, etag: Option<&str>, dir: &Path) -> Result<Download> {
        let mut object = match self.get(key, etag).await? {
            Get::NotModified => return Ok(Download::NotModified),
            Get::NotFound => return Ok(Download::NotFound),
            Get::Found(object) => object,
        };
        let file = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create a file in {}", dir.display()))?;
        // Network chunks are small (often 16 KiB): written in larger pieces
        let mut out = tokio::io::BufWriter::with_capacity(
            IO_CHUNK,
            tokio::fs::File::from_std(file.reopen()?),
        );
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        use tokio::io::AsyncWriteExt;
        while let Some(chunk) = object.chunk().await? {
            hasher.update(&chunk);
            out.write_all(&chunk).await?;
            bytes += chunk.len() as u64;
        }
        // Writes the rest of the buffer: the file is complete before it is used
        out.flush().await?;
        let sha256 = hex(&hasher.finalize());
        object.check(&sha256)?;
        Ok(Download::Downloaded {
            file,
            etag: object.etag,
            bytes,
            sha256,
        })
    }

    /// The ETag and content hash of `key`, `None` when S3 does not hold it
    /// (a HEAD request, no body)
    pub async fn head(&self, key: &str) -> Result<Option<ObjectInfo>> {
        let response = self
            .send(reqwest::Method::HEAD, key, &[], &[], Payload::Empty)
            .await?;
        match response.status().as_u16() {
            200 => Ok(Some(ObjectInfo {
                etag: header(&response, "etag"),
                sha256: header(&response, META_SHA256),
            })),
            404 => Ok(None),
            _ => Err(status_error(response, self.stall_timeout).await),
        }
    }

    /// Upload `file` as `key`, in parts above the multipart threshold;
    /// returns the new object's ETag. `sha256` is the file's hash (hex),
    /// which the caller computed while writing it: a single PUT is signed
    /// without reading the file twice, and the hash is stored with the
    /// object (`x-amz-meta-sha256`). The file is read at offsets, so the
    /// handle may be shared and is never moved.
    pub async fn put_file(
        &self,
        key: &str,
        file: &std::fs::File,
        sha256: &str,
    ) -> Result<Option<String>> {
        self.check_breaker(false)?;
        let len = file.metadata()?.len();
        if len > self.multipart_threshold {
            return self.put_multipart(key, file, len, sha256).await;
        }
        let payload = Payload::File {
            file,
            offset: 0,
            len,
            sha256,
        };
        let response = self
            .send(
                reqwest::Method::PUT,
                key,
                &[],
                &[(META_SHA256, sha256)],
                payload,
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, self.stall_timeout).await);
        }
        Ok(header(&response, "etag"))
    }

    /// Upload `len` bytes of `file` in parts, `PART_CONCURRENCY` at a time.
    /// Any failure, or dropping the future, aborts the upload, so S3 keeps
    /// no orphaned parts.
    async fn put_multipart(
        &self,
        key: &str,
        file: &std::fs::File,
        len: u64,
        sha256: &str,
    ) -> Result<Option<String>> {
        let parts = plan_parts(len, self.part_size)?;
        let response = self
            .send(
                reqwest::Method::POST,
                key,
                &[("uploads", "")],
                &[(META_SHA256, sha256)],
                Payload::Empty,
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, self.stall_timeout).await);
        }
        let reply = self.read_reply(response).await?;
        let upload_id = xml_value(&reply, "UploadId")
            .filter(|id| !id.is_empty())
            .context("S3 started a multipart upload without an UploadId")?;
        let pending = PendingUpload {
            bucket: self.clone(),
            key: key.to_string(),
            upload_id: Some(upload_id.clone()),
        };
        match self.upload_parts(key, &upload_id, file, &parts).await {
            Ok(etag) => {
                pending.completed();
                Ok(etag)
            }
            Err(e) => {
                pending.abort().await;
                Err(e)
            }
        }
    }

    /// Send `parts` and complete the upload
    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        file: &std::fs::File,
        parts: &[Part],
    ) -> Result<Option<String>> {
        // Each part in a task of its own; returning (the first failure) or
        // dropping the future drops the set, which ends the others
        let file = Arc::new(file.try_clone().context("Failed to open the archive")?);
        let mut queued = parts.iter().copied();
        let mut running = tokio::task::JoinSet::new();
        let mut etags: Vec<(u32, String)> = Vec::with_capacity(parts.len());
        loop {
            while running.len() < PART_CONCURRENCY {
                let Some(part) = queued.next() else { break };
                let (bucket, key, upload_id, file) = (
                    self.clone(),
                    key.to_string(),
                    upload_id.to_string(),
                    file.clone(),
                );
                running
                    .spawn(async move { bucket.upload_part(&key, &upload_id, &file, part).await });
            }
            match running.join_next().await {
                Some(joined) => etags.push(joined.context("Part upload task panicked")??),
                None => break,
            }
        }
        etags.sort();
        let xml = complete_multipart_xml(&etags);
        let response = self
            .send(
                reqwest::Method::POST,
                key,
                &[("uploadId", upload_id)],
                &[("content-type", "application/xml")],
                Payload::Bytes(xml.as_bytes()),
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, self.stall_timeout).await);
        }
        let etag = header(&response, "etag");
        // A failure after the 200 status comes as an <Error> in the body
        let reply = self.read_reply(response).await?;
        if reply.contains("<Error>") {
            let code = xml_value(&reply, "Code").unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!("S3 could not complete the multipart upload ({})", code);
        }
        Ok(xml_value(&reply, "ETag").or(etag))
    }

    /// Send one part, signed with its own hash; returns its number and ETag
    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        file: &std::fs::File,
        part: Part,
    ) -> Result<(u32, String)> {
        let sha256 = hash_range(file, part.offset, part.len).await?;
        let number = part.number.to_string();
        let payload = Payload::File {
            file,
            offset: part.offset,
            len: part.len,
            sha256: &sha256,
        };
        let response = self
            .send(
                reqwest::Method::PUT,
                key,
                &[("partNumber", &number), ("uploadId", upload_id)],
                &[],
                payload,
            )
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, self.stall_timeout).await);
        }
        let etag = header(&response, "etag")
            .with_context(|| format!("S3 took part {} without an ETag", part.number))?;
        Ok((part.number, etag))
    }

    /// Abort a multipart upload, so S3 drops its parts. Best effort: on
    /// failure a lifecycle rule (`AbortIncompleteMultipartUpload`) has to.
    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        let aborted = self
            .send(
                reqwest::Method::DELETE,
                key,
                &[("uploadId", upload_id)],
                &[],
                Payload::Empty,
            )
            .await;
        match aborted {
            // 404: S3 dropped the upload already
            Ok(response) if response.status().is_success() || response.status() == 404 => {}
            Ok(response) => tracing::warn!(
                "Could not abort an incomplete S3 upload: {:#}",
                status_error(response, self.stall_timeout).await
            ),
            Err(e) => tracing::warn!("Could not abort an incomplete S3 upload: {:#}", e),
        }
    }

    /// Whether the bucket can be used, as a message for the runner log
    pub async fn probe(&self) -> std::result::Result<(), String> {
        let bucket = &self.location.bucket;
        let endpoint = self.location.endpoint.as_str();
        match self
            .send(reqwest::Method::HEAD, "", &[], &[], Payload::Empty)
            .await
        {
            Ok(response) => match response.status().as_u16() {
                200..=299 => Ok(()),
                404 => Err(format!("bucket {} not found at {}", bucket, endpoint)),
                403 => Err(format!(
                    "access to bucket {} denied (check access_key and secret_key, \
                     and that they allow s3:ListBucket on the bucket)",
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
            .send(reqwest::Method::PUT, "", &[], &[], Payload::Empty)
            .await?;
        if !response.status().is_success() {
            return Err(status_error(response, self.stall_timeout).await);
        }
        Ok(())
    }
}

/// A response header as text
fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// An error naming the status and S3's error code (`<Code>...</Code>`). The
/// body is read for at most `limit`: a server that goes silent must not
/// hold the job.
async fn status_error(response: reqwest::Response, limit: Duration) -> anyhow::Error {
    let status = response.status();
    let body = tokio::time::timeout(limit, response.text())
        .await
        .ok()
        .and_then(|body| body.ok())
        .unwrap_or_default();
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
            "",
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

    /// The "GET Bucket Lifecycle" and "Get Bucket (List Objects)" examples
    /// of the same page: query strings, one with a parameter without value
    #[test]
    fn signs_the_aws_documentation_examples_with_a_query() {
        let signer = Signer {
            access_key: "AKIAIOSFODNN7EXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            region: "us-east-1",
        };
        let headers = [
            ("host", "examplebucket.s3.amazonaws.com"),
            ("x-amz-content-sha256", EMPTY_SHA256),
            ("x-amz-date", "20130524T000000Z"),
        ];
        for (query, signature) in [
            (
                vec![("lifecycle", "")],
                "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
            ),
            (
                vec![("prefix", "J"), ("max-keys", "2")],
                "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
            ),
        ] {
            let (_, canonical) = encode_query(&query);
            let authorization = signer.authorization(
                "GET",
                "/",
                &canonical,
                &headers,
                EMPTY_SHA256,
                "20130524T000000Z",
            );
            assert!(
                authorization.ends_with(&format!("Signature={}", signature)),
                "{:?}: {}",
                query,
                authorization
            );
        }
    }

    #[test]
    fn query_strings_are_sorted_and_encoded() {
        assert_eq!(
            encode_query(&[("uploads", "")]),
            ("uploads".to_string(), "uploads=".to_string())
        );
        assert_eq!(
            encode_query(&[("uploadId", "a+b/c=~"), ("partNumber", "7")]),
            (
                "partNumber=7&uploadId=a%2Bb%2Fc%3D~".to_string(),
                "partNumber=7&uploadId=a%2Bb%2Fc%3D~".to_string()
            )
        );
    }

    #[test]
    fn parts_cover_the_archive_exactly() {
        const MIB: u64 = 1 << 20;
        let part = |number, offset, len| Part {
            number,
            offset,
            len,
        };
        // An exact multiple of the part size
        assert_eq!(
            plan_parts(64 * MIB, 16 * MIB).unwrap(),
            vec![
                part(1, 0, 16 * MIB),
                part(2, 16 * MIB, 16 * MIB),
                part(3, 32 * MIB, 16 * MIB),
                part(4, 48 * MIB, 16 * MIB),
            ]
        );
        // A remainder makes a shorter last part
        assert_eq!(
            plan_parts(33 * MIB + 5, 16 * MIB).unwrap(),
            vec![
                part(1, 0, 16 * MIB),
                part(2, 16 * MIB, 16 * MIB),
                part(3, 32 * MIB, MIB + 5)
            ]
        );
        // Smaller than a part: one part
        assert_eq!(plan_parts(5, 16 * MIB).unwrap(), vec![part(1, 0, 5)]);
        // Parts below a MiB (tests) are not rounded up
        assert_eq!(
            plan_parts(2500, 1000).unwrap(),
            vec![part(1, 0, 1000), part(2, 1000, 1000), part(3, 2000, 500)]
        );
        // Parts grow (whole MiB) to stay within 10,000
        let huge = 1u64 << 40;
        let parts = plan_parts(huge, 16 * MIB).unwrap();
        assert!(parts.len() as u64 <= MAX_PARTS, "{}", parts.len());
        assert_eq!(parts[0].len % MIB, 0);
        assert!(parts[0].len > 16 * MIB);
        let last = parts.last().unwrap();
        assert_eq!(last.offset + last.len, huge);
        assert!(parts
            .windows(2)
            .all(|pair| pair[0].offset + pair[0].len == pair[1].offset));
        // The largest object S3 takes
        let max = plan_parts(MAX_OBJECT_SIZE, MIN_PART_SIZE).unwrap();
        assert!(max.len() as u64 <= MAX_PARTS && max[0].len <= 5 << 30);
        assert!(plan_parts(MAX_OBJECT_SIZE + 1, MIN_PART_SIZE).is_err());
    }

    #[test]
    fn xml_values_are_unescaped_and_etags_escaped() {
        let reply = "<InitiateMultipartUploadResult><UploadId>a&amp;b</UploadId>\
                     </InitiateMultipartUploadResult>";
        assert_eq!(xml_value(reply, "UploadId").as_deref(), Some("a&b"));
        assert_eq!(
            xml_value("<ETag>&quot;abc-3&quot;</ETag>", "ETag").as_deref(),
            Some("\"abc-3\"")
        );
        assert_eq!(xml_value("<Other/>", "ETag"), None);
        assert_eq!(
            complete_multipart_xml(&[(1, "\"a\"".to_string()), (2, "\"b\"".to_string())]),
            "<CompleteMultipartUpload>\
             <Part><PartNumber>1</PartNumber><ETag>&quot;a&quot;</ETag></Part>\
             <Part><PartNumber>2</PartNumber><ETag>&quot;b&quot;</ETag></Part>\
             </CompleteMultipartUpload>"
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

    #[test]
    fn rejects_credentials_in_the_url_and_an_empty_region() {
        for url in ["https://ak:hunter2@minio.corp/b", "https://ak@minio.corp/b"] {
            let error = S3Location::parse(url, None).unwrap_err().to_string();
            assert!(error.contains("access_key"), "{}", error);
            assert!(!error.contains("hunter2"), "{}", error);
        }
        for region in ["", "  "] {
            let error = S3Location::parse("https://minio.corp/b", Some(region))
                .unwrap_err()
                .to_string();
            assert_eq!(error, "cache_s3.region must not be empty");
        }
    }

    fn bucket_at(url: String) -> Bucket {
        Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url,
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::new(),
        )
        .unwrap()
        .with_retry_delay(std::time::Duration::from_millis(10))
    }

    fn bucket(server: &MockServer, prefix: &str) -> Bucket {
        bucket_at(format!("{}/ci-cache{}", server.uri(), prefix))
    }

    /// S3 shares GitLab's client, whose 30 s total timeout is for API calls
    #[tokio::test]
    async fn the_shared_clients_total_timeout_does_not_end_transfers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"zip".to_vec())
                    .set_delay(Duration::from_millis(400)),
            )
            .mount(&server)
            .await;
        let api_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let b = Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: format!("{}/ci-cache", server.uri()),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            api_client,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();

        let download = b
            .get_to_file(&b.object_key(1, "k.zip"), None, dir.path())
            .await
            .unwrap();

        assert!(
            matches!(download, Download::Downloaded { bytes: 3, .. }),
            "{:?}",
            download
        );
    }

    /// A server that answers one request with `reply` and then goes silent
    /// for `hold` (closing the connection right away for zero)
    async fn raw_server(
        reply: &'static [u8],
        hold: Duration,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(reply).await.unwrap();
            tokio::time::sleep(hold).await;
        });
        (address, server)
    }

    /// A server that accepts every connection and never answers, or with
    /// `hang_up` reads the request and closes the connection; counts the
    /// connections
    async fn mute_server(
        hang_up: bool,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        let server = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if hang_up {
                    let _ = socket.read(&mut [0u8; 4096]).await;
                } else {
                    open.push(socket);
                }
            }
        });
        (address, accepted, server)
    }

    async fn requests_to(server: &MockServer, name: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path().ends_with(name))
            .count()
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
            reqwest::Client::new(),
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
            Download::Downloaded {
                file,
                etag,
                bytes,
                sha256,
            } => {
                assert_eq!(std::fs::read(file.path()).unwrap(), b"zip-bytes");
                assert_eq!(etag.as_deref(), Some("\"def\""));
                assert_eq!(bytes, 9);
                assert_eq!(sha256, hex(&Sha256::digest(b"zip-bytes")));
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
            .and(header(META_SHA256, hash.as_str()))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"e1\""))
            .expect(1)
            .mount(&server)
            .await;
        let b = bucket(&server, "/pre");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("k.zip");
        std::fs::write(&file, &body).unwrap();

        let etag = b
            .put_file(
                &b.object_key(3, "k.zip"),
                &std::fs::File::open(&file).unwrap(),
                &hash,
            )
            .await
            .unwrap();

        assert_eq!(etag.as_deref(), Some("\"e1\""));
        let received = &server.received_requests().await.unwrap()[0];
        assert_eq!(received.body, body);
        // The content hash is signed with the other x-amz headers
        let authorization = received.headers["authorization"].to_str().unwrap();
        assert!(
            authorization
                .contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-meta-sha256,"),
            "{}",
            authorization
        );
    }

    #[tokio::test]
    async fn uploads_larger_than_a_chunk_arrive_whole() {
        let server = MockServer::start().await;
        let body: Vec<u8> = (0..IO_CHUNK * 2 + 123).map(|i| (i % 251) as u8).collect();
        let hash = hex(&Sha256::digest(&body));
        Mock::given(method("PUT"))
            .and(header("x-amz-content-sha256", hash.as_str()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let b = bucket(&server, "");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.zip");
        std::fs::write(&file, &body).unwrap();

        b.put_file(
            &b.object_key(1, "big.zip"),
            &std::fs::File::open(&file).unwrap(),
            &hash,
        )
        .await
        .unwrap();

        assert_eq!(server.received_requests().await.unwrap()[0].body, body);
    }

    #[tokio::test]
    async fn head_tells_the_etag_and_content_hash() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/ci-cache/project-1/k.zip"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"v1-3\"")
                    .insert_header(META_SHA256, "abc"),
            )
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/ci-cache/project-1/old.zip"))
            .respond_with(ResponseTemplate::new(200).insert_header("etag", "\"v0\""))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/ci-cache/project-1/gone.zip"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let b = bucket(&server, "");

        assert_eq!(
            b.head(&b.object_key(1, "k.zip")).await.unwrap(),
            Some(ObjectInfo {
                etag: Some("\"v1-3\"".to_string()),
                sha256: Some("abc".to_string())
            })
        );
        assert_eq!(
            b.head(&b.object_key(1, "old.zip")).await.unwrap(),
            Some(ObjectInfo {
                etag: Some("\"v0\"".to_string()),
                sha256: None
            })
        );
        assert_eq!(b.head(&b.object_key(1, "gone.zip")).await.unwrap(), None);
    }

    /// A file of `len` bytes that are not all the same
    fn archive_of(dir: &Path, len: usize) -> (std::fs::File, Vec<u8>) {
        let body: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        let file = dir.join("big.tar.zst");
        std::fs::write(&file, &body).unwrap();
        (std::fs::File::open(&file).unwrap(), body)
    }

    /// A server with `mount_multipart`
    async fn multipart_server() -> MockServer {
        let server = MockServer::start().await;
        mount_multipart(&server).await;
        server
    }

    /// Mounts CreateMultipartUpload (UploadId `up/1+x`), UploadPart (ETag
    /// `"etag-<number>"`), CompleteMultipartUpload and AbortMultipartUpload
    /// for `big.tar.zst`
    async fn mount_multipart(server: &MockServer) {
        use wiremock::matchers::query_param;
        let object = "/ci-cache/project-1/big.tar.zst";
        Mock::given(method("POST"))
            .and(path(object))
            .and(query_param("uploads", ""))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<InitiateMultipartUploadResult><UploadId>up/1+x</UploadId>\
                 </InitiateMultipartUploadResult>",
            ))
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(path(object))
            .and(query_param("uploadId", "up/1+x"))
            .respond_with(|request: &wiremock::Request| {
                let number = request
                    .url
                    .query_pairs()
                    .find(|(name, _)| name == "partNumber")
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default();
                ResponseTemplate::new(200).insert_header("etag", format!("\"etag-{}\"", number))
            })
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path(object))
            .and(query_param("uploadId", "up/1+x"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<CompleteMultipartUploadResult><ETag>&quot;final-3&quot;</ETag>\
                 </CompleteMultipartUploadResult>",
            ))
            .mount(server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(object))
            .and(query_param("uploadId", "up/1+x"))
            .respond_with(ResponseTemplate::new(204))
            .mount(server)
            .await;
    }

    async fn requests_with(server: &MockServer, verb: &str, query: &str) -> Vec<wiremock::Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| {
                request.method.as_str() == verb && request.url.query().unwrap_or("").contains(query)
            })
            .collect()
    }

    #[tokio::test]
    async fn large_archives_are_uploaded_in_signed_parts() {
        let server = multipart_server().await;
        let b = bucket(&server, "").with_multipart(2000, 1000);
        let dir = tempfile::tempdir().unwrap();
        let (file, body) = archive_of(dir.path(), 2500);
        let hash = hex(&Sha256::digest(&body));

        let etag = b
            .put_file(&b.object_key(1, "big.tar.zst"), &file, &hash)
            .await
            .unwrap();

        assert_eq!(etag.as_deref(), Some("\"final-3\""));
        let created = requests_with(&server, "POST", "uploads").await;
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].headers[META_SHA256], hash.as_str());
        let mut parts = requests_with(&server, "PUT", "partNumber").await;
        parts.sort_by_key(|part| part.url.query().unwrap().to_string());
        let slices = [&body[..1000], &body[1000..2000], &body[2000..]];
        assert_eq!(parts.len(), 3);
        for (part, slice) in parts.iter().zip(slices) {
            assert_eq!(part.body, slice);
            assert_eq!(
                part.headers["x-amz-content-sha256"],
                hex(&Sha256::digest(slice)).as_str()
            );
            assert_eq!(
                part.headers["content-length"],
                slice.len().to_string().as_str()
            );
        }
        let completed = requests_with(&server, "POST", "uploadId").await;
        assert_eq!(completed.len(), 1);
        assert_eq!(
            String::from_utf8_lossy(&completed[0].body),
            complete_multipart_xml(&[
                (1, "\"etag-1\"".to_string()),
                (2, "\"etag-2\"".to_string()),
                (3, "\"etag-3\"".to_string())
            ])
        );
        assert!(requests_with(&server, "DELETE", "uploadId")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn a_failed_part_aborts_the_upload() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(query_param("partNumber", "2"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("<Error><Code>AccessDenied</Code></Error>"),
            )
            .with_priority(1)
            .mount(&server)
            .await;
        mount_multipart(&server).await;
        let b = bucket(&server, "").with_multipart(2000, 1000);
        let dir = tempfile::tempdir().unwrap();
        let (file, body) = archive_of(dir.path(), 2500);

        let error = b
            .put_file(
                &b.object_key(1, "big.tar.zst"),
                &file,
                &hex(&Sha256::digest(&body)),
            )
            .await
            .unwrap_err();

        assert!(
            format!("{:#}", error).contains("AccessDenied"),
            "{:#}",
            error
        );
        assert!(requests_with(&server, "POST", "uploadId=").await.is_empty());
        assert_eq!(requests_with(&server, "DELETE", "uploadId=").await.len(), 1);
    }

    #[tokio::test]
    async fn a_completion_error_in_a_200_reply_aborts_the_upload() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(query_param("uploadId", "up/1+x"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<Error><Code>InternalError</Code><Message>try again</Message></Error>",
            ))
            .with_priority(1)
            .mount(&server)
            .await;
        mount_multipart(&server).await;
        let b = bucket(&server, "").with_multipart(2000, 1000);
        let dir = tempfile::tempdir().unwrap();
        let (file, body) = archive_of(dir.path(), 2500);

        let error = b
            .put_file(
                &b.object_key(1, "big.tar.zst"),
                &file,
                &hex(&Sha256::digest(&body)),
            )
            .await
            .unwrap_err();

        assert!(
            format!("{:#}", error).contains("InternalError"),
            "{:#}",
            error
        );
        assert_eq!(requests_with(&server, "DELETE", "uploadId=").await.len(), 1);
    }

    /// A job canceled while its cache uploads drops the upload: S3 is told
    /// to discard the parts
    #[tokio::test]
    async fn a_dropped_upload_is_aborted() {
        use wiremock::matchers::query_param;
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(query_param("uploadId", "up/1+x"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
            .with_priority(1)
            .mount(&server)
            .await;
        mount_multipart(&server).await;
        let b = bucket(&server, "").with_multipart(2000, 1000);
        let dir = tempfile::tempdir().unwrap();
        let (file, body) = archive_of(dir.path(), 2500);
        let key = b.object_key(1, "big.tar.zst");
        let hash = hex(&Sha256::digest(&body));

        let upload = b.put_file(&key, &file, &hash);
        let parts_sent = async {
            while requests_with(&server, "PUT", "partNumber").await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            _ = upload => panic!("the upload finished"),
            _ = parts_sent => {}
        }

        let started = std::time::Instant::now();
        while requests_with(&server, "DELETE", "uploadId=")
            .await
            .is_empty()
        {
            assert!(started.elapsed() < Duration::from_secs(5), "never aborted");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(requests_with(&server, "POST", "uploadId=").await.is_empty());
    }

    #[tokio::test]
    async fn probe_explains_the_bucket_state() {
        let server = MockServer::start().await;
        Mock::given(method("HEAD"))
            .and(path("/ci-cache"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("HEAD"))
            .and(path("/locked"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let message = bucket(&server, "").probe().await.unwrap_err();

        assert!(message.contains("not found"), "{}", message);
        let denied = bucket_at(format!("{}/locked", server.uri()))
            .probe()
            .await
            .unwrap_err();
        assert_eq!(
            denied,
            "access to bucket locked denied (check access_key and secret_key, \
             and that they allow s3:ListBucket on the bucket)"
        );
        let unreachable = Bucket::new(
            &crate::runner_daemon::config::S3CacheConfig {
                url: "http://127.0.0.1:9/ci-cache".to_string(),
                access_key: "AK".to_string(),
                secret_key: "SK".to_string(),
                region: None,
            },
            reqwest::Client::new(),
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
            .put_file(
                &b.object_key(1, "stuck.zip"),
                &std::fs::File::open(&file).unwrap(),
                EMPTY_SHA256,
            )
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
            .put_file(
                &other.object_key(1, "k.zip"),
                &std::fs::File::open(&file).unwrap(),
                EMPTY_SHA256,
            )
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
        // Sends the headers and part of the body, then goes silent
        let (address, server) = raw_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npart",
            Duration::from_secs(2),
        )
        .await;
        let b = bucket_at(format!("http://{}/ci-cache", address))
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

    #[tokio::test]
    async fn a_stalled_attempt_opens_the_breaker_without_retrying() {
        let (address, connections, server) = mute_server(false).await;
        let limit = Duration::from_millis(200);
        let b = bucket_at(format!("http://{}/ci-cache", address)).with_stall_timeout(limit);
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let stalled = b
            .get_to_file(&b.object_key(1, "k.zip"), None, dir.path())
            .await
            .unwrap_err();

        assert!(stalled.to_string().contains("S3 stalled"), "{:#}", stalled);
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
        // About one stall limit; three attempts take more than three
        assert!(started.elapsed() < limit * 3, "{:?}", started.elapsed());
        let skipped = b
            .get_to_file(&b.object_key(1, "next.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(skipped.to_string().contains("S3 skipped"), "{:#}", skipped);
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_opens_the_breaker_without_retrying() {
        // Retries would wait 1 s and 2 s first
        let b = bucket_at("http://127.0.0.1:9/ci-cache".to_string())
            .with_retry_delay(Duration::from_secs(1));
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let refused = b
            .get_to_file(&b.object_key(1, "k.zip"), None, dir.path())
            .await
            .unwrap_err();

        assert!(
            format!("{:#}", refused).contains("cannot reach S3"),
            "{:#}",
            refused
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        let skipped = b
            .get_to_file(&b.object_key(1, "next.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(skipped.to_string().contains("S3 skipped"), "{:#}", skipped);
    }

    #[tokio::test]
    async fn a_connection_closed_without_an_answer_is_retried() {
        let (address, connections, server) = mute_server(true).await;
        let b = bucket_at(format!("http://{}/ci-cache", address));
        let dir = tempfile::tempdir().unwrap();

        let closed = b
            .get_to_file(&b.object_key(1, "k.zip"), None, dir.path())
            .await
            .unwrap_err();

        assert!(
            format!("{:#}", closed).contains("cannot reach S3"),
            "{:#}",
            closed
        );
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn errors_do_not_name_the_object() {
        // The breaker repeats an error to every job: project 7 must not see
        // project 42's id or cache key
        let b = bucket_at("http://127.0.0.1:9/ci-cache".to_string());
        let dir = tempfile::tempdir().unwrap();
        let unreachable = b
            .get_to_file(&b.object_key(42, "feature%2Fx.zip"), None, dir.path())
            .await
            .unwrap_err();
        let skipped = b
            .get_to_file(&b.object_key(7, "main.zip"), None, dir.path())
            .await
            .unwrap_err();
        assert!(skipped.to_string().contains("S3 skipped"), "{:#}", skipped);

        // Closes the connection in the middle of the body
        let (address, server) = raw_server(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npart",
            Duration::ZERO,
        )
        .await;
        let b = bucket_at(format!("http://{}/ci-cache", address));
        let interrupted = b
            .get_to_file(&b.object_key(42, "feature%2Fx.zip"), None, dir.path())
            .await
            .unwrap_err();
        server.abort();

        for error in [unreachable, skipped, interrupted] {
            let text = format!("{:#}", error);
            assert!(!text.contains("project-"), "{}", text);
        }
    }

    #[tokio::test]
    async fn an_error_body_that_never_comes_does_not_hold_the_call() {
        let (address, server) = raw_server(
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 100\r\n\r\n",
            Duration::from_secs(5),
        )
        .await;
        let b = bucket_at(format!("http://{}/ci-cache", address))
            .with_stall_timeout(Duration::from_millis(200));
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();

        let error = b
            .get_to_file(&b.object_key(1, "k.zip"), None, dir.path())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("403"), "{:#}", error);
        assert!(started.elapsed() < Duration::from_secs(2));
        server.abort();
    }

    #[tokio::test]
    async fn calls_in_flight_stop_retrying_once_the_breaker_opens() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/down.zip"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/slow.zip"))
            .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(300)))
            .mount(&server)
            .await;
        let b = bucket(&server, "");
        let dir = tempfile::tempdir().unwrap();

        // down.zip trips the breaker while slow.zip waits for its first reply
        let (slow_key, down_key) = (b.object_key(1, "slow.zip"), b.object_key(1, "down.zip"));
        let (slow, down) = tokio::join!(
            b.get_to_file(&slow_key, None, dir.path()),
            b.get_to_file(&down_key, None, dir.path()),
        );

        let down = down.unwrap_err();
        assert!(down.to_string().contains("503"), "{:#}", down);
        let slow = slow.unwrap_err().to_string();
        assert!(slow.starts_with("S3 skipped"), "{}", slow);
        assert_eq!(requests_to(&server, "slow.zip").await, 1);
    }

    #[tokio::test]
    async fn after_the_cooldown_one_call_tries_s3_and_its_success_closes_the_breaker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/down.zip"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ci-cache/project-1/back.zip"))
            .respond_with(ResponseTemplate::new(404).set_delay(Duration::from_millis(100)))
            .mount(&server)
            .await;
        let b = bucket(&server, "").with_cooldown(Duration::from_millis(300));
        let dir = tempfile::tempdir().unwrap();
        assert!(b
            .get_to_file(&b.object_key(1, "down.zip"), None, dir.path())
            .await
            .is_err());
        tokio::time::sleep(Duration::from_millis(400)).await;

        // The trial extends the pause by 300 ms and gets its answer after
        // 100 ms: the last call below reaches S3 only if that answer closed
        // the breaker
        let key = b.object_key(1, "back.zip");
        let calls =
            futures_util::future::join_all((0..3).map(|_| b.get_to_file(&key, None, dir.path())))
                .await;

        assert_eq!(requests_to(&server, "back.zip").await, 1);
        let found = calls
            .iter()
            .filter(|call| matches!(call, Ok(Download::NotFound)))
            .count();
        assert_eq!(found, 1, "{:?}", calls);
        assert!(matches!(
            b.get_to_file(&key, None, dir.path()).await.unwrap(),
            Download::NotFound
        ));
        assert_eq!(requests_to(&server, "back.zip").await, 2);
    }
}
