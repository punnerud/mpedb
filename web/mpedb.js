// mpedb wasm glue — the whole of it.
//
// The module exports a hand-rolled C ABI (no wasm-bindgen), so this file is
// the complete binding layer: strings in via a heap buffer, JSON out via a
// length-prefixed one.

export class Mpedb {
  constructor(instance) {
    this.exports = instance.exports;
    this.mem = () => new Uint8Array(this.exports.memory.buffer);
    this.enc = new TextEncoder();
    this.dec = new TextDecoder();
  }

  static async load(url) {
    // Streaming compile where the server sets the right MIME type; the
    // arrayBuffer fallback keeps `file://` and misconfigured hosts working.
    let instance;
    // The ONE thing the module cannot fake and must not guess: the clock.
    // `SystemTime::now()` panics on wasm32-unknown-unknown, and stubbing it to
    // zero would make `date('now')` answer 1970 — a wrong answer rather than a
    // refusal. So the host supplies the real one.
    const imports = { mpedb: { mpedb_host_now_ms: () => Date.now() } };
    try {
      const res = await fetch(url);
      if (!res.ok) throw new Error(`HTTP ${res.status} fetching ${url}`);
      if (WebAssembly.instantiateStreaming) {
        ({ instance } = await WebAssembly.instantiateStreaming(res, imports));
      } else {
        const buf = await res.arrayBuffer();
        ({ instance } = await WebAssembly.instantiate(buf, imports));
      }
    } catch (e) {
      if (instance) throw e;
      const res = await fetch(url);
      const buf = await res.arrayBuffer();
      ({ instance } = await WebAssembly.instantiate(buf, imports));
    }
    return new Mpedb(instance);
  }

  // Read a `[u32 len][utf8 json]` result and free it.
  _take(ptr) {
    if (!ptr) throw new Error("mpedb returned a null result pointer");
    const mem = this.mem();
    const len =
      mem[ptr] | (mem[ptr + 1] << 8) | (mem[ptr + 2] << 16) | (mem[ptr + 3] << 24);
    const bytes = mem.slice(ptr + 4, ptr + 4 + (len >>> 0));
    this.exports.mpedb_free_result(ptr);
    return JSON.parse(this.dec.decode(bytes));
  }

  open() {
    return this._take(this.exports.mpedb_open());
  }

  version() {
    return this._take(this.exports.mpedb_version());
  }

  // The playground's example queries, defined in Rust so one list feeds both
  // this page and the native test that asserts each one still behaves.
  examples() {
    return this._take(this.exports.mpedb_examples());
  }

  run(sql) {
    const bytes = this.enc.encode(sql);
    const ptr = this.exports.mpedb_alloc(bytes.length);
    this.mem().set(bytes, ptr);
    try {
      return this._take(this.exports.mpedb_run(ptr, bytes.length));
    } finally {
      this.exports.mpedb_free(ptr, bytes.length);
    }
  }
}
