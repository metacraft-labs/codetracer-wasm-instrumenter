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
 * Group header of the withdrawn interior store pass.
 *
 * Retired in M36: `PipelineConfig::instrument_stores` is off by
 * default and this runtime records nothing for such a group. The
 * constant survives because the pass is still reachable behind the
 * flag, so a module instrumented for that experiment can still call
 * these hooks — and a group this runtime did not recognise would be
 * mis-framed as a boundary tuple, putting invented `import #4:arg0`
 * bindings into the recording. Recognising it is what lets it be
 * *ignored*.
 *
 * Spec §§ 2 and 11 explain why the interior model is withdrawn: it
 * cannot be complete (locals and operand-stack values have no
 * address), and it was measured at +2955 % runtime against +11 % for
 * boundary capture.
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
 * Name of the binding a boundary value is recorded under.
 *
 * The tuple element's position is part of the name because that is
 * all the recording can honestly say about it: WebAssembly boundary
 * signatures are positional and carry no parameter names, so
 * `compute_balance:arg1` is a fact while `compute_balance:amount`
 * would be a guess. A consumer that wants source-level names gets
 * them from the materialised trace the replayer produces (spec § 6),
 * which has DWARF in hand.
 *
 * @param {string} label Function label — an export name, or
 *   `import #<n>` for the import edge.
 * @param {"arg"|"ret"} role Which side of the crossing.
 * @param {number} slot Positional index within the tuple.
 * @returns {string}
 */
export function boundaryBindingName(label, role, slot) {
  return `${label}:${role}${slot}`;
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
 * @property {Record<string, string>} [returnValueNames] Per-export
 *   override of the binding an origin chain resumes on when it crosses
 *   *into* this recording.
 *
 *   Rarely needed. The default is the export's own result binding —
 *   `<export>:ret0`, the name this runtime records the crossing value
 *   under — which is the right answer whenever the value that crossed
 *   is the value the function returned. That is the WebAssembly
 *   boundary contract, so the module needs no annotation at all: it is
 *   the difference between this and `__ct.markCorrelation` on the
 *   JavaScript side, where a program can hand any of its bindings
 *   across and so has to name one.
 *
 *   An override is for the case where the return value is not the
 *   interesting one — a function returning a status code that writes
 *   its real output somewhere else, say. Keyed per export, because
 *   exports return different things and naming the wrong binding sends
 *   a chain crossing at one function looking for another's value.
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
  // Site of the innermost export frame, and so the only position this
  // recording can attribute anything to: an import call made from
  // inside it happens *at* that site, and the module reports no
  // finer-grained position.
  let currentSiteId = 0;
  // The correlation token counter lives here rather than in the
  // instrumented module so both sides of a crossing observe the same
  // value: the module calls `__ct_correlation_token()` and hands the
  // result straight back to `__ct_emit_realm_boundary`.
  let nextToken = 1n;
  // Per-export overrides of the binding a chain crossing out of this
  // module resumes on. Empty is the normal case — see the option docs.
  const returnValueNames = options.returnValueNames ?? {};
  // Binding name the most recent result run of each export was
  // recorded under, keyed by export index. Read when the `LEAVE`
  // marker fires, which the instrumenter emits *after* the result
  // tuple (spec § 5), so the name is always already known.
  /** @type {Map<number, string>} */
  const lastResultBinding = new Map();

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
    // The value that crossed outward is the export's result, and this
    // runtime has just recorded it under a name of its own choosing —
    // so it can name the crossing binding itself rather than asking
    // the module to declare one. An explicit override still wins.
    //
    // A `LEAVE` with no result run leaves this unset, which is
    // correct: a `-> ()` export sent no value, and pointing the walk
    // at a binding that does not exist would make an empty
    // continuation look like a failed lookup.
    const returnValueName =
      returnValueNames[exportName(fnIndex)] ?? lastResultBinding.get(fnIndex);
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
   * A `FUNC_KIND_STORE` group is dropped without a trace, which is
   * the whole of M36's browser-side change: the interior model is
   * withdrawn (spec §§ 2, 11) and the browser pipeline records
   * nothing from it. The run is still consumed rather than left in
   * the buffer, because a buffer carried across a group boundary
   * would reappear as somebody else's argument tuple.
   *
   * @param {{fnKind: number, fnIndex: number}|null|undefined} frame
   * @param {"arg"|"ret"} role
   * @returns {{slot: number, value: number|string, typeKind: string}[]}
   */
  function flushValues(frame, role) {
    const run = pendingValues;
    pendingValues = [];
    pendingOwner = null;
    if (!frame || run.length === 0) return [];
    if (frame.fnKind === FUNC_KIND_STORE) return [];
    const label =
      frame.fnKind === FUNC_KIND_EXPORT
        ? exportName(frame.fnIndex)
        : `import #${frame.fnIndex}`;
    // Each tuple gets a step of its own, and it has to: an origin walk
    // finds a binding's write by looking for the step where its value
    // first appears, which means there must be an earlier step in the
    // same frame where it did not. Arguments and results recorded onto
    // the frame's single entry step would be indistinguishable from
    // values that were always there, and the walk would run off the
    // start of the recording instead of landing on the module.
    producer.send({ kind: "Step", siteId: currentSiteId });
    for (const entry of run) {
      const name = boundaryBindingName(label, role, entry.slot);
      const isExportResult =
        role === "ret" && frame.fnKind === FUNC_KIND_EXPORT && entry.slot === 0;
      if (isExportResult) {
        // Remember the binding the outbound value landed in, so the
        // `LEAVE` marker that follows can name it without the module
        // having to declare it. Slot 0 because a chain follows one
        // value, and the first result is the one a single-value
        // return — every case a C-ABI `cdylib` can produce — puts it
        // in.
        lastResultBinding.set(frame.fnIndex, name);
      }
      producer.send({
        kind: "Value",
        name,
        value: { value: entry.value, typeKind: entry.typeKind },
      });
    }
    return run;
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
        currentSiteId = frame.fnIndex;
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
