// src/config.rs
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OciConfig {
    pub oci_version: String,
    pub process: ProcessConfig,
    pub root: RootConfig,
    pub linux: Option<LinuxConfig>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProcessConfig {
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    pub cwd: String,
    pub hostname: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RootConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LinuxConfig {
    #[serde(default)]
    pub namespaces: Vec<NamespaceConfig>,
    pub cgroups_path: Option<String>,
    pub resources: Option<LinuxResources>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceConfig {
    #[serde(rename = "type")]
    pub ns_type: String,
    pub path: Option<PathBuf>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LinuxResources {
    pub pids: Option<PidLimit>,
    pub memory: Option<MemoryLimit>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PidLimit {
    pub limit: i64,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MemoryLimit {
    pub limit: Option<i64>,
}
