use open_sandbox_agent::container::ContainerConfig;
use open_sandbox_contracts::error::AgentError;

pub fn generate_spec(_config: &ContainerConfig) -> Result<oci_spec::runtime::Spec, AgentError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use open_sandbox_agent::container::ContainerConfig;
    use open_sandbox_contracts::types::SandboxId;

    use super::*;

    fn test_config() -> ContainerConfig {
        ContainerConfig {
            sandbox_id: SandboxId::new(),
            image: "alpine:latest".to_string(),
            cpu_limit_millicores: 500,
            memory_limit_bytes: 256 * 1024 * 1024,
            env_vars: HashMap::from([("FOO".into(), "bar".into())]),
            exposed_port: 8080,
        }
    }

    #[test]
    fn spec_has_correct_cpu_quota_and_period() {
        let config = test_config();
        let spec = generate_spec(&config).unwrap();

        let linux = spec.linux().as_ref().unwrap();
        let resources = linux.resources().as_ref().unwrap();
        let cpu = resources.cpu().as_ref().unwrap();

        // OCI spec uses microseconds: 500 millicores = quota 50000, period 100000
        assert_eq!(cpu.quota(), Some(50000));
        assert_eq!(cpu.period(), Some(100000));
    }

    #[test]
    fn spec_has_correct_memory_limit() {
        let config = test_config();
        let spec = generate_spec(&config).unwrap();

        let linux = spec.linux().as_ref().unwrap();
        let resources = linux.resources().as_ref().unwrap();
        let memory = resources.memory().as_ref().unwrap();

        assert_eq!(memory.limit(), Some(256 * 1024 * 1024));
    }

    #[test]
    fn spec_has_correct_process_env() {
        let config = test_config();
        let spec = generate_spec(&config).unwrap();

        let process = spec.process().as_ref().unwrap();
        let env = process.env().as_ref().unwrap();

        assert!(env.iter().any(|e| e == "FOO=bar"));
    }

    #[test]
    fn spec_has_all_namespaces() {
        let config = test_config();
        let spec = generate_spec(&config).unwrap();

        let linux = spec.linux().as_ref().unwrap();
        let namespaces = linux.namespaces().as_ref().unwrap();

        let ns_types: Vec<_> = namespaces.iter().map(|ns| ns.typ()).collect();
        assert!(ns_types.contains(&oci_spec::runtime::LinuxNamespaceType::Pid));
        assert!(ns_types.contains(&oci_spec::runtime::LinuxNamespaceType::Network));
        assert!(ns_types.contains(&oci_spec::runtime::LinuxNamespaceType::Mount));
        assert!(ns_types.contains(&oci_spec::runtime::LinuxNamespaceType::Ipc));
        assert!(ns_types.contains(&oci_spec::runtime::LinuxNamespaceType::Uts));
    }

    #[test]
    fn spec_round_trip_serialization() {
        let config = test_config();
        let spec = generate_spec(&config).unwrap();

        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: oci_spec::runtime::Spec = serde_json::from_str(&json).unwrap();

        let linux_orig = spec.linux().as_ref().unwrap();
        let linux_de = deserialized.linux().as_ref().unwrap();
        let cpu_orig = linux_orig
            .resources()
            .as_ref()
            .unwrap()
            .cpu()
            .as_ref()
            .unwrap();
        let cpu_de = linux_de
            .resources()
            .as_ref()
            .unwrap()
            .cpu()
            .as_ref()
            .unwrap();

        assert_eq!(cpu_orig.quota(), cpu_de.quota());
        assert_eq!(cpu_orig.period(), cpu_de.period());
    }
}
