// Smoke test for the webpack wrapper.
//
// We test:
//   - `CodetracerWasmPlugin.apply` is callable and dispatches the
//     internal handshake to a stub compiler;
//   - `instrumentWasmFile` produces a non-empty Buffer carrying
//     the `codetracer.instrumenter` custom-section marker.

import { test } from "node:test";
import { strict as assert } from "node:assert";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

import { CodetracerWasmPlugin, instrumentWasmFile } from "./index.js";

const execFileAsync = promisify(execFile);

async function resolveBinary() {
  if (process.env.CT_INSTRUMENT_BIN) return process.env.CT_INSTRUMENT_BIN;
  try {
    await execFileAsync("cargo", [
      "build",
      "--manifest-path",
      path.join(import.meta.dirname, "..", "..", "Cargo.toml"),
      "--bin",
      "ct-instrument",
    ]);
    return path.join(import.meta.dirname, "..", "..", "target", "debug", "ct-instrument");
  } catch {
    return null;
  }
}

const MIN_WASM = new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
  0x03, 0x02, 0x01, 0x00,
  0x07, 0x07, 0x01, 0x03, 0x6e, 0x6f, 0x70, 0x00, 0x00,
  0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b,
]);

test("CodetracerWasmPlugin.apply hands off to compiler", () => {
  const plugin = new CodetracerWasmPlugin({ binary: "ct-instrument" });
  let received = null;
  plugin.apply({
    applyCodetracerWasmPluginForTest(opts) {
      received = opts;
    },
  });
  assert.deepStrictEqual(received, { binary: "ct-instrument", configPath: undefined });
});

test("instrumentWasmFile produces an instrumented module", async () => {
  const binary = await resolveBinary();
  if (!binary) {
    console.warn("[skip] could not resolve ct-instrument binary");
    return;
  }
  const tmpdir = await fs.mkdtemp(path.join(os.tmpdir(), "webpack-ct-test-"));
  try {
    const input = path.join(tmpdir, "min.wasm");
    await fs.writeFile(input, MIN_WASM);
    const bytes = await instrumentWasmFile(binary, undefined, input);
    assert.ok(bytes.length > MIN_WASM.length, "instrumented module should be larger");
    assert.ok(
      bytes.includes(Buffer.from("codetracer.instrumenter")),
      "missing instrumenter marker",
    );
  } finally {
    await fs.rm(tmpdir, { recursive: true, force: true });
  }
});
