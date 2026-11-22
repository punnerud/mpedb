//! Process config. The file describes the listener and the slot map. The
//! per-shard `.mpedb` geometry is derived from it and then frozen in `META`,
//! so a second start with a different shard count is a refusal rather than
//! a silent rehash.

use crate::cluster::{check_announce, Cluster, MapError, Node, SlotRange};
use crate::store::StoreOptions;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    pub announce: String,
    pub store: StoreOptions,
    pub cluster: Cluster,
    pub max_connections: usize,
    /// Event-loop threads. One by default: a second one makes pipelined
    /// reads faster and durable writes slower, because two loops commit
    /// out of phase and each flush carries half as many writes. Measured
    /// 2026-11-21 on 2 vCPUs shared with the client, eight connections,
    /// one thread against two: `GET` pipelined 16 deep 175 737 against
    /// 249 961 /s, `SET` under `wal` 1 175 against 700 /s, `SET` under
    /// `async` 22 787 against 20 183 /s.
    pub threads: usize,
    pub cleanup_interval: Duration,
    pub cleanup_chunk: usize,
    /// How often held `SET`s are written to the files. Zero, the default,
    /// holds nothing: a `SET` is answered after its commit. See
    /// `StoreOptions::write_behind`.
    pub write_behind: Duration,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    listen: String,
    data_dir: String,
    #[serde(default = "default_shards")]
    shards: usize,
    #[serde(default = "default_durability")]
    durability: String,
    #[serde(default = "default_size_mb")]
    size_mb: u64,
    #[serde(default = "default_zstd")]
    zstd_level: i32,
    #[serde(default = "default_max_value_mb")]
    max_value_mb: u64,
    #[serde(default = "default_cache_mb")]
    cache_mb: u64,
    #[serde(default)]
    write_behind_ms: u64,
    #[serde(default = "default_max_connections")]
    max_connections: usize,
    #[serde(default = "default_threads")]
    threads: usize,
    #[serde(default = "default_cleanup_ms")]
    cleanup_interval_ms: u64,
    #[serde(default = "default_chunk")]
    cleanup_chunk: usize,
    #[serde(default)]
    cluster: Option<FileCluster>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileCluster {
    announce: String,
    #[serde(default, rename = "node")]
    nodes: Vec<FileNode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNode {
    announce: String,
    ranges: Vec<String>,
}

fn default_shards() -> usize {
    1
}
fn default_durability() -> String {
    // Acknowledges before the next fsync. A power loss can drop a recent
    // window of commits and cannot tear a page. That is the durability the
    // UI-blob workload this server is shaped for asked for. `commit` and
    // `wal` are the modes that wait, and the modes where group commit runs.
    "async".into()
}
fn default_size_mb() -> u64 {
    64
}
fn default_zstd() -> i32 {
    3
}
fn default_max_value_mb() -> u64 {
    16
}
fn default_cache_mb() -> u64 {
    64
}
fn default_threads() -> usize {
    1
}
fn default_max_connections() -> usize {
    256
}
fn default_cleanup_ms() -> u64 {
    1000
}
fn default_chunk() -> usize {
    200
}

impl ServeConfig {
    pub fn from_toml(text: &str) -> Result<Self, String> {
        let file: FileConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        let listen = resolve_listen(&file.listen)?;
        let announce = file
            .cluster
            .as_ref()
            .map(|c| c.announce.clone())
            .unwrap_or_else(|| listen.to_string());
        let cluster = build_cluster(&announce, file.cluster.as_ref())?;
        let max_value_bytes = file
            .max_value_mb
            .checked_mul(1024 * 1024)
            .ok_or("max_value_mb is too large")? as usize;
        if file.cleanup_chunk == 0 {
            return Err("cleanup_chunk must be at least 1".into());
        }
        if file.threads == 0 {
            return Err("threads must be at least 1".into());
        }
        if file.max_connections == 0 {
            return Err("max_connections must be at least 1".into());
        }
        Ok(ServeConfig {
            listen,
            announce,
            store: StoreOptions {
                dir: PathBuf::from(file.data_dir),
                shards: file.shards,
                durability: file.durability,
                size_mb: file.size_mb,
                zstd_level: file.zstd_level,
                max_value_bytes,
                cache_bytes: file
                    .cache_mb
                    .checked_mul(1024 * 1024)
                    .ok_or("cache_mb is too large")? as usize,
                write_behind: file.write_behind_ms > 0,
            },
            cluster,
            max_connections: file.max_connections,
            threads: file.threads,
            cleanup_interval: Duration::from_millis(file.cleanup_interval_ms),
            cleanup_chunk: file.cleanup_chunk,
            write_behind: Duration::from_millis(file.write_behind_ms),
        })
    }

    /// One node, every slot, no config file. `durability` defaults to `async`.
    pub fn single(listen: SocketAddr, dir: PathBuf, shards: usize, durability: String) -> Self {
        let announce = listen.to_string();
        ServeConfig {
            listen,
            announce: announce.clone(),
            store: StoreOptions {
                dir,
                shards,
                durability,
                size_mb: default_size_mb(),
                zstd_level: default_zstd(),
                max_value_bytes: default_max_value_mb() as usize * 1024 * 1024,
                cache_bytes: default_cache_mb() as usize * 1024 * 1024,
                write_behind: false,
            },
            cluster: Cluster::single(announce),
            max_connections: default_max_connections(),
            threads: default_threads(),
            cleanup_interval: Duration::from_millis(default_cleanup_ms()),
            cleanup_chunk: default_chunk(),
            write_behind: Duration::ZERO,
        }
    }
}

fn build_cluster(announce: &str, cluster: Option<&FileCluster>) -> Result<Cluster, String> {
    let Some(cluster) = cluster else {
        check_announce(announce).map_err(|e: MapError| e.0)?;
        return Ok(Cluster::single(announce));
    };
    if cluster.announce != announce {
        // `announce` was copied from the section. This is a guard against a
        // future caller passing a different string.
        return Err(format!(
            "cluster.announce is {}, the server is announcing {announce}",
            cluster.announce
        ));
    }
    if cluster.nodes.is_empty() {
        return Err("cluster.announce is set but no [[cluster.node]] is listed".into());
    }
    let mut local = None;
    let mut nodes = Vec::with_capacity(cluster.nodes.len());
    for (index, node) in cluster.nodes.iter().enumerate() {
        if node.announce == cluster.announce {
            if local.is_some() {
                return Err(format!(
                    "cluster.announce {} is listed on more than one node",
                    cluster.announce
                ));
            }
            local = Some(index);
        }
        if nodes.iter().any(|n: &Node| n.announce == node.announce) {
            return Err(format!(
                "cluster node {} is listed more than once",
                node.announce
            ));
        }
        let (host, _) = crate::cluster::split_announce(&node.announce);
        if host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_unspecified())
        {
            return Err(format!(
                "cluster node {} is a wildcard address; announce one a client can dial",
                node.announce
            ));
        }
        let mut ranges = Vec::with_capacity(node.ranges.len());
        for range in &node.ranges {
            ranges.push(parse_range(range)?);
        }
        nodes.push(Node {
            announce: node.announce.clone(),
            ranges,
        });
    }
    let local = local.ok_or_else(|| {
        format!(
            "cluster.announce {} is not one of the [[cluster.node]] addresses",
            cluster.announce
        )
    })?;
    Cluster::from_nodes(local, nodes).map_err(|e| e.0)
}

fn parse_range(text: &str) -> Result<SlotRange, String> {
    let Some((start, end)) = text.split_once('-') else {
        return Err(format!("slot range `{text}` must look like 0-8191"));
    };
    let start: u32 = start
        .parse()
        .map_err(|_| format!("slot range `{text}` has a bad start"))?;
    let end: u32 = end
        .parse()
        .map_err(|_| format!("slot range `{text}` has a bad end"))?;
    if start > end || end >= u32::from(crate::crc16::SLOT_COUNT) {
        return Err(format!("slot range `{text}` is outside 0-16383"));
    }
    Ok(SlotRange {
        start: start as u16,
        end: end as u16,
    })
}

pub fn resolve_listen(text: &str) -> Result<SocketAddr, String> {
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let mut addrs: Vec<SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(text)
        .map_err(|e| format!("listen address `{text}`: {e}"))?
        .collect();
    addrs.sort_by_key(|a| !a.is_ipv4());
    addrs
        .into_iter()
        .next()
        .ok_or_else(|| format!("listen address `{text}` did not resolve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_node_map_parses_and_a_typo_is_refused() {
        let text = r#"
listen = "127.0.0.1:6379"
data_dir = "/tmp/mpedb-redis"
shards = 4
durability = "commit"

[cluster]
announce = "10.0.0.1:6379"

[[cluster.node]]
announce = "10.0.0.1:6379"
ranges = ["0-8191"]

[[cluster.node]]
announce = "10.0.0.2:6379"
ranges = ["8192-16383"]
"#;
        let cfg = ServeConfig::from_toml(text).unwrap();
        assert!(cfg.cluster.is_local(0));
        assert!(!cfg.cluster.is_local(16383));
        assert_eq!(cfg.store.shards, 4);
        assert_eq!(cfg.store.durability, "commit");
        assert_eq!(cfg.announce, "10.0.0.1:6379");

        let bad = text.replace("8192-16383", "8192-16382");
        let err = ServeConfig::from_toml(&bad).unwrap_err();
        assert!(err.contains("slot 16383"), "{err}");

        let typo = text.replace("shards = 4", "shardz = 4");
        assert!(ServeConfig::from_toml(&typo).is_err());
    }
}
