//! `mpedb queue` — the durable task queue (design/DESIGN-SERVICE.md §2, stage 1
//! of §7): enqueue + claim + PySpell-proc runner. This is the v1 spine of the
//! hibernating-service design — no resident process, no daemon: any process
//! enqueues, and `mpedb queue run` wakes (cron / by hand / socket-activated
//! later), drains due work, and EXITS when idle. Wake-ups beyond polling
//! (doorbell, `serve`, cron projection, webhooks) are later stages.
//!
//! # The claim protocol
//!
//! The queue is an ordinary table (`mq_task`, created on first use). Because
//! mpedb serializes writers (single writer lock + MVCC), a claim needs no
//! `FOR UPDATE SKIP LOCKED` machinery — it is ONE autocommit statement, atomic
//! by construction (the #97 subquery-in-UPDATE lift + RETURNING):
//!
//! ```sql
//! UPDATE mq_task SET state='claimed', claimed_by=$pid, claimed_at=$now,
//!                    attempts = attempts + 1
//! WHERE state='pending'
//!   AND id = (SELECT id FROM mq_task WHERE state='pending' AND run_at <= $now
//!             ORDER BY priority, id LIMIT 1)
//! RETURNING id, proc, payload, attempts, max_attempts
//! ```
//!
//! Two runners started simultaneously therefore claim DISJOINT tasks: the
//! writer lock orders the two statements, and the second one's subquery no
//! longer sees the first one's row as `pending`.
//!
//! Semantics are **at-least-once** with an explicit claim/complete protocol,
//! exactly the doc's `enqueue → claim → run proc → write result → done`:
//! the claim commits BEFORE the proc runs, the proc body runs in its own
//! atomic write transaction (the mpedb-proc engine), and completion is a
//! separate guarded commit. A runner SIGKILLed anywhere in between leaves a
//! `claimed` row whose lease (`claimed_at + --lease`) expires; the next
//! `queue run` reclaims it to `pending` (never lost). The completion UPDATE
//! is guarded on `(id, state='claimed', claimed_by, claimed_at)`, so a runner
//! that lost its lease mid-run can never double-complete silently — its late
//! completion affects 0 rows and is reported instead. Set `--lease` above the
//! longest task runtime: v1's reap rule is the lease timestamp alone (the
//! reader-table pid-liveness refinement in the doc is deferred — a naive
//! `/proc` check would let a recycled pid block reclaim forever).
//!
//! States: `pending → claimed → done`, plus `failed` (ran and errored,
//! retries exhausted — `error` has the message) and `dead` (lease expired
//! with retries exhausted: a crash loop that never completed). A runtime
//! error before `max_attempts` reschedules to `pending` with exponential
//! backoff (`--retry-delay << (attempts-1)`).
//!
//! The task body is a stored procedure (`mpedb proc define`) named by the
//! row's `proc` column; `payload` carries its arguments as newline-joined CLI
//! literals re-parsed by [`crate::util::parse_param`] — text, deliberately
//! not a binary format (nothing to truncate; hostile content parses to Text).

use std::time::{SystemTime, UNIX_EPOCH};

use mpedb::{Database, ExecResult, Value};
use mpedb_proc::{ProcEngine, ProcValue};

use crate::args;
use crate::render::value_str;
use crate::util::{parse_param, runtime, usage, CliResult, Failure};

const USAGE: &str = "\
usage: mpedb queue <subcommand>

  queue init    <target>                            create the mq_task table
  queue enqueue <target> <proc> [arg ...]           enqueue a task, print its id
        --queue <name>       queue name (default `default`)
        --priority <n>       lower runs first (default 100)
        --delay <secs>       run no earlier than now + secs
        --run-at <iso8601>   run no earlier than this instant
        --max-attempts <n>   claims before failed/dead (default 5)
  queue run     <target>                            drain due tasks, then exit
        --queue <name>       only this queue (default: all)
        --lease <secs>       claim visibility timeout (default 60);
                             set ABOVE the longest task runtime
        --retry-delay <secs> retry backoff base (default 1)
        --limit <n>          run at most n tasks (default: unlimited)
  queue list    <target> [--state <s>]              dump the queue table

<target> is a config.toml or a .mpedb file. Tasks are stored procedures
(`mpedb proc define`); args are CLI literals (int/float/text/null/0x…/ISO
timestamp). The runner is Model A of design/DESIGN-SERVICE.md: no daemon —
invoke it from cron/systemd/by hand; overlapping runners claim disjoint
tasks and a SIGKILLed runner's claims are reclaimed after the lease.";

/// The queue table (design/DESIGN-SERVICE.md §2's column list, plus `proc` —
/// the doc names the runner's proc but gives it no column; it has to live in
/// the row). `id INTEGER PRIMARY KEY` = rowid alias, so enqueue omits it and
/// gets max+1 auto-assignment.
const CREATE_TABLE: &str = "CREATE TABLE mq_task (
  id INTEGER PRIMARY KEY,
  queue TEXT NOT NULL,
  proc TEXT NOT NULL,
  payload TEXT NOT NULL,
  state TEXT NOT NULL,
  priority INTEGER NOT NULL,
  run_at TIMESTAMP NOT NULL,
  attempts INTEGER NOT NULL,
  max_attempts INTEGER NOT NULL,
  claimed_by INTEGER,
  claimed_at TIMESTAMP,
  result TEXT,
  error TEXT
)";

pub fn run(argv: &[String]) -> CliResult {
    let Some(sub) = argv.first() else {
        return usage(format!("queue needs a subcommand\n\n{USAGE}"));
    };
    let rest = &argv[1..];
    match sub.as_str() {
        "init" => cmd_init(rest),
        "enqueue" => cmd_enqueue(rest),
        "run" => cmd_run(rest),
        "list" => cmd_list(rest),
        other => usage(format!("unknown queue subcommand `{other}`\n\n{USAGE}")),
    }
}

pub(crate) fn now_micros() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before 1970");
    i64::try_from(now.as_micros()).expect("system clock past year 294246")
}

/// Create `mq_task` if it does not exist. Race-tolerant: two processes racing
/// here serialize on the writer lock and the loser's "already exists" is
/// success (plain `CREATE TABLE` has no IF NOT EXISTS yet).
pub(crate) fn ensure_table(db: &Database) -> Result<(), Failure> {
    match db.query(CREATE_TABLE, &[]) {
        Ok(_) => Ok(()),
        // The engine's duplicate-table refusal ("duplicate table name"); kept
        // loose enough to also cover an `already exists` spelling.
        Err(e)
            if e.to_string().contains("duplicate table name")
                || e.to_string().contains("already exists") =>
        {
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn cmd_init(argv: &[String]) -> CliResult {
    let [target] = argv else {
        return usage("queue init needs <target>");
    };
    let db = crate::util::open_target(target)?;
    ensure_table(&db)?;
    println!("mq_task ready");
    Ok(())
}

// ---------------------------------------------------------------- enqueue

pub(crate) struct EnqueueSpec<'a> {
    pub queue: &'a str,
    pub proc: &'a str,
    pub args: &'a [String],
    pub priority: i64,
    pub run_at: i64,
    pub max_attempts: i64,
}

/// Args → the payload column: newline-joined CLI literals. Not a binary
/// format — decoding is `split('\n')` + `parse_param`, total on any input.
/// The two shapes the join cannot represent are rejected here: an embedded
/// newline, and a SINGLE empty-string arg (indistinguishable from no args;
/// `null` or a quoted sentinel serves instead). An empty string BETWEEN other
/// args round-trips fine.
pub(crate) fn encode_payload(args: &[String]) -> Result<String, Failure> {
    if let Some(bad) = args.iter().find(|a| a.contains('\n')) {
        return runtime(format!("task args may not contain newlines: {bad:?}"));
    }
    if args.len() == 1 && args[0].is_empty() {
        return runtime("a single empty-string arg cannot be represented; pass `null` instead");
    }
    Ok(args.join("\n"))
}

pub(crate) fn decode_payload(payload: &str) -> Vec<Value> {
    if payload.is_empty() {
        return Vec::new();
    }
    payload.split('\n').map(parse_param).collect()
}

/// Insert one pending task; returns its id. One autocommit INSERT — durable
/// as of the configured durability mode, like any other write.
pub(crate) fn enqueue_task(db: &Database, spec: &EnqueueSpec<'_>) -> Result<i64, Failure> {
    let payload = encode_payload(spec.args)?;
    let res = db.query(
        "INSERT INTO mq_task (queue, proc, payload, state, priority, run_at, \
                              attempts, max_attempts) \
         VALUES ($1, $2, $3, 'pending', $4, $5, 0, $6) RETURNING id",
        &[
            Value::Text(spec.queue.to_owned()),
            Value::Text(spec.proc.to_owned()),
            Value::Text(payload),
            Value::Int(spec.priority),
            Value::Timestamp(spec.run_at),
            Value::Int(spec.max_attempts),
        ],
    )?;
    match res {
        ExecResult::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(id)) => Ok(*id),
            other => runtime(format!("enqueue RETURNING id gave {other:?}")),
        },
        other => runtime(format!("enqueue expected rows, got {other:?}")),
    }
}

fn cmd_enqueue(argv: &[String]) -> CliResult {
    let p = args::parse(
        argv,
        &["queue", "priority", "delay", "run-at", "max-attempts"],
        &[],
    )?;
    let [target, proc, task_args @ ..] = &p.positional[..] else {
        return usage("queue enqueue needs <target> <proc> [arg ...]");
    };
    let now = now_micros();
    let run_at = match (p.value("run-at"), p.value("delay")) {
        (Some(_), Some(_)) => return usage("--run-at and --delay are mutually exclusive"),
        (Some(ts), None) => match parse_param(ts) {
            Value::Timestamp(t) => t,
            _ => return usage(format!("--run-at is not an ISO-8601 timestamp: `{ts}`")),
        },
        (None, Some(_)) => now + i64::try_from(p.require_u64("delay")?)
            .map_err(|_| Failure::Usage("--delay too large".into()))?
            .saturating_mul(1_000_000),
        (None, None) => now,
    };
    let priority = match p.value("priority") {
        None => 100,
        Some(v) => v
            .parse::<i64>()
            .map_err(|_| Failure::Usage(format!("--priority must be an integer, got `{v}`")))?,
    };
    let max_attempts = i64::try_from(p.u64_or("max-attempts", 5)?.max(1))
        .map_err(|_| Failure::Usage("--max-attempts too large".into()))?;
    let db = crate::util::open_target(target)?;
    ensure_table(&db)?;
    let id = enqueue_task(
        &db,
        &EnqueueSpec {
            queue: p.value("queue").unwrap_or("default"),
            proc,
            args: task_args,
            priority,
            run_at,
            max_attempts,
        },
    )?;
    println!("{id}");
    Ok(())
}

// -------------------------------------------------------------------- run

/// One claimed task, as returned by the claim UPDATE.
pub(crate) struct Claimed {
    pub id: i64,
    pub proc: String,
    pub payload: String,
    /// Post-increment attempts — the number of this claim (1-based).
    pub attempts: i64,
    pub max_attempts: i64,
    /// The `claimed_at` we wrote — the completion guard.
    pub claimed_at: i64,
}

/// The doc's atomic claim (module doc above). `None` = nothing due.
fn claim_one(db: &Database, queue: Option<&str>, pid: i64) -> Result<Option<Claimed>, Failure> {
    let now = now_micros();
    let (sql, params): (&str, Vec<Value>) = match queue {
        None => (
            "UPDATE mq_task SET state = 'claimed', claimed_by = $1, claimed_at = $2, \
                                attempts = attempts + 1 \
             WHERE state = 'pending' \
               AND id = (SELECT id FROM mq_task \
                         WHERE state = 'pending' AND run_at <= $2 \
                         ORDER BY priority, id LIMIT 1) \
             RETURNING id, proc, payload, attempts, max_attempts",
            vec![Value::Int(pid), Value::Timestamp(now)],
        ),
        Some(q) => (
            "UPDATE mq_task SET state = 'claimed', claimed_by = $1, claimed_at = $2, \
                                attempts = attempts + 1 \
             WHERE state = 'pending' \
               AND id = (SELECT id FROM mq_task \
                         WHERE state = 'pending' AND run_at <= $2 AND queue = $3 \
                         ORDER BY priority, id LIMIT 1) \
             RETURNING id, proc, payload, attempts, max_attempts",
            vec![Value::Int(pid), Value::Timestamp(now), Value::Text(q.to_owned())],
        ),
    };
    let res = db.query(sql, &params)?;
    let ExecResult::Rows { rows, .. } = res else {
        return runtime(format!("claim expected RETURNING rows, got {res:?}"));
    };
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    match &row[..] {
        [Value::Int(id), Value::Text(proc), Value::Text(payload), Value::Int(attempts), Value::Int(max_attempts)] => {
            Ok(Some(Claimed {
                id: *id,
                proc: proc.clone(),
                payload: payload.clone(),
                attempts: *attempts,
                max_attempts: *max_attempts,
                claimed_at: now,
            }))
        }
        other => runtime(format!("claim RETURNING gave unexpected shape: {other:?}")),
    }
}

/// Guarded completion: only OUR claim (`claimed_by` + `claimed_at`) may flip
/// to done. Returns false when the lease was lost — the effect ran, the
/// completion is dropped LOUDLY (at-least-once, never a silent double-complete).
fn complete_task(db: &Database, t: &Claimed, pid: i64, result: &str) -> Result<bool, Failure> {
    let res = db.query(
        "UPDATE mq_task SET state = 'done', result = $2, error = NULL \
         WHERE id = $1 AND state = 'claimed' AND claimed_by = $3 AND claimed_at = $4",
        &[
            Value::Int(t.id),
            Value::Text(result.to_owned()),
            Value::Int(pid),
            Value::Timestamp(t.claimed_at),
        ],
    )?;
    Ok(matches!(res, ExecResult::Affected(1)))
}

/// A runtime failure: reschedule with exponential backoff while attempts
/// remain, else `failed`. Both statements carry the same lease guard as
/// completion. Returns what happened, for the runner's log line.
fn fail_task(
    db: &Database,
    t: &Claimed,
    pid: i64,
    err: &str,
    retry_delay_s: u64,
) -> Result<&'static str, Failure> {
    let guard = [
        Value::Int(t.id),
        Value::Int(pid),
        Value::Timestamp(t.claimed_at),
    ];
    if t.attempts < t.max_attempts {
        // Backoff: delay << (attempts-1). Base and shift are both clamped to
        // 2^20, so base<<shift ≤ 2^40 and the µs product stays far below i64.
        let shift = u32::try_from((t.attempts - 1).clamp(0, 20)).expect("clamped");
        let base = i64::try_from(retry_delay_s.min(1 << 20)).expect("clamped");
        let delay_us = (base << shift).saturating_mul(1_000_000);
        let res = db.query(
            "UPDATE mq_task SET state = 'pending', run_at = $4, error = $5, \
                                claimed_by = NULL, claimed_at = NULL \
             WHERE id = $1 AND state = 'claimed' AND claimed_by = $2 AND claimed_at = $3",
            &[
                guard[0].clone(),
                guard[1].clone(),
                guard[2].clone(),
                Value::Timestamp(now_micros().saturating_add(delay_us)),
                Value::Text(err.to_owned()),
            ],
        )?;
        if matches!(res, ExecResult::Affected(1)) {
            return Ok("retry");
        }
    } else {
        let res = db.query(
            "UPDATE mq_task SET state = 'failed', error = $4 \
             WHERE id = $1 AND state = 'claimed' AND claimed_by = $2 AND claimed_at = $3",
            &[
                guard[0].clone(),
                guard[1].clone(),
                guard[2].clone(),
                Value::Text(err.to_owned()),
            ],
        )?;
        if matches!(res, ExecResult::Affected(1)) {
            return Ok("failed");
        }
    }
    Ok("lease-lost")
}

/// Reclaim expired leases: `claimed` rows past `claimed_at + lease` go back to
/// `pending` (their previous error message is left in place), or to `dead`
/// when their attempts are exhausted — a crash loop that never completed.
/// Returns rows reclaimed + rows deadened.
pub(crate) fn reap_expired(db: &Database, lease_s: u64) -> Result<(u64, u64), Failure> {
    let cutoff = now_micros()
        - i64::try_from(lease_s.saturating_mul(1_000_000)).unwrap_or(i64::MAX);
    let reclaimed = match db.query(
        "UPDATE mq_task SET state = 'pending', claimed_by = NULL, claimed_at = NULL \
         WHERE state = 'claimed' AND claimed_at <= $1 AND attempts < max_attempts",
        &[Value::Timestamp(cutoff)],
    )? {
        ExecResult::Affected(n) => n,
        other => return runtime(format!("reap expected Affected, got {other:?}")),
    };
    let deadened = match db.query(
        "UPDATE mq_task SET state = 'dead', claimed_by = NULL, claimed_at = NULL, \
                            error = 'lease expired; max_attempts exhausted' \
         WHERE state = 'claimed' AND claimed_at <= $1 AND attempts >= max_attempts",
        &[Value::Timestamp(cutoff)],
    )? {
        ExecResult::Affected(n) => n,
        other => return runtime(format!("reap expected Affected, got {other:?}")),
    };
    Ok((reclaimed, deadened))
}

fn proc_value_str(v: &ProcValue) -> String {
    match v {
        ProcValue::Scalar(s) => value_str(s),
        other => other.to_string(),
    }
}

fn cmd_run(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["queue", "lease", "retry-delay", "limit"], &[])?;
    let [target] = &p.positional[..] else {
        return usage("queue run needs <target>");
    };
    let queue = p.value("queue");
    let lease_s = p.u64_or("lease", 60)?.max(1);
    let retry_delay_s = p.u64_or("retry-delay", 1)?;
    let limit = p.u64_or("limit", 0)?;
    let pid = i64::from(std::process::id());

    let db = crate::util::open_target(target)?;
    ensure_table(&db)?;
    let engine = ProcEngine::new(&db);

    let mut ran = 0u64;
    // Drain-and-exit: claim while due work exists; when the queue looks idle,
    // reap expired leases and re-check; exit (hibernate) only when both come
    // up empty. Future-scheduled work (backoff, --delay) is the next
    // invocation's business — this is the poll model, Model A.
    loop {
        if limit != 0 && ran >= limit {
            break;
        }
        let Some(task) = claim_one(&db, queue, pid)? else {
            let (reclaimed, deadened) = reap_expired(&db, lease_s)?;
            if deadened > 0 {
                eprintln!("reaped {deadened} dead task(s) (crash loop, attempts exhausted)");
            }
            if reclaimed > 0 {
                continue;
            }
            break;
        };
        // The claim is committed; run the proc (its body is one atomic write
        // txn of its own), then the guarded completion commit.
        match engine.call(&task.proc, &decode_payload(&task.payload)) {
            Ok(value) => {
                let result = proc_value_str(&value);
                if complete_task(&db, &task, pid, &result)? {
                    println!("task {} {} done: {result}", task.id, task.proc);
                } else {
                    eprintln!(
                        "task {} {}: lease lost before completion — the effect ran, \
                         the task was reclaimed (at-least-once); raise --lease above \
                         the longest task runtime",
                        task.id, task.proc
                    );
                }
            }
            Err(e) => {
                let outcome = fail_task(&db, &task, pid, &e.to_string(), retry_delay_s)?;
                eprintln!(
                    "task {} {} attempt {}/{} {outcome}: {e}",
                    task.id, task.proc, task.attempts, task.max_attempts
                );
            }
        }
        ran += 1;
    }
    println!("idle after {ran} task(s)");
    Ok(())
}

// ------------------------------------------------------------------- list

fn cmd_list(argv: &[String]) -> CliResult {
    let p = args::parse(argv, &["state"], &[])?;
    let [target] = &p.positional[..] else {
        return usage("queue list needs <target> [--state <s>]");
    };
    let db = crate::util::open_target(target)?;
    ensure_table(&db)?;
    let res = match p.value("state") {
        None => db.query("SELECT * FROM mq_task ORDER BY id", &[])?,
        Some(s) => db.query(
            "SELECT * FROM mq_task WHERE state = $1 ORDER BY id",
            &[Value::Text(s.to_owned())],
        )?,
    };
    crate::render::print_result(&res);
    Ok(())
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn payload_round_trips() {
        assert_eq!(encode_payload(&[]).unwrap(), "");
        assert_eq!(decode_payload(""), Vec::<Value>::new());

        let args = strs(&["42", "x", "", "null", "2026-07-16T12:00:00Z"]);
        let enc = encode_payload(&args).unwrap();
        assert_eq!(
            decode_payload(&enc),
            vec![
                Value::Int(42),
                Value::Text("x".into()),
                Value::Text(String::new()),
                Value::Null,
                Value::Timestamp(1_784_203_200_000_000),
            ]
        );
    }

    #[test]
    fn unrepresentable_payloads_are_rejected() {
        assert!(encode_payload(&strs(&["a\nb"])).is_err());
        assert!(encode_payload(&strs(&[""])).is_err());
        // …but an empty string between others survives (tested above).
    }
}
