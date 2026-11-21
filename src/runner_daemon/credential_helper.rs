//! Registry logins from a Docker client config (`DOCKER_AUTH_CONFIG`, the
//! runner's config.json), credential helpers included, as gitlab-runner
//! resolves them:
//!
//! - `credHelpers["<registry>"]` comes first, then `credsStore`, then the
//!   static `auths` entry (docker CLI and gitlab-runner order);
//! - a helper is the program `docker-credential-<name>` on the runner's
//!   PATH, run on the runner's host with the protocol of
//!   docker-credential-helpers: `get` with the server URL on stdin answers
//!   `{"ServerURL", "Username", "Secret"}` (Username `<token>`: the secret is
//!   an identity token), `list` answers `{"<server URL>": "<username>"}`;
//! - a helper that fails (missing, error, timeout) drops the logins of its
//!   config for that registry with a warning, and the next config is asked
//!   (gitlab-runner: "Credentials from this source will not be used");
//! - helpers of a job's DOCKER_AUTH_CONFIG run only when the runner's
//!   `allowed_docker_credential_helpers` lists them for the registry, as in
//!   gitlab-runner: otherwise any job could have the runner's host log in
//!   with the operator's credentials (e.g. the host's cloud role).
//!
//! Secrets are never logged: warnings name the helper, the registry and the
//! helper's error output only.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::docker_api::DockerCredentials;

/// Longest a helper may take to answer
pub const HELPER_TIMEOUT: Duration = Duration::from_secs(30);
/// Program name prefix of a helper (`docker-credential-ecr-login`)
const PROGRAM_PREFIX: &str = "docker-credential-";
/// What helpers print (with a non-zero exit) when they hold no login
const NOT_FOUND: &str = "credentials not found in native keychain";
/// The key helpers and `docker login` use for Docker Hub
const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";
/// Username of a helper answer whose secret is an identity token
const TOKEN_USERNAME: &str = "<token>";
/// Most characters of a helper's error output put in a warning
const MAX_DETAIL: usize = 400;

/// A Docker client config to take logins from
pub struct ConfigSource<'a> {
    /// Where it comes from, for warnings
    pub name: &'a str,
    pub json: &'a str,
    /// Helpers it may run, by name → registry patterns; `None`: all (the
    /// operator's own config)
    pub allowed_helpers: Option<&'a BTreeMap<String, Vec<String>>>,
}

/// Where helpers are found and how long they may take
#[derive(Debug, Clone, Copy)]
pub struct Helpers<'a> {
    /// Directory of the helper programs; `None`: the runner's PATH
    pub dir: Option<&'a Path>,
    pub timeout: Duration,
}

impl Default for Helpers<'_> {
    fn default() -> Self {
        Self {
            dir: None,
            timeout: HELPER_TIMEOUT,
        }
    }
}

/// Whether `name` can be a helper name: no path, no option, nothing a shell
/// would read
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Whether a config key (`harbor.corp`, `https://harbor.corp/v2/`) names
/// `registry` (Docker Hub has several names)
fn names_registry(key: &str, registry: &str) -> bool {
    let without_scheme = key.split_once("://").map_or(key, |(_, rest)| rest);
    let host = without_scheme
        .split('/')
        .next()
        .unwrap_or(without_scheme)
        .to_ascii_lowercase();
    let hub = |host: &str| {
        matches!(
            host,
            "docker.io" | "index.docker.io" | "registry-1.docker.io"
        )
    };
    host == registry.to_ascii_lowercase() || (hub(&host) && hub(registry))
}

/// The login of `source` for the registry of `image`, or `None`; problems
/// are added to `warnings`
pub async fn login(
    source: &ConfigSource<'_>,
    image: &str,
    helpers: Helpers<'_>,
    warnings: &mut Vec<String>,
) -> Option<DockerCredentials> {
    let registry = super::image::registry(image);
    let Ok(config) = serde_json::from_str::<serde_json::Value>(source.json) else {
        warnings.push(format!(
            "{} is not valid JSON: its registry logins are not used",
            source.name
        ));
        return None;
    };
    let permitted = |helper: &str, warnings: &mut Vec<String>| -> bool {
        let Some(allowed) = source.allowed_helpers else {
            return true;
        };
        let ok = allowed
            .get(helper)
            .is_some_and(|patterns| super::config::glob_matches(patterns, registry));
        if !ok {
            warnings.push(format!(
                "{} asks for the credential helper {:?} for {}, which executor.docker.allowed_docker_credential_helpers does not allow for this registry: it is not run",
                source.name, helper, registry
            ));
        }
        ok
    };
    let failed = |helper: &str, error: String, warnings: &mut Vec<String>| {
        warnings.push(format!(
            "{}: credential helper {:?} failed for {}: {}; logins from {} are not used for it",
            source.name, helper, registry, error, source.name
        ));
    };

    // 1. A helper for this registry
    let registry_helper = config
        .get("credHelpers")
        .and_then(|helpers| helpers.as_object())
        .and_then(|configured| {
            configured
                .iter()
                .filter(|(key, _)| names_registry(key, registry))
                .min_by_key(|(key, _)| key.as_str())
        });
    if let Some((key, helper)) = registry_helper {
        let helper = helper.as_str().unwrap_or_default();
        if permitted(helper, warnings) {
            return match get(helper, key, helpers).await {
                Ok(login) => login.map(|login| login.credentials(registry)),
                Err(error) => {
                    failed(helper, error, warnings);
                    None
                }
            };
        }
    }

    // 2. The config's credential store, when it holds a login for it
    let store = config
        .get("credsStore")
        .and_then(|store| store.as_str())
        .filter(|store| !store.is_empty());
    if let Some(store) = store {
        if permitted(store, warnings) {
            match store_get(store, registry, helpers).await {
                Ok(Some(login)) => return Some(login.credentials(registry)),
                Ok(None) => {}
                Err(error) => {
                    failed(store, error, warnings);
                    return None;
                }
            }
        }
    }

    // 3. The static `auths` entry
    super::image::docker_config_auth(source.json, image).map(|(username, password)| {
        DockerCredentials {
            username: Some(username),
            password: Some(password),
            serveraddress: Some(registry.to_string()),
            ..Default::default()
        }
    })
}

/// A helper's answer to `get` (no Debug: it holds the secret)
struct HelperLogin {
    username: String,
    secret: String,
}

impl HelperLogin {
    fn credentials(self, registry: &str) -> DockerCredentials {
        let mut credentials = DockerCredentials {
            serveraddress: Some(registry.to_string()),
            ..Default::default()
        };
        if self.username == TOKEN_USERNAME {
            credentials.identitytoken = Some(self.secret);
        } else {
            credentials.username = Some(self.username);
            credentials.password = Some(self.secret);
        }
        credentials
    }
}

/// The login a store holds for `registry`: its `list` gives the server URLs
/// it has logins for, then `get` reads the one naming the registry
async fn store_get(
    store: &str,
    registry: &str,
    helpers: Helpers<'_>,
) -> Result<Option<HelperLogin>, String> {
    let listed = run(store, "list", "", helpers)
        .await?
        .ok_or_else(|| "list found nothing".to_string())?;
    let servers: BTreeMap<String, serde_json::Value> = serde_json::from_slice(&listed)
        .map_err(|_| "list did not answer a JSON object".to_string())?;
    // Docker Hub's usual key first, then the others in order
    let mut keys: Vec<&String> = servers
        .keys()
        .filter(|key| names_registry(key, registry))
        .collect();
    keys.sort_by_key(|key| (key.as_str() != DOCKER_HUB_KEY, key.as_str()));
    match keys.first() {
        Some(key) => get(store, key, helpers).await,
        None => Ok(None),
    }
}

/// `docker-credential-<helper> get` for `server`; `None` when it holds no login
async fn get(
    helper: &str,
    server: &str,
    helpers: Helpers<'_>,
) -> Result<Option<HelperLogin>, String> {
    let Some(output) = run(helper, "get", server, helpers).await? else {
        return Ok(None);
    };
    #[derive(serde::Deserialize)]
    struct Answer {
        #[serde(rename = "Username", default)]
        username: String,
        #[serde(rename = "Secret", default)]
        secret: String,
    }
    // The answer holds the secret: the error does not quote it
    let answer: Answer = serde_json::from_slice(&output)
        .map_err(|_| "get did not answer the expected JSON".to_string())?;
    if answer.username.is_empty() && answer.secret.is_empty() {
        return Ok(None);
    }
    Ok(Some(HelperLogin {
        username: answer.username,
        secret: answer.secret,
    }))
}

/// Output of `docker-credential-<helper> <action>` given `input` on stdin;
/// `None` when it reports that it holds no login
async fn run(
    helper: &str,
    action: &str,
    input: &str,
    helpers: Helpers<'_>,
) -> Result<Option<Vec<u8>>, String> {
    if !valid_name(helper) {
        return Err(format!("{:?} is not a valid helper name", helper));
    }
    let program = format!("{}{}", PROGRAM_PREFIX, helper);
    let path = match helpers.dir {
        Some(dir) => dir.join(&program),
        None => program.clone().into(),
    };
    let mut child = tokio::process::Command::new(&path)
        .arg(action)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!(
                    "{} is not installed (not found in the runner's PATH)",
                    program
                )
            }
            _ => format!("cannot run {}: {}", program, e),
        })?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let answered = async move {
        // A helper that exits without reading its input is not an error here
        let _ = stdin.write_all(input.as_bytes()).await;
        drop(stdin);
        child.wait_with_output().await
    };
    let output = match tokio::time::timeout(helpers.timeout, answered).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("{} failed: {}", program, e)),
        // Dropping the child kills it
        Err(_) => {
            return Err(format!(
                "{} did not answer within {:?}",
                program, helpers.timeout
            ))
        }
    };
    if output.status.success() {
        return Ok(Some(output.stdout));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim() == NOT_FOUND {
        return Ok(None);
    }
    // On failure helpers print their error, not a secret
    let detail = [
        stdout.trim(),
        String::from_utf8_lossy(&output.stderr).trim(),
    ]
    .into_iter()
    .filter(|text| !text.is_empty())
    .collect::<Vec<_>>()
    .join(" / ");
    let detail: String = detail.chars().take(MAX_DETAIL).collect();
    Err(format!(
        "{} {} exited with {}{}",
        program,
        action,
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {}", detail)
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake helper `docker-credential-<name>` in `dir` running `script`
    /// (sh), which gets the server URL from stdin as `$server`
    fn fake_helper(dir: &Path, name: &str, script: &str) {
        let path = dir.join(format!("docker-credential-{}", name));
        std::fs::write(
            &path,
            format!("#!/bin/sh\nserver=$(cat)\naction=\"$1\"\n{}\n", script),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn source<'a>(
        json: &'a str,
        allowed: Option<&'a BTreeMap<String, Vec<String>>>,
    ) -> ConfigSource<'a> {
        ConfigSource {
            name: "DOCKER_AUTH_CONFIG",
            json,
            allowed_helpers: allowed,
        }
    }

    async fn resolve_within(
        source: &ConfigSource<'_>,
        image: &str,
        dir: &Path,
        timeout: Duration,
    ) -> (Option<DockerCredentials>, Vec<String>) {
        let mut warnings = Vec::new();
        let helpers = Helpers {
            dir: Some(dir),
            timeout,
        };
        let login = login(source, image, helpers, &mut warnings).await;
        (login, warnings)
    }

    async fn resolve(
        source: &ConfigSource<'_>,
        image: &str,
        dir: &Path,
    ) -> (Option<DockerCredentials>, Vec<String>) {
        resolve_within(source, image, dir, HELPER_TIMEOUT).await
    }

    fn user_and_password(login: &Option<DockerCredentials>) -> Option<(String, String)> {
        login.as_ref().map(|login| {
            (
                login.username.clone().unwrap_or_default(),
                login.password.clone().unwrap_or_default(),
            )
        })
    }

    #[tokio::test]
    async fn registry_helpers_come_before_the_store_and_the_store_before_auths() {
        let dir = tempfile::tempdir().unwrap();
        // The helpers answer with the server URL they were asked about
        fake_helper(
            dir.path(),
            "pass",
            r#"[ "$action" = get ] && printf '{"ServerURL":"%s","Username":"from-pass","Secret":"pass-%s"}' "$server" "$server""#,
        );
        fake_helper(
            dir.path(),
            "store",
            r#"case "$action" in
                list) printf '{"https://index.docker.io/v1/":"hub","harbor.corp":"h"}' ;;
                get) case "$server" in
                    harbor.corp|https://index.docker.io/v1/) printf '{"Username":"from-store","Secret":"store-%s"}' "$server" ;;
                    *) echo 'credentials not found in native keychain'; exit 1 ;;
                esac ;;
            esac"#,
        );
        // "static:pw" in base64
        let json = r#"{
            "auths": {"harbor.corp": {"auth": "c3RhdGljOnB3"}, "quay.io": {"auth": "c3RhdGljOnB3"}},
            "credsStore": "store",
            "credHelpers": {"registry.corp:5000": "pass"}
        }"#;
        let source = source(json, None);

        let (login, warnings) = resolve(&source, "registry.corp:5000/app:1", dir.path()).await;
        assert_eq!(
            user_and_password(&login),
            Some(("from-pass".into(), "pass-registry.corp:5000".into()))
        );
        assert_eq!(
            login.unwrap().serveraddress.as_deref(),
            Some("registry.corp:5000")
        );
        assert!(warnings.is_empty(), "{:?}", warnings);

        // The store holds harbor.corp, so its static auths entry is not used
        let (login, _) = resolve(&source, "harbor.corp/group/app", dir.path()).await;
        assert_eq!(
            user_and_password(&login),
            Some(("from-store".into(), "store-harbor.corp".into()))
        );
        // Docker Hub is asked for with its usual key
        let (login, _) = resolve(&source, "postgres:16", dir.path()).await;
        assert_eq!(
            user_and_password(&login),
            Some((
                "from-store".into(),
                "store-https://index.docker.io/v1/".into()
            ))
        );
        // Nothing in the store for quay.io: the static entry
        let (login, warnings) = resolve(&source, "quay.io/org/app", dir.path()).await;
        assert_eq!(
            user_and_password(&login),
            Some(("static".into(), "pw".into()))
        );
        assert!(warnings.is_empty(), "{:?}", warnings);
        // Nothing anywhere
        let (login, _) = resolve(&source, "ghcr.io/org/app", dir.path()).await;
        assert!(login.is_none());
    }

    #[tokio::test]
    async fn token_usernames_give_identity_tokens() {
        let dir = tempfile::tempdir().unwrap();
        fake_helper(
            dir.path(),
            "acr",
            r#"printf '{"ServerURL":"x","Username":"<token>","Secret":"refresh-token"}'"#,
        );
        let json = r#"{"credHelpers": {"corp.azurecr.io": "acr"}}"#;

        let (login, _) = resolve(&source(json, None), "corp.azurecr.io/app", dir.path()).await;
        let login = login.unwrap();
        assert_eq!(login.identitytoken.as_deref(), Some("refresh-token"));
        assert!(login.username.is_none() && login.password.is_none());
    }

    #[tokio::test]
    async fn a_failing_helper_drops_its_config_with_a_warning_and_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        fake_helper(
            dir.path(),
            "broken",
            "echo 'token expired' >&2; echo 'error getting credentials'; exit 3",
        );
        // "static:pw" is not used: the helper of this registry failed
        let json = r#"{"auths": {"harbor.corp": {"auth": "c3RhdGljOnB3"}}, "credHelpers": {"harbor.corp": "broken"}}"#;

        let (login, warnings) = resolve(&source(json, None), "harbor.corp/app", dir.path()).await;
        assert!(login.is_none());
        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(warnings[0].contains("\"broken\""), "{}", warnings[0]);
        assert!(warnings[0].contains("exit status: 3"), "{}", warnings[0]);
        assert!(warnings[0].contains("token expired"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("error getting credentials"),
            "{}",
            warnings[0]
        );
        assert!(!warnings[0].contains("pw"), "{}", warnings[0]);
    }

    #[tokio::test]
    async fn a_missing_helper_is_named_in_the_warning() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"credsStore": "desktop"}"#;

        let (login, warnings) = resolve(&source(json, None), "alpine", dir.path()).await;
        assert!(login.is_none());
        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(
            warnings[0].contains("docker-credential-desktop is not installed"),
            "{}",
            warnings[0]
        );
    }

    #[tokio::test]
    async fn a_helper_without_a_login_leaves_the_registry_to_the_next_config() {
        let dir = tempfile::tempdir().unwrap();
        fake_helper(
            dir.path(),
            "empty",
            "echo 'credentials not found in native keychain'; exit 1",
        );
        let json = r#"{"auths": {"harbor.corp": {"auth": "c3RhdGljOnB3"}}, "credHelpers": {"harbor.corp": "empty"}}"#;

        let (login, warnings) = resolve(&source(json, None), "harbor.corp/app", dir.path()).await;
        // Like gitlab-runner: the helper's empty answer replaces the static entry
        assert!(login.is_none());
        assert!(warnings.is_empty(), "{:?}", warnings);
    }

    #[tokio::test]
    async fn helpers_of_job_configs_run_only_when_allowed_for_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        fake_helper(
            dir.path(),
            "ecr-login",
            &format!(
                "touch {}; printf '{{\"Username\":\"AWS\",\"Secret\":\"ecr\"}}'",
                marker.display()
            ),
        );
        let json = r#"{"credHelpers": {
            "123.dkr.ecr.eu-west-1.amazonaws.com": "ecr-login",
            "harbor.corp": "ecr-login"
        }, "auths": {"harbor.corp": {"auth": "c3RhdGljOnB3"}}}"#;
        let allowed = BTreeMap::from([(
            "ecr-login".to_string(),
            vec!["*.dkr.ecr.*.amazonaws.com".to_string()],
        )]);

        let ecr = "123.dkr.ecr.eu-west-1.amazonaws.com/app:1";
        let (login, warnings) = resolve(&source(json, Some(&allowed)), ecr, dir.path()).await;
        assert_eq!(
            user_and_password(&login),
            Some(("AWS".into(), "ecr".into()))
        );
        assert!(warnings.is_empty(), "{:?}", warnings);
        std::fs::remove_file(&marker).unwrap();

        // Not allowed for harbor.corp: not run, the static entry applies
        let (login, warnings) =
            resolve(&source(json, Some(&allowed)), "harbor.corp/app", dir.path()).await;
        assert_eq!(
            user_and_password(&login),
            Some(("static".into(), "pw".into()))
        );
        assert_eq!(warnings.len(), 1, "{:?}", warnings);
        assert!(warnings[0].contains("allowed_docker_credential_helpers"));
        assert!(!marker.exists());

        // An empty allowlist runs no helper of a job
        let none = BTreeMap::new();
        let (login, _) = resolve(&source(json, Some(&none)), ecr, dir.path()).await;
        assert!(login.is_none());
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn helper_names_cannot_name_other_programs() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../../bin/sh", "/bin/sh", "-x", "a b"] {
            assert!(!valid_name(name), "{:?}", name);
            let json = serde_json::json!({ "credsStore": name }).to_string();
            let (login, warnings) = resolve(&source(&json, None), "alpine", dir.path()).await;
            assert!(login.is_none());
            assert!(
                warnings[0].contains("not a valid helper name"),
                "{:?}",
                warnings
            );
        }
        assert!(!valid_name(""));
        assert!(valid_name("ecr-login") && valid_name("osxkeychain") && valid_name("gcloud"));
    }

    #[tokio::test]
    async fn a_hanging_helper_is_stopped_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        fake_helper(dir.path(), "slow", "exec sleep 600");
        let json = r#"{"credsStore": "slow"}"#;

        let started = std::time::Instant::now();
        let (login, warnings) = resolve_within(
            &source(json, None),
            "alpine",
            dir.path(),
            Duration::from_millis(300),
        )
        .await;
        assert!(login.is_none());
        assert!(
            warnings[0].contains("did not answer within 300ms"),
            "{:?}",
            warnings
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
