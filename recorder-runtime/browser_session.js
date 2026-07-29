// codetracer-wasm-instrumenter / recorder-runtime / browser_session
//
// Turns the raw `__ct_emit_*` hooks an instrumented WASM module calls
// into a **first-class CodeTracer recording of its own**.
//
// # Why a separate recording
//
// A page that runs instrumented JavaScript which calls an instrumented
// WASM module contains two distinct execution realms. Merging them into
// one trace would flatten that structure: the debugger's process tree
// could not show them separately, and — decisively — an origin chain
// could not describe the JS↔WASM crossing as a boundary hop, because a
// boundary is by definition something that joins two *recordings*.
//
// So this module opens its **own** WebSocket session against the same
// `record-web` daemon the JS recorder talks to, announcing a distinct
// `program` name. The daemon writes one `.ct` per connection, so the
// page yields `frontend-js.ct` and `frontend-wasm.ct`, and a
// `session.toml` naming both (plus a server-side recording) is what the
// debugger loads as a single multi-process session.
//
// # Why the JS-recorder event vocabulary
//
// The events emitted here are exactly the ones
// `@codetracer/runtime-browser` emits — `SessionStart`, `Manifest`,
// `Step`, `Call`, `Return`, `CorrelationMarker`, `SessionEnd`. That is
// not incidental: the daemon's receiver
// (`codetracer/src/backend-manager/src/browser_stream_receiver.rs`)
// parses one vocabulary and drops anything it cannot deserialise. An
// earlier iteration of this runtime shipped a bespoke
// `{kind:"WasmCall"}` / `{kind:"RealmBoundary"}` shape; those lines were
// silently discarded and no WASM recording ever reached disk.
//
// # The realm boundary is marked on both sides
//
// A correlation boundary only pairs when *both* recordings carry a
// marker with the same `(boundary, key)` and opposite directions. The
// host glue is the only place that observes a realm crossing, so it
// marks both sides at that moment: the WASM-side marker goes into this
// module's stream, and the JS-side marker is pushed through the page's
// own `__ct` runtime. The instrumented module supplies the shared key
// via `__ct_correlation_token()`, which is strictly monotonic, so every
// crossing pairs unambiguously.
//
// Direction convention, stated in terms of *where the value flows*:
//
//   * `ENTER` (host → WASM, at the call site): JS **sends** the
//     arguments, WASM **receives** them.
//   * `LEAVE` (WASM → host, at the return site): WASM **sends** the
//     result, JS **receives** it.
//
// A backward origin walk starting in a server recording therefore
// arrives in the JS recording, finds its tail hop sitting on the `LEAVE`
// *receive* marker, and crosses into the WASM recording — which is the
// three-recording chain the cross-process composer is built to walk.

import { resolveEndpoint, createWebSocketProducer } from "./host_runtime.js";

/**
 * Boundary id shared with the db-backend's M25 marker family.
 * Must match `BOUNDARY_ID_JS_WASM_REALM` in
 * `codetracer/src/db-backend/src/correlation_markers.rs`.
 */
export const JS_WASM_REALM_BOUNDARY = "js-wasm-realm";

/** `fn_kind` values passed by the injected hooks. */
export const FUNC_KIND_IMPORT = 0;
export const FUNC_KIND_EXPORT = 1;
/**
 * Group header of the experimental interior store pass. Not a realm
 * crossing: `fn_index` carries the store's byte width and the value
 * tuple is `(addr, old, new)`. Retired together with the store pass in
 * M36.
 */
export const FUNC_KIND_STORE = 2;

/** `direction` values passed by `__ct_emit_realm_boundary`. */
export const REALM_DIRECTION_ENTER = 0;
export const REALM_DIRECTION_LEAVE = 1;

/** Default `program` name for the WASM-side recording. */
export const DEFAULT_WASM_PROGRAM = "frontend-wasm";

/**
 * Resolve the page-side JS runtime the mirrored markers are pushed
 * through.
 *
 * Returns `null` when the page has no instrumented JS runtime, in which
 * case the WASM recording still lands but carries no paired markers —
 * a standalone WASM recording rather than a cross-realm one. That is a
 * legitimate configuration (an instrumented module on an
 * uninstrumented page), so it is a quiet degradation, not an error.
 *
 * @param {unknown} explicit
 * @returns {{markCorrelation(direction: string, boundary: string, key: unknown, payload?: unknown): void} | null}
 */
export function resolveJsRuntime(explicit) {
  const candidate =
    explicit ??
    (typeof globalThis !== "undefined" ? globalThis.__ct : undefined);
  if (
    candidate &&
    typeof (/** @type {{markCorrelation?: unknown}} */ (candidate)
      .markCorrelation) === "function"
  ) {
    return /** @type {any} */ (candidate);
  }
  return null;
}

/**
 * Resolve the instrumentation manifest emitted by `ct-instrument`.
 *
 * Lookup order: explicit option, then `window.__codetracer_wasm_manifest`
 * (the global a bundler plugin bakes in). Without a manifest the
 * recording still lands but its frames are anonymous, so callers are
 * encouraged — not required — to supply one.
 *
 * @param {unknown} explicit
 * @returns {unknown}
 */
export function resolveWasmManifest(explicit) {
  if (explicit != null) return explicit;
  return typeof globalThis !== "undefined"
    ? globalThis.__codetracer_wasm_manifest
    : undefined;
}

/**
 * Read a module's published memory layout, if it exports one.
 *
 * WASM stores address raw offsets, so a recorded write reads as
 * `mem[1048576] = 620` unless something maps that address back to a
 * name. Nothing in the binary carries that mapping for a release
 * build, so it has to come from the module itself: a module that wants
 * its memory writes to be legible exports a base address, a slot count,
 * and the slot names.
 *
 * This reads that convention when present and returns `null` otherwise
 * — a module without it still records every write, just against
 * addresses. Nothing is guessed.
 *
 * @param {Record<string, any>} exports Instantiated module exports.
 * @param {string[]} [slotNames] Names for the slots, in order.
 * @returns {{name: string, start: number, size: number}[] | null}
 */
export function describeMemoryLayout(exports, slotNames) {
  if (
    typeof exports?.ledger_base !== "function" ||
    typeof exports?.ledger_len !== "function"
  ) {
    return null;
  }
  const base = exports.ledger_base() >>> 0;
  const count = exports.ledger_len() >>> 0;
  const slotSize = 4;
  const layout = [];
  for (let i = 0; i < count; i++) {
    layout.push({
      name: slotNames?.[i] ?? `slot${i}`,
      start: base + i * slotSize,
      size: slotSize,
    });
  }
  return layout;
}

/**
 * @typedef {Object} BrowserWasmRecorderOptions
 * @property {string} [endpoint] WebSocket endpoint; defaults to the
 *   shared `record-web` URL.
 * @property {string} [program] `program` name announced in
 *   `SessionStart`; becomes the `.ct` directory name on disk.
 * @property {unknown} [manifest] Manifest emitted by `ct-instrument`.
 * @property {unknown} [jsRuntime] Page-side `__ct` runtime used for the
 *   mirrored JS-side markers. Defaults to `globalThis.__ct`.
 * @property {(url: string) => any} [transportFactory] Test seam.
 * @property {number} [flushThreshold] Events buffered before a flush.
 * @property {Record<string, string>} [returnValueNames] Per-export map
 *   from function name to the binding holding the value that function
 *   hands back across the realm boundary.
 *
 *   An origin chain crossing *into* this recording resumes its walk on
 *   that name, so supplying it is what lets the chain continue into the
 *   module's own computation rather than stopping at its edge. It is
 *   the WASM-side counterpart of the `showText` argument to
 *   `__ct.markCorrelation` on the JavaScript side: in both cases the
 *   program declares which of its bindings crosses, because nothing
 *   else can know.
 *
 *   Keyed per function rather than set once for the module: exports
 *   return different things, and naming the wrong binding would send a
 *   chain crossing at one function looking for another's value.
 */

/**
 * Create a recorder that gives an instrumented WASM module its own
 * CodeTracer recording.
 *
 * The returned `imports` object is passed straight to
 * `WebAssembly.instantiate` as the `__codetracer` import namespace:
 *
 * ```js
 * const recorder = createBrowserWasmRecorder({ program: "frontend-wasm" });
 * const { instance } = await WebAssembly.instantiate(bytes, {
 *   __codetracer: recorder.imports,
 * });
 * // ... run the module ...
 * recorder.stop();
 * ```
 *
 * @param {BrowserWasmRecorderOptions} [options]
 */
export function createBrowserWasmRecorder(options = {}) {
  const endpoint = resolveEndpoint(options.endpoint);
  const program = options.program ?? DEFAULT_WASM_PROGRAM;
  const manifest = resolveWasmManifest(options.manifest);
  const jsRuntime = resolveJsRuntime(options.jsRuntime);

  const producer = createWebSocketProducer({
    endpoint,
    transportFactory: options.transportFactory,
    flushThreshold: options.flushThreshold,
  });

  let stopped = false;
  // Index of the export currently executing, so a write can be
  // attributed to a position in the recording.
  let lastFnIndex = 0;
  // Exported memory layout, when the module publishes one. See
  // `describeMemoryLayout`.
  let slotNames = options.memoryLayout ?? null;
  // The correlation token counter lives here rather than in the
  // instrumented module so both sides of a crossing observe the same
  // value: the module calls `__ct_correlation_token()` and hands the
  // result straight back to `__ct_emit_realm_boundary`.
  let nextToken = 1n;
  // Per-export bindings a chain crossing out of this module resumes on.
  const returnValueNames = options.returnValueNames ?? {};

  // Seed the session. `SessionStart` must be the first line on the wire
  // (the daemon rejects a duplicate and ignores events before it), and
  // the manifest immediately after so every subsequent `Step` resolves
  // to a real function.
  producer.send({ kind: "SessionStart", program, args: [] });
  if (manifest != null) {
    producer.send({ kind: "Manifest", manifest });
  }

  /**
   * Function name for an export index, per the manifest.
   *
   * @param {number} fnIndex
   * @returns {string}
   */
  function exportName(fnIndex) {
    const fns = /** @type {{functions?: {name?: string}[]}} */ (manifest ?? {})
      .functions;
    return fns?.[fnIndex]?.name ?? "";
  }

  /**
   * Name the ledger slot a written address falls in.
   *
   * A module can publish its layout (see `describeMemoryLayout`), in
   * which case writes are recorded against meaningful names instead of
   * bare addresses. Without a layout the address itself is the name —
   * honest, if less readable. Nothing is invented: an address outside
   * every declared slot keeps its numeric name.
   *
   * @param {number} address
   * @returns {string}
   */
  function resolveSlotName(address) {
    if (slotNames) {
      for (const slot of slotNames) {
        if (address >= slot.start && address < slot.start + slot.size) {
          return slot.name;
        }
      }
    }
    return `mem[${address}]`;
  }

  /**
   * Emit the paired markers for one realm crossing.
   *
   * @param {number} direction `REALM_DIRECTION_ENTER` or `..._LEAVE`.
   * @param {bigint|number} token Shared correlation key.
   * @param {number} fnIndex Export index, for the human-readable label.
   */
  function emitRealmBoundary(direction, token, fnIndex) {
    // The pair index matches on string equality of the key, so both
    // sides must stringify identically. BigInt tokens render without a
    // suffix, which is what the db-backend's decimal-string convention
    // expects.
    const key = String(token);
    const leaving = direction === REALM_DIRECTION_LEAVE;
    // Value flows WASM -> JS when leaving, JS -> WASM when entering.
    const wasmDirection = leaving ? "send" : "recv";
    const jsDirection = leaving ? "recv" : "send";
    const label = `wasm export #${fnIndex}`;

    // Name the crossing binding only on the side that *sends* the
    // value: that is the side a chain resumes its walk on. Naming the
    // receiving side too would point the walk at a binding in the wrong
    // recording.
    const wasmMarker = {
      kind: "CorrelationMarker",
      direction: wasmDirection,
      boundary: JS_WASM_REALM_BOUNDARY,
      key,
      payload: label,
    };
    const returnValueName = returnValueNames[exportName(fnIndex)];
    if (leaving && returnValueName) {
      wasmMarker.showText = returnValueName;
    }
    producer.send(wasmMarker);
    if (jsRuntime) {
      try {
        jsRuntime.markCorrelation(
          jsDirection,
          JS_WASM_REALM_BOUNDARY,
          key,
          label,
        );
      } catch {
        // Recording is best-effort: a failure to mark the JS side must
        // never propagate into the module's execution.
      }
    }
  }

  // --- boundary value capture (M35) ------------------------------------
  //
  // Values arrive one hook call at a time and have to be reassembled
  // into the argument / result tuples they came from. The framing rule
  // is the one the instrumenter documents: the run of value hooks
  // immediately after `__ct_emit_call` is the argument tuple, and the
  // run immediately before `__ct_emit_return` is the result tuple. That
  // is why this buffer exists rather than each hook emitting on its
  // own — a lone value has no way to say which tuple it belongs to.

  /** @type {{slot: number, value: number|string, typeKind: string}[]} */
  let pendingValues = [];
  /** Frame whose `__ct_emit_call` opened the buffered run, if any. */
  let pendingOwner = null;
  /** @type {{fnKind: number, fnIndex: number}[]} */
  let openFrames = [];

  /**
   * Buffer one boundary value.
   *
   * A WASM `i64` arrives as a `BigInt`, and a JS `Number` cannot hold
   * one: every magnitude above 2^53 would be silently rounded, so a
   * ledger balance or a 64-bit handle would be *displayed wrong* with
   * nothing to signal it. It is therefore carried as an exact decimal
   * string under the `BigInt` type kind — the same encoding the JS
   * recorder uses for a JS `BigInt` (`packages/cli/src/record-cmd.ts`),
   * so the receiver needs no new vocabulary. `JSON.stringify` cannot
   * serialise a `BigInt` either, which is the second reason the
   * conversion has to happen here rather than at the producer.
   *
   * @param {number} slot
   * @param {number|bigint} value
   * @param {string} typeKind
   */
  function recordValue(slot, value, typeKind) {
    if (stopped) return;
    const isBig = typeof value === "bigint";
    pendingValues.push({
      slot: slot | 0,
      value: isBig ? value.toString() : value,
      typeKind: isBig ? "BigInt" : typeKind,
    });
  }

  /**
   * Emit the buffered run as bindings of `frame` and return it.
   *
   * @param {{fnKind: number, fnIndex: number}|null|undefined} frame
   * @param {"arg"|"ret"} role
   * @returns {{slot: number, value: number, typeKind: string}[]}
   */
  function flushValues(frame, role) {
    const run = pendingValues;
    pendingValues = [];
    pendingOwner = null;
    if (!frame || run.length === 0) return [];
    if (frame.fnKind === FUNC_KIND_STORE) {
      emitStoreEvent(frame, run);
      return run;
    }
    const label =
      frame.fnKind === FUNC_KIND_EXPORT
        ? exportName(frame.fnIndex)
        : `import #${frame.fnIndex}`;
    for (const entry of run) {
      producer.send({
        kind: "Value",
        name: `${label}:${role}${entry.slot}`,
        value: { value: entry.value, typeKind: entry.typeKind },
      });
    }
    return run;
  }

  /**
   * Render one store-event group.
   *
   * The interior store pass is withdrawn by spec §§ 2 and 11 and is
   * retired in M36; it survives here only so an experiment run with
   * `instrument_stores = true` still produces the same recording it
   * did before the write hook left the surface. Its value tuple is
   * `(addr, old, new)`, and `fn_index` is the store's byte width.
   *
   * @param {{fnKind: number, fnIndex: number}} frame
   * @param {{slot: number, value: number|string, typeKind: string}[]} run
   */
  function emitStoreEvent(frame, run) {
    const address = Number(run[0]?.value ?? 0) >>> 0;
    const oldValue = run[1]?.value ?? 0;
    const newValue = run[2]?.value ?? 0;
    const size = frame.fnIndex >>> 0;
    // A `Step` precedes each write so the recording has a position to
    // stop at, giving the write somewhere to attach.
    producer.send({ kind: "Step", siteId: lastFnIndex });
    producer.send({
      kind: "Value",
      name: resolveSlotName(address),
      // The stored value comes through the `i64` slot of the group, so
      // it carries whatever type kind `recordValue` settled on — never
      // a hard-coded `Int`, which would relabel an exact 64-bit
      // decimal as a number the receiver would try to narrow.
      value: { value: newValue, typeKind: run[2]?.typeKind ?? "Int" },
    });
    // The previous value goes to the event log rather than the state
    // pane: it is history, not current state, and showing both as
    // bindings would make every slot appear twice.
    producer.send({
      kind: "Write",
      channel: "wasm-memory",
      content:
        `store ${resolveSlotName(address)} (addr=${address}, ${size}B): ` +
        `${String(oldValue)} -> ${String(newValue)}`,
    });
  }

  return {
    /** The `__codetracer` import namespace for `WebAssembly.instantiate`. */
    imports: {
      /**
       * Boundary value hooks — the module's value stream (spec § 3).
       *
       * One hook per WASM value type, because WASM has no polymorphic
       * call and widening `f32` would not preserve NaN payloads. Each
       * carries its position within the tuple it belongs to.
       */
      __ct_emit_i32(slot, value) {
        recordValue(slot, value | 0, "Int");
      },
      __ct_emit_i64(slot, value) {
        recordValue(slot, value, "Int");
      },
      __ct_emit_f32(slot, value) {
        recordValue(slot, value, "Float");
      },
      __ct_emit_f64(slot, value) {
        recordValue(slot, value, "Float");
      },

      /**
       * Function-entry hook. Emits a `Step` (so the recording has a
       * position to stop at) followed by a `Call` frame.
       *
       * `fnIndex` doubles as the `siteId` and the `fnId` because the
       * manifest's `sites` and `functions` arrays are parallel — see
       * `ModuleManifest::from_module`.
       */
      __ct_emit_call(fnKind, fnIndex) {
        if (stopped) return;
        // A run still buffered when a new group opens belongs to the
        // group that was open before it.
        flushValues(
          pendingOwner ?? openFrames[openFrames.length - 1],
          pendingOwner ? "arg" : "ret",
        );
        const frame = { fnKind: fnKind | 0, fnIndex: fnIndex >>> 0 };
        openFrames.push(frame);
        pendingOwner = frame;
        // Imported-call hooks describe the module calling *out*; those
        // are recorded as the boundary markers below rather than as
        // frames of this recording. Store groups are not calls at all.
        if (frame.fnKind !== FUNC_KIND_EXPORT) return;
        lastFnIndex = frame.fnIndex;
        producer.send({ kind: "Step", siteId: frame.fnIndex });
        producer.send({ kind: "Call", fnId: frame.fnIndex, args: [] });
      },

      /** Function-exit hook, closing the frame opened by the call hook. */
      __ct_emit_return(fnKind, fnIndex) {
        if (stopped) return;
        const frame = openFrames.pop();
        // If the frame's own `__ct_emit_call` is still the owner of the
        // buffered run, nothing separated it from the call — so it is
        // the argument tuple of a boundary with no results. Decide
        // before flushing, which clears the owner.
        const isArgs = frame != null && frame === pendingOwner;
        const flushed = flushValues(frame, isArgs ? "arg" : "ret");
        if ((fnKind | 0) !== FUNC_KIND_EXPORT) return;
        const returned =
          !isArgs && flushed.length === 1
            ? { value: flushed[0].value, typeKind: flushed[0].typeKind }
            : { value: null, typeKind: "None" };
        producer.send({
          kind: "Return",
          fnId: fnIndex >>> 0,
          returnValue: returned,
        });
      },

      /** Realm-crossing hook — marks both sides of the boundary. */
      __ct_emit_realm_boundary(direction, _fnKind, fnIndex, token) {
        if (stopped) return;
        // The realm marker sits between the argument tuple and the
        // call, so anything buffered here is that tuple.
        flushValues(pendingOwner, "arg");
        emitRealmBoundary(direction | 0, token, fnIndex >>> 0);
      },

      /** Strictly monotonic correlation key source. */
      __ct_correlation_token() {
        const t = nextToken;
        nextToken += 1n;
        return t;
      },
    },

    /**
     * Attach a memory layout discovered after instantiation.
     *
     * The layout has to come from the instantiated module (it reports
     * its own base address), which is necessarily after the recorder
     * was constructed — hence a setter rather than a constructor
     * option. Writes recorded before this point keep their address
     * names; in practice nothing runs between instantiation and this
     * call.
     */
    setMemoryLayout(layout) {
      slotNames = layout ?? null;
    },

    /** Force any buffered events onto the wire. */
    flush() {
      producer.flush();
    },

    /**
     * End the recording: emits `SessionEnd` so the daemon finalises the
     * `.ct`, then closes the socket. Idempotent.
     */
    stop() {
      if (stopped) return;
      producer.send({ kind: "SessionEnd" });
      producer.flush();
      stopped = true;
      producer.close();
    },

    /** Endpoint actually in use (introspection / tests). */
    get endpoint() {
      return endpoint;
    },
  };
}

export default createBrowserWasmRecorder;
