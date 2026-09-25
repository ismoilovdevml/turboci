//! Docker image references: registry, service aliases, registry credentials and
//! pull policy, following gitlab-runner's Docker executor.

use crate::gitlab::Credential;

/// Image name without tag or digest, e.g. `registry.example.com/group/app`
pub fn repository(image: &str) -> &str {
    let without_digest = image.split('@').next().unwrap_or(image);
    match without_digest.rfind(':') {
        // A ':' after the last '/' is a tag; before it, a registry port
        Some(colon) if !without_digest[colon..].contains('/') => &without_digest[..colon],
        _ => without_digest,
    }
}

/// The reference to pull: without a tag or digest Docker would pull every tag
pub fn with_default_tag(image: &str) -> String {
    if !image.contains('@') && repository(image) == image {
        format!("{}:latest", image)
    } else {
        image.to_string()
    }
}

/// Registry host of an image; Docker Hub images have none in their name
pub fn registry(image: &str) -> &str {
    match repository(image).split_once('/') {
        // Docker's rule (splitDockerDomain): a dot, a port, "localhost", or
        // any uppercase letter makes the first component a registry host
        Some((first, _))
            if first.contains('.')
                || first.contains(':')
                || first == "localhost"
                || first.chars().any(|c| c.is_ascii_uppercase()) =>
        {
            first
        }
        _ => "docker.io",
    }
}

/// Host names a service is reachable at: `group/app` gives `group__app` and
/// `group-app`, plus any aliases from `alias` (comma or space separated)
pub fn service_aliases(image: &str, alias: Option<&str>) -> Vec<String> {
    let repo = repository(image);
    let mut aliases = vec![repo.replace('/', "__"), repo.replace('/', "-")];
    aliases.extend(
        alias
            .unwrap_or("")
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|a| !a.is_empty())
            .map(str::to_string),
    );
    aliases.dedup();
    aliases
}

/// A Docker-in-Docker image (docker:dind, docker:27-dind, a mirror of them)
pub fn is_dind(image: &str) -> bool {
    image.to_ascii_lowercase().contains("dind")
}

/// Command of a dind service with `--insecure-registry` for each of
/// `insecure` it does not have yet. The dind entrypoint puts arguments that
/// start with `-` after dockerd's defaults, so flags alone are a full command.
/// `None` keeps the image's CMD.
pub fn dind_command(command: Option<&[String]>, insecure: &[String]) -> Option<Vec<String>> {
    if insecure.is_empty() {
        return command.map(<[String]>::to_vec);
    }
    let mut args = command.map(<[String]>::to_vec).unwrap_or_default();
    for host in insecure {
        let flag = format!("--insecure-registry={}", host);
        if !args.contains(&flag) {
            args.push(flag);
        }
    }
    Some(args)
}

fn host_of(url: &str) -> &str {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

fn is_docker_hub(host: &str) -> bool {
    matches!(
        host,
        "docker.io" | "index.docker.io" | "registry-1.docker.io"
    )
}

/// The registry credentials GitLab sent for the registry `image` is pulled from
pub fn credentials_for<'a>(image: &str, credentials: &'a [Credential]) -> Option<&'a Credential> {
    let registry = registry(image);
    credentials.iter().find(|c| {
        let host = host_of(&c.url);
        c.cred_type.eq_ignore_ascii_case("registry")
            && (host == registry || (is_docker_hub(host) && is_docker_hub(registry)))
    })
}

/// Username and password for the registry of `image` from a Docker client
/// config (`{"auths": {"<registry>": {"auth": "<base64 user:pass>"}}}`), the
/// format of `DOCKER_AUTH_CONFIG` and ~/.docker/config.json. Credential
/// helpers (`credsStore`, `credHelpers`) are not run.
pub fn docker_config_auth(config: &str, image: &str) -> Option<(String, String)> {
    use base64::Engine;

    let registry = registry(image);
    let config: serde_json::Value = serde_json::from_str(config).ok()?;
    let (_, entry) = config.get("auths")?.as_object()?.iter().find(|(key, _)| {
        let host = host_of(key);
        host == registry || (is_docker_hub(host) && is_docker_hub(registry))
    })?;
    if let (Some(user), Some(password)) = (
        entry.get("username").and_then(|v| v.as_str()),
        entry.get("password").and_then(|v| v.as_str()),
    ) {
        return Some((user.to_string(), password.to_string()));
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(entry.get("auth")?.as_str()?.trim())
        .ok()?;
    let (user, password) = String::from_utf8(decoded)
        .ok()?
        .split_once(':')
        .map(|(u, p)| (u.to_string(), p.to_string()))?;
    Some((user, password))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullPolicy {
    Always,
    IfNotPresent,
    Never,
}

impl PullPolicy {
    fn parse(policy: &str) -> Option<Self> {
        match policy {
            "always" => Some(PullPolicy::Always),
            "if-not-present" => Some(PullPolicy::IfNotPresent),
            "never" => Some(PullPolicy::Never),
            _ => None,
        }
    }

    /// The policy for a job image. Without a job policy the runner's default is
    /// used; a job may only pick policies the runner allows (`allowed`, or just
    /// the default when empty), like gitlab-runner's `allowed_pull_policies`.
    /// Otherwise a job could, for example, use `never` to run another project's
    /// private image from the local cache without registry credentials.
    pub fn resolve(
        job_policies: &[String],
        default: &str,
        allowed: &[String],
    ) -> Result<Self, String> {
        if job_policies.is_empty() {
            return Ok(Self::parse(default).unwrap_or(PullPolicy::Always));
        }
        let allowed: Vec<&str> = if allowed.is_empty() {
            vec![default]
        } else {
            allowed.iter().map(String::as_str).collect()
        };
        job_policies
            .iter()
            .map(String::as_str)
            .filter(|policy| allowed.contains(policy))
            .find_map(Self::parse)
            .ok_or_else(|| {
                format!(
                    "pull_policy {:?} is not one of the runner's allowed pull policies {:?}",
                    job_policies, allowed
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(url: &str) -> Credential {
        serde_json::from_value(serde_json::json!({
            "type": "registry", "url": url, "username": "u", "password": "p"
        }))
        .unwrap()
    }

    #[test]
    fn splits_repository_and_registry() {
        assert_eq!(repository("postgres:16"), "postgres");
        assert_eq!(repository("postgres"), "postgres");
        assert_eq!(repository("localhost:5000/app:1.0"), "localhost:5000/app");
        assert_eq!(repository("reg.io/g/app@sha256:abc"), "reg.io/g/app");
        assert_eq!(registry("postgres:16"), "docker.io");
        assert_eq!(registry("bitnami/redis"), "docker.io");
        assert_eq!(
            registry("registry.gitlab.com/g/app:1"),
            "registry.gitlab.com"
        );
        assert_eq!(registry("localhost:5000/app"), "localhost:5000");
        // Docker treats an uppercase first component as a host, so the Docker
        // Hub login must not be offered for it
        assert_eq!(registry("Myhost/app"), "Myhost");
        let hub = r#"{"auths": {"https://index.docker.io/v1/": {"auth": "aHViOnB3"}}}"#;
        assert_eq!(docker_config_auth(hub, "Myhost/app"), None);
    }

    #[test]
    fn pulls_latest_when_no_tag_is_given() {
        assert_eq!(with_default_tag("alpine"), "alpine:latest");
        assert_eq!(with_default_tag("alpine:3.20"), "alpine:3.20");
        assert_eq!(
            with_default_tag("localhost:5000/app"),
            "localhost:5000/app:latest"
        );
        assert_eq!(with_default_tag("app@sha256:abc"), "app@sha256:abc");
    }

    #[test]
    fn service_aliases_match_gitlab_runner() {
        assert_eq!(
            service_aliases("postgres:16", None),
            vec!["postgres".to_string()]
        );
        assert_eq!(
            service_aliases("registry.example.com/group/cache:7", Some("cache, redis")),
            vec![
                "registry.example.com__group__cache",
                "registry.example.com-group-cache",
                "cache",
                "redis"
            ]
        );
    }

    #[test]
    fn recognises_dind_images() {
        for image in [
            "docker:dind",
            "docker:27-dind",
            "harbor.corp:5000/library/docker:dind-rootless",
        ] {
            assert!(is_dind(image), "{}", image);
        }
        for image in ["docker:27-cli", "postgres:16", "alpine"] {
            assert!(!is_dind(image), "{}", image);
        }
    }

    #[test]
    fn dind_command_appends_insecure_registries_once() {
        let insecure = vec!["harbor.corp".to_string(), "10.0.0.5:5000".to_string()];
        let given = vec![
            "--tls=false".to_string(),
            "--insecure-registry=harbor.corp".to_string(),
        ];

        assert_eq!(
            dind_command(Some(&given), &insecure),
            Some(vec![
                "--tls=false".to_string(),
                "--insecure-registry=harbor.corp".to_string(),
                "--insecure-registry=10.0.0.5:5000".to_string(),
            ])
        );
        assert_eq!(
            dind_command(None, &insecure[..1]),
            Some(vec!["--insecure-registry=harbor.corp".to_string()])
        );
        assert_eq!(dind_command(None, &[]), None, "the image's CMD is kept");
        assert_eq!(dind_command(Some(&given), &[]), Some(given.clone()));
    }

    #[test]
    fn reads_registry_logins_from_docker_config() {
        // "user:secret" and "hub:pw" in base64
        let config = r#"{"auths": {
            "https://harbor.example.com": {"auth": "dXNlcjpzZWNyZXQ="},
            "https://index.docker.io/v1/": {"auth": "aHViOnB3"},
            "plain.example.com": {"username": "u", "password": "p"}
        }, "credsStore": "desktop"}"#;
        let auth = |image| docker_config_auth(config, image);

        assert_eq!(
            auth("harbor.example.com/library/app:1"),
            Some(("user".into(), "secret".into()))
        );
        assert_eq!(auth("postgres:16"), Some(("hub".into(), "pw".into())));
        assert_eq!(auth("plain.example.com/x"), Some(("u".into(), "p".into())));
        assert_eq!(auth("quay.io/org/app"), None);
        assert_eq!(docker_config_auth("not json", "postgres"), None);
    }

    #[test]
    fn picks_credentials_of_the_image_registry() {
        let creds = vec![
            cred("https://registry.gitlab.com"),
            cred("https://index.docker.io/v1/"),
        ];

        let gitlab = credentials_for("registry.gitlab.com/g/app:1", &creds).unwrap();
        assert_eq!(gitlab.url, "https://registry.gitlab.com");
        let hub = credentials_for("postgres:16", &creds).unwrap();
        assert_eq!(hub.url, "https://index.docker.io/v1/");
        assert!(credentials_for("quay.io/org/app", &creds).is_none());
    }

    #[test]
    fn job_pull_policy_limited_to_allowed_policies() {
        let s = |v: &[&str]| v.iter().map(|p| p.to_string()).collect::<Vec<_>>();

        assert_eq!(
            PullPolicy::resolve(&[], "if-not-present", &[]),
            Ok(PullPolicy::IfNotPresent)
        );
        // Only the runner's own policy is allowed by default
        assert!(PullPolicy::resolve(&s(&["never"]), "always", &[]).is_err());
        assert!(PullPolicy::resolve(&s(&["if-not-present"]), "always", &[]).is_err());
        assert_eq!(
            PullPolicy::resolve(&s(&["always"]), "always", &[]),
            Ok(PullPolicy::Always)
        );
        // The first job policy that is allowed wins
        assert_eq!(
            PullPolicy::resolve(
                &s(&["never", "if-not-present"]),
                "always",
                &s(&["always", "if-not-present"])
            ),
            Ok(PullPolicy::IfNotPresent)
        );
    }
}
