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

// --- the two edges are spelled apart (M39) -------------------------------
//
// An import whose signature is `() -> ()` emits no `Call`, no `Return`
// and no boundary value, so its pair of realm markers is the ONLY trace
// it leaves in a recording. While both edges said `wasm export #<n>`
// those markers could not be attributed to an import by their own
// content, and `codetracer-wasm-recorder` recovered no crossing from
// them — it replayed such a call unchecked, which spec §8 forbids.
// These tests pin the two spellings apart, since the consumer's
// `internal/boundarylog/recording.go` matches on them literally.

/** Every realm marker put on the wire, as `"<direction> <payload>"`. */
function markerLabels(transport) {
  return transport
    .lines()
    .filter((l) => l.kind === "CorrelationMarker")
    .map((l) => `${l.direction} ${l.payload}`);
}

test("an import crossing's markers name the IMPORT edge", () => {
  const { r, transport } = recorder();
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  // A `() -> ()` import: no arguments, no results, no Call/Return on the
  // wire — the markers are the whole record of it.
  r.imports.__ct_emit_call(FUNC_KIND_IMPORT, 7);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_IMPORT,
    7,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_return(FUNC_KIND_IMPORT, 7);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_IMPORT,
    7,
    r.imports.__ct_correlation_token(),
  );
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  r.stop();

  assert.deepEqual(markerLabels(transport), [
    "recv wasm export #0",
    "recv wasm import #7",
    "send wasm import #7",
    "send wasm export #0",
  ]);
  // The index reaches disk AND is attributable: no `Call`, no `Return`
  // and no `Value` was emitted for import #7.
  const kinds = transport.lines().map((l) => l.kind);
  assert.equal(kinds.filter((k) => k === "Call").length, 1);
  assert.equal(kinds.filter((k) => k === "Return").length, 1);
  assert.equal(kinds.filter((k) => k === "Value").length, 0);
});

test("an export and an import at the SAME index are told apart", () => {
  // The guard that matters: the two numberings overlap, so a label that
  // carried only the index would make `export #0` and `import #0`
  // identical records.
  const { r, transport } = recorder();
  exportCall(r, [1], [2], () => {
    importCall(r, 0, [], [], null);
  });
  r.stop();

  assert.deepEqual(markerLabels(transport), [
    "recv wasm export #0",
    "recv wasm import #0",
    "send wasm import #0",
    "send wasm export #0",
  ]);
});

test("an import's outbound marker never names an export's binding", () => {
  // `showText` is the binding an origin chain resumes its walk on, and
  // both sources of it (`returnValueNames`, `lastResultBinding`) are
  // keyed by the EXPORT index. Asking them about an import index that
  // happens to collide would resume the walk at a value that never
  // crossed this edge.
  const { r, transport } = recorder({
    returnValueNames: { balance_of: "handle" },
  });
  exportCall(r, [1], [2], () => {
    importCall(r, 0, [], [], null);
  });
  r.stop();

  const markers = transport
    .lines()
    .filter((l) => l.kind === "CorrelationMarker");
  const importLeave = markers.find((m) => m.payload === "wasm import #0" && m.direction === "send");
  assert.equal(importLeave.showText, undefined);
  // The control: the export's own outbound marker still names one, so
  // the assertion above is about the edge and not about `showText`
  // having stopped working.
  const exportLeave = markers.find((m) => m.payload === "wasm export #0" && m.direction === "send");
  assert.equal(exportLeave.showText, "handle");
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

// ---------------------------------------------------------------------
// Host-supplied state (spec §3.3) and host mutation during a call (§3.4)
// ---------------------------------------------------------------------
//
// The `WebAssembly.Memory` below is a real one and the "host writes" are
// real writes through a real `Uint8Array` view — which is the point: the
// capture is a diff of that memory, so a stub memory would test the
// arithmetic and nothing about the mechanism. What is driven by hand is
// the hook stream, exactly as in the tests above, because the framing
// windows are defined in terms of hook order.

/** Byte view over a tracked memory, rebuilt each time (grow detaches). */
function bytesOf(memory) {
  return new Uint8Array(memory.buffer);
}

/** Decode a recorded region payload the way the Go consumer does. */
function decodeB64(b64) {
  return Array.from(Buffer.from(b64, "base64"));
}

function hostStateRecorder(options = {}) {
  const memory = new WebAssembly.Memory({ initial: 1 });
  const { r, transport } = recorder({
    manifest: { functions: [{ name: "settle" }] },
    ...options,
  });
  return { r, transport, memory };
}

/** Drive one complete exported call, running `body` inside it. */
function exportCall(r, args, results, body) {
  r.imports.__ct_emit_call(FUNC_KIND_EXPORT, 0);
  args.forEach((a, i) => r.imports.__ct_emit_i32(i, a));
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
  if (body) body();
  results.forEach((v, i) => r.imports.__ct_emit_i32(i, v));
  r.imports.__ct_emit_return(FUNC_KIND_EXPORT, 0);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_EXPORT,
    0,
    r.imports.__ct_correlation_token(),
  );
}

/**
 * Drive one imported call the way `replace_imported_call` splices it,
 * running `hostBody` at the exact point the real host function would run.
 */
function importCall(r, importIndex, args, results, hostBody) {
  r.imports.__ct_emit_call(FUNC_KIND_IMPORT, importIndex);
  args.forEach((a, i) => r.imports.__ct_emit_i32(i, a));
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_ENTER,
    FUNC_KIND_IMPORT,
    importIndex,
    r.imports.__ct_correlation_token(),
  );
  if (hostBody) hostBody();
  results.forEach((v, i) => r.imports.__ct_emit_i32(i, v));
  r.imports.__ct_emit_return(FUNC_KIND_IMPORT, importIndex);
  r.imports.__ct_emit_realm_boundary(
    REALM_DIRECTION_LEAVE,
    FUNC_KIND_IMPORT,
    importIndex,
    r.imports.__ct_correlation_token(),
  );
}

const initialStates = (t) =>
  t.lines().filter((l) => l.kind === "HostInitialState");
const mutations = (t) => t.lines().filter((l) => l.kind === "HostMutation");

test("nothing host-state is recorded when nothing is registered", () => {
  // The overwhelmingly common case: a module that defines its own memory
  // needs neither record, and must not pay for a diff it does not need.
  const { r, transport } = hostStateRecorder();
  exportCall(r, [1], [2]);
  r.stop();
  assert.equal(initialStates(transport).length, 0);
  assert.equal(mutations(transport).length, 0);
});

test("what the host put in memory before the first export is §3.3 state", () => {
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ module: "env", name: "memory", memory });

  // The host writes its calldata, exactly as a wasm-bindgen glue layer
  // or a Stylus host would.
  bytesOf(memory).set([7, 0, 0, 0, 100, 0, 0, 0], 1024);

  exportCall(r, [0], [42]);
  r.stop();

  const records = initialStates(transport);
  assert.equal(records.length, 1, "emitted once, before the first call");
  assert.deepEqual(records[0].memories.length, 1);
  const m = records[0].memories[0];
  assert.equal(m.module, "env");
  assert.equal(m.name, "memory");
  assert.equal(m.minPages, 1);
  assert.equal(m.maxPages, null);
  assert.deepEqual(m.data.map((d) => d.offset), [1024]);
  assert.deepEqual(
    decodeB64(m.data[0].bytesB64),
    [7, 0, 0, 0, 100],
    "the run ends at the last byte that differs from the baseline: the " +
      "three trailing zeros are already zero, so recording them would be " +
      "recording bytes the host did not change",
  );
});

test("the §3.3 record excludes what was already there when registered", () => {
  // Registering after `WebAssembly.instantiate` is the documented time,
  // and it is what keeps the module's own data segments out of the
  // record: the replayer applies those from the `.wasm` itself.
  const { r, transport, memory } = hostStateRecorder();
  bytesOf(memory).set([1, 2, 3, 4], 16); // stands in for a data segment
  r.trackHostMemory({ memory });
  bytesOf(memory).set([9], 2048); // the host's own contribution

  exportCall(r, [0], [0]);
  r.stop();

  const m = initialStates(transport)[0].memories[0];
  assert.deepEqual(m.data.map((d) => d.offset), [2048]);
});

test("the §3.3 record is emitted before the first Call record", () => {
  // The replayer applies initial state before driving any export, so a
  // record that arrived later would describe state the first call had
  // already run without.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });
  bytesOf(memory)[8] = 1;
  exportCall(r, [0], [0]);
  r.stop();

  const kinds = transport.lines().map((l) => l.kind);
  assert.ok(
    kinds.indexOf("HostInitialState") < kinds.indexOf("Call"),
    `HostInitialState must precede the first Call: ${kinds.join(",")}`,
  );
});

test("a host write while servicing an import is a §3.4 mutation", () => {
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });
  bytesOf(memory).set([7, 0, 0, 0], 1024);

  exportCall(r, [0], [55], () => {
    importCall(r, 0, [7], [1], () => {
      // The host writes the fee into the calldata slot and returns only
      // a status code — the Stylus `storage_load_bytes32` shape.
      bytesOf(memory).set([250, 0, 0, 0], 1032);
    });
  });
  r.stop();

  const recorded = mutations(transport);
  assert.equal(recorded.length, 1);
  assert.equal(recorded[0].memoryWrites.length, 1);
  const w = recorded[0].memoryWrites[0];
  assert.equal(w.module, "env");
  assert.equal(w.name, "memory");
  assert.equal(w.offset, 1032);
  assert.deepEqual(decodeB64(w.bytesB64), [250]);
});

test("a §3.4 mutation is anchored to the import's own crossing", () => {
  // `afterCrossing` is the `Crossing.Seq` the Go assembler assigns while
  // recovering crossings from this recording. Crossing 0 is the export
  // (opened by its `Call` record); crossings 1 and 2 are the two
  // imported calls, in the order their argument runs close.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });

  exportCall(r, [0], [0], () => {
    importCall(r, 0, [1], [1], () => {
      bytesOf(memory)[100] = 11;
    });
    importCall(r, 1, [2], [1], () => {
      bytesOf(memory)[200] = 22;
    });
  });
  r.stop();

  assert.deepEqual(
    mutations(transport).map((m) => [
      m.afterCrossing,
      m.memoryWrites[0].offset,
    ]),
    [
      [1, 100],
      [2, 200],
    ],
  );
});

test("crossing numbering survives several exported calls", () => {
  // Three calls, each making one import: the crossings are
  // 0=export,1=import, 2=export,3=import, 4=export,5=import.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });

  for (let call = 0; call < 3; call++) {
    exportCall(r, [call], [0], () => {
      importCall(r, 0, [call], [1], () => {
        bytesOf(memory)[300 + call] = 1;
      });
    });
  }
  r.stop();

  assert.deepEqual(
    mutations(transport).map((m) => m.afterCrossing),
    [1, 3, 5],
  );
});

test("an import that takes no arguments still anchors its mutation", () => {
  // Such a crossing exists on disk only as its *result* run, and the
  // consumer numbers it when that run closes.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });

  exportCall(r, [0], [0], () => {
    importCall(r, 0, [], [1], () => {
      bytesOf(memory)[64] = 5;
    });
  });
  r.stop();

  assert.deepEqual(
    mutations(transport).map((m) => m.afterCrossing),
    [1],
  );
});

test("writes the MODULE makes are not reported as host mutations", () => {
  // The §3.4 window is exactly the span in which the module is suspended
  // inside the host call. A diff over a wider window would attribute the
  // module's own stores to the host, and the replayer would rewrite them
  // over a re-execution that had already produced them.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });

  exportCall(r, [0], [0], () => {
    bytesOf(memory)[10] = 1; // module store, before the call
    importCall(r, 0, [1], [1], null); // host writes nothing
    bytesOf(memory)[11] = 1; // module store, after the call
  });
  r.stop();

  assert.deepEqual(mutations(transport), []);
});

test("a host write between two exported calls is refused, not invented", () => {
  // Neither §3.3 (before the FIRST call) nor §3.4 (during an import)
  // covers it, and the replayer has no hook to apply it at. Reporting it
  // at the cause is spec §8's discipline; dropping it would surface as a
  // divergence far away from the write.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });
  const errors = [];
  const realError = console.error;
  console.error = (msg) => errors.push(String(msg));
  try {
    exportCall(r, [0], [0]);
    bytesOf(memory)[512] = 3;
    exportCall(r, [1], [0]);
  } finally {
    console.error = realError;
  }
  r.stop();

  assert.equal(r.hostStateDiagnostics.unrepresentableWrites, 1);
  assert.equal(mutations(transport).length, 0, "nothing is invented");
  assert.equal(errors.length, 1);
  assert.match(errors[0], /between two top-level exported calls/);
});

test("a global the host reassigns between two calls is refused too", () => {
  // The same unanchorable write in a smaller container, and the one the
  // memory-only check used to drop in silence: the replayer sets a
  // provider global from the §3.3 record and thereafter only from a §3.4
  // mutation, so an assignment made at neither point is never applied
  // and the replay diverges with nothing pointing back at the cause.
  const { r, transport } = hostStateRecorder();
  const global = new WebAssembly.Global({ value: "i32", mutable: true }, 25);
  r.trackHostGlobal({ global, name: "fee_bps", type: "i32", mutable: true });
  const errors = [];
  const realError = console.error;
  console.error = (msg) => errors.push(String(msg));
  try {
    exportCall(r, [0], [0]);
    global.value = 999;
    exportCall(r, [1], [0]);
  } finally {
    console.error = realError;
  }
  r.stop();

  assert.equal(r.hostStateDiagnostics.unrepresentableWrites, 1);
  assert.equal(mutations(transport).length, 0, "nothing is invented");
  assert.equal(errors.length, 1);
  assert.match(errors[0], /imported global env\.fee_bps/);
  assert.match(errors[0], /25 -> 999/);
});

test("a global set during an import is still a §3.4 mutation, not a refusal", () => {
  // The control for the test above: the between-calls check must not
  // start reporting the writes that DO have an anchor.
  const { r, transport } = hostStateRecorder();
  const global = new WebAssembly.Global({ value: "i32", mutable: true }, 25);
  r.trackHostGlobal({ global, name: "fee_bps", type: "i32", mutable: true });

  exportCall(r, [0], [0], () => {
    importCall(r, 0, [1], [1], () => {
      global.value = 250;
    });
  });
  exportCall(r, [1], [0]);
  r.stop();

  assert.equal(r.hostStateDiagnostics.unrepresentableWrites, 0);
  assert.deepEqual(
    mutations(transport).map((m) => [m.afterCrossing, m.globalSets]),
    [[1, [{ module: "env", name: "fee_bps", type: "i32", value: "250" }]]],
  );
});

test("registering after the first exported call is reported, not accepted", () => {
  // §3.3 is *the state before the first exported call*. A baseline taken
  // afterwards is a mid-execution state, so whatever the host staged
  // before that call is lost and what is recorded describes a moment the
  // replayer has no hook for — both silent, which is why the timing rule
  // is enforced rather than only documented.
  const { r, transport, memory } = hostStateRecorder();
  const errors = [];
  const realError = console.error;
  console.error = (msg) => errors.push(String(msg));
  try {
    exportCall(r, [0], [0]);
    r.trackHostMemory({ memory });
  } finally {
    console.error = realError;
  }
  r.stop();

  assert.equal(r.hostStateDiagnostics.lateRegistrations, 1);
  assert.equal(errors.length, 1);
  assert.match(errors[0], /after the module's first exported call/);
  // Registration before the first call stays silent, which is the
  // control proving the guard is on the timing and not on the call.
  const clean = hostStateRecorder();
  clean.r.trackHostMemory({ memory: clean.memory });
  exportCall(clean.r, [0], [0]);
  clean.r.stop();
  assert.equal(clean.r.hostStateDiagnostics.lateRegistrations, 0);
  assert.equal(initialStates(clean.transport).length, 1);
  assert.ok(transport.lines().length > 0);
});

test("a host write during a valueless import IS anchored (M39)", () => {
  // This used to be a refusal, and the refusal was a consequence of the
  // ambiguous marker label rather than of anything about the write: a
  // `() -> ()` import leaves no value run, and while both edges said
  // `wasm export #<n>` the consumer could recover no crossing from the
  // markers either — so there was no `afterCrossing` that would be true.
  //
  // M39 spells the import edge apart, so the `ENTER` marker now opens
  // the crossing and numbers it. The write has an anchor, and the
  // recording carries the mutation instead of a console error.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });
  const errors = [];
  const realError = console.error;
  console.error = (msg) => errors.push(String(msg));
  try {
    exportCall(r, [0], [0], () => {
      importCall(r, 3, [], [], () => {
        bytesOf(memory)[128] = 8;
      });
    });
  } finally {
    console.error = realError;
  }
  r.stop();

  assert.equal(r.hostStateDiagnostics.unanchorableWrites, 0);
  assert.deepEqual(errors, [], "nothing to refuse any more");
  // Crossing 0 is the export (`assembler` appends it at the `Call`
  // record); crossing 1 is the import, opened by its `ENTER` marker.
  assert.deepEqual(
    mutations(transport).map((m) => m.afterCrossing),
    [1],
  );
  assert.deepEqual(mutations(transport)[0].memoryWrites, [
    { module: "env", name: "memory", offset: 128, bytesB64: btoa("\x08") },
  ]);
});

test("a host write with no ENTER marker for the call is still refused", () => {
  // The refusal above is not gone, only re-aimed. §3.4 anchors a
  // mutation to a crossing, so a host call that opened no crossing has
  // nothing to anchor to whatever its signature is. Every edge
  // `ct-instrument` rewrites emits the marker, so this shape means the
  // hook stream did not come from it — which is exactly when guessing an
  // anchor would put the write at somebody else's crossing.
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });
  const errors = [];
  const realError = console.error;
  console.error = (msg) => errors.push(String(msg));
  try {
    exportCall(r, [0], [0], () => {
      // `importCall` without its realm markers: call hook, host work,
      // return hook.
      r.imports.__ct_emit_call(FUNC_KIND_IMPORT, 3);
      bytesOf(memory)[128] = 8;
      r.imports.__ct_emit_return(FUNC_KIND_IMPORT, 3);
    });
  } finally {
    console.error = realError;
  }
  r.stop();

  assert.equal(r.hostStateDiagnostics.unanchorableWrites, 1);
  assert.equal(mutations(transport).length, 0);
  assert.match(errors[0], /no crossing to anchor/);
  assert.match(errors[0], /ENTER marker/);
});

test("an imported global's initial value and later set are recorded", () => {
  const { r, transport } = hostStateRecorder();
  const fee = new WebAssembly.Global({ value: "i32", mutable: true }, 25);
  r.trackHostGlobal({ name: "fee_bps", type: "i32", mutable: true, global: fee });

  exportCall(r, [0], [0], () => {
    importCall(r, 0, [1], [1], () => {
      fee.value = 250;
    });
  });
  r.stop();

  assert.deepEqual(initialStates(transport)[0].globals, [
    {
      module: "env",
      name: "fee_bps",
      type: "i32",
      mutable: true,
      value: "25",
    },
  ]);
  assert.deepEqual(mutations(transport)[0].globalSets, [
    { module: "env", name: "fee_bps", type: "i32", value: "250" },
  ]);
});

test("an i64 global keeps its exact value", () => {
  const { r, transport } = hostStateRecorder();
  const handle = new WebAssembly.Global(
    { value: "i64", mutable: false },
    9223372036854775807n,
  );
  r.trackHostGlobal({ name: "handle", type: "i64", global: handle });
  exportCall(r, [0], [0]);
  r.stop();

  assert.equal(
    initialStates(transport)[0].globals[0].value,
    "9223372036854775807",
  );
});

test("a declared memory maximum reaches the record", () => {
  // Spec §7: `memory.grow`'s result depends on the host limit, so a
  // replay that does not know the limit can diverge on a failed grow.
  const memory = new WebAssembly.Memory({ initial: 1, maximum: 4 });
  const { r, transport } = recorder({
    manifest: { functions: [{ name: "settle" }] },
  });
  r.trackHostMemory({ memory, maxPages: 4 });
  exportCall(r, [0], [0]);
  r.stop();
  assert.equal(initialStates(transport)[0].memories[0].maxPages, 4);
});

test("host-state records are serialisable JSON like every other line", () => {
  const { r, transport, memory } = hostStateRecorder();
  r.trackHostMemory({ memory });
  bytesOf(memory)[1] = 1;
  exportCall(r, [0], [0], () => {
    importCall(r, 0, [1], [1], () => {
      bytesOf(memory)[2] = 2;
    });
  });
  r.stop();
  // `lines()` parses every chunk, so reaching here at all proves the
  // records went out as JSON — no Uint8Array, no BigInt.
  assert.equal(initialStates(transport).length, 1);
  assert.equal(mutations(transport).length, 1);
});

test("registering through the constructor is the same as trackHostMemory", () => {
  const memory = new WebAssembly.Memory({ initial: 1 });
  const { r, transport } = recorder({
    manifest: { functions: [{ name: "settle" }] },
    hostMemories: [{ memory }],
  });
  // The baseline is taken when the memory is registered — at
  // construction here — so this write is host state either way.
  bytesOf(memory)[7] = 3;
  exportCall(r, [0], [0]);
  r.stop();
  assert.deepEqual(
    initialStates(transport)[0].memories[0].data.map((d) => d.offset),
    [7],
  );
});

test("an unsupported global type is refused at registration", () => {
  const { r } = hostStateRecorder();
  assert.throws(
    () => r.trackHostGlobal({ name: "r", type: "externref", global: {} }),
    /unsupported global type/,
  );
});
