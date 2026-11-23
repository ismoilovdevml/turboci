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
    #[serde(rename = "ExtraHosts", skip_serializing_if = "Option::is_none")]
    pub extra_hosts: Option<Vec<String>>,
    #[serde(rename = "Dns", skip_serializing_if = "Option::is_none")]
    pub dns: Option<Vec<String>>,
    #[serde(rename = "DnsSearch", skip_serializing_if = "Option::is_none")]
    pub dns_search: Option<Vec<String>>,
    #[serde(rename = "CapAdd", skip_serializing_if = "Option::is_none")]
    pub cap_add: Option<Vec<String>>,
    #[serde(rename = "CapDrop", skip_serializing_if = "Option::is_none")]
    pub cap_drop: Option<Vec<String>>,
    #[serde(rename = "SecurityOpt", skip_serializing_if = "Option::is_none")]
    pub security_opt: Option<Vec<String>>,
    #[serde(rename = "Devices", skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<DeviceMapping>>,
    #[serde(rename = "DeviceRequests", skip_serializing_if = "Option::is_none")]
    pub device_requests: Option<Vec<DeviceRequest>>,
    #[serde(rename = "Sysctls", skip_serializing_if = "Option::is_none")]
    pub sysctls: Option<HashMap<String, String>>,
    #[serde(rename = "Ulimits", skip_serializing_if = "Option::is_none")]
    pub ulimits: Option<Vec<Ulimit>>,
    #[serde(rename = "Runtime", skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

/// A host device made available in a container
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceMapping {
    #[serde(rename = "PathOnHost")]
    pub path_on_host: String,
    #[serde(rename = "PathInContainer")]
    pub path_in_container: String,
    #[serde(rename = "CgroupPermissions")]
    pub cgroup_permissions: String,
}

/// A request for devices of a driver, such as GPUs (`docker run --gpus`)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeviceRequest {
    #[serde(rename = "Driver")]
    pub driver: String,
    /// -1 for all devices
    #[serde(rename = "Count")]
    pub count: i64,
    #[serde(rename = "DeviceIDs", skip_serializing_if = "Option::is_none")]
    pub device_ids: Option<Vec<String>>,
    #[serde(rename = "Capabilities")]
    pub capabilities: Vec<Vec<String>>,
    #[serde(rename = "Options")]
    pub options: HashMap<String, String>,
}

/// A resource limit (`ulimit`) of a container's processes
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ulimit {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Soft")]
    pub soft: i64,
    #[serde(rename = "Hard")]
    pub hard: i64,
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

/// `GET /images/{name}/json` response: the image's platform
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ImageInspect {
    #[serde(rename = "Os")]
    pub os: Option<String>,
    #[serde(rename = "Architecture")]
    pub architecture: Option<String>,
    #[serde(rename = "Variant")]
    pub variant: Option<String>,
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
    /// ID of the image the container was created from
    #[serde(rename = "ImageID")]
    pub image_id: Option<String>,
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

/// An entry of `GET /images/json`
#[derive(Debug, Clone, Deserialize)]
pub struct ImageSummary {
    #[serde(rename = "Id")]
    pub id: String,
    /// `repository:tag` references; `<none>:<none>` (or none at all) for an
    /// untagged image
    #[serde(rename = "RepoTags", default)]
    pub repo_tags: Option<Vec<String>>,
    #[serde(rename = "RepoDigests", default)]
    pub repo_digests: Option<Vec<String>>,
    /// When the image was built, in seconds since the epoch
    #[serde(rename = "Created", default)]
    pub created: i64,
    /// Bytes of the image, its layers shared with other images included
    #[serde(rename = "Size", default)]
    pub size: i64,
}

/// `POST /commit` response
#[cfg(test)]
#[derive(Debug, Deserialize)]
pub struct CommitResponse {
    #[serde(rename = "Id")]
    pub id: String,
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
    /// 409 when a network has the name: dockerd before 25 (API 1.44) would
    /// otherwise create a second one; later ones always check
    #[serde(rename = "CheckDuplicate", skip_serializing_if = "Option::is_none")]
    pub check_duplicate: Option<bool>,
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
