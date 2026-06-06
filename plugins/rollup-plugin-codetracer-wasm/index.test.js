import { test } from "node:test";
import { strict as assert } from "node:assert";
import codetracerWasm from "./index.js";

test("rollup plugin exposes the expected name + load hook", () => {
  const p = codetracerWasm();
  assert.equal(p.name, "rollup-plugin-codetracer-wasm");
  assert.equal(typeof p.load, "function");
});

test("rollup plugin returns null for non-.wasm ids", async () => {
  const p = codetracerWasm();
  assert.equal(await p.load("foo.js"), null);
  assert.equal(await p.load("bar.ts"), null);
});
