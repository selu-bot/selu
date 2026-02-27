/// Capability runner: spawns Docker containers with hardened flags,
/// waits for gRPC readiness, and tears them down when done.
///
/// Security posture for every container:
///   - read-only root filesystem
///   - all Linux capabilities dropped
///   - no-new-privileges
///   - non-root user (1000:1000)
///   - auto-removed on exit
///   - resource limits from the capability manifest
///   - connected to isolated internal bridge networks (no default route to internet)
///   - HTTP_PROXY / HTTPS_PROXY pointing at the orchestrator's egress proxy
use anyhow::{anyhow, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    StartContainerOptions,
};
use bollard::models::{
    ContainerInspectResponse, HostConfig, PortBinding, ResourcesUlimits,
};
use bollard::network::CreateNetworkOptions;
use bollard::Docker;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, info};
use uuid::Uuid;

use crate::capabilities::egress_proxy::{ContainerEgressPolicy, EgressRegistry};
use crate::capabilities::manifest::{CapabilityClass, CapabilityManifest};

/// The gRPC port that capability containers must listen on
pub const CAPABILITY_GRPC_PORT: u16 = 50051;

/// Name of the internal Docker bridge network for tool-class capabilities
pub const TOOLS_NETWORK: &str = "selu-tools-net";

/// Name of the internal Docker bridge network for environment-class capabilities
pub const ENVS_NETWORK: &str = "selu-envs-net";

/// A running capability container with its connection details
#[derive(Debug, Clone)]
pub struct RunningCapability {
    pub container_id: String,
    pub capability_id: String,
    /// Full gRPC address to reach this container, e.g. `http://127.0.0.1:55001`
    /// when running on the host, or `http://172.18.0.3:50051` when running
    /// inside Docker (using the container's IP on the shared bridge network).
    pub grpc_addr: String,
    /// Unique egress proxy auth token for this container
    pub egress_token: String,
}

/// Manages the Docker lifecycle for capability containers
#[derive(Clone)]
pub struct CapabilityRunner {
    docker: Arc<Docker>,
    egress_registry: EgressRegistry,
    /// Port the egress proxy listens on (resolved per-network with the gateway IP)
    egress_proxy_port: u16,
    /// When Selu itself runs inside Docker, this holds our own container ID
    /// so we can join the capability bridge networks and connect via container IP.
    self_container_id: Option<String>,
}

impl CapabilityRunner {
    pub fn new(egress_registry: EgressRegistry, egress_proxy_addr: impl Into<String>) -> Result<Self> {
        let docker = Docker::connect_with_local_defaults()
            .context("Failed to connect to Docker daemon")?;
        let addr_str = egress_proxy_addr.into();
        let port = addr_str
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .unwrap_or(8888);

        // Detect if we are running inside Docker by checking /.dockerenv
        let self_container_id = if std::path::Path::new("/.dockerenv").exists() {
            // Read our container ID from /proc/self/mountinfo (most reliable method)
            read_self_container_id()
        } else {
            None
        };

        if self_container_id.is_some() {
            info!("Running inside Docker — will connect to capabilities via container network IP");
        }

        Ok(Self {
            docker: Arc::new(docker),
            egress_registry,
            egress_proxy_port: port,
            self_container_id,
        })
    }

    /// Remove any orphaned `selu-cap-*` containers left over from a previous
    /// orchestrator run.  Called once at startup before accepting requests.
    pub async fn cleanup_stale_containers(&self) -> Result<()> {
        let mut filters = HashMap::new();
        filters.insert("name".to_string(), vec!["selu-cap-".to_string()]);

        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true, // include stopped containers too
                filters,
                ..Default::default()
            }))
            .await
            .context("Failed to list stale capability containers")?;

        if containers.is_empty() {
            debug!("No stale capability containers found");
            return Ok(());
        }

        info!(
            "Found {} stale capability container(s) from previous run, removing...",
            containers.len()
        );

        for container in &containers {
            let id = container.id.as_deref().unwrap_or("unknown");
            let names = container
                .names
                .as_ref()
                .map(|n| n.join(", "))
                .unwrap_or_default();

            if let Err(e) = self
                .docker
                .remove_container(
                    id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
            {
                tracing::warn!(container_id = %id, names = %names, "Failed to remove stale container: {e}");
            } else {
                info!(container_id = %id, names = %names, "Removed stale capability container");
            }
        }

        Ok(())
    }

    /// Ensure the isolated bridge networks exist (called once at startup)
    pub async fn ensure_networks(&self) -> Result<()> {
        self.ensure_network(TOOLS_NETWORK).await?;
        self.ensure_network(ENVS_NETWORK).await?;

        // When running inside Docker, join the Selu container to both capability
        // networks so we can reach capability containers by their internal IP.
        if let Some(ref self_id) = self.self_container_id {
            self.join_network_if_needed(self_id, TOOLS_NETWORK).await?;
            self.join_network_if_needed(self_id, ENVS_NETWORK).await?;
        }

        Ok(())
    }

    async fn ensure_network(&self, name: &str) -> Result<()> {
        // Check if the network already exists
        match self.docker.inspect_network::<&str>(name, None).await {
            Ok(_) => {
                debug!(network = name, "Docker network already exists");
                return Ok(());
            }
            Err(_) => {} // doesn't exist, create it
        }

        let mut options: HashMap<String, String> = HashMap::new();
        options.insert("com.docker.network.bridge.enable_ip_masquerade".into(), "false".into());

        self.docker
            .create_network(CreateNetworkOptions {
                name: name.to_string(),
                internal: false, // containers need to reach the host-bound egress proxy
                driver: "bridge".to_string(),
                options,
                ..Default::default()
            })
            .await
            .with_context(|| format!("Failed to create Docker network '{}'", name))?;

        info!(network = name, "Created Docker bridge network");
        Ok(())
    }

    /// Connect the Selu container to a capability bridge network (idempotent).
    async fn join_network_if_needed(&self, container_id: &str, network: &str) -> Result<()> {
        use bollard::network::ConnectNetworkOptions;
        use bollard::models::EndpointSettings;

        // Check if already connected by inspecting our container
        if let Ok(inspect) = self.docker.inspect_container(container_id, None).await {
            if let Some(nets) = inspect.network_settings.as_ref().and_then(|n| n.networks.as_ref()) {
                if nets.contains_key(network) {
                    debug!(network = network, "Selu container already connected to network");
                    return Ok(());
                }
            }
        }

        self.docker
            .connect_network(
                network,
                ConnectNetworkOptions {
                    container: container_id.to_string(),
                    endpoint_config: EndpointSettings::default(),
                },
            )
            .await
            .with_context(|| format!("Failed to connect Selu container to network '{}'", network))?;

        info!(network = network, "Connected Selu container to capability network");
        Ok(())
    }

    /// Start a capability container for the given manifest + session.
    /// Returns a `RunningCapability` once the container's gRPC port is ready.
    pub async fn start(
        &self,
        manifest: &CapabilityManifest,
        session_id: &str,
    ) -> Result<RunningCapability> {
        let container_name = format!(
            "selu-cap-{}-{}",
            manifest.id,
            Uuid::new_v4().as_simple()
        );

        // ── Choose network ────────────────────────────────────────────────────
        let network_name = match manifest.class {
            CapabilityClass::Environment => ENVS_NETWORK,
            CapabilityClass::Tool => TOOLS_NETWORK,
        };

        // ── Port bindings: bind gRPC port to a random host port ───────────────
        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        port_bindings.insert(
            format!("{}/tcp", CAPABILITY_GRPC_PORT),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".into()),
                host_port: Some("0".into()), // OS assigns a free port
            }]),
        );

        // ── Egress proxy address ───────────────────────────────────────────────
        // Generate a unique token for this container and embed it in the proxy URL
        // as basic-auth credentials. The egress proxy uses this to identify the
        // container and look up its allowlist policy.
        let egress_token = Uuid::new_v4().as_simple().to_string();
        let proxy_url = format!(
            "http://selu:{}@host.docker.internal:{}",
            egress_token, self.egress_proxy_port
        );

        // ── Environment variables ─────────────────────────────────────────────
        let env_vars = vec![
            format!("HTTP_PROXY={}", proxy_url),
            format!("HTTPS_PROXY={}", proxy_url),
            format!("http_proxy={}", proxy_url),
            format!("https_proxy={}", proxy_url),
            format!("SELU_SESSION_ID={}", session_id),
            format!("SELU_CAPABILITY_ID={}", manifest.id),
        ];

        // ── Exposed ports ─────────────────────────────────────────────────────
        let mut exposed_ports: HashMap<String, HashMap<(), ()>> = HashMap::new();
        exposed_ports.insert(format!("{}/tcp", CAPABILITY_GRPC_PORT), HashMap::new());

        // ── Resource limits ───────────────────────────────────────────────────
        let res = &manifest.resources;
        let memory_bytes = (res.max_memory_mb as i64) * 1024 * 1024;
        let nano_cpus = (res.max_cpu_fraction as f64 * 1e9) as i64;

        // ── Filesystem config ─────────────────────────────────────────────────
        let mut binds: Vec<String> = Vec::new();
        let tmpfs = {
            let mut m: HashMap<String, String> = HashMap::new();
            m.insert("/tmp".into(), "size=64m,noexec".into());
            m
        };

        if manifest.class == CapabilityClass::Environment {
            // Named Docker volume for the workspace
            let volume_name = format!("selu-workspace-{}", session_id);
            binds.push(format!("{}:/workspace:rw", volume_name));
        }

        let host_config = HostConfig {
            // Security hardening
            readonly_rootfs: Some(true),
            cap_drop: Some(vec!["ALL".into()]),
            security_opt: Some(vec!["no-new-privileges:true".into()]),
            // Networking
            network_mode: Some(network_name.into()),
            port_bindings: Some(port_bindings),
            // host.docker.internal → host IP (needed on Linux; Docker Desktop adds it automatically)
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".into()]),
            // Filesystem
            binds: if binds.is_empty() { None } else { Some(binds) },
            tmpfs: Some(tmpfs),
            // Resources
            memory: Some(memory_bytes),
            nano_cpus: Some(nano_cpus),
            pids_limit: Some(res.pids_limit as i64),
            ulimits: Some(vec![ResourcesUlimits {
                name: Some("nofile".into()),
                soft: Some(256),
                hard: Some(256),
            }]),
            // Auto-remove on exit
            auto_remove: Some(true),
            ..Default::default()
        };

        let config = Config {
            image: Some(manifest.image.clone()),
            env: Some(env_vars),
            exposed_ports: Some(exposed_ports),
            host_config: Some(host_config),
            user: Some("1000:1000".into()),
            ..Default::default()
        };

        // ── Create + start container ──────────────────────────────────────────
        let create_opts = CreateContainerOptions {
            name: container_name.clone(),
            platform: None,
        };

        let container = self.docker
            .create_container(Some(create_opts), config)
            .await
            .with_context(|| format!("Failed to create container for capability '{}'", manifest.id))?;

        let container_id = container.id.clone();

        self.docker
            .start_container(&container_id, None::<StartContainerOptions<&str>>)
            .await
            .with_context(|| format!("Failed to start container '{}'", container_id))?;

        info!(
            container_id = %container_id,
            capability = %manifest.id,
            "Started capability container"
        );

        // ── Inspect to get connection details ───────────────────────────────────
        let inspect = self.docker
            .inspect_container(&container_id, None)
            .await
            .with_context(|| format!("Failed to inspect container '{}'", container_id))?;

        let grpc_addr = if self.self_container_id.is_some() {
            // Running inside Docker — connect via the container's IP on the shared network
            let container_ip = extract_container_ip(&inspect, network_name)
                .ok_or_else(|| anyhow!(
                    "No container IP found for '{}' on network '{}'",
                    container_id, network_name
                ))?;
            format!("http://{}:{}", container_ip, CAPABILITY_GRPC_PORT)
        } else {
            // Running on the host — connect via the mapped host port on 127.0.0.1
            let host_port = extract_host_port(&inspect, CAPABILITY_GRPC_PORT)
                .ok_or_else(|| anyhow!("No host port assigned for container '{}'", container_id))?;
            format!("http://127.0.0.1:{}", host_port)
        };

        // ── Register egress policy (keyed by auth token) ──────────────────────
        let policy = ContainerEgressPolicy {
            capability_id: manifest.id.clone(),
            mode: manifest.network.mode.clone(),
            allowed_hosts: manifest.network.hosts.clone(),
        };
        crate::capabilities::egress_proxy::register(&self.egress_registry, egress_token.clone(), policy).await;

        // ── Wait for gRPC healthcheck ─────────────────────────────────────────
        wait_for_grpc_ready(&grpc_addr, 30).await
            .with_context(|| format!("Capability '{}' did not become ready in time", manifest.id))?;

        Ok(RunningCapability {
            container_id,
            capability_id: manifest.id.clone(),
            grpc_addr,
            egress_token,
        })
    }

    /// Stop and remove a running capability container
    pub async fn stop(&self, running: &RunningCapability) {
        // Deregister egress policy by token
        crate::capabilities::egress_proxy::deregister(&self.egress_registry, &running.egress_token).await;

        // Force-remove the container (auto_remove handles normal exit, but stop in case it's still running)
        if let Err(e) = self.docker
            .remove_container(
                &running.container_id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
        {
            // Not fatal — container may have already exited due to auto_remove
            debug!(
                container_id = %running.container_id,
                "Container removal: {e}"
            );
        } else {
            info!(container_id = %running.container_id, capability = %running.capability_id, "Stopped capability container");
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_host_port(inspect: &ContainerInspectResponse, container_port: u16) -> Option<u16> {
    let ports = inspect
        .network_settings
        .as_ref()?
        .ports
        .as_ref()?;

    let key = format!("{}/tcp", container_port);
    let bindings = ports.get(&key)?.as_ref()?;
    let binding = bindings.first()?;
    binding.host_port.as_ref()?.parse().ok()
}

/// Extract the container's IP address on a specific Docker network.
fn extract_container_ip(inspect: &ContainerInspectResponse, network_name: &str) -> Option<String> {
    let networks = inspect
        .network_settings
        .as_ref()?
        .networks
        .as_ref()?;

    let endpoint = networks.get(network_name)?;
    let ip = endpoint.ip_address.as_ref()?;
    if ip.is_empty() {
        None
    } else {
        Some(ip.clone())
    }
}

/// Read our own container ID when running inside Docker.
/// Parses /proc/self/mountinfo looking for the docker container ID in the path.
fn read_self_container_id() -> Option<String> {
    // Try /proc/self/mountinfo first — look for the container ID in the cgroup path
    if let Ok(contents) = std::fs::read_to_string("/proc/self/mountinfo") {
        for line in contents.lines() {
            // Look for Docker overlay paths like /var/lib/docker/containers/<id>/...
            if let Some(pos) = line.find("/docker/containers/") {
                let after = &line[pos + "/docker/containers/".len()..];
                if let Some(end) = after.find('/') {
                    let id = &after[..end];
                    if id.len() >= 12 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Some(id.to_string());
                    }
                }
            }
        }
    }

    // Fallback: read the hostname, which Docker sets to the short container ID
    if let Ok(hostname) = std::fs::read_to_string("/etc/hostname") {
        let hostname = hostname.trim();
        if hostname.len() == 12 && hostname.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hostname.to_string());
        }
    }

    None
}

/// Polls the gRPC healthcheck until the container is ready or timeout expires.
async fn wait_for_grpc_ready(grpc_addr: &str, timeout_secs: u64) -> Result<()> {
    use tonic::transport::Channel;
    use crate::capabilities::grpc::selu_capability::capability_client::CapabilityClient;
    use crate::capabilities::grpc::selu_capability::HealthRequest;

    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(anyhow!("gRPC healthcheck timed out after {}s", timeout_secs));
        }

        match Channel::from_shared(grpc_addr.to_string())
            .context("Invalid gRPC address")?
            .connect()
            .await
        {
            Ok(channel) => {
                let mut client = CapabilityClient::new(channel);
                match client.healthcheck(HealthRequest {}).await {
                    Ok(resp) => {
                        if resp.into_inner().ready {
                            debug!(addr = grpc_addr, "Capability gRPC endpoint is ready");
                            return Ok(());
                        }
                    }
                    Err(_) => {}
                }
            }
            Err(_) => {}
        }

        sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{EndpointSettings, NetworkSettings};

    fn make_inspect(networks: HashMap<String, EndpointSettings>, port_map: HashMap<String, Option<Vec<PortBinding>>>) -> ContainerInspectResponse {
        ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ports: Some(port_map),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn test_extract_host_port() {
        let mut ports = HashMap::new();
        ports.insert(
            "50051/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".into()),
                host_port: Some("54321".into()),
            }]),
        );
        let inspect = make_inspect(HashMap::new(), ports);
        assert_eq!(extract_host_port(&inspect, 50051), Some(54321));
    }

    #[test]
    fn test_extract_container_ip() {
        let mut networks = HashMap::new();
        networks.insert(
            TOOLS_NETWORK.to_string(),
            EndpointSettings {
                ip_address: Some("172.18.0.3".into()),
                ..Default::default()
            },
        );
        let inspect = make_inspect(networks, HashMap::new());
        assert_eq!(
            extract_container_ip(&inspect, TOOLS_NETWORK),
            Some("172.18.0.3".to_string())
        );
    }

    #[test]
    fn test_extract_container_ip_missing_network() {
        let inspect = make_inspect(HashMap::new(), HashMap::new());
        assert_eq!(extract_container_ip(&inspect, TOOLS_NETWORK), None);
    }

    #[test]
    fn test_extract_container_ip_empty_ip() {
        let mut networks = HashMap::new();
        networks.insert(
            TOOLS_NETWORK.to_string(),
            EndpointSettings {
                ip_address: Some("".into()),
                ..Default::default()
            },
        );
        let inspect = make_inspect(networks, HashMap::new());
        assert_eq!(extract_container_ip(&inspect, TOOLS_NETWORK), None);
    }
}
