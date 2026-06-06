// Unit test for the JS host runtime — exercises the imports
// surface directly (no WASM execution) to verify the batch
// encoding is stable.

import { test } from "node:test";
import { strict as assert } from "node:assert";
import { createRecorderRuntime } from "./host_runtime.js";

test("recorder runtime emits one batch event per __ct_emit_* call", () => {
  const flushed = [];
  const r = createRecorderRuntime({ onBatch(buf) { flushed.push(new Uint8Array(buf.slice(0))); } });
  r.imports.__ct_emit_call(1, 7);
  r.imports.__ct_emit_realm_boundary(0, 1, 7, r.imports.__ct_correlation_token());
  r.imports.__ct_emit_write(0x100, 4, 0n, 0xdeadbeefn);
  r.imports.__ct_emit_return(1, 7);
  assert.equal(r.bufferedEvents(), 4);
  r.flush();
  assert.equal(flushed.length, 1);
  assert.equal(flushed[0].length, 4 * 32, "one 32-byte slot per event");
  // Tag bytes in order: 2, 4, 1, 3.
  assert.equal(flushed[0][0 * 32], 2, "first event tag");
  assert.equal(flushed[0][1 * 32], 4, "second event tag");
  assert.equal(flushed[0][2 * 32], 1, "third event tag");
  assert.equal(flushed[0][3 * 32], 3, "fourth event tag");
});

test("__ct_correlation_token returns a strictly monotonic value", () => {
  const r = createRecorderRuntime({ onBatch() {} });
  const a = r.imports.__ct_correlation_token();
  const b = r.imports.__ct_correlation_token();
  const c = r.imports.__ct_correlation_token();
  assert.ok(a < b && b < c, `expected a<b<c, got ${a} ${b} ${c}`);
});
