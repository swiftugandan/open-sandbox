use open_sandbox_agent::container::ContainerConfig;
use open_sandbox_contracts::error::AgentError;

use oci_spec::runtime::{
    LinuxBuilder, LinuxCpuBuilder, LinuxMemoryBuilder, LinuxNamespaceBuilder, LinuxNamespaceType,
    LinuxResourcesBuilder, ProcessBuilder, RootBuilder, SpecBuilder,
};

const CPU_PERIOD_USEC: u64 = 100_000;

pub fn generate_spec(config: &ContainerConfig) -> Result<oci_spec::runtime::Spec, AgentError> {
    generate_full_spec(config, "rootfs", None)
}

pub fn generate_full_spec(
    config: &ContainerConfig,
    rootfs_path: &str,
    cgroup_path: Option<&str>,
) -> Result<oci_spec::runtime::Spec, AgentError> {
    let cpu_quota = (config.cpu_limit_millicores as i64) * (CPU_PERIOD_USEC as i64) / 1000;

    let cpu = LinuxCpuBuilder::default()
        .quota(cpu_quota)
        .period(CPU_PERIOD_USEC)
        .build()
        .map_err(spec_err)?;

    let memory = LinuxMemoryBuilder::default()
        .limit(config.memory_limit_bytes as i64)
        .build()
        .map_err(spec_err)?;

    let resources = LinuxResourcesBuilder::default()
        .cpu(cpu)
        .memory(memory)
        .build()
        .map_err(spec_err)?;

    let namespaces: Vec<_> = [
        LinuxNamespaceType::Pid,
        LinuxNamespaceType::Network,
        LinuxNamespaceType::Mount,
        LinuxNamespaceType::Ipc,
        LinuxNamespaceType::Uts,
        LinuxNamespaceType::Cgroup,
    ]
    .into_iter()
    .map(|typ| {
        LinuxNamespaceBuilder::default()
            .typ(typ)
            .build()
            .map_err(spec_err)
    })
    .collect::<Result<_, _>>()?;

    let mut linux_builder = LinuxBuilder::default();
    linux_builder = linux_builder.resources(resources).namespaces(namespaces);
    if let Some(path) = cgroup_path {
        linux_builder = linux_builder.cgroups_path(path);
    }
    let linux = linux_builder.build().map_err(spec_err)?;

    let mut env: Vec<String> = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "TERM=xterm".to_string(),
    ];
    env.extend(config.env_vars.iter().map(|(k, v)| format!("{k}={v}")));

    let process = ProcessBuilder::default()
        .terminal(false)
        .args(vec!["sleep".to_string(), "infinity".to_string()])
        .env(env)
        .cwd("/".to_string())
        .build()
        .map_err(spec_err)?;

    let root = RootBuilder::default()
        .path(rootfs_path)
        .readonly(false)
        .build()
        .map_err(spec_err)?;

    let spec = SpecBuilder::default()
        .version("1.0.2")
        .root(root)
        .process(process)
        .linux(linux)
        .build()
        .map_err(spec_err)?;

    Ok(spec)
}

fn spec_err(e: impl std::fmt::Display) -> AgentError {
    AgentError::Runtime {
        detail: format!("OCI spec generation failed: {e}"),
    }
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

        // 500 millicores = quota 50000, period 100000 (microseconds)
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
        assert!(ns_types.contains(&LinuxNamespaceType::Pid));
        assert!(ns_types.contains(&LinuxNamespaceType::Network));
        assert!(ns_types.contains(&LinuxNamespaceType::Mount));
        assert!(ns_types.contains(&LinuxNamespaceType::Ipc));
        assert!(ns_types.contains(&LinuxNamespaceType::Uts));
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
