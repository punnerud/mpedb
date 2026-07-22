// mpedb playground UI.
//
// Rule for everything below: render what the engine returned, and nothing
// else. Errors are shown verbatim, results are shown with the engine's own
// types, and no panel is filled in from a guess when the engine did not answer.

import { Mpedb } from "./mpedb.js";

const $ = (id) => document.getElementById(id);

// ---------------------------------------------------------------------------
// Examples
// ---------------------------------------------------------------------------
//
// The list is NOT defined here. It comes from the wasm module
// (`crates/mpedb-wasm/src/examples.rs`), which is the same list
// `tests/examples.rs` runs against a native database on every `cargo test`.
// One source of truth means a button cannot advertise a refusal the engine
// has quietly started accepting.

let EXAMPLES = [];

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

let db = null;
let lastResult = null;
let activeTab = "rows";

async function boot() {
  wireTheme();

  try {
    db = await Mpedb.load("./mpedb.wasm");
  } catch (e) {
    $("loading").innerHTML =
      `<b>The engine could not be loaded.</b><br><span class="mono">${esc(String(e))}</span>` +
      `<br><br>This page needs WebAssembly and must be served over http(s) — ` +
      `opening index.html straight from disk will not work in most browsers.`;
    return;
  }

  const v = db.version();
  $("build").textContent = `mpedb ${v.version} · plan format ${v.plan_format}`;

  EXAMPLES = db.examples().groups;
  buildExamples();

  const open = openDemo();
  if (!open) return;

  $("loading").hidden = true;
  $("run").disabled = false;
  $("run").addEventListener("click", runCurrent);
  $("reset").addEventListener("click", () => {
    if (openDemo()) setStatus("Database reset — 500 rows, freshly created.");
  });
  // The example catalogue is a sidebar on wide screens and sits BELOW the
  // editor on narrow ones, so "where are the examples" has two different
  // answers. This button gives one: open every group and scroll the list into
  // view, wherever it happens to be.
  $("toexamples").addEventListener("click", () => {
    const host = $("examples");
    for (const d of host.querySelectorAll("details.exgroup")) d.setAttribute("open", "");
    host.scrollIntoView({ behavior: "smooth", block: "start" });
  });
  $("sql").addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); runCurrent(); }
  });
  $("showseed").addEventListener("click", () => {
    const pre = $("seedsql");
    pre.hidden = !pre.hidden;
    $("showseed").textContent = pre.hidden ? "Show the full seed script" : "Hide the seed script";
  });

  runCurrent();
}

function openDemo() {
  const t0 = performance.now();
  const res = db.open();
  const ms = performance.now() - t0;
  if (!res.ok) {
    $("loading").innerHTML =
      `<b>The demo database could not be created.</b><br><span class="mono">${esc(res.error)}</span>`;
    return null;
  }
  renderSchema(res.tables);
  $("seedsql").textContent = res.seed_sql;
  $("dbstate").textContent = `demo db built in ${ms.toFixed(0)} ms`;
  return res;
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

function runCurrent() {
  const sql = $("sql").value;
  if (!sql.trim()) return;
  let res, ms;
  try {
    const t0 = performance.now();
    res = db.run(sql);
    ms = performance.now() - t0;
  } catch (e) {
    // A trap from the module is a bug, not a SQL error — say which.
    $("refused").innerHTML =
      `<div class="refused"><div class="label">engine fault</div>` +
      `<div class="msg">${esc(String(e))}</div>` +
      `<div class="note">This is a fault in the WebAssembly module itself, not a rejected ` +
      `statement. Reset the database to continue.</div></div>`;
    $("out").hidden = true;
    return;
  }
  lastResult = res;
  render(res, ms);
}

function render(res, ms) {
  $("refused").innerHTML = "";

  if (!res.ok) {
    const stage = res.stage === "execute" ? "refused at execution" : "refused at compile time";
    const note =
      res.stage === "execute"
        ? "The statement compiled — a plan exists — but the engine refused to " +
          "perform it. This is the message the native engine gives, unedited."
        : "The statement never became a plan. mpedb binds against a rigid schema, " +
          "so this was caught before a single row was touched.";
    $("refused").innerHTML =
      `<div class="refused"><div class="label">${esc(stage)}</div>` +
      `<div class="msg">${esc(res.error)}</div>` +
      `<div class="note">${note}</div></div>`;
    setStatus(`Refused in ${ms.toFixed(2)} ms.`);
    // A compile failure has no plan; an execute failure's plan is not
    // reported by this path either. Show nothing rather than something stale.
    $("out").hidden = true;
    return;
  }

  setStatus(
    `Ran in <span class="timing">${ms.toFixed(2)} ms</span> ` +
      `<span class="hint">(wall clock around the wasm call, measured by the page)</span>`
  );

  const tabs = [];
  tabs.push({ id: "rows", label: resultTabLabel(res.result) });
  if (res.no_plan) tabs.push({ id: "noplan", label: "Plan" });
  if (res.explain) tabs.push({ id: "explain", label: "EXPLAIN" });
  if (res.footprint) tabs.push({ id: "footprint", label: "Footprint" });
  if (res.mpee && res.mpee.applies) {
    tabs.push({ id: "mpee", label: "MPEE", n: res.mpee.reordered ? "reordered" : "unchanged" });
  }
  if (res.plan_hash) tabs.push({ id: "plan", label: "Plan hash" });

  if (!tabs.some((t) => t.id === activeTab)) activeTab = "rows";

  $("tabs").innerHTML = tabs
    .map(
      (t) =>
        `<button class="tab" role="tab" data-tab="${t.id}" aria-selected="${t.id === activeTab}">` +
        `${esc(t.label)}${t.n ? ` <span class="n">${esc(t.n)}</span>` : ""}</button>`
    )
    .join("");
  for (const b of $("tabs").querySelectorAll("button")) {
    b.addEventListener("click", () => { activeTab = b.dataset.tab; render(lastResult, ms); });
  }

  $("panes").innerHTML = `<div class="tabpane">${paneHtml(activeTab, res)}</div>`;
  $("out").hidden = false;
}

function resultTabLabel(r) {
  if (!r) return "Result";
  if (r.kind === "rows") return `Rows (${r.rows.length})`;
  if (r.kind === "affected") return `Affected (${r.n})`;
  return "Result";
}

function paneHtml(tab, res) {
  switch (tab) {
    case "rows": return rowsHtml(res.result);
    case "noplan":
      return (
        `<p class="explainer">This statement has <strong>no compiled plan</strong>: ` +
        `${esc(res.no_plan)}. DDL changes the catalog rather than being compiled into ` +
        `a content-hashed plan, so there is no hash, no footprint and no join order to ` +
        `show — and the page shows nothing rather than inventing them.</p>`
      );
    case "explain":
      return (
        `<p class="explainer">The engine's own <code>EXPLAIN</code> rendering of the plan ` +
        `that just ran — access path per table, the join order, and the footprint line.</p>` +
        `<pre class="plan">${esc(res.explain)}</pre>`
      );
    case "footprint": return footprintHtml(res.footprint);
    case "mpee": return mpeeHtml(res.mpee);
    case "plan": return planHtml(res);
    default: return "";
  }
}

function rowsHtml(r) {
  if (!r) return `<p class="explainer">No result.</p>`;
  if (r.kind === "affected") {
    return (
      `<p class="explainer">The statement wrote to the database. ` +
      `Reads in this tab see it immediately; a <strong>Reset database</strong> ` +
      `puts everything back.</p>` +
      `<p class="mono" style="font-size:15px"><b>${r.n}</b> row${r.n === 1 ? "" : "s"} affected.</p>`
    );
  }
  if (r.kind === "explain") return `<pre class="plan">${esc(r.text)}</pre>`;
  if (!r.rows.length) {
    return (
      `<p class="explainer">The query ran and matched <strong>no rows</strong>. ` +
      `Columns: ${r.columns.map((c) => `<code>${esc(c)}</code>`).join(", ")}.</p>`
    );
  }
  // Column type is taken from the first row's actual value tag — the engine's
  // answer, not a declared type.
  const tys = r.columns.map((_, i) => r.rows[0][i]?.t ?? "");
  const head = r.columns
    .map((c, i) => `<th>${esc(c)}<span class="ty">${esc(tys[i])}</span></th>`)
    .join("");
  const MAX = 300;
  const body = r.rows
    .slice(0, MAX)
    .map((row) => `<tr>${row.map(cellHtml).join("")}</tr>`)
    .join("");
  const more =
    r.rows.length > MAX
      ? `<p class="rowcount">Showing the first ${MAX} of ${r.rows.length} rows — the engine returned all of them.</p>`
      : `<p class="rowcount">${r.rows.length} row${r.rows.length === 1 ? "" : "s"}.</p>`;
  return `<div class="tablewrap"><table class="rows"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>${more}`;
}

function cellHtml(v) {
  if (!v || v.t === "null") return `<td class="t-null">NULL</td>`;
  if (v.t === "bool") return `<td class="t-bool">${v.v ? "true" : "false"}</td>`;
  if (v.t === "blob") return `<td class="t-blob">x'${esc(v.v)}'</td>`;
  if (v.t === "list") return `<td>${esc(JSON.stringify(v.v))}</td>`;
  return `<td class="t-${esc(v.t)}">${esc(String(v.v))}</td>`;
}

function footprintHtml(f) {
  const list = (xs) =>
    xs.length ? xs.map((x) => `<code>${esc(x)}</code>`).join(", ") : `<span class="t-null">none</span>`;
  return (
    `<p class="explainer">A plan carries a <strong>precomputed footprint</strong>: which tables it ` +
    `reads, which it writes, which index trees it touches, and how it reaches keys. ` +
    `The engine knows all of this <em>before</em> the statement runs — "pre-computed locks", ` +
    `Calvin-style — which is what lets it decide whether two statements can conflict without ` +
    `executing either.</p>` +
    `<dl class="kv">` +
    `<dt>read_only</dt><dd><span class="badge ${f.read_only ? "ro" : ""}">${f.read_only}</span></dd>` +
    `<dt>tables_read</dt><dd>${list(f.tables_read)}</dd>` +
    `<dt>tables_written</dt><dd>${list(f.tables_written)}</dd>` +
    `<dt>indexes_used</dt><dd>${esc(f.indexes_used)} <span class="hint">(bitmap; bit 0 = the PK tree)</span></dd>` +
    `<dt>key_access</dt><dd>${esc(f.key_access)}${keyNote(f.key_access)}</dd>` +
    `</dl>`
  );
}

function keyNote(k) {
  const notes = {
    Point: " — one exact key; two Point footprints on different keys never conflict",
    Range: " — a bounded key range",
    Full: " — the whole table is in scope",
  };
  return notes[k] ? `<span class="hint">${notes[k]}</span>` : "";
}

function mpeeHtml(m) {
  const side = (title, line, isChosen) =>
    `<div class="side${isChosen ? " chosen" : ""}"><h4>${title}</h4>` +
    `<pre class="plan">${esc(line || "(no join chain)")}</pre></div>`;
  const verdict = m.reordered
    ? `<p class="verdict changed">The solver chose a different order than the one written. ` +
      `The plan hashes differ, because the order is part of the plan.</p>`
    : `<p class="verdict">The solver evaluated alternatives and found nothing strictly better, ` +
      `so the statement keeps the order you wrote.</p>`;
  return (
    `<p class="explainer">mpedb compiles the join chain twice here — once with the ` +
    `<strong>MPEE</strong> solver enabled and once with it disabled — and shows both. ` +
    `This is the same A/B the repo runs natively via <code>MPEDB_NO_MPEE=1</code>, ` +
    `driven from a runtime switch because a browser has no environment variables. ` +
    `A <em>cartesian step</em> is a join with no usable predicate between what has ` +
    `been gathered so far and the next table — the thing the solver works hardest ` +
    `to avoid.</p>` +
    `<div class="ab">${side("Chosen (solver on)", m.chosen, true)}${side("As written (solver off)", m.textual, false)}</div>` +
    verdict
  );
}

function planHtml(res) {
  return (
    `<p class="explainer">SQL is compiled <strong>once</strong> into a plan with a blake3 ` +
    `content hash; the hot path is <code>execute(hash, params)</code> with zero parsing. ` +
    `Two statements that differ only in whitespace, keyword case, or <code>?</code> versus ` +
    `<code>$n</code> spelling compile to <em>identical bytes</em> and the same hash — try the ` +
    `"same plan, different spelling" example. Identifiers and literals are ` +
    `<em>not</em> normalised: they are case- and value-sensitive, so ` +
    `<code>country</code> and <code>COUNTRY</code> are different plans.</p>` +
    `<dl class="kv">` +
    `<dt>plan hash</dt><dd>${esc(res.plan_hash)}</dd>` +
    `<dt>plan bytes</dt><dd>${esc(String(res.plan_bytes))}</dd>` +
    `</dl>`
  );
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

function buildExamples() {
  const host = $("examples");
  // Each group is a <details>: the first is open so the page lands on something
  // runnable, the rest are collapsed so the whole list is scannable at a glance
  // rather than a column the visitor has to scroll past.
  host.innerHTML = EXAMPLES.map(
    (g, gi) =>
      `<details class="exgroup"${gi === 0 ? " open" : ""}>` +
      `<summary>${esc(g.name)}<span class="count">${g.items.length}</span></summary>` +
      g.items
        .map(
          (it, i) =>
            `<button class="ex${it.refuses ? " refusal" : ""}" type="button" ` +
            `data-g="${gi}" data-i="${i}">${esc(it.label)}` +
            `<span class="why">${esc(it.why)}</span></button>`
        )
        .join("") +
      `</details>`
  ).join("");

  for (const b of host.querySelectorAll("button.ex")) {
    b.addEventListener("click", () => {
      const it = EXAMPLES[Number(b.dataset.g)].items[Number(b.dataset.i)];
      b.closest("details.exgroup")?.setAttribute("open", "");
      $("sql").value = it.sql;
      for (const o of host.querySelectorAll("button.ex")) o.removeAttribute("aria-current");
      b.setAttribute("aria-current", "true");
      activeTab = "rows";
      runCurrent();
      // On narrow screens the editor sits ABOVE the list (see the <=900px
      // media query), so a click down in the catalogue needs to bring the
      // editor and its results back into view.
      if (window.matchMedia("(max-width: 900px)").matches) {
        $("sql").scrollIntoView({ behavior: "smooth", block: "start" });
      }
    });
  }
}

function renderSchema(tables) {
  $("schema").innerHTML = tables
    .map(
      (t) =>
        `<div class="tbl"><h4>${esc(t.name)}</h4><ul>` +
        t.columns
          .map(
            (c) =>
              `<li><b>${esc(c.name)}</b> ${esc(c.type.toLowerCase())}` +
              `${c.pk ? ' <span class="pk">PK</span>' : ""}` +
              `${c.nullable ? "" : " NOT NULL"}` +
              `${c.check ? ` <span class="ck">CHECK</span>` : ""}</li>`
          )
          .join("") +
        `</ul></div>`
    )
    .join("");
}

function setStatus(html) { $("status").innerHTML = html; }

function wireTheme() {
  const btn = $("theme");
  const stored = localStorage.getItem("mpedb-theme");
  if (stored) document.documentElement.setAttribute("data-theme", stored);
  btn.addEventListener("click", () => {
    const cur =
      document.documentElement.getAttribute("data-theme") ||
      (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
    const next = cur === "dark" ? "light" : "dark";
    document.documentElement.setAttribute("data-theme", next);
    localStorage.setItem("mpedb-theme", next);
  });
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c])
  );
}

boot();
