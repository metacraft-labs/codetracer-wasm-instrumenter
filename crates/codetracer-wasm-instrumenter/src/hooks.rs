//! Names and signatures of the imported host functions the
//! instrumenter injects calls to.
//!
//! Browser embedders wire these to the M26 WebSocket producer;
//! native test embedders wire them to a direct in-process buffer
//! (`codetracer-wasm-stub-host`).
//!
//! # The hook surface (spec § 5)
//!
//! | Hook | Signature |
//! | --- | --- |
//! | [`HOOK_CALL`] | `(fn_kind: i32, fn_index: i32) -> ()` |
//! | [`HOOK_RETURN`] | `(fn_kind: i32, fn_index: i32) -> ()` |
//! | [`HOOK_REALM_BOUNDARY`] | `(direction: i32, fn_kind: i32, fn_index: i32, token: i64) -> ()` |
//! | [`HOOK_CORRELATION_TOKEN`] | `() -> i64` |
//! | [`HOOK_EMIT_I32`] | `(slot: i32, value: i32) -> ()` |
//! | [`HOOK_EMIT_I64`] | `(slot: i32, value: i64) -> ()` |
//! | [`HOOK_EMIT_F32_BITS`] | `(slot: i32, bits: i32) -> ()` |
//! | [`HOOK_EMIT_F64_BITS`] | `(slot: i32, bits: i64) -> ()` |
//!
//! The value hooks are typed rather than universal because WASM has
//! no polymorphic call: a single `(slot, value)` hook would have to
//! pick one wire type, and every other type would reach the host
//! through a widening conversion. `f32 -> f64` is not
//! bit-preserving for signalling NaNs, and spec § 7 makes a NaN
//! payload mismatch a replay *divergence* — so the lossy hook would
//! turn a recording into something that cannot be replayed. One hook
//! per value type keeps every boundary value exact.
//!
//! # Why the float hooks carry integers (M52)
//!
//! Typing the float hooks `f32` / `f64` was necessary but not
//! sufficient: it makes the *WebAssembly* value exact, and then hands
//! it to a host that may not be able to hold it. A JavaScript embedder
//! receives a WASM `f32`/`f64` parameter as a `Number`, and the
//! WebAssembly JS API leaves a NaN's payload implementation-defined
//! across that conversion — so a module computing with a signalling
//! NaN or a payload-carrying quiet NaN handed the browser a *different*
//! NaN than it produced. `JSON.stringify` then rendered it `null`,
//! and `-0.0` rendered `0`. Spec § 7 makes a payload mismatch a
//! divergence, so the recording was not a faithful re-execution input.
//!
//! The fix is producer-first, the same move M39 made: the module
//! reinterprets the float to its integer bit pattern *before* the call
//! ([`walrus::ir::UnaryOp::I32ReinterpretF32`] /
//! `I64ReinterpretF64`), so no float ever crosses into the host and
//! there is no conversion to be lossy. The host reassembles the value
//! from bits it received exactly.
//!
//! [`HOOK_EMIT_F32_LEGACY`] and [`HOOK_EMIT_F64_LEGACY`] are the
//! pre-M52 spelling.
//! Nothing emits them any more, but they remain named here and hosts
//! are expected to keep serving them, because a module instrumented by
//! an older pipeline is an artefact users hold: it still imports them
//! and must still load.
//!
//! # Emission order (the framing contract)
//!
//! Around one boundary crossing the instrumented module emits:
//!
//! ```text
//!   __ct_emit_call(fn_kind, fn_index)
//!   __ct_emit_<t>(0, arg0) … __ct_emit_<t>(n-1, argN)     <- arguments
//!   __ct_emit_realm_boundary(ENTER, fn_kind, fn_index, tok)
//!       … the call itself (an import call, or an exported body) …
//!   __ct_emit_<t>(0, res0) … __ct_emit_<t>(m-1, resM)     <- results
//!   __ct_emit_return(fn_kind, fn_index)
//!   __ct_emit_realm_boundary(LEAVE, fn_kind, fn_index, tok)
//! ```
//!
//! Arguments are therefore the run of value hooks that *immediately
//! follows* [`HOOK_CALL`], and results the run that *immediately
//! precedes* [`HOOK_RETURN`]. A host can split the flat value stream
//! into the two tuples from that ordering alone, without knowing the
//! signature — which matters because a zero-argument or zero-result
//! boundary emits only one run, and the two cases are otherwise
//! indistinguishable. The authoritative decode still uses the
//! per-boundary signature the sidecar manifest records
//! (`ModuleManifest::boundaries`); the ordering rule is what lets a
//! host that has not loaded the manifest stay coherent.
//!
//! # Withdrawn hooks
//!
//! The V1 per-store write hook is **gone** from this surface (spec
//! §§ 5, 11). Modules produced by an older pipeline that still import
//! it remain loadable — a host may keep supplying a no-op — but
//! nothing in this crate emits it. See [`FUNC_KIND_STORE`] for how
//! the experimental store pass reports through the surviving hooks.

/// Default host module name. Embedders can override via
/// `PipelineConfig::host_module_name`.
pub const DEFAULT_HOST_MODULE: &str = "__codetracer";

/// `__ct_emit_call(fn_kind: i32, fn_index: i32)`.
///
/// Fired immediately before an instrumented call. `fn_kind` is
/// [`FUNC_KIND_IMPORT`] for an imported function (host call from
/// WASM), [`FUNC_KIND_EXPORT`] for an exported function (host call
/// into WASM). `fn_index` is the stable index within the
/// corresponding section.
pub const HOOK_CALL: &str = "__ct_emit_call";

/// `__ct_emit_return(fn_kind: i32, fn_index: i32)`.
///
/// Symmetric counterpart of [`HOOK_CALL`], fired immediately
/// after the call returns.
pub const HOOK_RETURN: &str = "__ct_emit_return";

/// `__ct_emit_realm_boundary(direction: i32, fn_kind: i32, fn_index: i32, token: i64)`.
///
/// Paired with the call / return hooks; `direction` is
/// [`REALM_DIRECTION_ENTER`] for entering a foreign realm (the call
/// site), [`REALM_DIRECTION_LEAVE`] for leaving (the return site).
/// `token` is the strictly monotonic correlation token returned by
/// [`HOOK_CORRELATION_TOKEN`] — the M27 → M25 bridge.
pub const HOOK_REALM_BOUNDARY: &str = "__ct_emit_realm_boundary";

/// `__ct_correlation_token() -> i64`.
///
/// Strictly monotonic 64-bit counter sourced from the host. Used
/// as the per-realm-crossing correlation key.
pub const HOOK_CORRELATION_TOKEN: &str = "__ct_correlation_token";

/// `__ct_emit_i32(slot: i32, value: i32)`. One per `i32` boundary
/// argument or result; `slot` is the position within its tuple.
pub const HOOK_EMIT_I32: &str = "__ct_emit_i32";

/// `__ct_emit_i64(slot: i32, value: i64)`. One per `i64` boundary
/// argument or result; `slot` is the position within its tuple.
pub const HOOK_EMIT_I64: &str = "__ct_emit_i64";

/// `__ct_emit_f32_bits(slot: i32, bits: i32)`. One per `f32` boundary
/// argument or result; `bits` is the value's IEEE-754 encoding,
/// produced in-module by `i32.reinterpret_f32`.
///
/// The parameter is an `i32` and not an `f32` on purpose — see the
/// module docs, "Why the float hooks carry integers (M52)". A host
/// that wants the number back does
/// `Float32Array`/`Int32Array`-style reinterpretation of its own; a
/// host that only needs to *record* it should store the bits, which
/// is the only lossless thing to do with a NaN.
pub const HOOK_EMIT_F32_BITS: &str = "__ct_emit_f32_bits";

/// `__ct_emit_f64_bits(slot: i32, bits: i64)`. One per `f64` boundary
/// argument or result; `bits` is the value's IEEE-754 encoding,
/// produced in-module by `i64.reinterpret_f64`.
pub const HOOK_EMIT_F64_BITS: &str = "__ct_emit_f64_bits";

/// `__ct_emit_f32(slot: i32, value: f32)`. **Pre-M52 spelling; nothing
/// emits it.** Retained so a host can keep serving modules instrumented
/// by an older pipeline, which import it and would otherwise fail to
/// instantiate.
pub const HOOK_EMIT_F32_LEGACY: &str = "__ct_emit_f32";

/// `__ct_emit_f64(slot: i32, value: f64)`. **Pre-M52 spelling; nothing
/// emits it.** See [`HOOK_EMIT_F32_LEGACY`].
pub const HOOK_EMIT_F64_LEGACY: &str = "__ct_emit_f64";

/// `fn_kind` for a call *out* of the module into an imported host
/// function.
pub const FUNC_KIND_IMPORT: i32 = 0;

/// `fn_kind` for a call *into* the module through an exported
/// function.
pub const FUNC_KIND_EXPORT: i32 = 1;

/// `fn_kind` for the experimental interior store pass.
///
/// The store pass is off the production path (spec § 11) and is
/// retired entirely in M36, but while it exists it has to report
/// through the hooks that remain. A store is framed exactly like a
/// boundary crossing — `__ct_emit_call(FUNC_KIND_STORE, size)`,
/// three values, `__ct_emit_return(FUNC_KIND_STORE, size)` — with
/// `fn_index` carrying the store's byte width and the value tuple
/// being `(0: effective address as i32, 1: previous value as i64,
/// 2: new value as i64)`. Floats are bit-reinterpreted into the i64
/// slots, matching what the withdrawn write hook reported.
pub const FUNC_KIND_STORE: i32 = 2;

/// `direction` for entering the foreign realm (the call site).
pub const REALM_DIRECTION_ENTER: i32 = 0;

/// `direction` for leaving the foreign realm (the return site).
pub const REALM_DIRECTION_LEAVE: i32 = 1;

/// Every hook name this pipeline declares, in declaration order.
///
/// A host that wants to supply the whole surface can iterate this
/// rather than transcribing the list, and a test can assert the
/// surface has not grown or shrunk unnoticed.
pub const ALL_HOOKS: &[&str] = &[
    HOOK_CALL,
    HOOK_RETURN,
    HOOK_REALM_BOUNDARY,
    HOOK_CORRELATION_TOKEN,
    HOOK_EMIT_I32,
    HOOK_EMIT_I64,
    HOOK_EMIT_F32_BITS,
    HOOK_EMIT_F64_BITS,
];

/// Hook names an instrumented module may import that are no longer
/// emitted, but that a host must still be able to serve so older
/// artefacts keep loading (M52's back-compat half).
pub const LEGACY_HOOKS: &[&str] = &[HOOK_EMIT_F32_LEGACY, HOOK_EMIT_F64_LEGACY];

/// Name of the custom section the instrumenter adds to every
/// instrumented module. Read by the bundler plugins to short-
/// circuit double-instrumentation on HMR reload.
pub const CUSTOM_SECTION_NAME: &str = "codetracer.instrumenter";

/// Body of the marker custom section: a UTF-8 string carrying the
/// pipeline version. The bundler plugins do a contains-match
/// rather than equality so newer instrumenters remain backwards-
/// compatible.
pub const CUSTOM_SECTION_BODY: &[u8] = b"codetracer-wasm-instrumenter v1";
