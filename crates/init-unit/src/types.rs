//! Core unit types and enums.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level unit definition — parsed from TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Unit {
    pub name: String,

    #[serde(default)]
    pub unit: UnitSection,

    /// Only set for .service units.
    #[serde(default)]
    pub service: Option<ServiceSection>,

    /// Only set for .socket units.
    #[serde(default)]
    pub socket: Option<SocketSection>,

    /// Only set for .mount units.
    #[serde(default)]
    pub mount: Option<MountSection>,

    /// Only set for .network units.
    #[serde(default)]
    pub network: Option<NetworkSection>,
}

impl Unit {
    pub fn is_service(&self) -> bool {
        self.service.is_some()
    }
    pub fn is_network(&self) -> bool {
        self.network.is_some()
    }
    pub fn hard_deps(&self) -> &[String] {
        &self.unit.requires
    }
    pub fn soft_deps(&self) -> &[String] {
        &self.unit.wants
    }
}

/// [unit] section — common to all unit types.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct UnitSection {
    #[serde(default)]
    pub description: String,

    #[serde(default)]
    pub documentation: Vec<String>,

    #[serde(default)]
    pub requires: Vec<String>,

    #[serde(default)]
    pub wants: Vec<String>,

    #[serde(default)]
    pub before: Vec<String>,

    #[serde(default)]
    pub after: Vec<String>,

    #[serde(default)]
    pub conflicts: Vec<String>,

    #[serde(default)]
    pub condition_path_exists: Vec<String>,
}

/// [service] section — process lifecycle configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServiceSection {
    /// Main command to start the service.
    pub exec_start: Vec<String>,

    /// Command to stop the service gracefully (optional).
    #[serde(default)]
    pub exec_stop: Option<Vec<String>>,

    /// Command to reload the service configuration (optional).
    #[serde(default)]
    pub exec_reload: Option<Vec<String>>,

    /// Service type: simple, forking, oneshot, notify.
    #[serde(default = "default_service_type")]
    pub service_type: ServiceType,

    /// Restart behavior.
    #[serde(default)]
    pub restart: RestartPolicy,

    /// Seconds to wait before restart.
    #[serde(default)]
    pub restart_sec: u32,

    /// Working directory for the service.
    #[serde(default)]
    pub working_directory: Option<String>,

    /// Environment variables.
    #[serde(default)]
    pub environment: HashMap<String, String>,

    /// Terminal device for native getty (e.g. "ttyAMA0").
    #[serde(default)]
    pub tty: Option<String>,

    /// Baud rate for the terminal device.
    #[serde(default)]
    pub tty_baud: Option<u32>,

    /// OOM score adjustment (-1000..1000).
    #[serde(default)]
    pub oom_score_adj: i32,

    /// cgroup memory limit (e.g. "512M").
    #[serde(default)]
    pub memory_max: Option<String>,

    /// cgroup CPU weight.
    #[serde(default)]
    pub cpu_weight: Option<u32>,

    /// Max number of tasks.
    #[serde(default)]
    pub tasks_max: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum ServiceType {
    #[serde(rename = "simple")]
    Simple,
    #[serde(rename = "forking")]
    Forking,
    #[serde(rename = "oneshot")]
    Oneshot,
    #[serde(rename = "notify")]
    Notify,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub enum RestartPolicy {
    #[serde(rename = "no")]
    #[default]
    No,
    #[serde(rename = "always")]
    Always,
    #[serde(rename = "on-failure")]
    OnFailure,
    #[serde(rename = "on-abnormal")]
    OnAbnormal,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SocketSection {
    /// Listen address: "0.0.0.0:8080" or "/run/myapp.sock".
    pub listen_stream: Vec<String>,

    /// Service to activate when a connection arrives.
    pub service: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MountSection {
    /// What to mount (device path, UUID, or filesystem label).
    pub what: String,

    /// Where to mount.
    pub mount_where: String,

    /// Filesystem type.
    pub fstype: String,

    /// Mount options.
    #[serde(default)]
    pub options: Vec<String>,
}

fn default_service_type() -> ServiceType {
    ServiceType::Simple
}

/// [network] section — static IP or DHCP configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NetworkSection {
    /// Interface name (e.g. "eth0").
    pub name: String,

    /// Use DHCP to obtain IP configuration.
    #[serde(default)]
    pub dhcp: bool,

    /// Static IP address in CIDR notation (e.g. "192.168.1.100/24").
    #[serde(default)]
    pub address: Option<String>,

    /// Default gateway (e.g. "192.168.1.1").
    #[serde(default)]
    pub gateway: Option<String>,

    /// DNS servers.
    #[serde(default)]
    pub dns: Option<Vec<String>>,
}
