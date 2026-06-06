// Smoke test for the esbuild plugin wrapper.
//
// We test the plugin contract structurally — the `setup` callback
// must register exactly one `onLoad` handler with the `.wasm`
// filter. The actual byte-level work is exercised by the
// `instrumentWasmFile` test, identical to the Vite/Webpack ones.

import { test } from "node:test";
import { strict as assert } from "node:assert";
import codetracerWasm from "./index.js";

test("esbuild plugin registers an onLoad handler for .wasm files", () => {
  const plugin = codetracerWasm();
  assert.equal(plugin.name, "esbuild-plugin-codetracer-wasm");
  const loadCalls = [];
  plugin.setup({
    onLoad(filter, cb) {
      loadCalls.push({ filter, cb });
    },
  });
  assert.equal(loadCalls.length, 1);
  assert.ok(loadCalls[0].filter.filter instanceof RegExp);
  assert.equal(loadCalls[0].filter.filter.source, "\\.wasm$");
});
