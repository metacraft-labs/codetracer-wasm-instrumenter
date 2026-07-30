// Unit tests for the host-supplied-state region logic.
//
// No substitutes of any kind: the functions under test are pure, take
// `Uint8Array`s and return regions, and the `WebAssembly.Memory` used by
// the size helpers is a real one — the JS engine running these tests has
// a complete WebAssembly implementation, so there is nothing to fake.

import { test } from "node:test";
import { strict as assert } from "node:assert";
import {
  DEFAULT_COALESCE_GAP,
  WASM_PAGE_BYTES,
  diffRegions,
  toBase64,
  encodeRegions,
  encodeGlobalValue,
  snapshotMemory,
  pagesOf,
} from "./host_state.js";

/** Decode a region's payload back to bytes, the way the consumer does. */
function decode(b64) {
  return new Uint8Array(Buffer.from(b64, "base64"));
}

test("a null baseline reports every non-zero byte", () => {
  const after = new Uint8Array(16);
  after[3] = 7;
  after[4] = 9;

  const regions = diffRegions(null, after);
  assert.deepEqual(
    regions.map((r) => [r.offset, Array.from(r.bytes)]),
    [[3, [7, 9]]],
  );
});

test("a baseline makes the diff exactly what changed since it", () => {
  const before = new Uint8Array([1, 2, 3, 4, 5, 6]);
  const after = new Uint8Array([1, 2, 30, 4, 5, 6]);

  const regions = diffRegions(before, after);
  assert.deepEqual(
    regions.map((r) => [r.offset, Array.from(r.bytes)]),
    [[2, [30]]],
    "only the changed byte, not the whole buffer",
  );
});

test("a host write that zeroes a byte the module had set is recorded", () => {
  // The "non-zero regions" reading of the schema cannot express this:
  // the byte's new value IS zero. Diffing against a baseline can, and
  // must — the module observes the cleared byte.
  const before = new Uint8Array([9, 9, 9]);
  const after = new Uint8Array([9, 0, 9]);

  const regions = diffRegions(before, after);
  assert.deepEqual(
    regions.map((r) => [r.offset, Array.from(r.bytes)]),
    [[1, [0]]],
  );
});

test("runs separated by more than the gap stay separate regions", () => {
  const before = new Uint8Array(4096);
  const after = new Uint8Array(4096);
  after[0] = 1;
  after[DEFAULT_COALESCE_GAP + 2] = 1;

  const regions = diffRegions(before, after);
  assert.equal(regions.length, 2, "the identical stretch is wider than the gap");
  assert.deepEqual(
    regions.map((r) => r.offset),
    [0, DEFAULT_COALESCE_GAP + 2],
  );
});

test("runs separated by less than the gap are merged into one", () => {
  const before = new Uint8Array(4096);
  const after = new Uint8Array(4096);
  after[0] = 1;
  after[4] = 2;

  const regions = diffRegions(before, after);
  assert.equal(regions.length, 1);
  assert.equal(regions[0].offset, 0);
  assert.deepEqual(
    Array.from(regions[0].bytes),
    [1, 0, 0, 0, 2],
    "the identical bytes bridged by the merge are carried verbatim, so " +
      "rewriting them is a no-op",
  );
});

test("coalescing never bridges a gap the caller narrowed", () => {
  const before = new Uint8Array(64);
  const after = new Uint8Array(64);
  after[0] = 1;
  after[3] = 1;

  assert.equal(diffRegions(before, after, { coalesceGap: 8 }).length, 1);
  assert.equal(diffRegions(before, after, { coalesceGap: 1 }).length, 2);
});

test("an identical memory produces no regions at all", () => {
  const bytes = new Uint8Array([4, 5, 6]);
  assert.deepEqual(diffRegions(bytes, new Uint8Array(bytes)), []);
});

test("growth past the baseline reports only the non-zero tail", () => {
  const before = new Uint8Array(4);
  const after = new Uint8Array(4 + 200);
  after[100] = 3;

  const regions = diffRegions(before, after);
  assert.deepEqual(
    regions.map((r) => [r.offset, Array.from(r.bytes)]),
    [[100, [3]]],
    "a grown page reads as zero, so only what was written shows up",
  );
});

test("base64 uses the padded standard alphabet Go's StdEncoding wants", () => {
  assert.equal(toBase64(new Uint8Array([0xff, 0xfe])), "//4=");
  assert.equal(toBase64(new Uint8Array([])), "");
});

test("base64 survives a region larger than the call-argument limit", () => {
  // `String.fromCharCode(...bytes)` throws around 100k arguments, which
  // is well inside the size of a single host-written buffer.
  const big = new Uint8Array(300_000);
  for (let i = 0; i < big.length; i++) big[i] = i & 0xff;
  const round = decode(toBase64(big));
  assert.equal(round.length, big.length);
  assert.deepEqual(Array.from(round.subarray(0, 8)), Array.from(big.subarray(0, 8)));
  assert.deepEqual(
    Array.from(round.subarray(big.length - 8)),
    Array.from(big.subarray(big.length - 8)),
  );
});

test("encodeRegions round-trips through the consumer's decoder", () => {
  const before = new Uint8Array(32);
  const after = new Uint8Array(32);
  after.set([0xde, 0xad, 0xbe, 0xef], 8);

  const encoded = encodeRegions(diffRegions(before, after));
  assert.deepEqual(encoded, [{ offset: 8, bytesB64: "3q2+7w==" }]);
  assert.deepEqual(Array.from(decode(encoded[0].bytesB64)), [
    0xde, 0xad, 0xbe, 0xef,
  ]);
});

test("a global's value is an exact decimal string, BigInt included", () => {
  assert.equal(encodeGlobalValue(-7), "-7");
  assert.equal(encodeGlobalValue(1.5), "1.5");
  assert.equal(
    encodeGlobalValue(9223372036854775807n),
    "9223372036854775807",
    "a Number would report ...808",
  );
});

test("a memory snapshot is a copy, not a view onto the live buffer", () => {
  const memory = new WebAssembly.Memory({ initial: 1 });
  const view = new Uint8Array(memory.buffer);
  view[0] = 1;

  const snap = snapshotMemory(memory);
  view[0] = 2;
  assert.equal(snap[0], 1, "the snapshot must not track later writes");
});

test("pagesOf reports the memory's current size in WASM pages", () => {
  const memory = new WebAssembly.Memory({ initial: 2 });
  assert.equal(pagesOf(memory), 2);
  memory.grow(1);
  assert.equal(pagesOf(memory), 3);
  assert.equal(memory.buffer.byteLength, 3 * WASM_PAGE_BYTES);
});

test("a snapshot taken before grow still diffs correctly after it", () => {
  // `grow` detaches the old ArrayBuffer; a cached view over it reads as
  // length zero, which would silently record a grown memory as empty.
  const memory = new WebAssembly.Memory({ initial: 1 });
  const before = snapshotMemory(memory);
  memory.grow(1);
  new Uint8Array(memory.buffer)[WASM_PAGE_BYTES + 5] = 42;

  const regions = diffRegions(before, snapshotMemory(memory));
  assert.deepEqual(
    regions.map((r) => [r.offset, Array.from(r.bytes)]),
    [[WASM_PAGE_BYTES + 5, [42]]],
  );
});
