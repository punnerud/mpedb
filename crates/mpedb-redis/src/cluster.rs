//! Static Redis Cluster slot map.
//!
//! This is routing, not replication. Each slot has exactly one owner. A key
//! that lands on another node gets `MOVED` and nothing is written here. Live
//! rebalancing and failover are a different problem — see
//! `design/DESIGN-DISTRIBUTED.md`. The map is whatever the config said at
//! process start; it does not change while the process runs.

use crate::crc16::{slot, SLOT_COUNT};
use crate::resp::Resp;
use std::fmt;

/// One contiguous inclusive range of slots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRange {
    pub start: u16,
    pub end: u16,
}

/// A node clients can be sent to, and the slots it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// `host:port` as a Redis client should dial it. Not necessarily the
    /// address this process binds.
    pub announce: String,
    pub ranges: Vec<SlotRange>,
}

/// The slot map plus which entry is this process.
#[derive(Debug, Clone)]
pub struct Cluster {
    nodes: Vec<Node>,
    /// `owner[slot] = index into nodes`.
    owner: Vec<u16>,
    local: usize,
}

impl Cluster {
    /// This process owns every slot. `MOVED` never fires. `CLUSTER SLOTS`
    /// still answers, so a cluster client pointed at one node works.
    pub fn single(announce: impl Into<String>) -> Self {
        Self::from_nodes(
            0,
            vec![Node {
                announce: announce.into(),
                ranges: vec![SlotRange {
                    start: 0,
                    end: SLOT_COUNT - 1,
                }],
            }],
        )
        .expect("one node covering every slot is a valid map")
    }

    /// `local` is the index of this process in `nodes`.
    pub fn from_nodes(local: usize, nodes: Vec<Node>) -> Result<Self, MapError> {
        if nodes.is_empty() {
            return Err(MapError::new("cluster has no nodes"));
        }
        if local >= nodes.len() {
            return Err(MapError::new(format!(
                "local node index {local} is outside the {} node(s)",
                nodes.len()
            )));
        }
        let mut owner = vec![u16::MAX; SLOT_COUNT as usize];
        for (index, node) in nodes.iter().enumerate() {
            check_announce(&node.announce)?;
            if node.ranges.is_empty() {
                return Err(MapError::new(format!(
                    "node {} owns no slots",
                    node.announce
                )));
            }
            for range in &node.ranges {
                if range.start > range.end || range.end >= SLOT_COUNT {
                    return Err(MapError::new(format!(
                        "node {} has a bad slot range {}-{}",
                        node.announce, range.start, range.end
                    )));
                }
                for slot in range.start..=range.end {
                    let cell = &mut owner[slot as usize];
                    if *cell != u16::MAX {
                        let other = &nodes[*cell as usize].announce;
                        return Err(MapError::new(format!(
                            "slot {slot} is owned by both {other} and {}",
                            node.announce
                        )));
                    }
                    *cell = index as u16;
                }
            }
        }
        if let Some(missing) = owner.iter().position(|o| *o == u16::MAX) {
            return Err(MapError::new(format!("slot {missing} has no node")));
        }
        Ok(Cluster {
            nodes,
            owner,
            local,
        })
    }

    pub fn local_announce(&self) -> &str {
        &self.nodes[self.local].announce
    }

    pub fn is_local(&self, slot: u16) -> bool {
        self.owner[slot as usize] as usize == self.local
    }

    pub fn owner_announce(&self, slot: u16) -> &str {
        &self.nodes[self.owner[slot as usize] as usize].announce
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn local_index(&self) -> usize {
        self.local
    }

    /// `MOVED` when `key` is not ours. `Ok` when it is.
    pub fn check_key(&self, key: &[u8]) -> Result<u16, Resp> {
        let slot = slot(key);
        if self.is_local(slot) {
            Ok(slot)
        } else {
            Err(moved(slot, self.owner_announce(slot)))
        }
    }

    /// Every key must share a slot, and that slot must be ours.
    ///
    /// Redis answers `CROSSSLOT` before `MOVED`: two keys that hash apart
    /// are a client bug even when one of them would have been a redirect.
    pub fn check_keys(&self, keys: &[&[u8]]) -> Result<u16, Resp> {
        let Some(first) = keys.first() else {
            return Err(Resp::err("ERR empty key list"));
        };
        let first_slot = slot(first);
        // One node owns every slot, so there is nowhere else for a key to
        // be. A plain client sending `DEL a b` gets its count, as it would
        // from a standalone Redis. The store handles each slot on its own.
        if self.nodes.len() == 1 {
            return Ok(first_slot);
        }
        for key in keys.iter().skip(1) {
            if slot(key) != first_slot {
                return Err(Resp::Error(
                    "CROSSSLOT Keys in request don't hash to the same slot".into(),
                ));
            }
        }
        if self.is_local(first_slot) {
            Ok(first_slot)
        } else {
            Err(moved(first_slot, self.owner_announce(first_slot)))
        }
    }

    /// `CLUSTER SLOTS` body.
    pub fn slots_reply(&self) -> Resp {
        let mut outer = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            for range in &node.ranges {
                let (host, port) = split_announce(&node.announce);
                // A wildcard listen address is not somewhere a client can
                // dial. An empty host is how Redis says "the address you
                // reached me on".
                let host = if is_unspecified(host) { "" } else { host };
                outer.push(Resp::Array(vec![
                    Resp::Int(i64::from(range.start)),
                    Resp::Int(i64::from(range.end)),
                    Resp::Array(vec![
                        Resp::Bulk(Some(host.as_bytes().to_vec())),
                        Resp::Int(port),
                        Resp::Bulk(Some(node_id(&node.announce).into_bytes())),
                    ]),
                ]));
            }
        }
        Resp::Array(outer)
    }

    /// `CLUSTER NODES` body, one line per node.
    pub fn nodes_reply(&self) -> Resp {
        let mut text = String::new();
        for (index, node) in self.nodes.iter().enumerate() {
            let id = node_id(&node.announce);
            let flags = if index == self.local {
                "myself,master"
            } else {
                "master"
            };
            let ranges: Vec<String> = node
                .ranges
                .iter()
                .map(|r| {
                    if r.start == r.end {
                        format!("{}", r.start)
                    } else {
                        format!("{}-{}", r.start, r.end)
                    }
                })
                .collect();
            let (host, port) = split_announce(&node.announce);
            let host = if is_unspecified(host) { "" } else { host };
            text.push_str(&format!(
                "{id} {host}:{port}@0 {flags} - 0 0 0 connected {}\n",
                ranges.join(" ")
            ));
        }
        Resp::Bulk(Some(text.into_bytes()))
    }
}

fn moved(slot: u16, announce: &str) -> Resp {
    // Redis prints an IPv6 host bare, and clients split on the last colon.
    let (host, port) = split_announce(announce);
    Resp::Error(format!("MOVED {slot} {host}:{port}"))
}

fn is_unspecified(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified())
}

fn node_id(announce: &str) -> String {
    // Stable for the life of the config, and 40 hex chars because that is
    // the width CLUSTER NODES readers split on. Not a secret and not a hash
    // of the data. It is a hash of the whole address: the first 20 bytes of
    // `mpedb-redis-cluster-0.svc:6379` and `...-1.svc:6379` are the same
    // bytes, and two nodes with one id are one node to a client.
    let mut hex = String::new();
    for seed in 0u8..3 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in std::iter::once(seed).chain(announce.bytes()) {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hex.push_str(&format!("{h:016x}"));
    }
    hex.truncate(40);
    hex
}

/// Split `host:port` or `[host]:port` into the host Redis should see and the
/// port number. A config that passed [`check_announce`] cannot fail here.
pub fn split_announce(announce: &str) -> (&str, i64) {
    if let Some(rest) = announce.strip_prefix('[') {
        if let Some((host, port)) = rest.rsplit_once("]:") {
            let port = port.parse::<i64>().unwrap_or(0);
            return (host, port);
        }
    }
    match announce.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<i64>().unwrap_or(0)),
        None => (announce, 0),
    }
}

pub fn check_announce(announce: &str) -> Result<(), MapError> {
    if announce.is_empty() || announce.chars().any(char::is_whitespace) {
        return Err(MapError::new(format!(
            "announce address `{announce}` is empty or contains whitespace"
        )));
    }
    let port = if announce.starts_with('[') {
        let Some((_, port)) = announce.rsplit_once("]:") else {
            return Err(MapError::new(format!(
                "announce address `{announce}` must look like [host]:port"
            )));
        };
        port
    } else {
        let Some((host, port)) = announce.rsplit_once(':') else {
            return Err(MapError::new(format!(
                "announce address `{announce}` must look like host:port"
            )));
        };
        if host.is_empty() || host.contains(':') {
            return Err(MapError::new(format!(
                "announce address `{announce}` must look like host:port (bracket an IPv6 host)"
            )));
        }
        port
    };
    if port.parse::<u16>().is_err() {
        return Err(MapError::new(format!(
            "announce address `{announce}` has a bad port"
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub struct MapError(pub String);

impl MapError {
    fn new(message: impl Into<String>) -> Self {
        MapError(message.into())
    }
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(announce: &str, start: u16, end: u16) -> Node {
        Node {
            announce: announce.into(),
            ranges: vec![SlotRange { start, end }],
        }
    }

    #[test]
    fn a_gap_and_an_overlap_are_named() {
        let gap = Cluster::from_nodes(
            0,
            vec![node("127.0.0.1:1", 0, 10), node("127.0.0.1:2", 12, 16383)],
        );
        let message = gap.unwrap_err().0;
        assert!(message.contains("slot 11"), "{message}");

        let overlap = Cluster::from_nodes(
            0,
            vec![node("127.0.0.1:1", 0, 10), node("127.0.0.1:2", 10, 16383)],
        );
        let message = overlap.unwrap_err().0;
        assert!(message.contains("slot 10"), "{message}");
        assert!(message.contains("127.0.0.1:1"), "{message}");
        assert!(message.contains("127.0.0.1:2"), "{message}");
    }

    #[test]
    fn moved_names_the_owner_and_crossslot_wins_over_moved() {
        let cluster = Cluster::from_nodes(
            0,
            vec![node("127.0.0.1:1", 0, 0), node("10.0.0.2:6379", 1, 16383)],
        )
        .unwrap();
        let key = b"not-slot-zero";
        assert_ne!(slot(key), 0);
        let err = cluster.check_key(key).unwrap_err();
        assert_eq!(
            err,
            Resp::Error(format!("MOVED {} 10.0.0.2:6379", slot(key)))
        );

        // Two keys in different slots, one of them even ours: still CROSSSLOT.
        let mut other = b"a".to_vec();
        while slot(b"a") == slot(&other) {
            other.push(b'b');
        }
        let err = cluster.check_keys(&[b"a", other.as_slice()]).unwrap_err();
        assert!(
            matches!(err, Resp::Error(ref s) if s.starts_with("CROSSSLOT")),
            "{err:?}"
        );
    }

    #[test]
    fn ipv6_announce_splits_on_the_bracket() {
        check_announce("[2001:db8::1]:6379").unwrap();
        assert_eq!(split_announce("[2001:db8::1]:6379"), ("2001:db8::1", 6379));
        assert!(check_announce("10.0.0.1").is_err());
    }

    #[test]
    fn node_ids_differ_past_the_first_twenty_bytes() {
        let a = node_id("mpedb-redis-cluster-0.svc:6379");
        let b = node_id("mpedb-redis-cluster-1.svc:6379");
        assert_eq!(a.len(), 40);
        assert_ne!(a, b);
    }
}
