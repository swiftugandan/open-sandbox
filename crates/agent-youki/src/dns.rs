use std::path::Path;

use open_sandbox_contracts::error::AgentError;

const FALLBACK_RESOLV_CONF: &str = "nameserver 8.8.8.8\nnameserver 8.8.4.4\n";

pub fn generate_resolv_conf(host_content: &str) -> String {
    let filtered: Vec<&str> = host_content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("nameserver") {
                return !trimmed.is_empty();
            }
            let addr = match trimmed.split_whitespace().nth(1) {
                Some(a) => a,
                None => return false,
            };
            !is_loopback(addr)
        })
        .collect();

    let has_nameserver = filtered.iter().any(|l| l.trim().starts_with("nameserver"));
    if !has_nameserver {
        return FALLBACK_RESOLV_CONF.to_string();
    }

    let mut result = filtered.join("\n");
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

fn is_loopback(addr: &str) -> bool {
    addr.starts_with("127.") || addr == "::1"
}

pub async fn write_resolv_conf(rootfs: &Path) -> Result<(), AgentError> {
    let etc_dir = rootfs.join("etc");
    tokio::fs::create_dir_all(&etc_dir)
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("failed to create {}/etc: {e}", rootfs.display()),
        })?;

    let host_content = tokio::fs::read_to_string("/etc/resolv.conf")
        .await
        .unwrap_or_default();

    let content = generate_resolv_conf(&host_content);
    tokio::fs::write(etc_dir.join("resolv.conf"), content.as_bytes())
        .await
        .map_err(|e| AgentError::Runtime {
            detail: format!("failed to write resolv.conf: {e}"),
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_ipv4_loopback() {
        let input = "nameserver 127.0.0.1\nnameserver 8.8.8.8\n";
        let result = generate_resolv_conf(input);
        assert!(!result.contains("127.0.0.1"));
        assert!(result.contains("8.8.8.8"));
    }

    #[test]
    fn filters_ipv6_loopback() {
        let input = "nameserver ::1\nnameserver 1.1.1.1\n";
        let result = generate_resolv_conf(input);
        assert!(!result.contains("::1"));
        assert!(result.contains("1.1.1.1"));
    }

    #[test]
    fn falls_back_to_public_dns_when_all_loopback() {
        let input = "nameserver 127.0.0.1\nnameserver 127.0.0.53\n";
        let result = generate_resolv_conf(input);
        assert!(result.contains("8.8.8.8"));
        assert!(result.contains("8.8.4.4"));
    }

    #[test]
    fn falls_back_on_empty_input() {
        let result = generate_resolv_conf("");
        assert!(result.contains("8.8.8.8"));
        assert!(result.contains("8.8.4.4"));
    }

    #[test]
    fn preserves_non_nameserver_lines() {
        let input = "search example.com\nnameserver 10.0.0.1\noptions ndots:5\n";
        let result = generate_resolv_conf(input);
        assert!(result.contains("search example.com"));
        assert!(result.contains("10.0.0.1"));
        assert!(result.contains("options ndots:5"));
    }

    #[test]
    fn preserves_valid_nameservers() {
        let input = "nameserver 10.0.0.1\nnameserver 10.0.0.2\n";
        let result = generate_resolv_conf(input);
        assert!(result.contains("10.0.0.1"));
        assert!(result.contains("10.0.0.2"));
        assert!(!result.contains("8.8.8.8"));
    }

    #[test]
    fn result_ends_with_newline() {
        let input = "nameserver 10.0.0.1";
        let result = generate_resolv_conf(input);
        assert!(result.ends_with('\n'));
    }

    #[tokio::test]
    async fn write_resolv_conf_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        write_resolv_conf(&rootfs).await.unwrap();

        let content = std::fs::read_to_string(rootfs.join("etc/resolv.conf")).unwrap();
        assert!(content.contains("nameserver"));
    }
}
