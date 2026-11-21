//! Request bodies and responses of the Docker Engine API calls the runner
//! makes, with only the fields it sets or reads. Request fields that are
//! `None` are left out, so dockerd applies its own defaults, and responses
//! ignore every field not listed here.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `POST /containers/create` body
#[derive(Debug, Clone, Default, Serialize)]
pub struct ContainerCreateBody {
    #[serde(rename = "User", skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(rename = "Env", skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "Cmd", skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Image", skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(rename = "WorkingDir", skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(rename = "Entrypoint", skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "NetworkDisabled", skip_serializing_if = "Option::is_none")]
    pub network_disabled: Option<bool>,
    #[serde(rename = "Labels", skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
    #[serde(rename = "HostConfig", skip_serializing_if = "Option::is_none")]
    pub host_config: Option<HostConfig>,
    #[serde(rename = "NetworkingConfig", skip_serializing_if = "Option::is_none")]
    pub networking_config: Option<NetworkingConfig>,
}

/// Host settings of a container to create
#[derive(Debug, Clone, Default, Serialize)]
pub struct HostConfig {
    #[serde(rename = "CpuShares", skip_serializing_if = "Option::is_none")]
    pub cpu_shares: Option<i64>,
    #[serde(rename = "Memory", skip_serializing_if = "Option::is_none")]
    pub memory: Option<i64>,
    #[serde(rename = "MemoryReservation", skip_serializing_if = "Option::is_none")]
    pub memory_reservation: Option<i64>,
    #[serde(rename = "MemorySwap", skip_serializing_if = "Option::is_none")]
    pub memory_swap: Option<i64>,
    #[serde(rename = "NanoCpus", skip_serializing_if = "Option::is_none")]
    pub nano_cpus: Option<i64>,
    #[serde(rename = "PidsLimit", skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<i64>,
    #[serde(rename = "Binds", skip_serializing_if = "Option::is_none")]
    pub binds: Option<Vec<String>>,
    #[serde(rename = "NetworkMode", skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    #[serde(rename = "Mounts", skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<Mount>>,
    #[serde(rename = "OomScoreAdj", skip_serializing_if = "Option::is_none")]
    pub oom_score_adj: Option<i64>,
    #[serde(rename = "Privileged", skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,
    #[serde(rename = "StorageOpt", skip_serializing_if = "Option::is_none")]
    pub storage_opt: Option<HashMap<String, String>>,
    #[serde(rename = "Tmpfs", skip_serializing_if = "Option::is_none")]
    pub tmpfs: Option<HashMap<String, String>>,
    #[serde(rename = "ShmSize", skip_serializing_if = "Option::is_none")]
    pub shm_size: Option<i64>,
}

/// A mount of a container to create
#[derive(Debug, Clone, Default, Serialize)]
pub struct Mount {
    #[serde(rename = "Target", skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(rename = "Source", skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub typ: Option<MountType>,
    #[serde(rename = "ReadOnly", skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// The kinds of mounts the runner creates
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MountType {
    #[serde(rename = "bind")]
    Bind,
}

/// Networks a container to create joins, by network name
#[derive(Debug, Clone, Default, Serialize)]
pub struct NetworkingConfig {
    #[serde(rename = "EndpointsConfig", skip_serializing_if = "Option::is_none")]
    pub endpoints_config: Option<HashMap<String, EndpointConfig>>,
}

/// How a container to create joins a network
#[derive(Debug, Clone, Default, Serialize)]
pub struct EndpointConfig {
    #[serde(rename = "Aliases", skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
}

/// `POST /containers/create` response
#[derive(Debug, Deserialize)]
pub struct ContainerCreateResponse {
    #[serde(rename = "Id")]
    pub id: String,
}

/// `GET /containers/{id}/json` response
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerInspectResponse {
    #[serde(rename = "Name")]
    pub name: Option<String>,
    #[serde(rename = "Config")]
    pub config: Option<ContainerConfig>,
    #[serde(rename = "NetworkSettings")]
    pub network_settings: Option<NetworkSettings>,
    #[cfg(test)]
    #[serde(rename = "HostConfig")]
    pub host_config: Option<InspectHostConfig>,
}

/// Configuration of an existing container
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    /// Keys like `80/tcp`; the values are always empty objects
    #[serde(rename = "ExposedPorts")]
    pub exposed_ports: Option<HashMap<String, serde::de::IgnoredAny>>,
    #[serde(rename = "Labels")]
    pub labels: Option<HashMap<String, String>>,
}

/// Network settings of an existing container
#[derive(Debug, Clone, Default, Deserialize)]
pub struct NetworkSettings {
    #[serde(rename = "Networks")]
    pub networks: Option<HashMap<String, EndpointSettings>>,
}

/// An existing container's place on one network
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EndpointSettings {
    #[serde(rename = "Gateway")]
    pub gateway: Option<String>,
    #[serde(rename = "IPAddress")]
    pub ip_address: Option<String>,
}

/// Host settings of an existing container
#[cfg(test)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct InspectHostConfig {
    #[serde(rename = "CpuShares")]
    pub cpu_shares: Option<i64>,
}

/// An entry of `GET /containers/json`
#[derive(Debug, Deserialize)]
pub struct ContainerSummary {
    #[serde(rename = "Id")]
    pub id: Option<String>,
    #[cfg(test)]
    #[serde(rename = "Names")]
    pub names: Option<Vec<String>>,
}

/// One message of `POST /containers/{id}/wait`
#[derive(Debug, Deserialize)]
pub struct ContainerWaitResponse {
    #[serde(rename = "StatusCode")]
    pub status_code: i64,
    #[serde(rename = "Error")]
    pub error: Option<ContainerWaitExitError>,
}

/// Why waiting for a container failed
#[derive(Debug, Deserialize)]
pub struct ContainerWaitExitError {
    #[serde(rename = "Message")]
    pub message: Option<String>,
}

/// One message of the progress of `POST /images/create`
#[derive(Debug, Deserialize)]
pub struct CreateImageInfo {
    pub error: Option<String>,
}

/// Login for a registry, sent base64-encoded in the X-Registry-Auth header.
/// Every field is sent, unset ones as `null`.
#[derive(Clone, Default, Serialize)]
pub struct DockerCredentials {
    pub username: Option<String>,
    pub password: Option<String>,
    pub auth: Option<String>,
    pub email: Option<String>,
    pub serveraddress: Option<String>,
    pub identitytoken: Option<String>,
    pub registrytoken: Option<String>,
}

/// `POST /networks/create` body
#[derive(Debug, Default, Serialize)]
pub struct NetworkCreateRequest {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Driver", skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(rename = "Labels", skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
}

/// An entry of `GET /networks`
#[derive(Debug, Deserialize)]
pub struct NetworkSummary {
    #[serde(rename = "Name")]
    pub name: Option<String>,
}

/// `POST /volumes/create` body
#[derive(Debug, Default, Serialize)]
pub struct VolumeCreateOptions {
    #[serde(rename = "Name", skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "Labels", skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
}

/// `GET /volumes` response
#[cfg(test)]
#[derive(Debug, Deserialize)]
pub struct VolumeListResponse {
    #[serde(rename = "Volumes")]
    pub volumes: Option<Vec<Volume>>,
}

/// A volume of `GET /volumes`
#[cfg(test)]
#[derive(Debug, Deserialize)]
pub struct Volume {
    #[serde(rename = "Name")]
    pub name: String,
}

/// `POST /containers/{id}/exec` body
#[derive(Debug, Default, Serialize)]
pub struct ExecConfig<'a> {
    #[serde(rename = "AttachStdout", skip_serializing_if = "Option::is_none")]
    pub attach_stdout: Option<bool>,
    #[serde(rename = "AttachStderr", skip_serializing_if = "Option::is_none")]
    pub attach_stderr: Option<bool>,
    #[serde(rename = "Env", skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<&'a str>>,
    #[serde(rename = "Cmd", skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<&'a str>>,
    #[serde(rename = "User", skip_serializing_if = "Option::is_none")]
    pub user: Option<&'a str>,
    #[serde(rename = "WorkingDir", skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<&'a str>,
}

/// `POST /containers/{id}/exec` response
#[derive(Debug, Deserialize)]
pub struct CreateExecResults {
    #[serde(rename = "Id")]
    pub id: String,
}

/// `GET /exec/{id}/json` response
#[derive(Debug, Deserialize)]
pub struct ExecInspectResponse {
    #[serde(rename = "ExitCode")]
    pub exit_code: Option<i64>,
}

/// `GET /info` response: the daemon's settings the runner checks
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SystemInfo {
    #[serde(rename = "HttpProxy")]
    pub http_proxy: Option<String>,
    #[serde(rename = "HttpsProxy")]
    pub https_proxy: Option<String>,
    #[serde(rename = "RegistryConfig")]
    pub registry_config: Option<RegistryServiceConfig>,
}

/// dockerd's registry settings
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistryServiceConfig {
    #[serde(rename = "IndexConfigs")]
    pub index_configs: Option<HashMap<String, IndexInfo>>,
    #[serde(rename = "InsecureRegistryCIDRs")]
    pub insecure_registry_cidrs: Option<Vec<String>>,
}

/// dockerd's settings of one registry
#[derive(Debug, Clone, Default, Deserialize)]
pub struct IndexInfo {
    #[serde(rename = "Secure")]
    pub secure: Option<bool>,
}
