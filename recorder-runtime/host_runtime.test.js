// Unit test for the JS host runtime — exercises the imports
// surface directly (no WASM execution) to verify the batch
// encoding is stable.

import { test } from "node:test";
import { strict as assert } from "node:assert";
import {
  createRecorderRuntime,
  createWebSocketProducer,
  DEFAULT_ENDPOINT,
  decodeSlot,
  resolveEndpoint,
} from "./host_runtime.js";

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

// ---------------------------------------------------------------------------
// M27 WebSocket-producer bridge tests — exercise the wiring of the
// recorder runtime to the M26 ~ws://localhost:9230/ct-stream~ producer
// the JS-side recorder also uses. The receiver decodes events by `kind`
// (`browser_stream_receiver::BrowserEvent`) so the test pins the exact
// `kind` strings and the field shapes per the M27 ABI.
// ---------------------------------------------------------------------------

/**
 * Minimal in-memory WebSocket fake — replaces `globalThis.WebSocket`
 * so the test never touches a real socket. `send()` records the
 * concatenated payload so we can split + assert on the
 * newline-delimited JSON lines the recorder ships.
 */
class FakeSocket {
  constructor(url) {
    this.url = url;
    this.readyState = 1; // WHATWG OPEN
    this.sent = [];
    this.closed = false;
    this.onopen = null;
    // Defer the open callback so the test can install it via the
    // constructor pathway just like a real WHATWG WebSocket would.
    queueMicrotask(() => this.onopen && this.onopen());
  }
  send(payload) { this.sent.push(payload); }
  close() { this.closed = true; }
}

test("resolveEndpoint honours the documented lookup order", () => {
  assert.equal(resolveEndpoint("ws://override"), "ws://override");
  assert.equal(
    resolveEndpoint(undefined, { __codetracer_endpoint: "ws://from-global" }),
    "ws://from-global",
  );
  assert.equal(resolveEndpoint(undefined, {}), DEFAULT_ENDPOINT);
  assert.equal(
    DEFAULT_ENDPOINT,
    "ws://localhost:9230/ct-stream",
    "must match the M26 daemon receiver default bind",
  );
});

test("createWebSocketProducer ships newline-delimited JSON over the M26 transport", () => {
  let last = null;
  const fakes = [];
  const factory = (url) => {
    const s = new FakeSocket(url);
    fakes.push(s);
    last = s;
    return s;
  };
  const producer = createWebSocketProducer({
    endpoint: "ws://test/ct-stream",
    transportFactory: factory,
    flushThreshold: 2,
  });
  assert.equal(producer.endpoint, "ws://test/ct-stream");
  producer.send({ kind: "WasmCall", fn_kind: 0, fn_index: 1 });
  // Below threshold — nothing should have been shipped yet.
  assert.equal(last.sent.length, 0, "buffer still holds the single event");
  producer.send({ kind: "WasmReturn", fn_kind: 0, fn_index: 1 });
  // Threshold of 2 → drain.
  assert.equal(last.sent.length, 1);
  const lines = last.sent[0].trim().split("\n").map((l) => JSON.parse(l));
  assert.equal(lines.length, 2);
  assert.equal(lines[0].kind, "WasmCall");
  assert.equal(lines[1].kind, "WasmReturn");
  producer.close();
  assert.equal(last.closed, true);
});

test("createRecorderRuntime routes __ct_emit_* events through the M26 producer", () => {
  const sent = [];
  // Capture sent JSON events through a fake producer; the runtime
  // never touches a real WebSocket in this path.
  const producer = {
    send(evt) { sent.push(evt); },
    flush() {},
    close() {},
  };
  const r = createRecorderRuntime({ producer });
  // Drive the exact six-event JS↔WASM boundary sequence the M27
  // verification test pins (`test_wasm_recorder_runtime_emits_realm_crossing_events`).
  const enterToken = r.imports.__ct_correlation_token();
  r.imports.__ct_emit_call(1, 0); // export enter
  r.imports.__ct_emit_realm_boundary(0, 1, 0, enterToken);
  r.imports.__ct_emit_write(0x1000, 4, 0n, 42n);
  const leaveToken = r.imports.__ct_correlation_token();
  r.imports.__ct_emit_realm_boundary(1, 1, 0, leaveToken);
  r.imports.__ct_emit_return(1, 0); // export leave
  r.flush();
  // Five emits → five JSON events on the wire (Call → RealmBoundary →
  // Write → RealmBoundary → Return). The real M27 verification fixture
  // wraps the boundary pair around two ~__ct_emit_call~ / ~__ct_emit_return~
  // events for the imported host call; here we test the minimal export
  // wrap because the producer-wiring contract is per-event symmetric.
  assert.equal(sent.length, 5);
  // First: WasmCall.
  assert.deepEqual(sent[0], { kind: "WasmCall", fn_kind: 1, fn_index: 0 });
  // Second: RealmBoundary — must match `wasm_realm_marker_payload`'s
  // expected `{direction, fn_kind, fn_index, token}` tuple, with
  // `token` as a decimal string (M25 ↔ M27 PairIndex bridge).
  assert.deepEqual(sent[1], {
    kind: "RealmBoundary",
    token: "1",
    direction: 0,
    fn_kind: 1,
    fn_index: 0,
  });
  // Third: WasmWrite carries the memory-store `(addr, size, old, new)`
  // tuple; BigInt fields render as decimal strings so JSON survives
  // values above 2^53.
  assert.deepEqual(sent[2], {
    kind: "WasmWrite",
    addr: 0x1000,
    size: 4,
    old: "0",
    new: "42",
  });
  // Fourth: RealmBoundary leave with the next monotonic token.
  assert.deepEqual(sent[3], {
    kind: "RealmBoundary",
    token: "2",
    direction: 1,
    fn_kind: 1,
    fn_index: 0,
  });
  assert.deepEqual(sent[4], { kind: "WasmReturn", fn_kind: 1, fn_index: 0 });
});

test("createRecorderRuntime end-to-end through a FakeSocket lands on the wire", () => {
  let socket = null;
  const r = createRecorderRuntime({
    endpoint: "ws://test/ct-stream",
    transportFactory: (url) => {
      socket = new FakeSocket(url);
      return socket;
    },
  });
  assert.equal(r.endpoint, "ws://test/ct-stream");
  r.imports.__ct_emit_write(0x200, 8, 0n, 0xcafebaben);
  r.imports.__ct_emit_realm_boundary(0, 0, 7, r.imports.__ct_correlation_token());
  r.flush();
  // The producer drains synchronously because the FakeSocket reports
  // `readyState === 1` immediately.
  assert.equal(socket.sent.length, 1);
  const lines = socket.sent[0].trim().split("\n").map((l) => JSON.parse(l));
  assert.equal(lines.length, 2);
  assert.equal(lines[0].kind, "WasmWrite");
  assert.equal(lines[0].new, (0xcafebaben).toString());
  assert.equal(lines[1].kind, "RealmBoundary");
  assert.equal(lines[1].direction, 0);
  assert.equal(lines[1].fn_kind, 0);
  assert.equal(lines[1].fn_index, 7);
  assert.equal(lines[1].token, "1");
  r.close();
  assert.equal(socket.closed, true);
});

test("decodeSlot reproduces the M27 ABI shapes verbatim", () => {
  // Pin the wire shape independently of the batching path — this is
  // the contract the daemon receiver (and the synthetic Playwright
  // browser-fullstack test in M27) decodes against.
  const buf = new ArrayBuffer(32);
  const view = new DataView(buf);
  // Hand-craft a RealmBoundary slot.
  view.setUint8(0, 4); // tag
  view.setUint8(1, 1); // fn_kind = export
  view.setUint8(2, 0); // direction = enter
  view.setUint32(4, 13, true); // fn_index
  view.setBigUint64(16, 42n, true); // token
  assert.deepEqual(decodeSlot(view, 0), {
    kind: "RealmBoundary",
    token: "42",
    direction: 0,
    fn_kind: 1,
    fn_index: 13,
  });
});

test("createRecorderRuntime keeps the onBatch path working alongside producer", () => {
  // Sanity: the binary sidecar consumer (native hosts) and the JSON
  // producer (browser hosts) can be wired simultaneously without
  // double-consuming the buffer.
  const flushed = [];
  const sent = [];
  const r = createRecorderRuntime({
    onBatch(buf) { flushed.push(new Uint8Array(buf.slice(0))); },
    producer: { send(e) { sent.push(e); }, flush() {}, close() {} },
  });
  r.imports.__ct_emit_call(0, 5);
  r.flush();
  assert.equal(flushed.length, 1);
  assert.equal(flushed[0].length, 32);
  assert.equal(sent.length, 1);
  assert.deepEqual(sent[0], { kind: "WasmCall", fn_kind: 0, fn_index: 5 });
});

test("typed value hooks record exact bit patterns", () => {
  const events = [];
  const r = createRecorderRuntime({
    producer: {
      send(e) {
        events.push(e);
      },
      flush() {},
      close() {},
    },
  });
  r.imports.__ct_emit_i32(0, -1);
  r.imports.__ct_emit_i64(1, 0x0123456789abcdefn);
  r.imports.__ct_emit_f32(2, 1.5);
  // Negative zero is the case a naive `Number` comparison cannot see:
  // `-0 === 0`, and only the sign bit tells them apart.
  r.imports.__ct_emit_f64(3, -0);
  r.flush();

  assert.deepEqual(events, [
    { kind: "WasmValue", slot: 0, valueType: "i32", bits: "4294967295" },
    { kind: "WasmValue", slot: 1, valueType: "i64", bits: "81985529216486895" },
    { kind: "WasmValue", slot: 2, valueType: "f32", bits: "1069547520" },
    {
      kind: "WasmValue",
      slot: 3,
      valueType: "f64",
      bits: "9223372036854775808",
    },
  ]);
});

test("decodeSlot round-trips a boundary value slot", () => {
  const view = new DataView(new ArrayBuffer(32));
  view.setUint8(0, 5);
  view.setUint8(1, 3); // f64
  view.setUint32(4, 6, true); // slot
  view.setBigUint64(16, 0x4008000000000000n, true); // 3.0
  assert.deepEqual(decodeSlot(view, 0), {
    kind: "WasmValue",
    slot: 6,
    valueType: "f64",
    bits: "4613937818241073152",
  });
});
