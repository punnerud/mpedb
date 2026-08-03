// The Pages gate that was missing: "deployed" is not "runs". Instantiate
// the freshly built module exactly as web/mpedb.js does (same single host
// import) and require mpedb_open() to answer — the trap this catches is
// the std::time/std::process class that only exists on wasm32, which no
// native test can see (the demo shipped broken for nine days on exactly
// that gap). Usage: node web/ci-open-check.mjs <path-to-mpedb_wasm.wasm>
import { readFileSync } from "fs";

const wasm = readFileSync(process.argv[2]);
const imports = { mpedb: { mpedb_host_now_ms: () => Date.now() } };
const { instance } = await WebAssembly.instantiate(wasm, imports);
const ex = instance.exports;

const take = (ptr) => {
  if (!ptr) throw new Error("null result pointer");
  const mem = new Uint8Array(ex.memory.buffer);
  const len =
    mem[ptr] | (mem[ptr + 1] << 8) | (mem[ptr + 2] << 16) | (mem[ptr + 3] << 24);
  const bytes = mem.slice(ptr + 4, ptr + 4 + (len >>> 0));
  ex.mpedb_free_result(ptr);
  return JSON.parse(new TextDecoder().decode(bytes));
};

const r = take(ex.mpedb_open());
if (!r.ok) {
  console.error("mpedb_open answered but not ok:", JSON.stringify(r).slice(0, 200));
  process.exit(1);
}
console.log("wasm open check: ok");
