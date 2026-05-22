use std::collections::HashMap;
use std::path::Path;

use open_sandbox_contracts::error::AgentError;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CniConfList {
    pub cni_version: String,
    pub name: String,
    pub plugins: Vec<CniPlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CniPlugin {
    #[serde(rename = "type")]
    pub plugin_type: String,
    #[serde(flatten)]
    pub config: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniResult {
    #[serde(rename = "cniVersion")]
    pub cni_version: String,
    pub ips: Option<Vec<CniIpResult>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniIpResult {
    pub address: String,
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CniError {
    pub code: u32,
    pub msg: String,
    pub details: Option<String>,
}

pub fn generate_conflist(_network_name: &str) -> CniConfList {
    todo!()
}

pub fn build_cni_env(
    _command: &str,
    _container_id: &str,
    _netns: &str,
    _ifname: &str,
    _cni_path: &str,
) -> HashMap<String, String> {
    todo!()
}

pub fn allocate_port() -> Result<u16, AgentError> {
    todo!()
}

pub async fn invoke_cni(
    _conflist: &CniConfList,
    _command: &str,
    _container_id: &str,
    _netns: &str,
    _cni_path: &Path,
) -> Result<CniResult, AgentError> {
    todo!()
}

pub fn parse_cni_result(_output: &[u8]) -> Result<CniResult, AgentError> {
    todo!()
}

pub fn parse_cni_error(_output: &[u8]) -> Result<CniError, AgentError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn conflist_has_bridge_and_portmap() {
        let conflist = generate_conflist("open-sandbox");

        assert_eq!(conflist.cni_version, "1.0.0");
        assert_eq!(conflist.name, "open-sandbox");

        let types: Vec<_> = conflist.plugins.iter().map(|p| p.plugin_type.as_str()).collect();
        assert!(types.contains(&"bridge"));
        assert!(types.contains(&"portmap"));
    }

    #[test]
    fn port_allocation_returns_unique_ports() {
        let mut ports = HashSet::new();
        for _ in 0..100 {
            let port = allocate_port().unwrap();
            assert!(port > 0);
            assert!(ports.insert(port), "duplicate port: {port}");
        }
    }

    #[test]
    fn cni_env_vars_for_add_command() {
        let env = build_cni_env(
            "ADD",
            "sandbox-abc123",
            "/proc/42/ns/net",
            "eth0",
            "/opt/cni/bin",
        );

        assert_eq!(env.get("CNI_COMMAND").unwrap(), "ADD");
        assert_eq!(env.get("CNI_CONTAINERID").unwrap(), "sandbox-abc123");
        assert_eq!(env.get("CNI_NETNS").unwrap(), "/proc/42/ns/net");
        assert_eq!(env.get("CNI_IFNAME").unwrap(), "eth0");
        assert_eq!(env.get("CNI_PATH").unwrap(), "/opt/cni/bin");
    }

    #[test]
    fn cni_env_vars_for_del_command() {
        let env = build_cni_env(
            "DEL",
            "sandbox-abc123",
            "/proc/42/ns/net",
            "eth0",
            "/opt/cni/bin",
        );

        assert_eq!(env.get("CNI_COMMAND").unwrap(), "DEL");
    }

    #[test]
    fn parse_valid_cni_result() {
        let json = br#"{"cniVersion":"1.0.0","ips":[{"address":"10.88.0.2/16","gateway":"10.88.0.1"}]}"#;
        let result = parse_cni_result(json).unwrap();

        assert_eq!(result.cni_version, "1.0.0");
        let ips = result.ips.unwrap();
        assert_eq!(ips.len(), 1);
        assert_eq!(ips[0].address, "10.88.0.2/16");
        assert_eq!(ips[0].gateway.as_deref(), Some("10.88.0.1"));
    }

    #[test]
    fn parse_valid_cni_error() {
        let json = br#"{"code":7,"msg":"failed to allocate for range 0: no IP addresses available in range set","details":"some details"}"#;
        let err = parse_cni_error(json).unwrap();

        assert_eq!(err.code, 7);
        assert!(err.msg.contains("no IP addresses available"));
        assert_eq!(err.details.as_deref(), Some("some details"));
    }
}
