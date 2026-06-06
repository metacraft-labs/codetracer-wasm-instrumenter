// codetracer-wasm-instrumenter / recorder-runtime
//
// Reference JS host implementation of the `__ct_emit_*` imports
// the instrumented WASM module declares. Batches the
// per-instruction events into a fixed-size ring and flushes
// through `__ct_emit_batch(buf, len)` — which the embedder wires
// to the M26 WebSocket producer for browser hosts, or to a
// direct CTFS writer for native hosts.
//
// Spec: M27 deliverables #4, #5.
//
// Usage:
//
//   import { createRecorderRuntime } from "codetracer-wasm-instrumenter/recorder-runtime/host_runtime.js";
//   const runtime = createRecorderRuntime({
//     onBatch(buf) { /* forward to WebSocket producer */ },
//   });
//   const instance = await WebAssembly.instantiate(bytes, {
//     __codetracer: runtime.imports,
//   });

/**
 * @typedef {Object} RecorderRuntimeOptions
 * @property {(buf: ArrayBuffer) => void} onBatch
 *   Called every time the batch buffer flushes.
 * @property {number} [batchCapacityBytes=65536]
 *   Soft upper bound on the per-batch buffer size.
 */

/**
 * Create a fresh recorder runtime instance. The returned
 * `runtime.imports` object can be passed directly as the
 * `__codetracer` import object when instantiating an
 * instrumented module.
 *
 * Event layout in the batch buffer (little-endian, all fields
 * naturally aligned):
 *
 *   - u8  tag      (1 = write, 2 = call, 3 = return, 4 = realm_boundary)
 *   - u8  fn_kind  (write: padding 0; call/return: 0|1; realm: 0|1)
 *   - u8  direction (realm only: 0|1; padding 0 otherwise)
 *   - u8  reserved
 *   - u32 fn_index (call/return/realm); padding 0 for write
 *   - u32 size   (write only); padding 0 otherwise
 *   - u32 addr   (write only); padding 0 otherwise
 *   - u64 old / token (write: old; realm: token; padding 0 otherwise)
 *   - u64 new    (write only); padding 0 otherwise
 *
 * Total: 32 bytes per event. The receiver demarshals using the
 * tag byte.
 *
 * @param {RecorderRuntimeOptions} options
 */
export function createRecorderRuntime(options) {
  const capacity = options.batchCapacityBytes ?? 65536;
  const eventSize = 32;
  let buf = new ArrayBuffer(capacity);
  let view = new DataView(buf);
  let bytes = new Uint8Array(buf);
  let cursor = 0;
  let monotonic = 1n;

  function flush() {
    if (cursor === 0) return;
    const out = new ArrayBuffer(cursor);
    new Uint8Array(out).set(bytes.subarray(0, cursor));
    options.onBatch(out);
    cursor = 0;
  }

  function reserve() {
    if (cursor + eventSize > buf.byteLength) {
      flush();
    }
  }

  return {
    imports: {
      __ct_emit_write(addr, size, oldLo, newLo) {
        // The WASM ABI hands i64 args back as BigInt in modern
        // Node / browsers when WebAssembly.BigInt is enabled.
        // Fall back to Number for environments that pass i64 as
        // a Number (loses high bits but keeps the lower 32 — V1).
        reserve();
        view.setUint8(cursor, 1);
        view.setUint8(cursor + 1, 0);
        view.setUint8(cursor + 2, 0);
        view.setUint8(cursor + 3, 0);
        view.setUint32(cursor + 4, 0, true);
        view.setUint32(cursor + 8, size >>> 0, true);
        view.setUint32(cursor + 12, addr >>> 0, true);
        view.setBigUint64(cursor + 16, asBigInt(oldLo), true);
        view.setBigUint64(cursor + 24, asBigInt(newLo), true);
        cursor += eventSize;
      },
      __ct_emit_call(fnKind, fnIndex) {
        reserve();
        view.setUint8(cursor, 2);
        view.setUint8(cursor + 1, fnKind & 0xff);
        view.setUint8(cursor + 2, 0);
        view.setUint8(cursor + 3, 0);
        view.setUint32(cursor + 4, fnIndex >>> 0, true);
        view.setUint32(cursor + 8, 0, true);
        view.setUint32(cursor + 12, 0, true);
        view.setBigUint64(cursor + 16, 0n, true);
        view.setBigUint64(cursor + 24, 0n, true);
        cursor += eventSize;
      },
      __ct_emit_return(fnKind, fnIndex) {
        reserve();
        view.setUint8(cursor, 3);
        view.setUint8(cursor + 1, fnKind & 0xff);
        view.setUint8(cursor + 2, 0);
        view.setUint8(cursor + 3, 0);
        view.setUint32(cursor + 4, fnIndex >>> 0, true);
        view.setUint32(cursor + 8, 0, true);
        view.setUint32(cursor + 12, 0, true);
        view.setBigUint64(cursor + 16, 0n, true);
        view.setBigUint64(cursor + 24, 0n, true);
        cursor += eventSize;
      },
      __ct_emit_realm_boundary(direction, fnKind, fnIndex, token) {
        reserve();
        view.setUint8(cursor, 4);
        view.setUint8(cursor + 1, fnKind & 0xff);
        view.setUint8(cursor + 2, direction & 0xff);
        view.setUint8(cursor + 3, 0);
        view.setUint32(cursor + 4, fnIndex >>> 0, true);
        view.setUint32(cursor + 8, 0, true);
        view.setUint32(cursor + 12, 0, true);
        view.setBigUint64(cursor + 16, asBigInt(token), true);
        view.setBigUint64(cursor + 24, 0n, true);
        cursor += eventSize;
      },
      __ct_correlation_token() {
        // The token must be strictly monotonic — a fresh value
        // for every realm crossing. We use a JS BigInt that
        // increments per call.
        const t = monotonic;
        monotonic += 1n;
        return t;
      },
    },
    /** Force a flush of any buffered events. */
    flush,
    /** Number of events currently buffered (introspection). */
    bufferedEvents() {
      return cursor / eventSize;
    },
  };
}

function asBigInt(v) {
  if (typeof v === "bigint") return v;
  if (typeof v === "number") return BigInt(v >>> 0);
  return 0n;
}
