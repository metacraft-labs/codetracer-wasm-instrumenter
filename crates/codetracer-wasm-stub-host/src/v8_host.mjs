// The JavaScript half of `codetracer_wasm_stub_host::v8`.
//
// Reads a run specification on argv, instantiates the module under the
// host V8, calls one export, and prints everything observed as JSON on
// stdout. It is a *host*, not a harness: it supplies the
// `__codetracer` hook surface and the module's own imports exactly the
// way an embedding page does, and it knows nothing about what is being
// tested.
//
// Kept as a separate file rather than a `node -e` string so that it is
// readable, lintable and diffable. It is embedded into the crate with
// `include_str!` and written beside the module under test at run time,
// so the crate stays self-contained and path-independent.

import fs from "node:fs";

const spec = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const wasm = fs.readFileSync(process.argv[2]);

// --- value coding -------------------------------------------------------
//
// The wire form mirrors `RecordedValue`. `i64` travels as a decimal
// string because JSON has no 64-bit integer; floats travel as their
// IEEE bit patterns, for the reason `runtime.rs` gives — a NaN payload
// does not survive a round trip through a JSON number, and losing one
// is a replay divergence under spec § 7.

const scratch = new DataView(new ArrayBuffer(8));

function toWasm(v) {
  switch (v.t) {
    case "i32":
      return v.v;
    case "i64":
      return BigInt(v.v);
    case "f32":
      scratch.setUint32(0, v.v >>> 0);
      return scratch.getFloat32(0);
    case "f64":
      scratch.setBigUint64(0, BigInt(v.v));
      return scratch.getFloat64(0);
    default:
      throw new Error(`unknown value tag ${v.t}`);
  }
}

// Results come back from `Function.prototype.call` as JS values, so
// their WASM type has to be recovered. `bigint` is unambiguously
// `i64`; a `number` may be `i32`, `f32` or `f64`, and only the export's
// declared signature can tell them apart. The caller passes that
// signature in `resultTypes`.
function fromWasm(value, ty) {
  switch (ty) {
    case "i32":
      return { t: "i32", v: value | 0 };
    case "i64":
      return { t: "i64", v: String(value) };
    case "f32":
      scratch.setFloat32(0, value);
      return { t: "f32", v: scratch.getUint32(0) };
    case "f64":
      scratch.setFloat64(0, value);
      return { t: "f64", v: String(scratch.getBigUint64(0)) };
    default:
      throw new Error(`unknown result type ${ty}`);
  }
}

// --- the recording host -------------------------------------------------

const events = [];
let token = 0n;

const hooks = {
  __ct_emit_call: (fnKind, fnIndex) => {
    events.push({ e: "call", fn_kind: fnKind, fn_index: fnIndex });
  },
  __ct_emit_return: (fnKind, fnIndex) => {
    events.push({ e: "return", fn_kind: fnKind, fn_index: fnIndex });
  },
  __ct_emit_realm_boundary: (direction, fnKind, fnIndex, tok) => {
    events.push({
      e: "realm",
      direction,
      fn_kind: fnKind,
      fn_index: fnIndex,
      token: String(tok),
    });
  },
  __ct_correlation_token: () => {
    token += 1n;
    return token;
  },
  __ct_emit_i32: (slot, v) => events.push({ e: "value", slot, v: { t: "i32", v: v | 0 } }),
  __ct_emit_i64: (slot, v) => events.push({ e: "value", slot, v: { t: "i64", v: String(v) } }),
  __ct_emit_f32_bits: (slot, bits) =>
    events.push({ e: "value", slot, v: { t: "f32", v: bits >>> 0 } }),
  __ct_emit_f64_bits: (slot, bits) =>
    events.push({ e: "value", slot, v: { t: "f64", v: String(BigInt.asUintN(64, bits)) } }),
};

let instance = null;

function makeStub(stub) {
  let nth = 0;
  return (..._args) => {
    const n = nth++;
    if (stub.reentry) {
      const callee = instance.exports[stub.reentry.export];
      if (typeof callee !== "function") {
        throw new Error(`re-entry target \`${stub.reentry.export}\` is not an export`);
      }
      return callee(...stub.reentry.args.map(toWasm));
    }
    const tuple = stub.results[n] ?? stub.results[stub.results.length - 1] ?? [];
    const values = tuple.map(toWasm);
    // The JS API takes a bare value for a one-result import, an
    // iterable for a multi-value one, and `undefined` for none — so
    // the stub never needs the import's signature.
    if (values.length === 0) return undefined;
    if (values.length === 1) return values[0];
    return values;
  };
}

function main() {
  const module = new WebAssembly.Module(wasm);

  const imports = { [spec.hostModule]: { ...hooks } };
  for (const stub of spec.stubs) {
    imports[stub.module] ??= {};
    imports[stub.module][stub.name] = makeStub(stub);
  }
  // Any import the run specification did not name is still required
  // for instantiation. Serving it with a trap rather than a silent
  // zero keeps an under-specified test loud.
  for (const imp of WebAssembly.Module.imports(module)) {
    if (imp.kind !== "function") continue;
    imports[imp.module] ??= {};
    imports[imp.module][imp.name] ??= () => {
      throw new Error(`unstubbed import ${imp.module}.${imp.name} was called`);
    };
  }

  instance = new WebAssembly.Instance(module, imports);

  const fn = instance.exports[spec.export];
  if (typeof fn !== "function") {
    throw new Error(`module has no exported function \`${spec.export}\``);
  }
  const raw = fn(...spec.args.map(toWasm));

  let results;
  if (spec.resultTypes.length === 0) {
    results = [];
  } else if (spec.resultTypes.length === 1) {
    results = [fromWasm(raw, spec.resultTypes[0])];
  } else {
    results = spec.resultTypes.map((ty, i) => fromWasm(raw[i], ty));
  }

  // Memory is found by kind, not by a conventional name, matching
  // `runtime.rs`: a module that exports its memory as `mem` must be
  // compared just as closely as one that calls it `memory`.
  let memory = null;
  for (const value of Object.values(instance.exports)) {
    if (value instanceof WebAssembly.Memory) {
      memory = Buffer.from(new Uint8Array(value.buffer)).toString("hex");
      break;
    }
  }

  process.stdout.write(JSON.stringify({ ok: true, events, results, memory }));
}

try {
  main();
} catch (err) {
  // The events recorded before the failure are reported too. A trap or
  // an escaping exception mid-crossing is exactly the case where the
  // *partial* stream is the interesting artefact — it is what a
  // replayer would be handed.
  process.stdout.write(
    JSON.stringify({
      ok: false,
      error: String(err && err.stack ? err.stack : err),
      events,
    }),
  );
}
