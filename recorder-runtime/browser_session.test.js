// Unit tests for the browser-side WASM session runtime.
//
// The hooks are driven directly rather than through a real
// `WebAssembly.instantiate`, because what is under test is the
// translation from the flat `__ct_emit_*` stream into the recording
// vocabulary the daemon parses — not the engine.
//
// The only substitute for a real component is the WebSocket transport,
// replaced by an in-memory recorder through the module's own
// `transportFactory` seam. That is unavoidable: the alternative is a
// live `record-web` daemon, and the socket is not the thing being
// verified — the JSON lines put on it are. Everything else (the
// producer, the framing logic, the encoders) is the real code path.

import { test } from "node:test";
import { strict as assert } from "node:assert";
import {
  createBrowserWasmRecorder,
  FUNC_KIND_EXPORT,
  FUNC_KIND_IMPORT,
  FUNC_KIND_STORE,
  REALM_DIRECTION_ENTER,
  REALM_DIRECTION_LEAVE,
} from "./browser_session.js";

/** In-memory stand-in for a WebSocket, capturing the JSON lines sent. */
class CapturingTransport {
  constructor() {
    this.readyState = 1;
    this.chunks = [];
  }
  send(data) {
    this.chunks.push(data);
  }
  close() {
    this.readyState = 3;
  }
  /** Every line sent, parsed. */
  lines() {
    return this.chunks
      .join("")
      .split("\n")
      .filter((l) => l.length > 0)
      .map((l) => JSON.parse(l));
  }
}

function recorder(options = {}) {
  const transport = new CapturingTransport();
  const r = createBrowserWasmRecorder({
    endpoint: "ws://localhost:0/ct-stream",
    transportFactory: () => transport,
    flushThreshold: 1,
    manifest: { functions: [{ name: "balance_of" }] },
    ...options,
  });
  return { r, transport };
}

test("an i64 boundary value is recorded exactly, not narrowed to a Number", () => {
  const { r, transport } = recorder();
  // 2^63 - 1: the largest i64. A JS Number rounds this to
  // 9223372036854775808 — one more than the true value — and nothing
  // in the recording would say so.
  const huge = 9223372036854775807n;

  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i64(0, huge);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_i64(0, -1n);
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.stop();

  const values = transport.lines().filter((l) => l.kind === "Value");
  assert.deepEqual(
    values.map((v) => v.name),
    ["balance_of:arg0", "balance_of:ret0"],
    "one argument binding and one result binding",
  );
  assert.equal(values[0].value.typeKind, "BigInt");
  assert.equal(
    values[0].value.value,
    "9223372036854775807",
    "the exact i64 must survive; a Number would report ...808",
  );
  assert.notEqual(
    values[0].value.value,
    String(Number(huge)),
    "the guard: a narrowed value would differ from the exact one",
  );

  const ret = transport.lines().find((l) => l.kind === "Return");
  assert.deepEqual(ret.returnValue, { value: "-1", typeKind: "BigInt" });
});

test("i32, f32 and f64 boundary values keep their existing encodings", () => {
  const { r, transport } = recorder();
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i32(0, -7);
  r.imports.__ct_emit_f32(1, 1.5);
  r.imports.__ct_emit_f64(2, -0.25);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.stop();

  const values = transport.lines().filter((l) => l.kind === "Value");
  assert.deepEqual(
    values.map((v) => v.value),
    [
      { value: -7, typeKind: "Int" },
      { value: 1.5, typeKind: "Float" },
      { value: -0.25, typeKind: "Float" },
    ],
  );
});

test("arguments and results are framed to the right side of the crossing", () => {
  const { r, transport } = recorder();
  // An export with two arguments and one result, with an import call
  // in between: the argument run is separated from the result run by
  // the realm marker and by the nested crossing, and neither may leak
  // into the other.
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i32(0, 10);
  r.imports.__ct_emit_i32(1, 20);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_call(FUNC_KIND_IMPORT, 3);
  r.imports.__ct_emit_i32(0, 99);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_IMPORT,
    3,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_return(FUNC_KIND_IMPORT, 3);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_IMPORT,
    3,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_i32(0, 30);
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.stop();

  const names = transport
    .lines()
    .filter((l) => l.kind === "Value")
    .map((l) => `${l.name}=${l.value.value}`);
  assert.deepEqual(names, [
    "balance_of:arg0=10",
    "balance_of:arg1=20",
    "import #3:arg0=99",
    "balance_of:ret0=30",
  ]);

  const ret = transport.lines().find((l) => l.kind === "Return");
  assert.deepEqual(
    ret.returnValue,
    { value: 30, typeKind: "Int" },
    "the export's return value is the result run, never its argument run",
  );
});

test("a void export does not report its arguments as a return value", () => {
  const { r, transport } = recorder();
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i32(0, 5);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.stop();

  const ret = transport.lines().find((l) => l.kind === "Return");
  assert.deepEqual(ret.returnValue, { value: null, typeKind: "None" });
  const values = transport.lines().filter((l) => l.kind === "Value");
  assert.deepEqual(
    values.map((v) => v.name),
    ["balance_of:arg0"],
    "the single run belongs to the argument side",
  );
});

// --- M36: the interior store pass is retired from the browser path ------

test("a store group puts nothing on the wire", () => {
  const { r, transport } = recorder();
  // The shape an `instrument_stores = true` module emits for one
  // `i32.store`: a group header whose `fn_index` is the byte width,
  // the `(addr, old, new)` tuple, and the closing marker.
  r.imports.__ct_emit_call(FUNC_KIND_STORE, 4);
  r.imports.__ct_emit_i32(0, 1049372);
  r.imports.__ct_emit_i32(1, 0);
  r.imports.__ct_emit_i32(2, 620);
  r.imports.__ct_emit_return(FUNC_KIND_STORE, 4);
  r.stop();

  const kinds = transport.lines().map((l) => l.kind);
  assert.deepEqual(
    kinds,
    ["SessionStart", "Manifest", "SessionEnd"],
    "the store pass is withdrawn (spec §§ 2, 11): no Step, no Value, no Write",
  );
});

test("a store group inside an export does not disturb the boundary framing", () => {
  // The regression this guards: a run buffered by a store group and
  // never consumed would be re-flushed as the *next* group's tuple,
  // so the export's result would be reported as `620, 1049372, 0`.
  // Dropping the group's values is not enough — they have to be
  // dropped at the point the group closes.
  const { r, transport } = recorder();
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i32(0, 42);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_call(FUNC_KIND_STORE, 4);
  r.imports.__ct_emit_i32(0, 1049372);
  r.imports.__ct_emit_i32(1, 0);
  r.imports.__ct_emit_i32(2, 620);
  r.imports.__ct_emit_return(FUNC_KIND_STORE, 4);
  r.imports.__ct_emit_i32(0, 620);
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.stop();

  const values = transport.lines().filter((l) => l.kind === "Value");
  assert.deepEqual(
    values.map((v) => `${v.name}=${v.value.value}`),
    ["balance_of:arg0=42", "balance_of:ret0=620"],
    "only the boundary tuples are recorded, and they keep their sides",
  );
  const ret = transport.lines().find((l) => l.kind === "Return");
  assert.deepEqual(ret.returnValue, { value: 620, typeKind: "Int" });
});

test("each boundary tuple lands on a step of its own", () => {
  // Load-bearing, not cosmetic. An origin walk locates a binding's
  // write by finding the step where its value first appears, which
  // requires an earlier step in the same frame where it did not. With
  // the arguments and the result on the frame's single entry step, a
  // chain crossing into this recording finds no change and walks off
  // the start of the recording instead of landing on the module.
  const { r, transport } = recorder();
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i32(0, 42);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_i32(0, 620);
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.stop();

  // Reconstruct which step each binding was recorded against, the way
  // the daemon's writer does: a `Value` belongs to the most recent
  // `Step`.
  const stepOf = new Map();
  let step = -1;
  for (const line of transport.lines()) {
    if (line.kind === "Step") step += 1;
    if (line.kind === "Value") stepOf.set(line.name, step);
  }
  assert.equal(stepOf.get("balance_of:arg0"), 1, "entry step, then arguments");
  assert.equal(stepOf.get("balance_of:ret0"), 2, "then results");
});

// --- the binding an origin chain resumes on ------------------------------

/** The outbound (`send`) realm marker of the first crossing. */
function outboundMarker(transport) {
  return transport
    .lines()
    .find((l) => l.kind === "CorrelationMarker" && l.direction === "send");
}

/** Drive one export call returning `results`, with `options` on the recorder. */
function callExport(options, results) {
  const { r, transport } = recorder(options);
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i32(0, 42);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  results.forEach((v, slot) => r.imports.__ct_emit_i32(slot, v));
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.stop();
  return transport;
}

test("the outbound marker names the export's own result binding", () => {
  // Under the withdrawn store model the crossing binding was a
  // linear-memory slot the module had to publish, so the page had to
  // declare its name. The value that leaves a WebAssembly export is
  // its result, and the recorder has just recorded it under a name of
  // its own — so it names the crossing itself, and a module needs no
  // annotation. An origin chain arriving from the page resumes here.
  const transport = callExport({}, [620]);
  assert.equal(outboundMarker(transport).showText, "balance_of:ret0");
});

test("an explicit returnValueNames entry overrides the default", () => {
  const transport = callExport(
    { returnValueNames: { balance_of: "handle" } },
    [620],
  );
  assert.equal(outboundMarker(transport).showText, "handle");
});

test("a void export's outbound marker names no binding", () => {
  // Pointing a chain at a binding that does not exist would turn
  // "this export sent nothing" into what looks like a failed lookup
  // in the sibling recording.
  const transport = callExport({}, []);
  assert.equal(outboundMarker(transport).showText, undefined);
});

test("every line put on the wire is serialisable JSON", () => {
  const { r, transport } = recorder();
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_i64(0, 2n ** 62n);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.stop();
  // `JSON.stringify` throws on a BigInt, so a runtime that buffered one
  // would have taken down the page rather than recorded anything. The
  // parse below is the proof it never reaches the producer.
  assert.ok(transport.lines().length > 0);
});
