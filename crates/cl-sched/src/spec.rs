//! Turning a stack service into the container spec docker is given.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use cl_config::{Command, RestartPolicy as StackRestart, Service};
use cl_docker::{
    BindMount, ContainerSpec, Healthcheck, PortMapping, PortProtocol, ResourceLimits, RestartPolicy,
};
use cl_raft::ids::InstanceId;

use crate::materialize::Materialized;
use crate::reconcile::LABEL_REVISION;

/// Build the spec for one instance.
///
/// `objects` are the configs and secrets already written to disk, `network` is
/// the overlay network to attach to, and `digest` pins the image when one has
/// been resolved, so every replica of a `:latest` service runs the same bits.
/// Where a container sits on the overlay: its own address, the resolver that
/// answers `service.stack` for it, and the zone that resolver is authoritative
/// for. Empty for a container on no stack network, which keeps docker's own
/// defaults.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    /// Docker network the container attaches to.
    pub network: Option<String>,
    pub ip: Option<std::net::Ipv4Addr>,
    pub resolver: Option<std::net::Ipv4Addr>,
    pub zone: Option<String>,
    /// Docker network for a second, egress-kind attachment, if the service
    /// names one of those in its `networks:` list.
    pub egress_network: Option<String>,
}

pub fn build(
    instance: &InstanceId,
    service: &Service,
    objects: &[Materialized],
    drive_roots: &BTreeMap<String, PathBuf>,
    placement: Placement,
    digest: Option<&str>,
    revision: u64,
) -> ContainerSpec {
    let mut labels: HashMap<String, String> = service
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    labels.insert(LABEL_REVISION.to_string(), revision.to_string());

    let mut mounts: Vec<BindMount> = service
        .volumes
        .iter()
        .filter_map(|v| {
            // A volume binds to the node's local directory for its drive.
            // Anonymous mounts name no drive, so there is nothing to bind.
            let (drive, volume) = (v.drive.as_ref()?, v.volume.as_ref()?);
            let root = drive_roots.get(drive)?;
            Some(BindMount {
                host_path: root.join(volume).display().to_string(),
                container_path: v.target.display().to_string(),
                read_only: v.read_only,
            })
        })
        .collect();
    mounts.extend(objects.iter().map(|o| o.mount()));

    ContainerSpec {
        name: instance.container_name(),
        // A pinned digest wins, so a restart cannot silently pull something new.
        image: match digest {
            Some(digest) => pin(&service.image, digest),
            None => service.image.clone(),
        },
        stack: instance.stack.clone(),
        service: instance.service.clone(),
        instance: instance.to_string(),
        env: service
            .environment
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        command: service.command.as_ref().map(command_words),
        labels,
        mounts,
        ports: service.ports.iter().flat_map(port_mappings).collect(),
        network: placement.network,
        ip: placement.ip,
        dns: placement.resolver.into_iter().map(std::net::IpAddr::V4).collect(),
        dns_search: placement.zone.iter().cloned().collect(),
        // `service.stack` has one dot, and docker's own resolver defaults the
        // container to ndots:0, which makes a name with any dot in it absolute
        // and skips the search list entirely: the short name then comes back
        // NXDOMAIN from upstream while the fully qualified one resolves. Two
        // is the smallest value that sends `service.stack` through the search
        // list, and leaves ordinary two dot internet names resolving directly.
        dns_options: placement
            .zone
            .map(|_| vec!["ndots:2".to_string()])
            .unwrap_or_default(),
        restart_policy: restart_policy(service),
        resources: resources(service),
        healthcheck: healthcheck(service),
        stop_signal: service.stop_signal.clone(),
        egress_network: placement.egress_network,
    }
}

/// Replace the tag with a digest, keeping the repository.
fn pin(image: &str, digest: &str) -> String {
    let repository = image.split('@').next().unwrap_or(image);
    let repository = match repository.rsplit_once(':') {
        // A colon in the host part of a registry is a port, not a tag.
        Some((base, tag)) if !tag.contains('/') => base,
        _ => repository,
    };
    format!("{repository}@{digest}")
}

fn command_words(command: &Command) -> Vec<String> {
    match command {
        Command::Shell(line) => shell_words(line),
        Command::Exec(args) => args.clone(),
    }
}

/// Good enough splitting for a compose command line: quoted runs stay together.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'') | (None, '"') => quote = Some(c),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn port_mappings(port: &cl_config::PortMapping) -> Vec<PortMapping> {
    let protocol = match port.protocol {
        cl_config::Protocol::Tcp => PortProtocol::Tcp,
        cl_config::Protocol::Udp => PortProtocol::Udp,
    };

    // A range maps position by position onto the published range, so
    // "8080-8090:80-90" pairs 8080 with 80 and so on.
    (port.target.start..=port.target.end)
        .enumerate()
        .map(|(offset, container_port)| PortMapping {
            container_port,
            protocol,
            host_port: port
                .published
                .as_ref()
                .map(|p| p.start.saturating_add(offset as u16)),
            host_ip: port.host_ip.map(|ip| ip.to_string()),
        })
        .collect()
}

fn restart_policy(service: &Service) -> RestartPolicy {
    match service.restart {
        Some(StackRestart::No) | None => RestartPolicy::No,
        Some(StackRestart::Always) => RestartPolicy::Always,
        Some(StackRestart::UnlessStopped) => RestartPolicy::UnlessStopped,
        Some(StackRestart::OnFailure { max_attempts }) => RestartPolicy::OnFailure {
            max_retries: max_attempts.unwrap_or(0) as i64,
        },
    }
}

fn resources(service: &Service) -> ResourceLimits {
    let limits = service
        .deploy
        .as_ref()
        .and_then(|d| d.resources.as_ref())
        .and_then(|r| r.limits.as_ref());

    ResourceLimits {
        memory_bytes: limits.and_then(|l| l.memory.as_ref()).and_then(|m| parse_bytes(m)),
        nano_cpus: limits
            .and_then(|l| l.cpus.as_ref())
            .and_then(|c| c.parse::<f64>().ok())
            .map(|c| (c * 1e9) as i64),
    }
}

/// Compose sizes: `512M`, `2g`, `1024`. Bytes when there is no suffix.
fn parse_bytes(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (digits, multiplier) = match raw.chars().last()? {
        'b' | 'B' => (&raw[..raw.len() - 1], 1i64),
        'k' | 'K' => (&raw[..raw.len() - 1], 1024),
        'm' | 'M' => (&raw[..raw.len() - 1], 1024 * 1024),
        'g' | 'G' => (&raw[..raw.len() - 1], 1024 * 1024 * 1024),
        _ => (raw, 1),
    };
    digits.trim().parse::<i64>().ok().map(|n| n * multiplier)
}

fn healthcheck(service: &Service) -> Option<Healthcheck> {
    let check = service.healthcheck.as_ref()?;
    if check.disable {
        return None;
    }
    Some(Healthcheck {
        test: check.test.as_ref().map(test_words)?,
        interval: check.interval,
        timeout: check.timeout,
        retries: check.retries.map(|r| r as i64),
        start_period: check.start_period,
    })
}

fn test_words(test: &cl_config::HealthcheckTest) -> Vec<String> {
    match test {
        cl_config::HealthcheckTest::Shell(line) => {
            vec!["CMD-SHELL".to_string(), line.clone()]
        }
        cl_config::HealthcheckTest::Exec(args) => args.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cl_config::StackFile;

    fn service(yaml: &str) -> Service {
        let file: StackFile = StackFile::parse(yaml).expect("valid stack");
        file.services.values().next().expect("one service").clone()
    }

    fn instance() -> InstanceId {
        InstanceId::new("site", "web", 0)
    }

    fn roots() -> BTreeMap<String, PathBuf> {
        BTreeMap::from([
            ("fast".to_string(), PathBuf::from("/srv/fast")),
            ("bulk".to_string(), PathBuf::from("/mnt/bulk")),
        ])
    }

    #[test]
    fn a_minimal_service_builds() {
        let s = service("services:\n  web:\n    image: nginx\n");
        let spec = build(&instance(), &s, &[], &roots(), Placement::default(), None, 1);

        assert_eq!(spec.name, "site_web_0");
        assert_eq!(spec.image, "nginx");
        assert_eq!(spec.stack, "site");
        assert_eq!(spec.instance, "site.web.0");
        assert_eq!(spec.labels[LABEL_REVISION], "1");
    }

    /// The scheduler hands the container its overlay address and the resolver
    /// that answers for the cluster zone; both have to reach the spec or a
    /// container cannot find the service next to it.
    #[test]
    fn a_placement_reaches_the_container_spec() {
        let s = service("services:\n  web:\n    image: nginx\n");
        let placement = Placement {
            network: Some("site_default".into()),
            ip: Some(std::net::Ipv4Addr::new(10, 244, 0, 7)),
            resolver: Some(std::net::Ipv4Addr::new(10, 244, 0, 1)),
            zone: Some("int.example.com".into()),
            egress_network: None,
        };
        let spec = build(&instance(), &s, &[], &roots(), placement, None, 1);

        assert_eq!(spec.network.as_deref(), Some("site_default"));
        assert_eq!(spec.ip, Some(std::net::Ipv4Addr::new(10, 244, 0, 7)));
        assert_eq!(spec.dns, vec![std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 244, 0, 1))]);
        assert_eq!(spec.dns_search, vec!["int.example.com".to_string()]);
        assert_eq!(spec.dns_options, vec!["ndots:2".to_string()]);
    }

    #[test]
    fn a_digest_pins_the_image() {
        let s = service("services:\n  web:\n    image: nginx:latest\n");
        let spec = build(&instance(), &s, &[], &roots(), Placement::default(), Some("sha256:abc"), 1);
        assert_eq!(spec.image, "nginx@sha256:abc");
    }

    #[test]
    fn pinning_keeps_a_registry_port() {
        // The colon in registry:5000 is a port, not a tag.
        assert_eq!(
            pin("registry:5000/app:v1", "sha256:abc"),
            "registry:5000/app@sha256:abc"
        );
        assert_eq!(pin("nginx", "sha256:abc"), "nginx@sha256:abc");
    }

    #[test]
    fn objects_become_read_only_mounts() {
        let s = service("services:\n  web:\n    image: nginx\n");
        let objects = vec![Materialized {
            host_path: "/var/lib/clouduler/objects/site_web_0/conf".into(),
            container_path: "/etc/nginx/nginx.conf".into(),
            mode: 0o444,
            secret: false,
            revision: 1,
        }];

        let spec = build(&instance(), &s, &objects, &roots(), Placement::default(), None, 1);
        assert_eq!(spec.mounts.len(), 1);
        assert!(spec.mounts[0].read_only);
        assert_eq!(spec.mounts[0].container_path, "/etc/nginx/nginx.conf");
    }

    #[test]
    fn drive_volumes_become_mounts() {
        let s = service(
            "services:\n  web:\n    image: nginx\n    volumes:\n      - \"fast/data:/data\"\n      - \"bulk/backup:/backup:ro\"\n",
        );
        let spec = build(&instance(), &s, &[], &roots(), Placement::default(), None, 1);

        assert_eq!(spec.mounts.len(), 2);
        assert_eq!(spec.mounts[0].host_path, "/srv/fast/data");
        assert_eq!(spec.mounts[0].container_path, "/data");
        assert!(!spec.mounts[0].read_only);
        assert_eq!(spec.mounts[1].host_path, "/mnt/bulk/backup");
        assert!(spec.mounts[1].read_only);
    }

    #[test]
    fn environment_and_labels_carry_over() {
        let s = service(
            "services:\n  web:\n    image: nginx\n    environment:\n      TZ: UTC\n    labels:\n      team: infra\n",
        );
        let spec = build(&instance(), &s, &[], &roots(), Placement::default(), None, 3);

        assert!(spec.env.contains(&("TZ".to_string(), "UTC".to_string())));
        assert_eq!(spec.labels["team"], "infra");
        assert_eq!(spec.labels[LABEL_REVISION], "3");
    }

    #[test]
    fn a_shell_command_is_split_respecting_quotes() {
        assert_eq!(shell_words("sh -c 'echo hello world'"), vec!["sh", "-c", "echo hello world"]);
        assert_eq!(shell_words("nginx -g \"daemon off;\""), vec!["nginx", "-g", "daemon off;"]);
        assert!(shell_words("   ").is_empty());
    }

    #[test]
    fn resource_limits_convert() {
        let s = service(
            "services:\n  web:\n    image: nginx\n    deploy:\n      resources:\n        limits:\n          cpus: \"1.5\"\n          memory: 512M\n",
        );
        let spec = build(&instance(), &s, &[], &roots(), Placement::default(), None, 1);

        assert_eq!(spec.resources.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(spec.resources.nano_cpus, Some(1_500_000_000));
    }

    #[test]
    fn byte_sizes_parse_with_and_without_suffixes() {
        assert_eq!(parse_bytes("1024"), Some(1024));
        assert_eq!(parse_bytes("2g"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_bytes("512M"), Some(512 * 1024 * 1024));
        assert_eq!(parse_bytes("nonsense"), None);
    }

    #[test]
    fn restart_policies_map_across() {
        let s = service("services:\n  web:\n    image: nginx\n    restart: always\n");
        assert_eq!(
            build(&instance(), &s, &[], &roots(), Placement::default(), None, 1).restart_policy,
            RestartPolicy::Always
        );

        let s = service("services:\n  web:\n    image: nginx\n");
        assert_eq!(
            build(&instance(), &s, &[], &roots(), Placement::default(), None, 1).restart_policy,
            RestartPolicy::No
        );
    }

    #[test]
    fn a_disabled_healthcheck_produces_none() {
        let s = service(
            "services:\n  web:\n    image: nginx\n    healthcheck:\n      disable: true\n",
        );
        assert!(build(&instance(), &s, &[], &roots(), Placement::default(), None, 1).healthcheck.is_none());
    }

    #[test]
    fn a_shell_healthcheck_becomes_cmd_shell() {
        let s = service(
            "services:\n  web:\n    image: nginx\n    healthcheck:\n      test: curl -f localhost\n      retries: 3\n",
        );
        let check = build(&instance(), &s, &[], &roots(), Placement::default(), None, 1)
            .healthcheck
            .expect("healthcheck");
        assert_eq!(check.test[0], "CMD-SHELL");
        assert_eq!(check.retries, Some(3));
    }
}
