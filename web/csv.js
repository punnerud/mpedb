// CSV → CREATE TABLE + INSERT.
//
// The rule from the rest of the page applies here too: never make the engine
// answer a question about data that is not what the file said. mpedb is
// rigidly typed, so the importer has to CHOOSE a type per column, and a wrong
// choice is a wrong answer, not a formatting nit — "01234" as an integer is a
// different value than the file contains. Every inference below is therefore
// conservative: TEXT unless every value in the column proves otherwise.

const MAX_ROWS = 20000;
const CHUNK = 200;

// SQL words that would turn a column name into a parse error. Not the full
// reserved list — just the ones a real spreadsheet header actually hits.
const RESERVED = new Set([
  "select", "from", "where", "group", "order", "by", "table", "index", "insert",
  "update", "delete", "values", "into", "set", "join", "left", "right", "inner",
  "outer", "on", "as", "and", "or", "not", "null", "default", "check", "unique",
  "primary", "key", "foreign", "references", "create", "drop", "alter", "limit",
  "offset", "distinct", "case", "when", "then", "else", "end", "in", "like",
  "between", "is", "exists", "union", "all", "having", "int", "integer", "text",
  "float", "real", "bool", "boolean", "blob", "date", "time", "timestamp",
]);

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

// RFC 4180 with the two deviations every real file has: bare CR, and a
// delimiter that is not always a comma.
export function parseCsv(text, delim) {
  if (text.charCodeAt(0) === 0xfeff) text = text.slice(1);
  delim = delim || sniffDelimiter(text);

  const rows = [];
  let row = [], field = "", inQuotes = false, quoted = false, i = 0;

  while (i < text.length) {
    const c = text[i];
    if (inQuotes) {
      if (c === '"') {
        if (text[i + 1] === '"') { field += '"'; i += 2; continue; }
        inQuotes = false; i++; continue;
      }
      field += c; i++; continue;
    }
    if (c === '"' && field === "") { inQuotes = true; quoted = true; i++; continue; }
    if (c === delim) { row.push(quoted ? field : field.trim()); field = ""; quoted = false; i++; continue; }
    if (c === "\r") { i++; continue; }
    if (c === "\n") {
      row.push(quoted ? field : field.trim());
      if (row.length > 1 || row[0] !== "") rows.push(row);
      row = []; field = ""; quoted = false; i++; continue;
    }
    field += c; i++;
  }
  if (field !== "" || row.length) {
    row.push(quoted ? field : field.trim());
    if (row.length > 1 || row[0] !== "") rows.push(row);
  }
  return { delim, rows };
}

// Count each candidate outside quotes on the first few lines and take the
// winner. Semicolons beat commas in most of Europe; tabs are common exports.
function sniffDelimiter(text) {
  const head = text.slice(0, 64 * 1024);
  let best = ",", bestScore = -1;
  for (const d of [",", ";", "\t", "|"]) {
    let n = 0, inQuotes = false, lines = 0;
    for (let i = 0; i < head.length && lines < 5; i++) {
      const c = head[i];
      if (c === '"') inQuotes = !inQuotes;
      else if (!inQuotes && c === d) n++;
      else if (!inQuotes && c === "\n") lines++;
    }
    if (n > bestScore) { bestScore = n; best = d; }
  }
  return best;
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

function ident(raw, fallback) {
  let x = String(raw).trim().toLowerCase()
    .replace(/[^a-z0-9_]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 48);
  if (!x) x = fallback;
  if (/^[0-9]/.test(x)) x = `c_${x}`;
  if (RESERVED.has(x)) x = `${x}_`;
  return x;
}

function unique(names) {
  const seen = new Map();
  return names.map((n) => {
    const k = seen.get(n) ?? 0;
    seen.set(n, k + 1);
    return k === 0 ? n : `${n}_${k + 1}`;
  });
}

const INT_RE = /^[+-]?\d{1,15}$/;
const FLOAT_RE = /^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$/;

// "0150" is a postcode, not 150. A digit after a leading zero means the file
// is using the string, and storing it as a number silently destroys it — as
// an integer AND as a float, which is the trap: rejecting it for INT only
// hands it to FLOAT, which loses exactly as much.
function leadingZeroed(v) {
  const d = v.replace(/^[+-]/, "");
  return d.length > 1 && d[0] === "0" && d[1] !== ".";
}

function isInt(v) {
  return INT_RE.test(v) && !leadingZeroed(v);
}

function isFloat(v) {
  return FLOAT_RE.test(v) && !leadingZeroed(v) && Number.isFinite(Number(v));
}

// A header row is one where every cell is non-empty and at least one is not a
// number — a first row of pure numbers is data, however tempting it looks.
function looksLikeHeader(row) {
  return row.every((c) => c !== "") && row.some((c) => !isInt(c) && !isFloat(c));
}

/// Decide the table: name, columns, types, and the rows to insert.
export function planTable(fileName, parsed) {
  const rows = parsed.rows;
  if (!rows.length) throw new Error("the file has no rows");

  const width = rows.reduce((m, r) => Math.max(m, r.length), 0);
  const hasHeader = looksLikeHeader(rows[0]) && rows.length > 1;
  const headerRow = hasHeader ? rows[0] : [];
  const dataRows = (hasHeader ? rows.slice(1) : rows).map((r) => {
    const out = r.slice(0, width);
    while (out.length < width) out.push("");
    return out;
  });
  if (!dataRows.length) throw new Error("the file has a header but no data rows");

  const truncated = dataRows.length > MAX_ROWS ? dataRows.length : 0;
  const kept = truncated ? dataRows.slice(0, MAX_ROWS) : dataRows;

  const columns = unique(
    Array.from({ length: width }, (_, i) => ident(headerRow[i] ?? "", `c${i + 1}`))
  );

  // One pass per column over EVERY kept row: a type that holds for the first
  // hundred rows and not the thousandth is how an importer invents data.
  const types = columns.map((_, i) => {
    let sawValue = false, allInt = true, allFloat = true;
    for (const r of kept) {
      const v = r[i];
      if (v === "") continue;
      sawValue = true;
      if (!isInt(v)) allInt = false;
      if (!isFloat(v)) allFloat = false;
      if (!allInt && !allFloat) break;
    }
    if (!sawValue) return "TEXT";
    return allInt ? "INT" : allFloat ? "FLOAT" : "TEXT";
  });

  const base = ident(fileName.replace(/\.[^.]*$/, ""), "csv");
  return { table: base, columns, types, rows: kept, truncated, hasHeader, delim: parsed.delim };
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

function lit(v, ty) {
  if (v === "") return "NULL";
  if (ty === "INT") return v.replace(/^\+/, "");
  if (ty === "FLOAT") {
    const n = Number(v);
    return Number.isFinite(n) ? String(n) : "NULL";
  }
  return `'${v.replace(/'/g, "''")}'`;
}

/// The statements that create the table and fill it, in order. Returned rather
/// than executed so the caller can show them — this page does not run SQL the
/// visitor cannot read.
export function toSql(plan) {
  const cols = plan.columns.map((c, i) => `${c} ${plan.types[i]}`).join(", ");
  const stmts = [
    `DROP TABLE IF EXISTS ${plan.table}`,
    `CREATE TABLE ${plan.table} (${cols})`,
  ];
  const names = plan.columns.join(", ");
  for (let i = 0; i < plan.rows.length; i += CHUNK) {
    const values = plan.rows
      .slice(i, i + CHUNK)
      .map((r) => `(${plan.columns.map((_, c) => lit(r[c] ?? "", plan.types[c])).join(", ")})`)
      .join(", ");
    stmts.push(`INSERT INTO ${plan.table} (${names}) VALUES ${values}`);
  }
  return stmts;
}

// ---------------------------------------------------------------------------
// Examples for imported data
// ---------------------------------------------------------------------------

/// Starter queries for a table nobody has seen before, built from its real
/// columns and types. Every one is a query the engine accepts — no refusal
/// demos here, because a refusal the visitor did not ask for reads as a bug.
export function examplesFor(plan) {
  const num = plan.columns.filter((_, i) => plan.types[i] !== "TEXT");
  const txt = plan.columns.filter((_, i) => plan.types[i] === "TEXT");
  const items = [
    {
      label: `All of ${plan.table}`,
      why: `${plan.rows.length} row${plan.rows.length === 1 ? "" : "s"}, ${plan.columns.length} columns`,
      sql: `SELECT * FROM ${plan.table} LIMIT 100`,
      refuses: false,
    },
    {
      label: "Row count",
      why: "count(*) over the whole table",
      sql: `SELECT count(*) FROM ${plan.table}`,
      refuses: false,
    },
  ];
  if (txt.length) {
    items.push({
      label: `Group by ${txt[0]}`,
      why: "the classic first question of any dataset",
      sql: `SELECT ${txt[0]}, count(*) FROM ${plan.table} GROUP BY ${txt[0]} ` +
        `ORDER BY count(*) DESC LIMIT 20`,
      refuses: false,
    });
  }
  if (num.length) {
    items.push({
      label: `Range of ${num[0]}`,
      why: "min / max / avg / sum in one pass",
      sql: `SELECT min(${num[0]}), max(${num[0]}), avg(${num[0]}), sum(${num[0]}) FROM ${plan.table}`,
      refuses: false,
    });
    items.push({
      label: `Top by ${num[0]}`,
      why: "ORDER BY … DESC with a LIMIT",
      sql: `SELECT * FROM ${plan.table} ORDER BY ${num[0]} DESC LIMIT 20`,
      refuses: false,
    });
  }
  if (txt.length && num.length) {
    items.push({
      label: `${num[0]} per ${txt[0]}`,
      why: "aggregate inside a group",
      sql: `SELECT ${txt[0]}, count(*), sum(${num[0]}), avg(${num[0]}) FROM ${plan.table} ` +
        `GROUP BY ${txt[0]} ORDER BY count(*) DESC LIMIT 20`,
      refuses: false,
    });
  }
  return { name: plan.table, items };
}
