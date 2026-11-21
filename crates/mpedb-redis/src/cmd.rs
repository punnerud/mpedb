//! The command subset. Names match Redis so an existing client can switch
//! the address and keep its calls.
//!
//! `GET` `SET` `DEL` `EXISTS` `EXPIRE` `TTL` `PTTL` `PERSIST` on strings,
//! `HGET` `HSET` `HDEL` on hashes, plus `PING`, `QUIT`, `SELECT 0`, and the
//! `CLUSTER` queries a cluster client sends before its first key. Anything
//! else is a named refusal, not a silent no-op.

use crate::cluster::Cluster;
use crate::resp::Resp;
use crate::store::{BlobStore, SetCond, SetExpiry, SetWritten, StoreError, Ttl};

pub struct Outcome {
    pub reply: Resp,
    pub close: bool,
}

impl Outcome {
    fn reply(reply: Resp) -> Self {
        Outcome {
            reply,
            close: false,
        }
    }

    fn close(reply: Resp) -> Self {
        Outcome { reply, close: true }
    }
}

pub fn dispatch(store: &BlobStore, cluster: &Cluster, argv: &[Vec<u8>], now: i64) -> Outcome {
    if argv.is_empty() {
        return Outcome::reply(Resp::err("ERR empty command"));
    }
    let name = upper(&argv[0]);
    match name.as_str() {
        "PING" => ping(argv),
        "QUIT" => Outcome::close(Resp::ok()),
        "SELECT" => select(argv),
        "GET" => one_key(argv, "get", |key| {
            cluster.check_key(key)?;
            Ok(match store.get(key, now).map_err(store_err)? {
                Some(bytes) => Resp::Bulk(Some(bytes)),
                None => Resp::Bulk(None),
            })
        }),
        "SET" => set(store, cluster, argv, now),
        "DEL" => keys(argv, "del", |keys| {
            let _slot = cluster.check_keys(keys)?;
            Ok(Resp::Int(store.del(keys, now).map_err(store_err)? as i64))
        }),
        "EXISTS" => keys(argv, "exists", |keys| {
            let _slot = cluster.check_keys(keys)?;
            Ok(Resp::Int(store.exists(keys, now).map_err(store_err)? as i64))
        }),
        "EXPIRE" => expire(store, cluster, argv, now),
        "PERSIST" => one_key(argv, "persist", |key| {
            cluster.check_key(key)?;
            Ok(Resp::Int(i64::from(
                store.persist(key, now).map_err(store_err)?,
            )))
        }),
        "TTL" => ttl_cmd(store, cluster, argv, now, false),
        "PTTL" => ttl_cmd(store, cluster, argv, now, true),
        "HGET" => hget(store, cluster, argv, now),
        "HSET" => hset(store, cluster, argv, now),
        "HDEL" => hdel(store, cluster, argv, now),
        "CLUSTER" => cluster_cmd(cluster, argv),
        other => {
            // The name is the client's bytes. Echo enough to recognise it.
            let shown: String = other.chars().take(64).collect();
            Outcome::reply(Resp::err(format!("ERR unknown command '{shown}'")))
        }
    }
}

fn ping(argv: &[Vec<u8>]) -> Outcome {
    match argv.len() {
        1 => Outcome::reply(Resp::pong()),
        2 => Outcome::reply(Resp::Bulk(Some(argv[1].clone()))),
        _ => Outcome::reply(arity("ping")),
    }
}

fn select(argv: &[Vec<u8>]) -> Outcome {
    if argv.len() != 2 {
        return Outcome::reply(arity("select"));
    }
    match std::str::from_utf8(&argv[1])
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(0) => Outcome::reply(Resp::ok()),
        Some(_) => Outcome::reply(Resp::err("ERR DB index is out of range")),
        None => Outcome::reply(Resp::err("ERR value is not an integer or out of range")),
    }
}

fn set(store: &BlobStore, cluster: &Cluster, argv: &[Vec<u8>], now: i64) -> Outcome {
    if argv.len() < 3 {
        return Outcome::reply(arity("set"));
    }
    let key = &argv[1];
    let value = &argv[2];
    let mut cond = SetCond::Always;
    let mut expiry = SetExpiry::None;
    let mut i = 3;
    while i < argv.len() {
        let opt = upper(&argv[i]);
        match opt.as_str() {
            "NX" => {
                cond = SetCond::IfAbsent;
                i += 1;
            }
            "XX" => {
                cond = SetCond::IfPresent;
                i += 1;
            }
            "EX" | "PX" => {
                // One expiry per SET, as Redis has it. Last-wins would make
                // `EX 10 PX 5` a five-millisecond key.
                if !matches!(expiry, SetExpiry::None) {
                    return Outcome::reply(Resp::err("ERR syntax error"));
                }
                let Some(raw) = argv.get(i + 1) else {
                    return Outcome::reply(Resp::err("ERR syntax error"));
                };
                let Ok(n) = parse_i64(raw) else {
                    return Outcome::reply(Resp::err(
                        "ERR value is not an integer or out of range",
                    ));
                };
                if n <= 0 {
                    return Outcome::reply(Resp::err("ERR invalid expire time in set"));
                }
                let ms = if opt == "EX" {
                    match n.checked_mul(1000) {
                        Some(ms) => ms,
                        None => {
                            return Outcome::reply(Resp::err(
                                "ERR value is not an integer or out of range",
                            ))
                        }
                    }
                } else {
                    n
                };
                let Some(deadline) = now.checked_add(ms) else {
                    return Outcome::reply(Resp::err(
                        "ERR value is not an integer or out of range",
                    ));
                };
                expiry = SetExpiry::At(deadline);
                i += 2;
            }
            other => {
                return Outcome::reply(Resp::err(format!("ERR unsupported SET option '{other}'")))
            }
        }
    }
    // Only the option tail counts. The key itself may be the bytes `NX`.
    let nx = argv[3..].iter().any(|a| upper(a) == "NX");
    let xx = argv[3..].iter().any(|a| upper(a) == "XX");
    if nx && xx {
        return Outcome::reply(Resp::err("ERR syntax error"));
    }
    Outcome::reply(match cluster.check_key(key) {
        Ok(_) => match store.set(key, value, expiry, cond, now) {
            Ok(SetWritten::Written) => Resp::ok(),
            Ok(SetWritten::Skipped) => Resp::Bulk(None),
            Err(e) => store_err(e),
        },
        Err(reply) => reply,
    })
}

fn expire(store: &BlobStore, cluster: &Cluster, argv: &[Vec<u8>], now: i64) -> Outcome {
    if argv.len() < 3 {
        return Outcome::reply(arity("expire"));
    }
    if let Some(option) = argv.get(3) {
        return Outcome::reply(Resp::err(format!(
            "ERR unsupported EXPIRE option '{}'",
            upper(option)
        )));
    }
    let Ok(seconds) = parse_i64(&argv[2]) else {
        return Outcome::reply(Resp::err("ERR value is not an integer or out of range"));
    };
    Outcome::reply(match cluster.check_key(&argv[1]) {
        Ok(_) => {
            let result = if seconds <= 0 {
                store.del(&[&argv[1]], now).map(|n| n > 0)
            } else {
                match now.checked_add(match seconds.checked_mul(1000) {
                    Some(ms) => ms,
                    None => {
                        return Outcome::reply(Resp::err(
                            "ERR value is not an integer or out of range",
                        ))
                    }
                }) {
                    Some(deadline) => store.expire_at(&argv[1], deadline, now),
                    None => {
                        return Outcome::reply(Resp::err(
                            "ERR value is not an integer or out of range",
                        ))
                    }
                }
            };
            match result {
                Ok(set) => Resp::Int(i64::from(set)),
                Err(e) => store_err(e),
            }
        }
        Err(reply) => reply,
    })
}

fn ttl_cmd(
    store: &BlobStore,
    cluster: &Cluster,
    argv: &[Vec<u8>],
    now: i64,
    millis: bool,
) -> Outcome {
    let name = if millis { "pttl" } else { "ttl" };
    one_key(argv, name, |key| {
        cluster.check_key(key)?;
        let ttl = store.ttl(key, now).map_err(store_err)?;
        Ok(Resp::Int(match ttl {
            Ttl::Missing => -2,
            Ttl::Persistent => -1,
            Ttl::Remaining(ms) if millis => ms,
            // Redis rounds to the nearest second: ten seconds read back one
            // millisecond later are still 10.
            Ttl::Remaining(ms) => ms.saturating_add(500) / 1000,
        }))
    })
}

fn hget(store: &BlobStore, cluster: &Cluster, argv: &[Vec<u8>], now: i64) -> Outcome {
    if argv.len() != 3 {
        return Outcome::reply(arity("hget"));
    }
    Outcome::reply(match cluster.check_key(&argv[1]) {
        Ok(_) => match store.hget(&argv[1], &argv[2], now) {
            Ok(Some(bytes)) => Resp::Bulk(Some(bytes)),
            Ok(None) => Resp::Bulk(None),
            Err(e) => store_err(e),
        },
        Err(reply) => reply,
    })
}

fn hset(store: &BlobStore, cluster: &Cluster, argv: &[Vec<u8>], now: i64) -> Outcome {
    if argv.len() < 4 || !argv.len().is_multiple_of(2) {
        return Outcome::reply(arity("hset"));
    }
    let fields: Vec<(&[u8], &[u8])> = argv[2..]
        .chunks(2)
        .map(|pair| (pair[0].as_slice(), pair[1].as_slice()))
        .collect();
    Outcome::reply(match cluster.check_key(&argv[1]) {
        Ok(_) => match store.hset(&argv[1], &fields, now) {
            Ok(n) => Resp::Int(n as i64),
            Err(e) => store_err(e),
        },
        Err(reply) => reply,
    })
}

fn hdel(store: &BlobStore, cluster: &Cluster, argv: &[Vec<u8>], now: i64) -> Outcome {
    if argv.len() < 3 {
        return Outcome::reply(arity("hdel"));
    }
    let fields: Vec<&[u8]> = argv[2..].iter().map(Vec::as_slice).collect();
    Outcome::reply(match cluster.check_key(&argv[1]) {
        Ok(_) => match store.hdel(&argv[1], &fields, now) {
            Ok(n) => Resp::Int(n as i64),
            Err(e) => store_err(e),
        },
        Err(reply) => reply,
    })
}

fn cluster_cmd(cluster: &Cluster, argv: &[Vec<u8>]) -> Outcome {
    if argv.len() != 2 {
        return Outcome::reply(arity("cluster"));
    }
    let sub = upper(&argv[1]);
    let reply = match sub.as_str() {
        "SLOTS" => cluster.slots_reply(),
        "NODES" => cluster.nodes_reply(),
        "INFO" => Resp::Bulk(Some(cluster_info(cluster).into_bytes())),
        other => Resp::err(format!("ERR unknown subcommand '{other}'")),
    };
    Outcome::reply(reply)
}

fn cluster_info(cluster: &Cluster) -> String {
    let nodes = cluster.nodes().len();
    format!(
        "cluster_state:ok\r\n\
         cluster_slots_assigned:{slots}\r\n\
         cluster_slots_ok:{slots}\r\n\
         cluster_known_nodes:{nodes}\r\n\
         cluster_size:{nodes}\r\n",
        slots = crate::crc16::SLOT_COUNT,
    )
}

fn one_key(argv: &[Vec<u8>], name: &str, f: impl FnOnce(&[u8]) -> Result<Resp, Resp>) -> Outcome {
    if argv.len() != 2 {
        return Outcome::reply(arity(name));
    }
    Outcome::reply(match f(&argv[1]) {
        Ok(reply) => reply,
        Err(reply) => reply,
    })
}

fn keys(argv: &[Vec<u8>], name: &str, f: impl FnOnce(&[&[u8]]) -> Result<Resp, Resp>) -> Outcome {
    if argv.len() < 2 {
        return Outcome::reply(arity(name));
    }
    let keys: Vec<&[u8]> = argv[1..].iter().map(Vec::as_slice).collect();
    Outcome::reply(match f(&keys) {
        Ok(reply) => reply,
        Err(reply) => reply,
    })
}

fn store_err(err: StoreError) -> Resp {
    match err {
        StoreError::WrongType => Resp::Error(err.to_string()),
        other => Resp::err(format!("ERR {other}")),
    }
}

fn arity(name: &str) -> Resp {
    Resp::err(format!(
        "ERR wrong number of arguments for '{name}' command"
    ))
}

fn upper(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| b.to_ascii_uppercase() as char)
        .collect()
}

fn parse_i64(bytes: &[u8]) -> Result<i64, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return Err(());
    }
    text.parse().map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Cluster;
    use crate::crc16::slot;
    use crate::store::StoreOptions;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn store() -> BlobStore {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = mpedb_testkit::scratch_base().join(format!(
            "mpedb-redis-cmd-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        BlobStore::open(&StoreOptions {
            dir,
            shards: 1,
            durability: "none".into(),
            size_mb: 8,
            zstd_level: 3,
            max_value_bytes: 64,
        })
        .unwrap()
    }

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn set_get_nx_and_unknown_option() {
        let store = store();
        let cluster = Cluster::single("127.0.0.1:6379");
        let now = 5_000;
        let set = dispatch(&store, &cluster, &args(&["SET", "k", "v"]), now);
        assert_eq!(set.reply, Resp::ok());
        let get = dispatch(&store, &cluster, &args(&["get", "k"]), now);
        assert_eq!(get.reply, Resp::Bulk(Some(b"v".to_vec())));
        let nx = dispatch(&store, &cluster, &args(&["SET", "k", "w", "NX"]), now);
        assert_eq!(nx.reply, Resp::Bulk(None));
        let bad = dispatch(&store, &cluster, &args(&["SET", "k", "w", "KEEPTTL"]), now);
        assert!(
            matches!(bad.reply, Resp::Error(ref s) if s.contains("KEEPTTL")),
            "{:?}",
            bad.reply
        );
        let huge = vec![b"SET".to_vec(), b"k".to_vec(), vec![b'a'; 65]];
        let err = dispatch(&store, &cluster, &huge, now);
        assert!(
            matches!(err.reply, Resp::Error(ref s) if s.contains("max")),
            "{:?}",
            err.reply
        );
    }

    #[test]
    fn expire_ttl_and_zero_ttl_deletes() {
        let store = store();
        let cluster = Cluster::single("127.0.0.1:6379");
        let now = 1_000;
        dispatch(
            &store,
            &cluster,
            &args(&["SET", "k", "v", "PX", "1500"]),
            now,
        );
        let ttl = dispatch(&store, &cluster, &args(&["TTL", "k"]), now);
        assert_eq!(ttl.reply, Resp::Int(2));
        let pttl = dispatch(&store, &cluster, &args(&["PTTL", "k"]), now);
        assert_eq!(pttl.reply, Resp::Int(1500));
        let gone = dispatch(&store, &cluster, &args(&["EXPIRE", "k", "0"]), now);
        assert_eq!(gone.reply, Resp::Int(1));
        let missing = dispatch(&store, &cluster, &args(&["TTL", "k"]), now);
        assert_eq!(missing.reply, Resp::Int(-2));
    }

    #[test]
    fn moved_uses_the_peer_address() {
        let store = store();
        let key = b"alpha";
        assert_ne!(slot(key), 0);
        let cluster = Cluster::from_nodes(
            0,
            vec![
                crate::cluster::Node {
                    announce: "127.0.0.1:1".into(),
                    ranges: vec![crate::cluster::SlotRange { start: 0, end: 0 }],
                },
                crate::cluster::Node {
                    announce: "10.0.0.9:6379".into(),
                    ranges: vec![crate::cluster::SlotRange {
                        start: 1,
                        end: 16383,
                    }],
                },
            ],
        )
        .unwrap();
        let reply = dispatch(&store, &cluster, &[b"GET".to_vec(), key.to_vec()], 1);
        assert_eq!(
            reply.reply,
            Resp::Error(format!("MOVED {} 10.0.0.9:6379", slot(key)))
        );
    }

    #[test]
    fn a_single_node_deletes_across_slots_and_checks_keys_first() {
        let store = store();
        let cluster = Cluster::single("127.0.0.1:6379");
        for key in ["a", "b", "c"] {
            dispatch(&store, &cluster, &args(&["SET", key, "v"]), 1);
        }
        let gone = dispatch(&store, &cluster, &args(&["DEL", "a", "b", "missing"]), 1);
        assert_eq!(gone.reply, Resp::Int(2));
        // A key that cannot exist refuses the command before `c` is touched.
        let long = "k".repeat(2000);
        let refused = dispatch(&store, &cluster, &args(&["DEL", "c", &long]), 1);
        assert!(matches!(refused.reply, Resp::Error(_)));
        let still = dispatch(&store, &cluster, &args(&["EXISTS", "c"]), 1);
        assert_eq!(still.reply, Resp::Int(1));
    }
}
