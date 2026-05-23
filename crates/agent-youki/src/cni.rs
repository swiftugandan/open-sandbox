use std::collections::HashMap;
use std::net::TcpListener;
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

pub fn generate_conflist(network_name: &str) -> CniConfList {
    let bridge_config = HashMap::from([
        (
            "bridge".to_string(),
            serde_json::Value::String("cni0".to_string()),
        ),
        ("isGateway".to_string(), serde_json::Value::Bool(true)),
        ("ipMasq".to_string(), serde_json::Value::Bool(true)),
        (
            "ipam".to_string(),
            serde_json::json!({
                "type": "host-local",
                "subnet": "10.88.0.0/16",
                "routes": [{ "dst": "0.0.0.0/0" }]
            }),
        ),
    ]);

    let portmap_config = HashMap::from([(
        "capabilities".to_string(),
        serde_json::json!({ "portMappings": true }),
    )]);

    CniConfList {
        cni_version: "1.0.0".to_string(),
        name: network_name.to_string(),
        plugins: vec![
            CniPlugin {
                plugin_type: "bridge".to_string(),
                config: bridge_config,
            },
            CniPlugin {
                plugin_type: "portmap".to_string(),
                config: portmap_config,
            },
        ],
    }
}

pub fn build_cni_env(
    command: &str,
    container_id: &str,
    netns: &str,
    ifname: &str,
    cni_path: &str,
) -> HashMap<String, String> {
    HashMap::from([
        ("CNI_COMMAND".to_string(), command.to_string()),
        ("CNI_CONTAINERID".to_string(), container_id.to_string()),
        ("CNI_NETNS".to_string(), netns.to_string()),
        ("CNI_IFNAME".to_string(), ifname.to_string()),
        ("CNI_PATH".to_string(), cni_path.to_string()),
    ])
}

pub fn allocate_port() -> Result<u16, AgentError> {
    let listener = TcpListener::bind("0.0.0.0:0").map_err(|e| AgentError::Runtime {
        detail: format!("failed to bind for port allocation: {e}"),
    })?;

    let port = listener
        .local_addr()
        .map_err(|e| AgentError::Runtime {
            detail: format!("failed to get local address: {e}"),
        })?
        .port();

    drop(listener);
    Ok(port)
}

pub async fn invoke_cni(
    conflist: &CniConfList,
    command: &str,
    container_id: &str,
    netns: &str,
    cni_path: &Path,
) -> Result<CniResult, AgentError> {
    let env = build_cni_env(
        command,
        container_id,
        netns,
        "eth0",
        &cni_path.to_string_lossy(),
    );

    let mut prev_result: Option<serde_json::Value> = None;

    for plugin in &conflist.plugins {
        let plugin_path = cni_path.join(&plugin.plugin_type);

        let mut plugin_conf = plugin.config.clone();
        plugin_conf.insert(
            "type".to_string(),
            serde_json::Value::String(plugin.plugin_type.clone()),
        );
        plugin_conf.insert(
            "cniVersion".to_string(),
            serde_json::Value::String(conflist.cni_version.clone()),
        );
        plugin_conf.insert(
            "name".to_string(),
            serde_json::Value::String(conflist.name.clone()),
        );
        if let Some(ref result) = prev_result {
            plugin_conf.insert("prevResult".to_string(), result.clone());
        }

        let plugin_json = serde_json::to_vec(&plugin_conf).map_err(|e| AgentError::Runtime {
            detail: format!("failed to serialize plugin config: {e}"),
        })?;

        use tokio::io::AsyncWriteExt;

        let mut child = tokio::process::Command::new(&plugin_path)
            .envs(&env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to spawn CNI plugin {}: {e}", plugin.plugin_type),
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(&plugin_json)
                .await
                .map_err(|e| AgentError::Runtime {
                    detail: format!("failed to write to CNI plugin stdin: {e}"),
                })?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("CNI plugin {} failed: {e}", plugin.plugin_type),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Ok(cni_err) = parse_cni_error(&output.stdout) {
                return Err(AgentError::Runtime {
                    detail: format!(
                        "CNI plugin {} error: {} ({})",
                        plugin.plugin_type, cni_err.msg, stderr
                    ),
                });
            }
            return Err(AgentError::Runtime {
                detail: format!(
                    "CNI plugin {} exited with {}: {}",
                    plugin.plugin_type, output.status, stderr
                ),
            });
        }

        if command == "ADD" && !output.stdout.is_empty() {
            prev_result = serde_json::from_slice(&output.stdout).ok();
        }
    }

    match prev_result {
        Some(result) => serde_json::from_value(result).map_err(|e| AgentError::Runtime {
            detail: format!("failed to parse final CNI result: {e}"),
        }),
        None => Ok(CniResult {
            cni_version: conflist.cni_version.clone(),
            ips: None,
        }),
    }
}

pub fn inject_port_mappings(conflist: &mut CniConfList, host_port: u16, container_port: u16) {
    for plugin in &mut conflist.plugins {
        if plugin.plugin_type == "portmap" {
            plugin.config.insert(
                "runtimeConfig".to_string(),
                serde_json::json!({
                    "portMappings": [{
                        "hostPort": host_port,
                        "containerPort": container_port,
                        "protocol": "tcp"
                    }]
                }),
            );
        }
    }
}

pub fn parse_cni_result(output: &[u8]) -> Result<CniResult, AgentError> {
    serde_json::from_slice(output).map_err(|e| AgentError::Runtime {
        detail: format!("failed to parse CNI result: {e}"),
    })
}

pub fn parse_cni_error(output: &[u8]) -> Result<CniError, AgentError> {
    serde_json::from_slice(output).map_err(|e| AgentError::Runtime {
        detail: format!("failed to parse CNI error: {e}"),
    })
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

        let types: Vec<_> = conflist
            .plugins
            .iter()
            .map(|p| p.plugin_type.as_str())
            .collect();
        assert!(types.contains(&"bridge"));
        assert!(types.contains(&"portmap"));
    }

    #[test]
    fn port_allocation_returns_valid_ports() {
        let mut ports = HashSet::new();
        for _ in 0..10 {
            let port = allocate_port().unwrap();
            assert!(port > 0);
            ports.insert(port);
        }
        // Most ports should be unique; kernel can reuse after close
        assert!(
            ports.len() >= 5,
            "too many duplicate ports: only {} unique out of 10",
            ports.len()
        );
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
        let json =
            br#"{"cniVersion":"1.0.0","ips":[{"address":"10.88.0.2/16","gateway":"10.88.0.1"}]}"#;
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
