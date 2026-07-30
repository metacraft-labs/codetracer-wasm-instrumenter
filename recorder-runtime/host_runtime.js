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
// Two integration shapes are supported simultaneously:
//
//   * **Native / custom hosts** pass an `onBatch(buf)` callback and
//     consume the 32-byte slot binary format directly (e.g. write
//     it to a CTFS sidecar).
//   * **Browser hosts** pass `{producer}` or `{endpoint}` and the
//     runtime translates each batched event into a JSON line and
//     ships it through the M26 WebSocket producer used by
//     ~@codetracer/runtime-browser~ (newline-delimited JSON over
//     ~ws://localhost:9230/ct-stream~ — see
//     ~codetracer-js-recorder/packages/runtime-browser/src/index.ts~
//     for the canonical sibling implementation).
//
// Usage (browser host wiring into the M26 producer):
//
//   import { createRecorderRuntime } from "codetracer-wasm-instrumenter/recorder-runtime/host_runtime.js";
//   const runtime = createRecorderRuntime({});
//   const instance = await WebAssembly.instantiate(bytes, {
//     __codetracer: runtime.imports,
//   });
//   // ...execution emits `__ct_emit_write` / `__ct_emit_realm_boundary`
//   // events; the runtime translates them and ships JSON lines to
//   // ~ws://localhost:9230/ct-stream~ where the M26 daemon receiver
//   // (`session-manager record-web`) merges them with the JS-side
//   // recording into a single `.ct` file.
//
// Usage (native host wiring to a custom binary sink):
//
//   const runtime = createRecorderRuntime({
//     onBatch(buf) { ctfsWriter.write(new Uint8Array(buf)); },
//   });

/**
 * Default endpoint URL — matches the M26 daemon receiver's bind
 * (`session-manager record-web`, see
 * `codetracer/src/backend-manager/src/browser_stream_host.rs`
 * `DEFAULT_BIND` / `DEFAULT_ENDPOINT_PATH`). Kept in sync with
 * `@codetracer/runtime-browser`'s `DEFAULT_ENDPOINT` constant.
 */
export const DEFAULT_ENDPOINT = "ws://localhost:9230/ct-stream";

// ── Flush policy ──────────────────────────────────────────────────────────
//
// Two bounds, and the buffer drains at whichever is reached first. Both
// numbers live here rather than inline because the trade-off between them is
// the whole design, and the count alone had no rationale recorded against it
// (M38d). One policy, mirrored in `@codetracer/runtime-browser`.
//
// The cost being traded is a WebSocket frame per event against a recording
// that only exists once the page has ended.
// `codetracer-specs/Recording-Backends/WASM-Replay-Snapshots-And-Slices.md`
// §2 requires the second not to happen: a replaying consumer derives
// snapshots from this stream *while the page runs*, so a recording delivered
// in one batch at `close()` makes §2's timeline unreachable however promptly
// everything downstream works. A count-only policy does exactly that for any
// page producing fewer events than the threshold — which is most short pages,
// and every fixture that consumes this runtime.

/**
 * Events buffered before a flush. Bounds the *memory* a burst can occupy and
 * amortises the per-frame overhead over a batch; 256 events is a few tens of
 * kilobytes of JSON. This is the bound a hot loop is governed by.
 */
export const DEFAULT_FLUSH_THRESHOLD = 256;

/**
 * Milliseconds an event may wait before it is shipped regardless of how full
 * the buffer is, measured from the batch's first event.
 *
 * Bounds the *latency* an event can suffer, and deliberately is not "small":
 * the interval caps timer-driven frames at `1000 / interval` per second no
 * matter how fast events arrive, so the policy's cost is a constant rather
 * than something proportional to the workload. At 50ms that is 20 frames per
 * second, and any page producing more than `256 / 0.05 = 5120` events per
 * second reaches the count threshold first and never arms the timer at all —
 * a hot loop pays nothing. Below that rate the page is human-scale, where
 * 50ms is under the ~100ms at which a person perceives a reaction as
 * immediate.
 *
 * `0` disables the time-based flush and restores a purely count-based policy.
 */
export const DEFAULT_FLUSH_INTERVAL_MS = 50;

/**
 * Resolve the effective WebSocket endpoint per the M26 producer
 * lookup order (explicit option > `window.__codetracer_endpoint` >
 * default). Mirrors `resolveEndpoint` from
 * `@codetracer/runtime-browser/src/index.ts` so the two recorders
 * agree on the same URL when no override is supplied.
 *
 * @param {string | undefined} optsEndpoint
 * @param {{ __codetracer_endpoint?: string } | undefined} [globalRef]
 * @returns {string}
 */
export function resolveEndpoint(optsEndpoint, globalRef) {
  if (optsEndpoint) return optsEndpoint;
  const fromGlobal =
    globalRef?.__codetracer_endpoint ??
    (typeof globalThis !== "undefined"
      ? globalThis.__codetracer_endpoint
      : undefined);
  if (fromGlobal) return fromGlobal;
  return DEFAULT_ENDPOINT;
}

/**
 * Default WebSocket transport factory. Resolves the `WebSocket`
 * global lazily so non-browser environments (Node test harnesses,
 * SSR contexts) never trip a `ReferenceError` at module load.
 *
 * @param {string} url
 * @returns {{send(payload: string): void, close(): void, readyState: number}}
 */
export const defaultWebSocketFactory = (url) => {
  const Ctor = globalThis.WebSocket;
  if (!Ctor) {
    throw new Error(
      "WebSocket is not available in this environment; provide a custom transportFactory",
    );
  }
  return new Ctor(url);
};

/**
 * Build a JSON-line producer that ships WASM-host events to the
 * M26 WebSocket receiver. The shape mirrors `createBrowserRuntime`
 * from `@codetracer/runtime-browser`:
 *
 *   * buffers events while the socket is CONNECTING,
 *   * flushes whenever the buffer reaches `flushThreshold`,
 *   * flushes whenever `flushIntervalMs` has elapsed since the
 *     batch's first event, whatever the buffer holds,
 *   * tolerates transport errors silently (recording is
 *     best-effort — the host program must never crash because
 *     the daemon socket went away mid-recording).
 *
 * The two bounds and the reasoning behind their defaults are
 * {@link DEFAULT_FLUSH_THRESHOLD} and {@link DEFAULT_FLUSH_INTERVAL_MS}.
 * This is one policy shared with `@codetracer/runtime-browser`, and the
 * two implementations are mirrors: change one, change the other.
 *
 * @typedef {{send(payload: string): void, close(): void, readyState: number, onopen?: () => void}} WasmHostTransport
 * @typedef {(url: string) => WasmHostTransport} WasmHostTransportFactory
 *
 * @param {{
 *   endpoint?: string,
 *   transportFactory?: WasmHostTransportFactory,
 *   flushThreshold?: number,
 *   flushIntervalMs?: number,
 * }} [options]
 * @returns {{
 *   send(event: object): void,
 *   flush(): void,
 *   close(): void,
 *   readonly endpoint: string,
 *   readonly bufferedCount: number,
 * }}
 */
export function createWebSocketProducer(options = {}) {
  const endpoint = resolveEndpoint(options.endpoint);
  const factory = options.transportFactory ?? defaultWebSocketFactory;
  const threshold = options.flushThreshold ?? DEFAULT_FLUSH_THRESHOLD;
  const flushIntervalMs = options.flushIntervalMs ?? DEFAULT_FLUSH_INTERVAL_MS;
  /** @type {string[]} */
  let queue = [];
  /** @type {WasmHostTransport | null} */
  let transport = null;
  let closed = false;
  /**
   * Pending time-based flush, armed when a batch starts and cleared the
   * moment the batch leaves. `null` means no deadline is outstanding.
   * @type {ReturnType<typeof setTimeout> | null}
   */
  let flushTimer = null;
  try {
    transport = factory(endpoint);
  } catch {
    // Recording is best-effort — fall through to a no-op producer.
    transport = null;
  }
  if (transport) {
    try {
      // Some fake transports may not expose `onopen`; that's fine —
      // explicit `flush()` still drains.
      transport.onopen = () => {
        drain();
      };
    } catch {
      /* ignore */
    }
  }

  function cancelFlushTimer() {
    if (flushTimer === null) return;
    try {
      clearTimeout(flushTimer);
    } catch {
      /* some sandboxed contexts restrict timers */
    }
    flushTimer = null;
  }

  function armFlushTimer() {
    if (flushIntervalMs <= 0 || flushTimer !== null || closed) return;
    if (typeof setTimeout !== "function") return;
    try {
      flushTimer = setTimeout(onFlushDeadline, flushIntervalMs);
      // Node keeps its event loop alive for a pending timer, and a recorder
      // must never be the reason a process refuses to exit. Browsers have
      // no `unref` and need none.
      flushTimer?.unref?.();
    } catch {
      flushTimer = null;
    }
  }

  function onFlushDeadline() {
    flushTimer = null;
    drain();
    // Still queued means the socket has not opened yet — `drain` is a no-op
    // while CONNECTING. Renew the deadline for as long as it is still
    // trying, and no longer: a page with no daemon behind it must not poll
    // forever, and `onopen` drains the backlog anyway.
    if (queue.length > 0 && transport?.readyState === 0) armFlushTimer();
  }

  function drain() {
    if (!transport || closed) return;
    if (transport.readyState !== 1) return;
    if (queue.length === 0) return;
    const payload = queue.join("\n") + "\n";
    try {
      transport.send(payload);
    } catch {
      // Drop the batch and keep going; the daemon may have torn
      // the socket down mid-stream.
    }
    queue = [];
    // The batch this deadline belonged to has left; the next one arms its own.
    cancelFlushTimer();
  }

  return {
    send(event) {
      if (closed) return;
      queue.push(JSON.stringify(event));
      if (queue.length >= threshold) {
        drain();
        return;
      }
      // The deadline runs from the FIRST event of a batch, not the last, so
      // a steady dribble cannot postpone its own delivery indefinitely.
      // Arming on the empty-to-non-empty transition is what makes that true,
      // and keeps the per-event cost to one integer comparison.
      if (queue.length === 1) armFlushTimer();
    },
    flush() {
      drain();
    },
    close() {
      if (closed) return;
      drain();
      closed = true;
      cancelFlushTimer();
      try {
        transport?.close();
      } catch {
        /* ignore */
      }
    },
    get endpoint() {
      return endpoint;
    },
    get bufferedCount() {
      return queue.length;
    },
  };
}

/**
 * @typedef {Object} RecorderRuntimeOptions
 * @property {(buf: ArrayBuffer) => void} [onBatch]
 *   Called every time the batch buffer flushes. Mutually
 *   compatible with `producer` / `endpoint` — when both are
 *   supplied, the batch buffer feeds both sinks.
 * @property {number} [batchCapacityBytes=65536]
 *   Soft upper bound on the per-batch buffer size.
 * @property {{
 *   send(event: object): void,
 *   flush(): void,
 *   close(): void,
 *   endpoint?: string,
 * }} [producer]
 *   M26 JSON producer. When supplied, every batched event is
 *   translated to its JSON shape and shipped via `producer.send`.
 * @property {string} [endpoint]
 *   Convenience: when set (and `producer` is not), the runtime
 *   builds an internal {@link createWebSocketProducer} for the
 *   given URL.
 * @property {(url: string) => {send(payload: string): void, close(): void, readyState: number, onopen?: () => void}} [transportFactory]
 *   Forwarded to the internal producer (test seam).
 * @property {number} [flushThreshold]
 *   Forwarded to the internal producer.
 * @property {number} [flushIntervalMs]
 *   Forwarded to the internal producer.
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
 *   - u8  tag      (1 = write, 2 = call, 3 = return, 4 = realm_boundary,
 *                   5 = boundary value)
 *   - u8  fn_kind  (write: padding 0; call/return: 0|1|2; realm: 0|1;
 *                   value: the value's type, 0=i32 1=i64 2=f32 3=f64)
 *   - u8  direction (realm only: 0|1; padding 0 otherwise)
 *   - u8  reserved
 *   - u32 fn_index (call/return/realm); slot index for a value;
 *                   padding 0 for write
 *   - u32 size   (write only); padding 0 otherwise
 *   - u32 addr   (write only); padding 0 otherwise
 *   - u64 old / token / bits (write: old; realm: token; value: the
 *                   value's raw bit pattern; padding 0 otherwise)
 *   - u64 new    (write only); padding 0 otherwise
 *
 * Total: 32 bytes per event. The receiver demarshals using the
 * tag byte.
 *
 * Boundary values carry raw *bit patterns* rather than numbers: spec
 * § 7 makes a NaN payload mismatch a replay divergence, so a float
 * that round-tripped through a JS number would be a recording the
 * replayer must reject.
 *
 * Known limit of a *JavaScript* host: WASM hands `f32` and `f64` to JS
 * as a `Number`, and the WebAssembly JS API leaves NaN payloads
 * implementation-defined across that conversion. So the bits recorded
 * here are exact for every finite value, for infinities and for
 * negative zero, but a signalling-NaN payload may already have been
 * canonicalised before this function sees it. That is a property of
 * the JS boundary, not of the instrumentation — a host that reads the
 * values without going through a `Number` (the wazero replayer, or the
 * `wasmi`-based test harness in `codetracer-wasm-stub-host`) sees them
 * bit-exact. A recording made in a browser from a module that computes
 * with NaN payloads is therefore not replay-safe under § 7, and the
 * replayer's divergence check is what will say so.
 *
 * When a `producer` (or `endpoint`) is supplied, the runtime
 * also translates each event into its JSON shape and ships it
 * through the producer:
 *
 *   * tag=1 → `{kind:"WasmWrite", addr, size, old, new}` — BigInt
 *     fields are rendered as decimal strings to survive JSON.
 *   * tag=2 → `{kind:"WasmCall", fn_kind, fn_index}`
 *   * tag=3 → `{kind:"WasmReturn", fn_kind, fn_index}`
 *   * tag=4 → `{kind:"RealmBoundary", token, direction, fn_kind,
 *     fn_index}` — token is a decimal string (matches
 *     `db-backend::correlation_markers::wasm_realm_marker_payload`'s
 *     `key_value` format; see the M25 ↔ M27 bridge note in
 *     `codetracer/src/db-backend/src/correlation_markers.rs`).
 *
 * @param {RecorderRuntimeOptions} [options]
 */
export function createRecorderRuntime(options = {}) {
  const capacity = options.batchCapacityBytes ?? 65536;
  const eventSize = 32;
  const buf = new ArrayBuffer(capacity);
  const view = new DataView(buf);
  const bytes = new Uint8Array(buf);
  let cursor = 0;
  let monotonic = 1n;

  // Resolve the JSON producer eagerly so the endpoint is locked in
  // at construction time (matches the M26 producer contract).
  let producer = options.producer ?? null;
  let ownsProducer = false;
  if (!producer && options.endpoint !== undefined) {
    producer = createWebSocketProducer({
      endpoint: options.endpoint,
      transportFactory: options.transportFactory,
      flushThreshold: options.flushThreshold,
      flushIntervalMs: options.flushIntervalMs,
    });
    ownsProducer = true;
  }

  const onBatch = options.onBatch;

  function flush() {
    if (cursor === 0) return;
    if (producer) {
      // Translate the binary slots into JSON events and ship.
      // Iterating the buffer rather than emitting per-call avoids
      // synchronising the JSON path with the binary path on the
      // hot instrumentation calls.
      for (let off = 0; off < cursor; off += eventSize) {
        producer.send(decodeSlot(view, off));
      }
      producer.flush();
    }
    if (onBatch) {
      const out = new ArrayBuffer(cursor);
      new Uint8Array(out).set(bytes.subarray(0, cursor));
      onBatch(out);
    }
    cursor = 0;
  }

  function reserve() {
    if (cursor + eventSize > buf.byteLength) {
      flush();
    }
  }

  /**
   * Append one boundary-value slot.
   *
   * @param {number} typeTag 0=i32, 1=i64, 2=f32, 3=f64.
   * @param {number} slot Position within the argument or result tuple.
   * @param {bigint} bits Raw bit pattern of the value.
   */
  function recordValue(typeTag, slot, bits) {
    reserve();
    view.setUint8(cursor, 5);
    view.setUint8(cursor + 1, typeTag);
    view.setUint8(cursor + 2, 0);
    view.setUint8(cursor + 3, 0);
    view.setUint32(cursor + 4, slot >>> 0, true);
    view.setUint32(cursor + 8, 0, true);
    view.setUint32(cursor + 12, 0, true);
    view.setBigUint64(cursor + 16, bits, true);
    view.setBigUint64(cursor + 24, 0n, true);
    cursor += eventSize;
  }

  return {
    imports: {
      /**
       * Boundary value hooks (spec § 5). One per WASM value type,
       * because a single hook would have to widen `f32`, which is not
       * bit-preserving for signalling NaNs.
       */
      __ct_emit_i32(slot, value) {
        recordValue(0, slot, BigInt(value >>> 0));
      },
      __ct_emit_i64(slot, value) {
        recordValue(1, slot, asBigInt(value));
      },
      __ct_emit_f32(slot, value) {
        recordValue(2, slot, BigInt(f32Bits(value)));
      },
      __ct_emit_f64(slot, value) {
        recordValue(3, slot, f64Bits(value));
      },

      /**
       * Legacy per-store write hook.
       *
       * Nothing this pipeline emits calls it — spec § 5 withdrew it
       * from the hook surface. It stays here so a module produced by
       * an older instrumenter, which still declares the import,
       * remains instantiable against this runtime.
       */
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
    /**
     * Tear down the recorder. Flushes any pending events and
     * closes the producer (if the runtime owns it). Safe to call
     * multiple times.
     */
    close() {
      flush();
      if (ownsProducer && producer) {
        try {
          producer.close();
        } catch {
          /* ignore */
        }
      }
    },
    /** Endpoint URL for the JSON producer (introspection / tests). */
    get endpoint() {
      return producer && "endpoint" in producer ? producer.endpoint : undefined;
    },
  };
}

/**
 * Decode one 32-byte event slot into the JSON shape consumed by
 * the M26 daemon receiver. Kept as a module-level helper so the
 * test suite can pin the wire shape independently of the
 * batching path.
 *
 * The shapes match:
 *   * the M27 ABI documented in `__ct_emit_*` (see
 *     `crates/codetracer-wasm-instrumenter/src/hooks.rs`), and
 *   * the `kind`-tagged receiver vocabulary in
 *     `codetracer/src/backend-manager/src/browser_stream_receiver.rs`.
 *
 * `BigInt` values (memory-store `old`/`new`, realm-boundary
 * `token`) are rendered as decimal strings so the receiver can
 * round-trip them through `serde_json` without losing the high
 * bits — JSON numbers are IEEE-754 doubles and lose precision
 * above 2^53.
 *
 * @param {DataView} view
 * @param {number} off
 * @returns {object}
 */
export function decodeSlot(view, off) {
  const tag = view.getUint8(off);
  const fnKind = view.getUint8(off + 1);
  const direction = view.getUint8(off + 2);
  const fnIndex = view.getUint32(off + 4, true);
  const size = view.getUint32(off + 8, true);
  const addr = view.getUint32(off + 12, true);
  const oldOrToken = view.getBigUint64(off + 16, true);
  const newVal = view.getBigUint64(off + 24, true);
  switch (tag) {
    case 1:
      return {
        kind: "WasmWrite",
        addr,
        size,
        old: oldOrToken.toString(),
        new: newVal.toString(),
      };
    case 2:
      return { kind: "WasmCall", fn_kind: fnKind, fn_index: fnIndex };
    case 3:
      return { kind: "WasmReturn", fn_kind: fnKind, fn_index: fnIndex };
    case 5:
      return {
        kind: "WasmValue",
        slot: fnIndex,
        valueType: ["i32", "i64", "f32", "f64"][fnKind] ?? "unknown",
        // A decimal string, like the other 64-bit fields: JSON numbers
        // are doubles and would lose the high bits of an i64 and the
        // payload of a NaN alike.
        bits: oldOrToken.toString(),
      };
    case 4:
      // Per `wasm_realm_marker_payload` (correlation_markers.rs):
      // token is the M25 pair-index key, rendered as a decimal
      // string so it composes with the JS-side
      // `js_realm_marker_payload` adapter's `key_value` field
      // byte-for-byte.
      return {
        kind: "RealmBoundary",
        token: oldOrToken.toString(),
        direction,
        fn_kind: fnKind,
        fn_index: fnIndex,
      };
    default:
      return { kind: "Unknown", tag };
  }
}

function asBigInt(v) {
  if (typeof v === "bigint") return v;
  if (typeof v === "number") return BigInt(v >>> 0);
  return 0n;
}

// Scratch buffers for reading a float's raw bits. Module-level so the
// hot value hooks allocate nothing.
const floatScratch = new DataView(new ArrayBuffer(8));

/**
 * IEEE-754 bit pattern of an `f32`, as an unsigned 32-bit number.
 * @param {number} v
 * @returns {number}
 */
function f32Bits(v) {
  floatScratch.setFloat32(0, v, true);
  return floatScratch.getUint32(0, true);
}

/**
 * IEEE-754 bit pattern of an `f64`, as an unsigned BigInt.
 * @param {number} v
 * @returns {bigint}
 */
function f64Bits(v) {
  floatScratch.setFloat64(0, v, true);
  return floatScratch.getBigUint64(0, true);
}
