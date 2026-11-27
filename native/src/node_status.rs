//! Bounded daemon sampling and asynchronous status for the selected tree node.

use crate::cluster_api::{resolve_call, ResolvedCall};
use remuda_core::protocol::{NodeMetrics, Request, ResourceUsage, Response};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use sysinfo::{Disks, System};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(15);

type Sample = Option<(Instant, NodeMetrics)>;

pub(crate) fn metrics(
    host_storage: impl Fn() -> (Option<bool>, Option<bool>) + Send + 'static,
) -> Option<NodeMetrics> {
    // Requests only read the cache. Even a stalled disk query cannot block
    // daemon requests or start another sampler.
    static CACHE: OnceLock<Arc<Mutex<Sample>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let cache = Arc::new(Mutex::new(None));
        let worker = Arc::clone(&cache);
        let _ = std::thread::Builder::new()
            .name("node-metrics".into())
            .spawn(move || sample_loop(&worker, host_storage));
        cache
    });
    let guard = cache.lock().ok()?;
    let (at, sample) = guard.as_ref()?;
    let mut sample = sample.clone();
    sample.age_ms = at.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    Some(sample)
}

fn sample_loop(cache: &Mutex<Sample>, host_storage: impl Fn() -> (Option<bool>, Option<bool>)) {
    let mut system = System::new();
    let mut disks = Disks::new();
    let mount = system_mount();
    let cli_path = std::env::current_exe().ok().map(|path| {
        // Linux can append this suffix when the installed file was replaced
        // before the first status request.
        #[cfg(target_os = "linux")]
        if let Some(original) = path
            .to_str()
            .and_then(|path| path.strip_suffix(" (deleted)"))
        {
            return PathBuf::from(original);
        }
        path
    });
    let mut cli_build = None;
    system.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    loop {
        let at = Instant::now();
        system.refresh_cpu_usage();
        system.refresh_memory();
        disks.refresh_list();
        let cli_version = cli_path.as_deref().and_then(|path| {
            let metadata = path.metadata().ok()?;
            let key = (metadata.modified().ok()?, metadata.len());
            if cli_build
                .as_ref()
                .is_none_or(|(last_key, version): &(_, Option<String>)| {
                    *last_key != key || version.is_none()
                })
            {
                cli_build = Some((key, installed_cli_version(path)));
            }
            cli_build.as_ref().and_then(|(_, version)| version.clone())
        });
        let (host_inventory_in_temp, host_sockets_in_temp) = host_storage();
        let sample = NodeMetrics {
            age_ms: 0,
            daemon_version: Some(crate::dist::BUILD_VERSION.into()),
            cli_version,
            host_inventory_in_temp,
            host_sockets_in_temp,
            cpu_percent: (!system.cpus().is_empty())
                .then(|| system.global_cpu_usage())
                .filter(|usage| (0.0..=100.0).contains(usage))
                .map(|usage| usage.round() as u8),
            memory: system
                .total_memory()
                .checked_sub(system.used_memory())
                .and_then(|available| resource_usage(system.total_memory(), available)),
            system_disk: mount.as_ref().and_then(|mount| {
                disks
                    .iter()
                    .find(|disk| disk.mount_point() == mount)
                    .and_then(|disk| resource_usage(disk.total_space(), disk.available_space()))
            }),
        };
        if let Ok(mut guard) = cache.lock() {
            *guard = Some((at, sample));
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

// Probe the executable at the daemon's launch path, so an atomically replaced
// CLI can report its own build while the old daemon is still running.
fn installed_cli_version(path: &Path) -> Option<String> {
    let output = crate::process::run_sync(
        vec![path.to_str()?.to_owned(), "--version".into()],
        None,
        1.0,
        None,
        None,
    )
    .ok()?;
    if output.code != 0 || output.timed_out {
        return None;
    }
    let version = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .strip_prefix("remuda ")?;
    displayable_version(version).map(str::to_owned)
}

fn displayable_version(version: &str) -> Option<&str> {
    (!version.is_empty() && version.len() <= 200 && version.bytes().all(|b| b.is_ascii_graphic()))
        .then_some(version)
}

fn system_mount() -> Option<PathBuf> {
    #[cfg(unix)]
    return Some(PathBuf::from("/"));
    #[cfg(windows)]
    return std::env::var_os("SystemDrive").map(|mut drive| {
        drive.push("\\");
        PathBuf::from(drive)
    });
    #[cfg(not(any(unix, windows)))]
    return None;
}

fn resource_usage(total_bytes: u64, available_bytes: u64) -> Option<ResourceUsage> {
    if total_bytes == 0 {
        return None;
    }
    Some(ResourceUsage {
        used_bytes: total_bytes.checked_sub(available_bytes)?,
        total_bytes,
    })
}

#[derive(Clone, Copy)]
pub struct Metric {
    pub label: &'static str,
    pub percent: Option<u8>,
    pub capacity: Option<ResourceUsage>,
}

impl Metric {
    fn for_capacity(label: &'static str, capacity: Option<ResourceUsage>) -> Self {
        let capacity =
            capacity.filter(|usage| usage.total_bytes > 0 && usage.used_bytes <= usage.total_bytes);
        let percent = capacity.map(|usage| {
            let used = u128::from(usage.used_bytes);
            let total = u128::from(usage.total_bytes);
            ((used * 100 + total / 2) / total) as u8
        });
        Self {
            label,
            percent,
            capacity,
        }
    }
}

pub struct Details {
    pub health: &'static str,
    pub age: Option<Duration>,
    pub stale: bool,
    pub metrics: [Metric; 3],
    pub daemon_version: Option<String>,
    pub cli_version: Option<String>,
    pub host_inventory_in_temp: Option<bool>,
    pub host_sockets_in_temp: Option<bool>,
}

impl Details {
    pub fn storage_lines(&self) -> [String; 2] {
        let suffix = if self.stale { " (stale)" } else { "" };
        [
            ("Host inventory", self.host_inventory_in_temp),
            ("Host sockets", self.host_sockets_in_temp),
        ]
        .map(|(label, location)| {
            let location = match location {
                Some(true) => "Temporary directory",
                Some(false) => "Outside temporary directory",
                None => return format!("{label}  unavailable"),
            };
            format!("{label}  {location}{suffix}")
        })
    }

    pub fn version_lines(&self) -> [String; 2] {
        let suffix = if self.stale { " (stale)" } else { "" };
        [
            ("Daemon (running)", self.daemon_version.as_deref()),
            ("CLI (installed)", self.cli_version.as_deref()),
        ]
        .map(
            |(label, version)| match version.and_then(displayable_version) {
                Some(version) => format!("{label}  {version}{suffix}"),
                None => format!("{label}  unavailable"),
            },
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Local(PathBuf),
    Peer(String),
}

impl Node {
    pub fn local(path: &Path) -> Self {
        Self::Local(path.to_path_buf())
    }

    pub fn request(self) -> io::Result<Option<NodeMetrics>> {
        let response = match self {
            Self::Local(path) => crate::client::request_with_timeout(
                &path,
                &Request::NodeStatus,
                Duration::from_secs(2),
            ),
            Self::Peer(peer) => request_peer(&peer, &Request::NodeStatus, Duration::from_secs(2)),
        }?;
        match response {
            Response::NodeStatus(sample) => Ok(sample),
            _ => Err(io::Error::other("node status unavailable")),
        }
    }

    pub fn force_close(self, name: String, instance_id: String) -> io::Result<Response> {
        let request = Request::ForceClose {
            name,
            instance_id,
            confirm: true,
        };
        let timeout = Duration::from_secs(8);
        match self {
            Self::Local(path) => crate::client::request_with_timeout(&path, &request, timeout),
            Self::Peer(peer) => request_peer(&peer, &request, timeout),
        }
    }
}

fn request_peer(peer: &str, request: &Request, timeout: Duration) -> io::Result<Response> {
    let resolve = || match resolve_call(peer, None)? {
        ResolvedCall::Remote {
            fingerprint,
            address,
            pinned_static_key,
        } if fingerprint == peer => Ok((address, pinned_static_key)),
        _ => Err(io::Error::other("node identity changed")),
    };
    let (address, pinned_key) = resolve()?;
    let private_key = crate::cluster::identity::load_static_private_key()?;
    let response = crate::net::cluster_client::ClusterClient::system()
        .with_request_timeout(timeout)
        .request(address, &pinned_key, &private_key, request)
        .map_err(io::Error::other)?;
    if resolve()? != (address, pinned_key) {
        return Err(io::Error::other("node identity changed"));
    }
    Ok(response)
}

type Reply = (u64, Instant, io::Result<Option<NodeMetrics>>);

/// A single outstanding request, with generation checks when selection changes.
#[derive(Default)]
pub struct Status {
    node: Option<Node>,
    generation: u64,
    pending: Option<(Instant, mpsc::Receiver<Reply>)>,
    next: Option<Instant>,
    last: Option<(Instant, Option<NodeMetrics>)>,
    failed: bool,
}

impl Status {
    pub fn update(&mut self, node: Option<Node>) {
        self.update_with(node, Node::request);
    }

    pub fn update_with(
        &mut self,
        node: Option<Node>,
        fetch: impl FnOnce(Node) -> io::Result<Option<NodeMetrics>> + Send + 'static,
    ) {
        let now = Instant::now();
        if self.node != node {
            self.node = node;
            self.generation = self.generation.wrapping_add(1);
            self.next = None;
            self.last = None;
            self.failed = false;
        }
        if let Some((started, receiver)) = &self.pending {
            match receiver.try_recv() {
                Ok((generation, at, result)) => {
                    if generation == self.generation {
                        self.failed = result.is_err();
                        if let Ok(sample) = result {
                            self.last = Some((at, sample));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if now.saturating_duration_since(*started) >= STALE_AFTER {
                        self.failed = true;
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.failed = true,
            }
            self.pending = None;
        }
        if self.next.is_some_and(|due| now < due) {
            return;
        }
        let Some(node) = self.node.clone() else {
            return;
        };
        let generation = self.generation;
        let (sender, receiver) = mpsc::channel();
        self.next = Some(now + Duration::from_secs(2));
        match std::thread::Builder::new()
            .name("node-status".into())
            .spawn(move || {
                let started = Instant::now();
                let _ = sender.send((generation, started, fetch(node)));
            }) {
            Ok(_) => self.pending = Some((now, receiver)),
            Err(_) => self.failed = true,
        }
    }

    pub fn details_for(&self, node: &Node) -> Details {
        if self.node.as_ref() == Some(node) {
            self.details()
        } else {
            Self::default().details()
        }
    }

    pub fn unavailable() -> Details {
        Self {
            failed: true,
            ..Self::default()
        }
        .details()
    }

    fn details(&self) -> Details {
        let last = self.last.as_ref();
        let elapsed = last
            .map(|(requested, _)| requested.elapsed())
            .unwrap_or_default();
        let sample = last.and_then(|(_, sample)| sample.as_ref());
        let age = sample.map(|sample| Duration::from_millis(sample.age_ms).saturating_add(elapsed));
        let expired = age.is_some_and(|age| age >= STALE_AFTER);
        let stale = expired || self.failed || elapsed >= STALE_AFTER;
        let health = match last {
            None if self.failed => "Status unavailable",
            None => "Checking daemon…",
            Some(_) if self.failed || elapsed >= STALE_AFTER => "Daemon status stale",
            Some(_) if expired => "Daemon reachable · metrics stale",
            Some(_) if sample.is_none() => "Daemon reachable · sampling metrics…",
            Some(_) => "Daemon reachable",
        };
        Details {
            health,
            age,
            stale,
            daemon_version: sample.and_then(|sample| sample.daemon_version.clone()),
            cli_version: sample.and_then(|sample| sample.cli_version.clone()),
            host_inventory_in_temp: sample.and_then(|sample| sample.host_inventory_in_temp),
            host_sockets_in_temp: sample.and_then(|sample| sample.host_sockets_in_temp),
            metrics: [
                Metric {
                    label: "CPU",
                    percent: sample
                        .and_then(|s| s.cpu_percent)
                        .filter(|percent| *percent <= 100),
                    capacity: None,
                },
                Metric::for_capacity("Memory", sample.and_then(|s| s.memory)),
                Metric::for_capacity("System disk", sample.and_then(|s| s.system_disk)),
            ],
        }
    }

    pub fn lines(&self) -> [String; 8] {
        let details = self.details();
        let health = details.age.map_or_else(
            || details.health.into(),
            |age| format!("{} · updated {}s ago", details.health, age.as_secs()),
        );
        let suffix = if details.stale { " (stale)" } else { "" };
        let [daemon, cli] = details.version_lines();
        let [cpu, memory, disk] = details.metrics.map(|metric| match metric.percent {
            Some(percent) => format!("{}  {percent}%{suffix}", metric.label),
            None => format!("{}  unavailable", metric.label),
        });
        let [inventory, sockets] = details.storage_lines();
        [health, daemon, cli, cpu, memory, disk, inventory, sockets]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_reply_cannot_populate_another_node_and_old_samples_are_stale() {
        let now = Instant::now();
        let a = Node::Peer("a".into());
        let b = Node::Peer("b".into());
        let sample = NodeMetrics {
            age_ms: 20_000,
            host_inventory_in_temp: Some(false),
            host_sockets_in_temp: Some(true),
            daemon_version: Some("daemon-build".into()),
            cli_version: Some("cli-build".into()),
            cpu_percent: Some(18),
            memory: Some(ResourceUsage {
                used_bytes: 50,
                total_bytes: 100,
            }),
            system_disk: None,
        };
        let (send, receive) = mpsc::channel();
        send.send((0, now, Ok(Some(sample.clone())))).unwrap();
        let mut status = Status {
            node: Some(a),
            pending: Some((now, receive)),
            ..Status::default()
        };
        status.update_with(Some(b.clone()), |_| Err(io::Error::other("offline")));
        assert!(status.details_for(&b).age.is_none());
        assert_eq!(status.details_for(&b).metrics[0].percent, None);

        status.last = Some((now, Some(sample)));
        let details = status.details_for(&b);
        assert!(details.stale);
        assert_eq!(
            details.version_lines(),
            [
                "Daemon (running)  daemon-build (stale)",
                "CLI (installed)  cli-build (stale)",
            ]
        );
        assert_eq!(
            details.storage_lines(),
            [
                "Host inventory  Outside temporary directory (stale)",
                "Host sockets  Temporary directory (stale)",
            ]
        );
        assert_eq!(details.metrics[0].percent, Some(18));
        assert_eq!(details.metrics[1].percent, Some(50));
        assert_eq!(details.metrics[2].percent, None);
    }
}
