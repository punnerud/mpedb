// mpedb playground UI.
//
// Rule for everything below: render what the engine returned, and nothing
// else. Errors are shown verbatim, results are shown with the engine's own
// types, and no panel is filled in from a guess when the engine did not answer.

import { Mpedb } from "./mpedb.js";
import { parseCsv, planTable, toSql, examplesFor } from "./csv.js";

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
// The engine's own list, kept so a reset can put it back after a CSV import
// has replaced it with queries over the visitor's columns.
let DEMO_EXAMPLES = [];

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

let db = null;
let lastResult = null;
let activeTab = "rows";
let TABLES = [];

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

  DEMO_EXAMPLES = db.examples().groups;
  EXAMPLES = DEMO_EXAMPLES;
  buildExamples();

  const open = openDemo();
  if (!open) return;

  $("loading").hidden = true;
  $("run").disabled = false;
  $("run").addEventListener("click", runCurrent);
  $("reset").addEventListener("click", () => {
    if (openDemo()) setStatus("Database reset — 500 rows, freshly created.");
    // A reset destroys imported tables, so the examples that queried them
    // would be buttons pointing at nothing. Put the engine's own list back.
    if (EXAMPLES !== DEMO_EXAMPLES) { EXAMPLES = DEMO_EXAMPLES; buildExamples(); }
    if (mode === "data") buildData();
  });
  wireCsv();
  // The catalogue is a sidebar on wide screens and sits below the editor on
  // narrow ones, where scrolling to it means losing the editor off the top.
  // The button opens the same list as a small modal that scrolls inside itself
  // instead: never taller than 70% of the viewport, and Escape, the backdrop
  // or the ✕ all close it.
  $("toexamples").addEventListener("click", () => $("exdlg").showModal());
  $("exdlgx").addEventListener("click", () => $("exdlg").close());
  // A <dialog> backdrop is not a child, so a click that lands on the element
  // itself (rather than on its content) came from outside the box.
  $("exdlg").addEventListener("click", (e) => {
    if (e.target === $("exdlg")) $("exdlg").close();
  });
  // SQL and Data are two views of the same panel. Data exists because the demo
  // is only interesting if you can see what you are querying — the schema panel
  // lists columns, this shows the actual rows in every table.
  $("tosql").addEventListener("click", () => setMode("sql"));
  $("todata").addEventListener("click", () => setMode("data"));
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
  TABLES = res.tables.map((t) => t.name);
  renderSchema(res.tables);
  $("seedsql").textContent = res.seed_sql;
  $("dbstate").textContent = `demo db built in ${ms.toFixed(0)} ms`;
  return res;
}

// ---------------------------------------------------------------------------
// CSV import
// ---------------------------------------------------------------------------

function wireCsv() {
  $("loadcsv").addEventListener("click", () => $("csvfile").click());
  $("csvfile").addEventListener("change", (e) => {
    importFiles(e.target.files);
    e.target.value = "";  // so the same file can be picked twice
  });

  // `dragover` fires repeatedly while a drag is over the window — the spec runs
  // the model at least every 350 ms — so a heartbeat is a more reliable signal
  // than pairing dragenter with dragleave, which flickers across child nodes
  // and is simply never delivered when the drag ends outside the window. Show
  // on the beat, hide when it stops: the strip cannot get stuck.
  let beat = 0;
  const isFiles = (e) => Array.from(e.dataTransfer?.types || []).includes("Files");
  const hide = () => { clearTimeout(beat); beat = 0; $("dropzone").hidden = true; };
  window.addEventListener("dragover", (e) => {
    if (!isFiles(e)) return;
    e.preventDefault();  // without this the browser navigates to the file
    $("dropzone").hidden = false;
    clearTimeout(beat);
    beat = setTimeout(hide, 700);
  });
  window.addEventListener("dragend", hide);
  window.addEventListener("drop", (e) => {
    if (!isFiles(e)) return;
    e.preventDefault();
    hide();
    importFiles(e.dataTransfer.files);
  });
}

async function importFiles(fileList) {
  const files = Array.from(fileList || []);
  if (!files.length) return;

  const made = [];
  const failed = [];
  const t0 = performance.now();

  for (const f of files) {
    let plan;
    try {
      plan = planTable(f.name, parseCsv(await f.text()));
    } catch (e) {
      failed.push(`${f.name}: ${String(e.message || e)}`);
      continue;
    }
    // The engine decides. Every statement's error is shown as the engine
    // wrote it — an import that half-succeeds says so rather than pretending.
    let bad = null;
    for (const sql of toSql(plan)) {
      const res = db.run(sql);
      if (!res.ok) { bad = `${res.error} — while running: ${sql.slice(0, 120)}`; break; }
    }
    if (bad) failed.push(`${f.name}: ${bad}`);
    else made.push(plan);
  }

  const ms = performance.now() - t0;

  if (made.length) {
    // Imported data replaces the catalogue: the demo's examples are about the
    // demo's tables, and leaving them would hand the visitor buttons that
    // query rows they did not bring.
    EXAMPLES = made.map(examplesFor);
    buildExamples();
    refreshSchema();
    $("sql").value = `SELECT * FROM ${made[0].table} LIMIT 100`;
    setMode("data");
  }

  const parts = made.map(
    (p) =>
      `<code>${esc(p.table)}</code> — ${p.rows.length} row${p.rows.length === 1 ? "" : "s"}, ` +
      `${p.columns.length} column${p.columns.length === 1 ? "" : "s"}` +
      `${p.truncated ? ` (first ${p.rows.length} of ${p.truncated})` : ""}` +
      `${p.hasHeader ? "" : ", no header row — columns named c1…"}`
  );
  setStatus(
    (made.length
      ? `Imported in <span class="timing">${ms.toFixed(0)} ms</span>: ${parts.join(" · ")}. ` +
        `The examples now query your tables; <strong>Reset database</strong> brings the demo back.`
      : "") +
      (failed.length
        ? `${made.length ? "<br>" : ""}<span class="t-null">Not imported:</span> ` +
          failed.map((f) => esc(f)).join("<br>")
        : "")
  );
}

// The schema panel must show what the engine holds, not what the importer
// believes it created — so ask the engine.
function refreshSchema() {
  const res = db.schema();
  if (!res.ok) return;
  TABLES = res.tables.map((t) => t.name);
  renderSchema(res.tables);
}

// ---------------------------------------------------------------------------
// SQL / Data
// ---------------------------------------------------------------------------

let mode = "sql";

function setMode(next) {
  mode = next;
  const data = next === "data";
  $("tosql").setAttribute("aria-selected", String(!data));
  $("todata").setAttribute("aria-selected", String(data));
  // The editor and the result tabs are one view, the table browser the other.
  // Nothing is destroyed by switching — the last result is still there when you
  // come back. The status line stays in both: it is where a CSV import reports.
  $("refused").hidden = data;
  document.querySelector(".editor").hidden = data;
  $("out").hidden = data || !lastResult || !lastResult.ok;
  $("data").hidden = !data;
  // Always rebuilt on entry: a statement you ran a moment ago may have changed
  // rows, and a browser showing a stale table would be lying about the engine.
  if (data) buildData();
}

function buildData() {
  const N = 200;
  $("data").innerHTML =
    `<p class="explainer">Every table in the live database, straight from the engine — ` +
    `each is a <code>SELECT * FROM …</code> run just now, capped at ${N} rows and scrolling ` +
    `in its own box. Anything you INSERT or UPDATE shows up here immediately; ` +
    `<strong>Reset database</strong> puts it back.</p>` +
    TABLES.map((t) => {
      const res = db.run(`SELECT * FROM ${t} LIMIT ${N}`);
      if (!res.ok || !res.result || res.result.kind !== "rows") {
        return `<div class="dtbl"><h4>${esc(t)}</h4>` +
          `<p class="rowcount">${esc(res.error || "no rows returned")}</p></div>`;
      }
      const n = tableCount(t);
      const shown = res.result.rows.length;
      const more = n !== null && n > shown ? ` — showing the first ${shown}` : "";
      return (
        `<div class="dtbl"><h4>${esc(t)}` +
        `<span class="dn">${n === null ? shown : n} row${(n ?? shown) === 1 ? "" : "s"}${more}</span></h4>` +
        `<div class="dscroll">${rowsTableHtml(res.result)}</div></div>`
      );
    }).join("");
}

// count(*) is its own statement rather than rows.length, because the LIMIT
// above means the row array is not the answer to "how big is this table".
function tableCount(t) {
  const res = db.run(`SELECT count(*) FROM ${t}`);
  if (!res.ok || !res.result || res.result.kind !== "rows" || !res.result.rows.length) return null;
  return res.result.rows[0][0]?.v ?? null;
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
  const MAX = 300;
  const more =
    r.rows.length > MAX
      ? `<p class="rowcount">Showing the first ${MAX} of ${r.rows.length} rows — the engine returned all of them.</p>`
      : `<p class="rowcount">${r.rows.length} row${r.rows.length === 1 ? "" : "s"}.</p>`;
  return `${rowsTableHtml(r, MAX)}${more}`;
}

// The table itself, with no surrounding prose — shared by the result pane and
// the Data browser so the two never render a value differently.
function rowsTableHtml(r, max = 300) {
  // Column type is taken from the first row's actual value tag — the engine's
  // answer, not a declared type.
  const tys = r.columns.map((_, i) => r.rows[0]?.[i]?.t ?? "");
  const head = r.columns
    .map((c, i) => `<th>${esc(c)}<span class="ty">${esc(tys[i])}</span></th>`)
    .join("");
  const body = r.rows
    .slice(0, max)
    .map((row) => `<tr>${row.map(cellHtml).join("")}</tr>`)
    .join("");
  return `<div class="tablewrap"><table class="rows"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`;
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
  // Two hosts show the same catalogue: the sidebar, and the modal the Examples
  // button opens. They are rendered from one source and wired by delegation, so
  // there is no second copy of the click logic to drift.
  for (const host of [$("examples"), $("exdlgbody")]) {
    // Each group is a <details>: the first is open so the page lands on
    // something runnable, the rest collapsed so the whole list is scannable at
    // a glance rather than a column the visitor has to scroll past.
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

    host.addEventListener("click", (e) => {
      const b = e.target.closest("button.ex");
      if (!b) return;
      const gi = Number(b.dataset.g), i = Number(b.dataset.i);
      const it = EXAMPLES[gi].items[i];
      b.closest("details.exgroup")?.setAttribute("open", "");
      $("sql").value = it.sql;
      // The selection is marked in both hosts, so opening the modal after
      // clicking in the sidebar (or the reverse) shows where you are.
      for (const o of document.querySelectorAll("button.ex")) o.removeAttribute("aria-current");
      for (const o of document.querySelectorAll(`button.ex[data-g="${gi}"][data-i="${i}"]`)) {
        o.setAttribute("aria-current", "true");
      }
      activeTab = "rows";
      runCurrent();
      // Running is the point of picking an example, so get out of the way and
      // put the editor and its results back on screen.
      if ($("exdlg").open) $("exdlg").close();
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
