// codetracer-wasm-instrumenter / recorder-runtime / host_state
//
// Capture of **host-supplied state**: spec §3.3 (what the host had put
// into the module's imported memory and globals before the first
// exported call) and spec §3.4 (what the host wrote while servicing an
// imported call).
//
// # Why this lives on the host side and not in the bytecode rewrite
//
// Everything else `ct-instrument` records is reported by the module
// itself, through hooks spliced into its own code. §3.3 and §3.4 cannot
// be, and the reason is not an implementation gap:
//
//     const view = new Uint8Array(memory.buffer);
//     view.set(calldata, ptr);            // <- the host writing
//     instance.exports.settle(0);
//
// That `set` happens in **JavaScript**, outside the module. No
// instruction of the module executes; there is nothing for a rewriter to
// splice a hook into. `ArrayBuffer` writes are not interceptable either —
// a `Proxy` around the `Memory` object does not see writes made through a
// `Uint8Array` or `DataView` the page already holds, and the page
// routinely makes a fresh view per write. So the only vantage point that
// can observe a host write at all is the **host-side runtime**, and the
// only thing it can do from there is compare the memory now against the
// memory as it was.
//
// That is what this module does, and it is why the capture is
// snapshot-and-diff rather than an event stream. The *windows* are what
// make it exact rather than approximate — see `browser_session.js`, which
// owns them.
//
// # What is recorded for a host write: byte-exact runs, not pages
//
// A `MemoryWrite` on the consumer side
// (`codetracer-wasm-recorder/internal/boundarylog/hoststate.go`) is
// `(module, name, offset, bytes)` and is applied with
// `api.Memory.Write(offset, bytes)`. There is no alignment requirement to
// satisfy, so there is nothing to be gained by rounding a write out to a
// page boundary — and two things to lose:
//
//   * Padding to 64 KiB pages would put bytes into the record that the
//     host never wrote. They would be *reapplied* on replay. Today they
//     would be the module's own bytes and reapplying them is a no-op, but
//     it makes the record say something untrue about who wrote what, and
//     it is exactly the sort of overlap that hides a producer bug: a run
//     whose extent is computed wrongly is invisible behind the padding.
//   * A page is 64 KiB. A four-byte fee written into a calldata slot
//     would be recorded as 65536 bytes, base64'd into ~87 KB of JSON,
//     per crossing.
//
// So runs are byte-exact, with one deliberate relaxation: two differing
// runs separated by at most {@link DEFAULT_COALESCE_GAP} identical bytes
// are merged. A struct written field by field would otherwise become one
// region per field; the gap bytes are unchanged, so writing them back is
// a no-op, and the merge is bounded so a sparse memory can never be
// coalesced into one enormous region.
//
// "A no-op" is worth stating precisely, because it is the one place this
// record carries a byte the host did not write. The bridged bytes are by
// construction identical at both ends of the window, and the replayer
// applies the region at the same point in the same deterministic
// re-execution, so it overwrites them with what is already there. The
// case where it would *not* be a no-op is a replay that had already
// diverged before this point — and there the bridge would paper over up
// to `DEFAULT_COALESCE_GAP` bytes of that divergence rather than letting
// it surface. That is the same objection this file raises against
// page-granular padding, at 64 bytes instead of 65536; the bound is what
// makes it a considered trade rather than the same mistake.

/**
 * Identical bytes tolerated inside a single recorded region.
 *
 * Small on purpose. The cost of merging across a gap is that `gap` bytes
 * the host did not write are carried in the record and rewritten on
 * replay; the cost of not merging is one region — and one base64 header,
 * offset and JSON object — per contiguous run. 64 bytes is about where
 * the second cost overtakes the first for the shapes this exists for
 * (a `#[repr(C)]` calldata struct, a length-prefixed byte buffer).
 */
export const DEFAULT_COALESCE_GAP = 64;

/** Bytes in one WebAssembly page. */
export const WASM_PAGE_BYTES = 65536;

/**
 * Byte ranges in which `after` differs from `before`.
 *
 * `before` may be `null`, which means "an all-zero memory" — the state a
 * freshly constructed `WebAssembly.Memory` is in. That is not a special
 * case in the caller: registering a memory before instantiation captures
 * a zero baseline and the same diff then yields every non-zero byte,
 * which is precisely the "non-zero regions of the memory's initial
 * contents" the consumer's schema describes.
 *
 * Growth is handled by the same rule: `after` may be longer than
 * `before`, and the tail is compared against zero because that is what a
 * grown page contains.
 *
 * Shrinking cannot happen (WebAssembly memories only grow), so a shorter
 * `after` is treated as a truncated comparison rather than as deletions —
 * there is no way to express "these bytes went away" in the record, and
 * inventing one would be worse than the diagnostic the caller gets when
 * the replay diverges.
 *
 * @param {Uint8Array|null} before Baseline contents, or null for zeros.
 * @param {Uint8Array} after Current contents.
 * @param {{coalesceGap?: number}} [options]
 * @returns {{offset: number, bytes: Uint8Array}[]} Regions in ascending
 *   offset order; never overlapping, never empty.
 */
export function diffRegions(before, after, options = {}) {
  const gap = options.coalesceGap ?? DEFAULT_COALESCE_GAP;
  /** @type {{offset: number, bytes: Uint8Array}[]} */
  const regions = [];
  const baselineLength = before === null ? 0 : before.length;

  /** Start of the run being accumulated, or -1 when none is open. */
  let runStart = -1;
  /** Offset one past the last *differing* byte seen in the open run. */
  let runEnd = -1;

  const closeRun = () => {
    if (runStart < 0) return;
    regions.push({ offset: runStart, bytes: after.slice(runStart, runEnd) });
    runStart = -1;
    runEnd = -1;
  };

  for (let i = 0; i < after.length; i++) {
    const differs =
      i >= baselineLength
        ? after[i] !== 0
        : /** @type {Uint8Array} */ (before)[i] !== after[i];
    if (!differs) continue;
    if (runStart < 0) {
      runStart = i;
    } else if (i - runEnd > gap) {
      // The identical stretch is wider than the tolerance: close the
      // open run rather than carrying the untouched bytes.
      closeRun();
      runStart = i;
    }
    runEnd = i + 1;
  }
  closeRun();
  return regions;
}

/**
 * Base64-encode bytes with the standard alphabet and padding.
 *
 * The consumer decodes with Go's `base64.StdEncoding`, which requires
 * padding, so the URL-safe alphabet and the unpadded forms are both
 * wrong here.
 *
 * `btoa` is used where it exists (every browser) and `Buffer` otherwise
 * (Node, for the unit tests). The chunking matters: `String.fromCharCode`
 * applied to a whole megabyte-sized memory region overflows the argument
 * limit and throws, which would turn a large host write into a lost
 * recording.
 *
 * @param {Uint8Array} bytes
 * @returns {string}
 */
export function toBase64(bytes) {
  if (typeof btoa === "function") {
    const CHUNK = 0x8000;
    let binary = "";
    for (let i = 0; i < bytes.length; i += CHUNK) {
      binary += String.fromCharCode.apply(
        null,
        /** @type {any} */ (bytes.subarray(i, i + CHUNK)),
      );
    }
    return btoa(binary);
  }
  // @ts-ignore - Node-only fallback for the test environment.
  return Buffer.from(bytes).toString("base64");
}

/**
 * Render diff regions as the consumer's `MemoryRegion` / `MemoryWrite`
 * payload shape.
 *
 * @param {{offset: number, bytes: Uint8Array}[]} regions
 * @returns {{offset: number, bytesB64: string}[]}
 */
export function encodeRegions(regions) {
  return regions.map((r) => ({ offset: r.offset, bytesB64: toBase64(r.bytes) }));
}

/**
 * Encode a WebAssembly global's value the way the consumer's
 * `ImportedGlobal` / `GlobalSet` expects: an exact decimal string.
 *
 * An `i64` global's `.value` is a `BigInt`, which `JSON.stringify` refuses
 * outright, and which a `Number` would round above 2^53 — the same reason
 * `browser_session.js` carries `i64` boundary values as strings.
 *
 * @param {number|bigint} value
 * @returns {string}
 */
export function encodeGlobalValue(value) {
  return typeof value === "bigint" ? value.toString() : String(value);
}

/**
 * Snapshot of one tracked entity, as `browser_session.js` holds it.
 *
 * @typedef {Object} TrackedMemory
 * @property {string} module Import module name, e.g. `"env"`.
 * @property {string} name Import field name, e.g. `"memory"`.
 * @property {WebAssembly.Memory} memory The live memory object.
 * @property {number|null} maxPages Declared maximum, or null for
 *   unbounded. Spec §7 notes `memory.grow`'s result depends on the host
 *   limit, so the limit is part of the recorded initial state.
 * @property {Uint8Array|null} baseline Contents at registration.
 * @property {Uint8Array|null} shadow Contents at the last window open.
 */

/**
 * @typedef {Object} TrackedGlobal
 * @property {string} module
 * @property {string} name
 * @property {string} type One of "i32" / "i64" / "f32" / "f64".
 * @property {boolean} mutable
 * @property {WebAssembly.Global} global
 * @property {string|null} baseline Value at registration, and thereafter
 *   at the last quiescent point — the counterpart of a memory's
 *   `baseline`, and what makes a host assignment between two exported
 *   calls detectable rather than silently lost.
 * @property {string|null} shadow Value at the last window open.
 */

/**
 * Read a tracked memory's current bytes.
 *
 * The view is rebuilt every time rather than cached: `memory.grow()`
 * detaches the old `ArrayBuffer`, and a cached view over a detached
 * buffer reads as zero length — which would silently record a grown
 * memory as empty.
 *
 * @param {WebAssembly.Memory} memory
 * @returns {Uint8Array}
 */
export function readMemory(memory) {
  return new Uint8Array(memory.buffer);
}

/**
 * Copy a tracked memory's current bytes.
 *
 * @param {WebAssembly.Memory} memory
 * @returns {Uint8Array}
 */
export function snapshotMemory(memory) {
  return new Uint8Array(readMemory(memory));
}

/**
 * Page count of a memory's current size, which is what the consumer
 * builds its replacement memory with.
 *
 * @param {WebAssembly.Memory} memory
 * @returns {number}
 */
export function pagesOf(memory) {
  return Math.ceil(memory.buffer.byteLength / WASM_PAGE_BYTES);
}
