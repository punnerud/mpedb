#!/usr/bin/env python3.12
"""The external system, with every property that makes syncing hard.

A sqlite database plus a deterministic mutator, wrapped in an API that is
awkward on purpose — because every awkwardness here is one somebody ships:

  * `updated_at` is set on INSERT and, depending on the knobs, not always on
    UPDATE (`lying_pct`, `insert_only_ts`). This is the single most common
    reason a sync silently loses rows.
  * deletes leave no trace: the row is simply gone, and the delta endpoint
    cannot report it.
  * a bulk backfill touches every row at once, so the next delta is the
    whole table.
  * activity is diurnal: quiet ticks and busy ticks.
  * `/probe` answers "did table X change since T" with imperfect precision
    and recall, exactly like a real lastmod signal.

Every read goes through `_charge`, so calls and bytes are counted BY THE
SYSTEM, not self-reported by the client under test. That is the instrument;
a client cannot flatter itself.

Determinism: a xorshift64* seeded per run. No `random`, no clock — a tick
counter is the only time there is, so two runs with the same seed and the
same client produce byte-identical numbers.
"""

import json
import sqlite3

CASE_SUBJECTS = [
    "printer jam", "vpn down", "badge lost", "laptop swap", "door sensor",
    "mail loop", "disk full", "cert expiry", "phone port", "seat move",
]


class Rng:
    """xorshift64*, so a seed reproduces a run exactly."""

    def __init__(self, seed):
        self.s = seed & 0xFFFFFFFFFFFFFFFF or 0x9E3779B97F4A7C15

    def next(self):
        x = self.s
        x ^= (x >> 12) & 0xFFFFFFFFFFFFFFFF
        x ^= (x << 25) & 0xFFFFFFFFFFFFFFFF
        x ^= (x >> 27) & 0xFFFFFFFFFFFFFFFF
        self.s = x & 0xFFFFFFFFFFFFFFFF
        return (self.s * 0x2545F4914F6CDD1D) & 0xFFFFFFFFFFFFFFFF

    def below(self, n):
        return self.next() % n if n > 0 else 0

    def pct(self, p):
        """True with probability p percent."""
        return self.below(100) < p

    def pick(self, seq):
        return seq[self.below(len(seq))]


DEFAULTS = {
    # How the timestamp lies. 0 = an honest API; 100 = updated_at is set on
    # insert and never touched again.
    "lying_pct": 25,
    # Chance per tick that a given row is updated / deleted / a new row lands.
    "update_pct": 12,
    "delete_pct": 3,
    "insert_per_tick": 2,
    # Every Nth tick touches every row (a migration, a re-index, a script).
    "bulk_every": 40,
    # Diurnal: ticks where (tick % 24) is outside work_hours get a fraction
    # of the activity.
    "work_hours": (6, 18),
    "quiet_divisor": 6,
    # The probe endpoint's honesty, in percent.
    "probe_recall": 80,
    "probe_precision": 50,
    # Wire cost, so bytes mean something without a real socket.
    "bytes_per_row": 120,
    "bytes_per_call": 200,
}


class System:
    """The external system under sync. `tick()` advances its world."""

    def __init__(self, path=":memory:", seed=1, initial=20, **knobs):
        self.k = dict(DEFAULTS)
        self.k.update(knobs)
        self.rng = Rng(seed)
        self.now = 0                 # the only clock: a tick counter
        self.calls = 0
        self.bytes = 0
        self.by_endpoint = {}
        self.next_case = 1
        self.next_contract = 1000
        self.changed_tables = set()  # what /probe has to answer about
        # The HTTP facade serves from one thread and ticks the world from
        # another, both under its own lock; sqlite's default thread check
        # would refuse that even though the access is serialised.
        self.db = sqlite3.connect(path, check_same_thread=False)
        self.db.executescript(
            """
            CREATE TABLE cases (
                id INTEGER PRIMARY KEY, subject TEXT, state TEXT,
                updated_at INTEGER);
            CREATE TABLE contracts (
                id INTEGER PRIMARY KEY, case_id INTEGER, amount INTEGER,
                updated_at INTEGER);
            """
        )
        for _ in range(initial):
            self._insert_case()
        self.changed_tables.clear()

    # ------------------------------------------------------------ the world

    def _insert_case(self):
        cid = self.next_case
        self.next_case += 1
        self.db.execute(
            "INSERT INTO cases (id, subject, state, updated_at) VALUES (?,?,?,?)",
            (cid, self.rng.pick(CASE_SUBJECTS), "open", self.now),
        )
        # Every case gets one or two contracts — the fan-out the cascade has
        # to discover, since nothing declares it.
        for _ in range(1 + self.rng.below(2)):
            k = self.next_contract
            self.next_contract += 1
            self.db.execute(
                "INSERT INTO contracts (id, case_id, amount, updated_at) VALUES (?,?,?,?)",
                (k, cid, 100 + self.rng.below(9900), self.now),
            )
        self.changed_tables.update(("cases", "contracts"))
        return cid

    def _touch(self, cid):
        """Update a case — and decide whether the timestamp tells the truth."""
        honest = not self.rng.pct(self.k["lying_pct"])
        self.db.execute(
            "UPDATE cases SET subject = ?, state = ?, updated_at = ? WHERE id = ?",
            (
                self.rng.pick(CASE_SUBJECTS) + "!",
                self.rng.pick(["open", "waiting", "closed"]),
                self.now if honest else
                self.db.execute("SELECT updated_at FROM cases WHERE id = ?", (cid,))
                    .fetchone()[0],
                cid,
            ),
        )
        self.changed_tables.add("cases")

    def tick(self, n=1):
        """Advance the world by n ticks. Returns how many rows it changed."""
        touched = 0
        for _ in range(n):
            self.now += 1
            busy = self.k["work_hours"][0] <= (self.now % 24) < self.k["work_hours"][1]
            div = 1 if busy else self.k["quiet_divisor"]
            ids = [r[0] for r in self.db.execute("SELECT id FROM cases")]
            bulk = self.k["bulk_every"] and self.now % self.k["bulk_every"] == 0
            for cid in ids:
                if bulk:
                    self._touch(cid)
                    touched += 1
                    continue
                if self.rng.pct(max(1, self.k["update_pct"] // div)):
                    self._touch(cid)
                    touched += 1
                elif self.rng.pct(max(0, self.k["delete_pct"] // div)):
                    self.db.execute("DELETE FROM cases WHERE id = ?", (cid,))
                    self.db.execute("DELETE FROM contracts WHERE case_id = ?", (cid,))
                    self.changed_tables.update(("cases", "contracts"))
                    touched += 1
            for _ in range(max(1, self.k["insert_per_tick"]) // div if not busy
                           else self.k["insert_per_tick"]):
                self._insert_case()
                touched += 1
        self.db.commit()
        return touched

    # -------------------------------------------------------------- the API
    #
    # Everything below is what the client is allowed to see. Each entry
    # charges calls and bytes before it returns.

    def _charge(self, endpoint, rows):
        self.calls += 1
        n = self.k["bytes_per_call"] + rows * self.k["bytes_per_row"]
        self.bytes += n
        c = self.by_endpoint.setdefault(endpoint, [0, 0])
        c[0] += 1
        c[1] += n

    def _rows(self, sql, params, endpoint):
        cur = self.db.execute(sql, params)
        cols = [d[0] for d in cur.description]
        out = [dict(zip(cols, r)) for r in cur.fetchall()]
        self._charge(endpoint, len(out))
        return out

    def cases_since(self, since, limit=200):
        """The delta endpoint. It can only answer with what it believes."""
        return self._rows(
            "SELECT id, subject, state, updated_at FROM cases WHERE updated_at >= ? "
            "ORDER BY updated_at, id LIMIT ?",
            (since, limit),
            "cases_since",
        )

    def cases_page(self, page=0, size=100):
        """The full dump, paged. The only place a delete becomes visible."""
        return self._rows(
            "SELECT id, subject, state, updated_at FROM cases "
            "ORDER BY id LIMIT ? OFFSET ?",
            (size, page * size),
            "cases_page",
        )

    def case(self, cid):
        return self._rows(
            "SELECT id, subject, state, updated_at FROM cases WHERE id = ?",
            (cid,),
            "case",
        )

    def contracts_for(self, case_ids):
        """The derived call: parameters come from another call's result."""
        ids = list(case_ids)
        marks = ",".join("?" * len(ids))
        return self._rows(
            f"SELECT id, case_id, amount, updated_at FROM contracts "
            f"WHERE case_id IN ({marks}) ORDER BY id",
            ids,
            "contracts_for",
        )

    def contracts_page(self, page=0, size=200):
        return self._rows(
            "SELECT id, case_id, amount, updated_at FROM contracts "
            "ORDER BY id LIMIT ? OFFSET ?",
            (size, page * size),
            "contracts_page",
        )

    def probe(self):
        """"Did anything change?" — with the recall and precision of a real
        lastmod signal, which is to say: not 100 %."""
        self._charge("probe", 0)
        out = {}
        for t in ("cases", "contracts"):
            really = t in self.changed_tables
            if really:
                out[t] = self.rng.pct(self.k["probe_recall"])
            else:
                out[t] = not self.rng.pct(self.k["probe_precision"])
        self.changed_tables.clear()
        return out

    def write_case(self, cid, subject=None, state=None):
        """The writeback door, for the two-way half. Returns the new row."""
        row = self.db.execute(
            "SELECT id, subject, state, updated_at FROM cases WHERE id = ?", (cid,)
        ).fetchone()
        if row is None:
            self._charge("write_case", 0)
            return None
        self.db.execute(
            "UPDATE cases SET subject = ?, state = ?, updated_at = ? WHERE id = ?",
            (subject or row[1], state or row[2], self.now, cid),
        )
        self.db.commit()
        self.changed_tables.add("cases")
        self._charge("write_case", 1)
        return {"id": cid, "subject": subject or row[1],
                "state": state or row[2], "updated_at": self.now}

    # ------------------------------------------------------------- the truth
    #
    # Not part of the API: the oracle the bench compares against. A client
    # that calls this is cheating, and the bench says so.

    def truth(self, table="cases"):
        cur = self.db.execute(f"SELECT * FROM {table} ORDER BY id")
        cols = [d[0] for d in cur.description]
        return {r[0]: dict(zip(cols, r)) for r in cur.fetchall()}

    def cost(self):
        return {"calls": self.calls, "bytes": self.bytes,
                "by_endpoint": {k: {"calls": v[0], "bytes": v[1]}
                                for k, v in sorted(self.by_endpoint.items())}}


if __name__ == "__main__":
    s = System(seed=7)
    s.tick(30)
    print(json.dumps({"cases": len(s.truth()), "cost": s.cost()}, indent=2))
