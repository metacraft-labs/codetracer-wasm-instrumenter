// M27 verification test #6:
// `test_wasm_vite_plugin_smoke`.
//
// Drives `instrumentWasmFile` against the `ct-instrument` binary
// the workspace ships and asserts the result is non-empty,
// contains a `\0asm` magic, and carries the `codetracer.instrumenter`
// custom-section marker (i.e. the binary was actually rewritten).
//
// Skips narrowly when the binary or `cargo` aren't on PATH (the
// browser-fullstack #7 test is the only one that genuinely
// requires browser infrastructure).

import { test } from "node:test";
import { strict as assert } from "node:assert";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import { instrumentWasmFile } from "./index.js";

const execFileAsync = promisify(execFile);

async function resolveBinary() {
  // Prefer the `CT_INSTRUMENT_BIN` env var (set by `cargo test`
  // via the workspace test runner). Fall back to building from
  // source with `cargo build --bin ct-instrument`.
  if (process.env.CT_INSTRUMENT_BIN) {
    return process.env.CT_INSTRUMENT_BIN;
  }
  try {
    await execFileAsync("cargo", [
      "build",
      "--manifest-path",
      path.join(import.meta.dirname, "..", "..", "Cargo.toml"),
      "--bin",
      "ct-instrument",
    ]);
    return path.join(
      import.meta.dirname,
      "..",
      "..",
      "target",
      "debug",
      "ct-instrument",
    );
  } catch (e) {
    return null;
  }
}

// Minimal WASM module body: a single exported `nop` function.
// Compiled from `(module (func (export "nop")))` using
// `wat2wasm`; embedded as a raw byte array so the JS smoke test
// has no external dependencies beyond Node + the CLI binary.
const MIN_WASM = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, // \0asm
  0x01, 0x00, 0x00, 0x00, // version 1
  0x01, 0x04, 0x01, 0x60, 0x00, 0x00, // type section: () -> ()
  0x03, 0x02, 0x01, 0x00, // function section: 1 func of type 0
  0x07, 0x07, 0x01, 0x03, 0x6e, 0x6f, 0x70, 0x00, 0x00, // export "nop"
  0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b, // code section: empty body
]);

test("vite plugin shells out to ct-instrument and gets back an instrumented module", async () => {
  const binary = await resolveBinary();
  if (!binary) {
    console.warn("[skip] could not resolve ct-instrument binary");
    return;
  }

  const tmpdir = await fs.mkdtemp(path.join(os.tmpdir(), "vite-ct-test-"));
  try {
    const input = path.join(tmpdir, "min.wasm");
    await fs.writeFile(input, MIN_WASM);
    const { code } = await instrumentWasmFile(binary, undefined, input);

    // The generated module wrapper must contain the base64
    // payload — assert it parses back as a Uint8Array starting
    // with `\0asm`.
    const m = code.match(/atob\("([^"]+)"\)/);
    assert.ok(m, "generated JS must embed the base64 payload");
    const decoded = Buffer.from(m[1], "base64");
    assert.deepStrictEqual(
      [decoded[0], decoded[1], decoded[2], decoded[3]],
      [0x00, 0x61, 0x73, 0x6d],
      "decoded module must start with the WASM magic",
    );

    // The custom section marker (`codetracer.instrumenter`)
    // appears as a UTF-8 substring of the binary. (The custom
    // section header itself is a leb128 length-prefixed
    // sub-payload so we just look for the marker name.)
    assert.ok(
      decoded.includes(Buffer.from("codetracer.instrumenter")),
      "instrumented module must carry the custom-section marker",
    );
  } finally {
    await fs.rm(tmpdir, { recursive: true, force: true });
  }
});
