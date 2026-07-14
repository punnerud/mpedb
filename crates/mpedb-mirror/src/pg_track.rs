//! PostgreSQL tracked-mode change capture (DESIGN-MIRROR §5.2): one shared
//! `mpedb_mirror.changelog` table plus per-table AFTER ROW + AFTER TRUNCATE
//! triggers. Triggers are `ENABLE ALWAYS` (fire even under
//! `session_replication_role='replica'`), record the PK as a jsonb array, and
//! stamp `origin` from the `mpedb.mirror_origin` GUC (residue-safe echo
//! suppression, NULL for genuine source writes). `xid` is `pg_current_xact_id()`
//! so the pull can use consecutive-snapshot windows (avoids the torn-read of a
//! naive `txid < xmin` window — review CONF#30).

use mpedb_types::{Error, Result};
use postgres::Client;

use crate::pg::PgTable;

pub const OP_UPSERT: i16 = 1;
pub const OP_TOMBSTONE: i16 = 2;
pub const OP_TRUNCATE: i16 = 3;

fn q(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Create the shared changelog schema/table (idempotent).
pub fn install_changelog(client: &mut Client) -> Result<()> {
    client
        .batch_execute(
            "CREATE SCHEMA IF NOT EXISTS mpedb_mirror;
             CREATE TABLE IF NOT EXISTS mpedb_mirror.changelog (
                 seq    bigserial PRIMARY KEY,
                 tbl    text     NOT NULL,
                 op     smallint NOT NULL,
                 pk     jsonb,
                 xid    xid8     NOT NULL DEFAULT pg_current_xact_id(),
                 origin text,
                 at     timestamptz NOT NULL DEFAULT clock_timestamp()
             );",
        )
        .map_err(|e| Error::Config(format!("install changelog: {e}")))
}

/// Install the capture triggers for one mirrored table (idempotent).
pub fn install_triggers(client: &mut Client, src: &PgTable) -> Result<()> {
    install_changelog(client)?;
    let name = &src.name;
    let pk_new = pk_array(src, "NEW");
    let pk_old = pk_array(src, "OLD");
    let cap = format!("cap_{name}");
    let captrunc = format!("captrunc_{name}");

    let ddl = format!(
        r#"
CREATE OR REPLACE FUNCTION mpedb_mirror.{cap_q}() RETURNS trigger LANGUAGE plpgsql AS $BODY$
BEGIN
  IF TG_OP = 'INSERT' THEN
    INSERT INTO mpedb_mirror.changelog(tbl, op, pk, origin)
      VALUES ('{name_lit}', {UPSERT}, {pk_new}, current_setting('mpedb.mirror_origin', true));
  ELSIF TG_OP = 'UPDATE' THEN
    IF {pk_old} IS DISTINCT FROM {pk_new} THEN
      INSERT INTO mpedb_mirror.changelog(tbl, op, pk, origin)
        VALUES ('{name_lit}', {TOMB}, {pk_old}, current_setting('mpedb.mirror_origin', true));
    END IF;
    INSERT INTO mpedb_mirror.changelog(tbl, op, pk, origin)
      VALUES ('{name_lit}', {UPSERT}, {pk_new}, current_setting('mpedb.mirror_origin', true));
  ELSE
    INSERT INTO mpedb_mirror.changelog(tbl, op, pk, origin)
      VALUES ('{name_lit}', {TOMB}, {pk_old}, current_setting('mpedb.mirror_origin', true));
  END IF;
  RETURN NULL;
END;
$BODY$;
DROP TRIGGER IF EXISTS {cap_q} ON {tbl};
CREATE TRIGGER {cap_q} AFTER INSERT OR UPDATE OR DELETE ON {tbl}
  FOR EACH ROW EXECUTE FUNCTION mpedb_mirror.{cap_q}();
ALTER TABLE {tbl} ENABLE ALWAYS TRIGGER {cap_q};

CREATE OR REPLACE FUNCTION mpedb_mirror.{trunc_q}() RETURNS trigger LANGUAGE plpgsql AS $BODY$
BEGIN
  INSERT INTO mpedb_mirror.changelog(tbl, op, pk, origin)
    VALUES ('{name_lit}', {TRUNC}, NULL, current_setting('mpedb.mirror_origin', true));
  RETURN NULL;
END;
$BODY$;
DROP TRIGGER IF EXISTS {trunc_q} ON {tbl};
CREATE TRIGGER {trunc_q} AFTER TRUNCATE ON {tbl}
  FOR EACH STATEMENT EXECUTE FUNCTION mpedb_mirror.{trunc_q}();
ALTER TABLE {tbl} ENABLE ALWAYS TRIGGER {trunc_q};
"#,
        cap_q = q(&cap),
        trunc_q = q(&captrunc),
        tbl = format!("\"public\".{}", q(name)),
        name_lit = name.replace('\'', "''"),
        UPSERT = OP_UPSERT,
        TOMB = OP_TOMBSTONE,
        TRUNC = OP_TRUNCATE,
    );
    client
        .batch_execute(&ddl)
        .map_err(|e| Error::Config(format!("install triggers for `{name}`: {e}")))
}

/// `jsonb_build_array(REC."pk0", REC."pk1", …)` for the PK columns.
fn pk_array(src: &PgTable, rec: &str) -> String {
    let elems = src
        .pk
        .iter()
        .map(|&i| format!("{rec}.{}", q(&src.columns[i].name)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("jsonb_build_array({elems})")
}

/// The current changelog head (max seq), 0 if empty — a lag probe.
pub fn log_head(client: &mut Client) -> Result<u64> {
    client
        .query_one("SELECT COALESCE(MAX(seq), 0) FROM mpedb_mirror.changelog", &[])
        .map(|r| r.get::<_, i64>(0) as u64)
        .map_err(|e| Error::Config(format!("changelog head: {e}")))
}
