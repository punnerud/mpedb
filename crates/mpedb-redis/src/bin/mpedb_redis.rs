//! `mpedb-redis serve` — a Redis-protocol blob server on one or more
//! `.mpedb` files.

use mpedb_redis::ServeConfig;
use std::net::SocketAddr;
use std::path::PathBuf;

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("mpedb-redis: {message}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let verb = args.next().unwrap_or_default();
    if verb != "serve" {
        return Err(usage());
    }
    let mut config_path: Option<PathBuf> = None;
    let mut listen: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut shards: Option<usize> = None;
    let mut durability: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(());
            }
            "--config" => {
                config_path = Some(PathBuf::from(need(&mut args, "--config")?));
            }
            "--listen" => listen = Some(need(&mut args, "--listen")?),
            "--data-dir" => data_dir = Some(PathBuf::from(need(&mut args, "--data-dir")?)),
            "--shards" => {
                let raw = need(&mut args, "--shards")?;
                shards = Some(
                    raw.parse()
                        .map_err(|_| format!("--shards {raw} is not a number"))?,
                );
            }
            "--durability" => durability = Some(need(&mut args, "--durability")?),
            other => return Err(format!("unknown argument {other}\n{}", usage())),
        }
    }
    let config = if let Some(path) = config_path {
        if listen.is_some() || data_dir.is_some() || shards.is_some() || durability.is_some() {
            return Err(
                "--config stands alone; put listen, data_dir, shards and durability in the file"
                    .into(),
            );
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        ServeConfig::from_toml(&text)?
    } else {
        let listen = listen.ok_or_else(usage)?;
        let data_dir = data_dir.ok_or_else(usage)?;
        let addr: SocketAddr = listen
            .parse()
            .map_err(|_| format!("--listen {listen} must be an ip:port"))?;
        ServeConfig::single(
            addr,
            data_dir,
            shards.unwrap_or(1),
            durability.unwrap_or_else(|| "async".into()),
        )
    };
    mpedb_redis::serve(config)
}

fn need(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn usage() -> String {
    "\
usage: mpedb-redis serve --listen IP:PORT --data-dir DIR [--shards N] [--durability none|commit|async|wal]
       mpedb-redis serve --config FILE

One process, one data directory. The directory remembers its shard count and
file size and refuses a later start that changes them. Durability `async`
(the default) acknowledges before the next fsync. `commit` and `wal` wait,
and those are the modes where group commit runs.

There is no AUTH. Bind an address the operating system can keep private.
There is no replica and no failover: a slot this process does not own is
answered with MOVED, and a dead node takes its slots with it."
        .into()
}
