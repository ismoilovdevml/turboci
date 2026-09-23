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
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
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
