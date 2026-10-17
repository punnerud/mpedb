# Host services

`mpedb-host` is a database-independent Rust library for controlled file and HTTP
operations. It has no MPEdb, Oracle, PostgreSQL or SQLite dependency. A host can
embed it directly or use its JSON gateway transport with a service behind Nginx.

Every operation must pass **both** authorization layers:

1. An immutable `Environment`, supplied by the host outside the database, owns
   directory roots, endpoint URLs/IPs/methods/headers and maximum resource limits.
2. Mutable `Grants`, selected by an authenticated database/user/routine principal,
   permit a subset of operations. Authorized inner administrators can grant,
   revoke and expand inner permissions, including their own. They cannot change
   the environment. `allow_acl_changes=false` also denies inner administration.

This supports database-side extension/ACL administration without letting a
routine enlarge its environment. The current inner ACL implementation is in
memory; database catalogs, role inheritance, Oracle `AUTHID` and
`DBMS_NETWORK_ACL_ADMIN`/directory-object bindings remain adapter work. A caller
must supply the principal from trusted execution context, never from an
untrusted SQL argument. Administrators may store grants beyond the environment;
the intersection still refuses the corresponding I/O.

## Providers and limits

`Runtime::native(environment, grants)` opens named directory capabilities once
with `cap-std`, and prepares pooled HTTP clients when feature `http` is enabled.
File reads are bounded and require regular files. Writes use a fresh private
file plus atomic rename; they do not truncate through symlinks or other hard
links. Path traversal and symlink escapes outside a directory are refused.
File data is synced; Unix also syncs the pinned parent directory. Filesystem/OS
power-loss behavior still needs validation on each deployment platform.

HTTP requires explicit endpoint/IP/method/header grants in both layers. Runtime
connections use the configured IPs while preserving the URL host for HTTP/TLS.
There is no DNS fallback, environment proxy, automatic redirect or automatic
retry. TLS verification stays enabled. Response bodies, request bodies, headers,
URLs and network deadlines are bounded. Repeated response headers are retained.
`timeout_ms` bounds network calls; it is not a kernel-level file-I/O deadline.
The blocking API belongs in a bounded worker pool when embedded in an async
service.

`GatewayBackend` uses the same authorization envelope and verifies result
shape/size locally. Its target, pinned IPs and optional bearer credential are
trusted host setup. The remote service must authenticate/bind the asserted
principal and independently enforce its own environment and ACL before dispatch.
`GatewayEnvelope.principal` is not authentication. Nginx alone does not implement
file/HTTP operations: an authenticated dispatch service is required behind it.
The library currently supplies the client/protocol, not a listening daemon.

Default host operations are denied. Without feature `http`, network targets are
refused at configuration time. Limits are intersected per call; the maximum byte
budget is 64 MiB. JSON gateway request/response frames also have a 64 MiB encoded
limit. `max_in_flight_per_runtime` bounds simultaneous host operations and refuses
excess work before dispatch. This limit is per Runtime instance, not a shared
quota across independent service processes. Revocation applies to subsequent
calls; ACL administration does not wait for an in-flight network operation.

External effects do **not** roll back with a database transaction. A lost reply
can follow a completed effect. Use durable jobs/outboxes and destination
idempotency where effects must follow commit; do not blindly retry impure
routines or execute network calls while holding the database writer lock.

## Configuration and embedding

The host can construct all parameters in Rust or parse an external TOML ceiling:

```toml
allow_acl_changes = true
max_in_flight_per_runtime = 64

[limits]
read_bytes = 1048576
write_bytes = 1048576
timeout_ms = 10000

[directories.exports]
root = "/srv/app/exports"
read = true
write = true

[endpoints.billing]
base_url = "https://billing.example/api/"
connect_ips = ["192.0.2.20"]
methods = ["GET", "POST"]
headers = ["content-type", "idempotency-key"]
```

The example IP is documentation-only. The deployment operator supplies real
addresses and keeps this file outside database-controlled write areas. Inner
principals start with no grants; the embedding host supplies initial grants,
then authorized callers can use `Runtime::set_grants`. An Oracle-compatible
adapter will map its role/ACL operations onto this boundary rather than modify
the environment. Existing PL/SQL routines do not yet have `UTL_HTTP`/`UTL_FILE`
bindings to this library.

```rust
let ceiling = mpedb_host::Environment::from_toml(environment_text)?;
let host = mpedb_host::Runtime::native(ceiling, authenticated_principals)?;
let result = host.execute(trusted_principal, &mpedb_host::Request::ReadFile {
    directory: "exports".into(), path: "report.csv".into(),
})?;
```

## Optional services and scale

A separate service/worker executable can embed this library and attach MPEdb
through the database API. It must never modify a database using raw `WriteFile`.
Several processes on the **same host** may attach the same MPEdb file/shared
mapping. Readers run concurrently; the default engine has one coordinated writer
per database file. More workers can overlap reads, I/O and preparation, but do not
create unlimited simultaneous commits. Separate database files/partitions can
supply independent writers; cross-file routing, query combination and atomicity
are separate work, not an automatic consequence of adding service instances.

The service is optional. Existing socket-activated PostgreSQL ingress/Nginx and
embedded access remain separate deployment choices. A future persistent Rust
service needs bounded queues, backpressure, streaming results, batching and
measured crash recovery. A local mmap/shared file is not cross-host shared
memory; remote nodes need the synchronization protocol.

MPEdb's configuration ceiling is currently 16 TiB with 64-bit page ids, but this
is not a TB load-test result. Database capacity is reserved up front. Future
MacBook Pro testing with a 4 TB USB-C disk should start around a 1 TB database
file on local APFS, retain room for WAL/backup/migration, and measure actual
populated data separately from reserved size. Measure point/range reads, sustained
mixed load, reader/writer scaling, p95/p99 latency, durability sync, working sets
larger than RAM, Oracle/PG/SQLite synchronization lag and crash recovery. Advertised
sequential disk throughput is not a database benchmark. These hardware tests are
deferred at the user's request; no TB or macOS execution is claimed here.

## Validation

```sh
cargo test -p mpedb-host
cargo test -p mpedb-host --all-features
cargo clippy -p mpedb-host --all-targets -- -D warnings
cargo clippy -p mpedb-host --all-features --all-targets -- -D warnings
```

Tests perform real file I/O and local HTTP/gateway calls, exercise inner
self-administration under the outer ceiling, and verify byte/concurrency limits,
path/symlink escapes, hard-link-safe replacement, special files, deadlines,
connection reuse, repeated headers and malformed gateway responses. All tests
run locally without external network services. Public API references:
[cap-std directory capabilities](https://docs.rs/cap-std/latest/cap_std/fs/struct.Dir.html),
[reqwest blocking client](https://docs.rs/reqwest/0.12.28/reqwest/blocking/struct.ClientBuilder.html).
