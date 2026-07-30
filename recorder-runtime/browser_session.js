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
import {
  diffRegions,
  encodeRegions,
  encodeGlobalValue,
  snapshotMemory,
  readMemory,
  pagesOf,
} from "./host_state.js";

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
 * @property {HostMemoryDescriptor[]} [hostMemories] Memories the module
 *   **imports** from this host, whose contents are therefore
 *   host-supplied state the recording has to carry (spec §3.3 / §3.4).
 *   A module that defines its own memory needs none of this — the
 *   `.wasm` already contains it — which is why the default is empty.
 *   Equivalent to calling {@link trackHostMemory} for each entry, and
 *   subject to the same timing rule: see that method.
 * @property {HostGlobalDescriptor[]} [hostGlobals] Globals the module
 *   imports from this host. Same contract as `hostMemories`.
 */

/**
 * @typedef {Object} HostMemoryDescriptor
 * @property {WebAssembly.Memory} memory The live memory handed to
 *   `WebAssembly.instantiate`.
 * @property {string} [module] Import module name; defaults to `"env"`,
 *   which is what `rust-lld --import-memory` and every C/C++ toolchain
 *   emit.
 * @property {string} [name] Import field name; defaults to `"memory"`.
 * @property {number} [maxPages] Declared maximum, when the page created
 *   the memory with one. Recorded because spec §7 makes `memory.grow`'s
 *   result depend on the host limit, so a replay that does not know the
 *   limit can diverge on a `grow` that failed in the browser.
 */

/**
 * @typedef {Object} HostGlobalDescriptor
 * @property {WebAssembly.Global} global The live global.
 * @property {string} name Import field name. Required: unlike a memory
 *   there is no conventional default.
 * @property {"i32"|"i64"|"f32"|"f64"} type The global's value type. The
 *   `WebAssembly.Global` object does not portably report it, and
 *   guessing from the JS value cannot tell `i32` from `f32`.
 * @property {string} [module] Import module name; defaults to `"env"`.
 * @property {boolean} [mutable] Whether the module may write it —
 *   which is also what decides whether a §3.4 mutation may target it.
 *   Defaults to false.
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

  // --- host-supplied state (spec §3.3 / §3.4) ---------------------------
  //
  // See `host_state.js` for *why* this is captured by snapshot-and-diff
  // from the host side rather than by hooks inside the module. What is
  // decided here is the two capture **windows**, and they are what make
  // the diff exact rather than a guess:
  //
  //   §3.3  from the moment the memory is registered, to the module's
  //         first exported call. The `__ct_emit_call` hook of an export
  //         runs before any instruction of its body, so nothing the
  //         *module* wrote can be inside this window — everything in it
  //         was put there by the host or by the module's own data
  //         segments, and the data segments are in the `.wasm`, so
  //         registering after `WebAssembly.instantiate` (the natural
  //         time, since the memory is already built) makes the record
  //         exactly the host's contribution and nothing else.
  //
  //   §3.4  from `__ct_emit_call(IMPORT, n)` to `__ct_emit_return(IMPORT,
  //         n)`. Between those two hooks the module is *suspended* inside
  //         the host call: the only code that runs is the host's. So the
  //         diff over this window is precisely the host's writes, with no
  //         need to distinguish them from the module's — the module made
  //         none. That is the whole reason §3.4 is anchored to an
  //         imported call rather than sampled on a timer.
  //
  // Nothing is captured at all unless a memory or global is registered,
  // which is the common case: a module that defines its own memory needs
  // neither record.
  //
  // # What the windows rest on, and what closes the remaining hole
  //
  // Both windows are exact only because the *module* cannot run inside
  // them. That is guaranteed by the instrumenter's emitted code — the
  // export prologue is spliced at index 0 of the entry block, and the
  // only module code between an import's two hooks is `local.set` /
  // `local.get` spilling, which touches no linear memory. There is
  // exactly one way for a module store to land inside a §3.4 window: the
  // host function calls *back* into an exported function. Such a
  // recording carries an export crossing at a non-zero depth, and the
  // consumer **refuses** it outright (`replay.go`'s
  // `refuseNestedExports`, on both the batch and the streaming path), so
  // a host/module write to overlapping addresses cannot reach a
  // materialised trace. It is refused rather than mis-attributed, which
  // is the same spec §8 discipline as the two refusals below — it just
  // happens to be enforced on the consumer's side.

  /** @type {import("./host_state.js").TrackedMemory[]} */
  const trackedMemories = [];
  /** @type {import("./host_state.js").TrackedGlobal[]} */
  const trackedGlobals = [];
  /** Whether the §3.3 record has been emitted. */
  let initialStateSent = false;
  /**
   * Host writes seen at a point the record cannot express — see
   * `checkForUnrepresentableHostWrites`. Exposed for tests and for the
   * fixture scripts, which assert it is zero.
   */
  let unrepresentableHostWrites = 0;
  /**
   * Imported calls during which the host wrote, but whose crossing the
   * recording does not carry (a `() -> ()` import leaves no value run,
   * so the replayer recovers no crossing to anchor a mutation to).
   */
  let unanchorableHostWrites = 0;
  /**
   * Registrations that arrived too late to define a §3.3 window.
   *
   * The baseline is taken at registration, so a memory registered after
   * the module has already run puts whatever the *module* wrote into the
   * baseline and, worse, makes the §3.3 record describe a state that is
   * not the one preceding the first exported call. Both directions are
   * silent losses, which is why this is counted rather than tolerated.
   */
  let lateRegistrations = 0;
  /** Whether a top-level exported call has already been observed. */
  let firstExportSeen = false;

  // --- crossing sequence numbers ---------------------------------------
  //
  // A §3.4 mutation is anchored by `afterCrossing`, which is the
  // `Crossing.Seq` the *consumer* assigns while recovering crossings from
  // this recording's rendered records. So the number has to be predicted
  // here, and it is predicted by mirroring the consumer's rule rather
  // than by inventing one:
  //
  //   * `internal/boundarylog/assembler.go` appends an export crossing
  //     when the `Call` record arrives, and an import crossing when that
  //     import's value run *closes*. `Seq` is the append order.
  //   * This runtime emits the `Call` record in `__ct_emit_call`, after
  //     flushing whatever run was pending — so a pending import run is
  //     numbered before the export, exactly as over there.
  //   * An import's argument run is flushed at its realm marker, before
  //     the host call; its result run at `__ct_emit_return`. An import
  //     with neither contributes no crossing at all, on both sides.
  //
  // The prediction is not left to inspection: the fixture in
  // `codetracer/src/db-backend/tests/fixtures/wasm-memory-calldata/`
  // replays a real browser recording whose module reads the mutated
  // bytes, so an anchor off by one produces a divergence rather than a
  // subtly wrong trace.

  /** Next `Crossing.Seq` the consumer will assign. */
  let nextCrossingSeq = 0;
  /**
   * The import crossing the consumer currently holds open, if any.
   * Mirrors `assembler.stack`'s import entries.
   * @type {{label: string, seq: number}|null}
   */
  let openImportCrossing = null;
  /**
   * Seq of the crossing the imported call in flight will be recovered
   * as, or null while that is not yet decided (or never will be).
   * @type {number|null}
   */
  let inFlightImportSeq = null;

  /**
   * Register an imported memory whose contents are host-supplied.
   *
   * **Call this right after `WebAssembly.instantiate` and before the
   * first exported call.** The memory's contents at this moment become
   * the baseline the §3.3 record is a diff against, so registering after
   * instantiation excludes the module's own data segments (which the
   * replayer applies from the `.wasm` itself) and records only what the
   * host put there. Registering *before* instantiation is also correct,
   * just larger: the baseline is then an all-zero memory and the record
   * carries every non-zero byte, data segments included, which the
   * replayer rewrites over identical bytes.
   *
   * @param {HostMemoryDescriptor} descriptor
   */
  function trackHostMemory(descriptor) {
    const memory = descriptor.memory;
    if (memory == null || typeof memory.buffer === "undefined") {
      throw new TypeError(
        "trackHostMemory needs the WebAssembly.Memory object the module imports",
      );
    }
    noteRegistrationTiming("trackHostMemory");
    trackedMemories.push({
      module: descriptor.module ?? "env",
      name: descriptor.name ?? "memory",
      memory,
      maxPages: descriptor.maxPages ?? null,
      baseline: snapshotMemory(memory),
      shadow: null,
    });
  }

  /**
   * Register an imported global whose value is host-supplied.
   *
   * @param {HostGlobalDescriptor} descriptor
   */
  function trackHostGlobal(descriptor) {
    if (descriptor == null || descriptor.global == null) {
      throw new TypeError(
        "trackHostGlobal needs the WebAssembly.Global object the module imports",
      );
    }
    if (!descriptor.name) {
      throw new TypeError("trackHostGlobal needs the global's import name");
    }
    if (!/^[if](32|64)$/.test(descriptor.type)) {
      throw new TypeError(
        `trackHostGlobal: unsupported global type ${JSON.stringify(descriptor.type)}; ` +
          "only i32/i64/f32/f64 can cross a recorded boundary",
      );
    }
    noteRegistrationTiming("trackHostGlobal");
    trackedGlobals.push({
      module: descriptor.module ?? "env",
      name: descriptor.name,
      type: descriptor.type,
      mutable: descriptor.mutable === true,
      global: descriptor.global,
      baseline: encodeGlobalValue(descriptor.global.value),
      shadow: null,
    });
  }

  /**
   * Report a registration that arrived after the §3.3 window had closed.
   *
   * §3.3 is *the state before the first exported call*. Registering after
   * one has run makes the baseline a mid-execution state, so the record
   * would silently omit whatever the host staged before that call and
   * would be applied by the replayer at a point it never described. Like
   * the two refusals below, this is reported at the cause instead of
   * surfacing as a divergence somewhere unrelated (spec §8).
   *
   * It is not thrown: recording is best-effort and must never take a
   * page down. The counter is what a fixture harness fails on.
   *
   * @param {string} api Name of the entry point, for the diagnostic.
   */
  function noteRegistrationTiming(api) {
    if (!firstExportSeen) return;
    lateRegistrations += 1;
    // eslint-disable-next-line no-console
    console.error(
      `[codetracer] ${api} was called after the module's first exported ` +
        "call. Spec §3.3 is the host-supplied state that preceded that " +
        "call, and the baseline this takes now is a mid-execution state, " +
        "so anything the host staged earlier is lost and what is recorded " +
        "describes a moment the replayer has no hook for. Register every " +
        "imported memory and global immediately after " +
        "`WebAssembly.instantiate` and before calling into the module.",
    );
  }

  /** Whether anything host-supplied is being tracked at all. */
  function tracksHostState() {
    return trackedMemories.length > 0 || trackedGlobals.length > 0;
  }

  /** Take the "window open" snapshot of every tracked entity. */
  function openHostStateWindow() {
    for (const m of trackedMemories) m.shadow = snapshotMemory(m.memory);
    for (const g of trackedGlobals) {
      g.shadow = encodeGlobalValue(g.global.value);
    }
  }

  /**
   * Close the window and return what the host changed inside it.
   *
   * @returns {{memoryWrites: object[], globalSets: object[]}}
   */
  function closeHostStateWindow() {
    /** @type {object[]} */
    const memoryWrites = [];
    for (const m of trackedMemories) {
      const regions = diffRegions(m.shadow, readMemory(m.memory));
      m.shadow = null;
      for (const r of encodeRegions(regions)) {
        memoryWrites.push({ module: m.module, name: m.name, ...r });
      }
    }
    /** @type {object[]} */
    const globalSets = [];
    for (const g of trackedGlobals) {
      const now = encodeGlobalValue(g.global.value);
      const was = g.shadow;
      g.shadow = null;
      if (was !== null && was !== now) {
        globalSets.push({
          module: g.module,
          name: g.name,
          type: g.type,
          value: now,
        });
      }
    }
    return { memoryWrites, globalSets };
  }

  /**
   * Emit the spec §3.3 record, once, immediately before the module's
   * first exported call.
   */
  function sendInitialState() {
    initialStateSent = true;
    const memories = trackedMemories.map((m) => {
      const current = readMemory(m.memory);
      const record = {
        module: m.module,
        name: m.name,
        minPages: pagesOf(m.memory),
        maxPages: m.maxPages,
        data: encodeRegions(diffRegions(m.baseline, current)),
      };
      // The baseline's job is done; from here the same field carries the
      // "state at the last quiescent point", which is what makes a host
      // write between two exported calls detectable.
      m.baseline = new Uint8Array(current);
      return record;
    });
    const globals = trackedGlobals.map((g) => {
      const value = encodeGlobalValue(g.global.value);
      g.shadow = null;
      // Same handover as a memory's: from here the field carries the
      // value at the last quiescent point, so a host assignment made
      // between two exported calls is detectable rather than lost.
      g.baseline = value;
      return {
        module: g.module,
        name: g.name,
        type: g.type,
        mutable: g.mutable,
        value,
      };
    });
    producer.send({ kind: "HostInitialState", memories, globals });
  }

  /**
   * Report a host write the record cannot place.
   *
   * The two records the spec defines are anchored: §3.3 is "before the
   * first exported call" and §3.4 is "while servicing crossing N". A host
   * write made *between* two top-level exported calls is neither, and
   * there is no third anchor — the replayer applies initial state once
   * and mutations inside import stubs, and has no hook between calls.
   *
   * Dropping it silently is the failure spec §8 exists to prevent: the
   * replay would proceed and diverge later, at a point unrelated to the
   * cause. So it is reported here, at the cause, and counted so the
   * page's own harness can fail on it.
   *
   * Globals are checked alongside memories and for the same reason. An
   * imported global the host reassigns between two calls is the same
   * unanchorable write in a smaller container: the replayer sets a
   * provider global once from the §3.3 record and thereafter only from a
   * §3.4 mutation, so a reassignment made at neither point is simply
   * never applied.
   */
  function checkForUnrepresentableHostWrites() {
    for (const m of trackedMemories) {
      const regions = diffRegions(m.baseline, readMemory(m.memory));
      m.baseline = snapshotMemory(m.memory);
      if (regions.length === 0) continue;
      unrepresentableHostWrites += regions.length;
      const where = regions
        .map((r) => `${r.offset}..${r.offset + r.bytes.length}`)
        .join(", ");
      reportUnrepresentableWrite(
        `host wrote to imported memory ${m.module}.${m.name} ` +
          `between two top-level exported calls (${where})`,
      );
    }
    for (const g of trackedGlobals) {
      const now = encodeGlobalValue(g.global.value);
      const was = g.baseline;
      g.baseline = now;
      if (was === null || was === now) continue;
      unrepresentableHostWrites += 1;
      reportUnrepresentableWrite(
        `host assigned imported global ${g.module}.${g.name} ` +
          `between two top-level exported calls (${was} -> ${now})`,
      );
    }
  }

  /**
   * The shared half of the "no anchor exists for this" diagnostic.
   *
   * @param {string} what The write, described at its cause.
   */
  function reportUnrepresentableWrite(what) {
    // eslint-disable-next-line no-console
    console.error(
      `[codetracer] ${what}. The boundary record has no anchor for such a ` +
        "write — spec §3.3 covers only what preceded the FIRST exported " +
        "call and §3.4 only what the host wrote while servicing an " +
        "imported call — so the replay will not see it and will diverge. " +
        "Move the write inside a host function the module calls, or make " +
        "it part of the state that precedes the first call.",
    );
  }

  /** Refresh the "state at the last quiescent point" baseline. */
  function noteQuiescentPoint() {
    for (const m of trackedMemories) m.baseline = snapshotMemory(m.memory);
    for (const g of trackedGlobals) {
      g.baseline = encodeGlobalValue(g.global.value);
    }
  }

  /**
   * Close the §3.4 window opened by an imported call and emit what the
   * host changed inside it.
   *
   * @param {number} fnIndex Import index, for the diagnostic only.
   */
  function sendHostMutation(fnIndex) {
    const { memoryWrites, globalSets } = closeHostStateWindow();
    if (memoryWrites.length === 0 && globalSets.length === 0) return;
    if (inFlightImportSeq === null) {
      // The import contributed no boundary values, so the recording
      // carries no crossing for it and there is nothing to anchor to.
      // (The realm markers are on disk, but they use the same label
      // template for both edges, so the consumer cannot attribute them
      // to an import — see `recording.go`'s "How a crossing appears in a
      // browser `.ct`".) Say so rather than dropping the write.
      unanchorableHostWrites += memoryWrites.length + globalSets.length;
      // eslint-disable-next-line no-console
      console.error(
        `[codetracer] the host wrote to imported state while servicing ` +
          `import #${fnIndex}, but that import's signature carries no ` +
          "boundary values, so the recording holds no crossing to anchor " +
          "the write to (spec §3.4 anchors a mutation to the crossing it " +
          "accompanied). The replay will not see it and will diverge. " +
          "Give the import at least one argument or result.",
      );
      return;
    }
    producer.send({
      kind: "HostMutation",
      afterCrossing: inFlightImportSeq,
      memoryWrites,
      globalSets,
    });
    inFlightImportSeq = null;
  }

  // Seed the session. `SessionStart` must be the first line on the wire
  // (the daemon rejects a duplicate and ignores events before it), and
  // the manifest immediately after so every subsequent `Step` resolves
  // to a real function.
  producer.send({ kind: "SessionStart", program, args: [] });
  if (manifest != null) {
    producer.send({ kind: "Manifest", manifest });
  }

  for (const descriptor of options.hostMemories ?? []) {
    trackHostMemory(descriptor);
  }
  for (const descriptor of options.hostGlobals ?? []) {
    trackHostGlobal(descriptor);
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
    noteRunForCrossingSeq(frame, role, label);
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

  /**
   * Advance the mirrored crossing counter for a value run about to be
   * emitted.
   *
   * This is the whole of the prediction described above; keeping it in
   * one function is deliberate, because a second copy of the consumer's
   * rule is exactly how the two would drift apart.
   *
   * @param {{fnKind: number, fnIndex: number}} frame
   * @param {"arg"|"ret"} role
   * @param {string} label
   */
  function noteRunForCrossingSeq(frame, role, label) {
    if (frame.fnKind !== FUNC_KIND_IMPORT) {
      // `assembler.closeRun` calls `closeDanglingImports("")` for any run
      // that is not an open import's own result run, so an export's run
      // closes whatever import was open.
      openImportCrossing = null;
      return;
    }
    if (role === "arg") {
      // An argument run opens an import crossing — including a *second*
      // argument run for the same import, which is a new call.
      openImportCrossing = { label, seq: nextCrossingSeq++ };
      inFlightImportSeq = openImportCrossing.seq;
      return;
    }
    if (openImportCrossing !== null && openImportCrossing.label === label) {
      inFlightImportSeq = openImportCrossing.seq;
    } else {
      // A result run with no matching open crossing is an import that
      // took no arguments: its only trace on disk is this run.
      inFlightImportSeq = nextCrossingSeq++;
    }
    // Either way the result run closes the crossing.
    openImportCrossing = null;
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
        const topLevel = openFrames.length === 0;
        openFrames.push(frame);
        pendingOwner = frame;
        // Imported-call hooks describe the module calling *out*; those
        // are recorded as the boundary markers below rather than as
        // frames of this recording. Store groups are not calls at all.
        if (frame.fnKind !== FUNC_KIND_EXPORT) {
          if (frame.fnKind === FUNC_KIND_IMPORT && tracksHostState()) {
            // Open the §3.4 window. Nothing but the host runs between
            // here and the matching return hook, so whatever changes in
            // between is the host's doing by construction.
            inFlightImportSeq = null;
            openHostStateWindow();
          }
          return;
        }
        if (tracksHostState() && topLevel) {
          // This hook runs before any instruction of the export's body,
          // so the memory still holds exactly what the host supplied.
          if (!initialStateSent) {
            sendInitialState();
          } else {
            checkForUnrepresentableHostWrites();
          }
        }
        if (topLevel) firstExportSeen = true;
        openImportCrossing = null;
        nextCrossingSeq++;
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
        if ((fnKind | 0) !== FUNC_KIND_EXPORT) {
          if ((fnKind | 0) === FUNC_KIND_IMPORT && tracksHostState()) {
            sendHostMutation(fnIndex >>> 0);
          }
          return;
        }
        // An export's `Return` closes any import the consumer still
        // holds open (`assembler.push`'s Return arm).
        openImportCrossing = null;
        if (tracksHostState() && openFrames.length === 0) {
          // Back at a quiescent point: re-baseline so a host write made
          // before the next top-level call is detected rather than
          // confused with what the module just did.
          noteQuiescentPoint();
        }
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

    /** Register an imported memory — see the inner docs. */
    trackHostMemory,
    /** Register an imported global — see the inner docs. */
    trackHostGlobal,

    /**
     * Host writes this recording could not place, by category.
     *
     * Every counter is zero for a well-formed page. A non-zero one means
     * the recording is missing an input and its replay will diverge; a
     * harness that regenerates a fixture should fail on any of them
     * rather than commit a recording that cannot be replayed. Check them
     * as a set — `Object.values(...).some((n) => n !== 0)` — so a counter
     * added later is not silently ignored.
     */
    get hostStateDiagnostics() {
      return {
        unrepresentableWrites: unrepresentableHostWrites,
        unanchorableWrites: unanchorableHostWrites,
        lateRegistrations,
      };
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
