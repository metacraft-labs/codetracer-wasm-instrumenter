//! Names and signatures of the imported host functions the
//! instrumenter injects calls to.
//!
//! Browser embedders wire these to the M26 WebSocket producer;
//! native test embedders wire them to a direct in-process buffer
//! (`codetracer-wasm-stub-host`).

/// Default host module name. Embedders can override via
/// `PipelineConfig::host_module_name`.
pub const DEFAULT_HOST_MODULE: &str = "__codetracer";

/// `__ct_emit_write(addr: i32, size: i32, old: i64, new: i64)`.
///
/// Fired after every instrumented `*.store`. `addr` is the
/// effective address (base + static offset). `size` is the byte
/// width. `old` and `new` are the previous / new values widened
/// to i64 (zero-extended for narrower integer stores;
/// `i32.reinterpret_f32` + zero-extend for f32; `i64.reinterpret_f64`
/// for f64). For SIMD `v128.store` V1 emits `old = new = 0` as a
/// placeholder.
pub const HOOK_WRITE: &str = "__ct_emit_write";

/// `__ct_emit_call(fn_kind: i32, fn_index: i32)`.
///
/// Fired immediately before an instrumented call. `fn_kind` is
/// `0` for an imported function (host call from WASM), `1` for an
/// exported function (host call into WASM). `fn_index` is the
/// stable index within the corresponding section.
pub const HOOK_CALL: &str = "__ct_emit_call";

/// `__ct_emit_return(fn_kind: i32, fn_index: i32)`.
///
/// Symmetric counterpart of `__ct_emit_call`, fired immediately
/// after the call returns.
pub const HOOK_RETURN: &str = "__ct_emit_return";

/// `__ct_emit_realm_boundary(direction: i32, fn_kind: i32, fn_index: i32, token: i64)`.
///
/// Paired with the call / return hooks; `direction` is `0` for
/// entering a foreign realm (the call site), `1` for leaving (the
/// return site). `token` is the strictly monotonic correlation
/// token returned by `__ct_correlation_token()` — the M27 → M25
/// bridge.
pub const HOOK_REALM_BOUNDARY: &str = "__ct_emit_realm_boundary";

/// `__ct_correlation_token() -> i64`.
///
/// Strictly monotonic 64-bit counter sourced from the host. Used
/// as the per-realm-crossing correlation key.
pub const HOOK_CORRELATION_TOKEN: &str = "__ct_correlation_token";

/// Name of the custom section the instrumenter adds to every
/// instrumented module. Read by the bundler plugins to short-
/// circuit double-instrumentation on HMR reload.
pub const CUSTOM_SECTION_NAME: &str = "codetracer.instrumenter";

/// Body of the marker custom section: a UTF-8 string carrying the
/// pipeline version. The bundler plugins do a contains-match
/// rather than equality so newer instrumenters remain backwards-
/// compatible.
pub const CUSTOM_SECTION_BODY: &[u8] = b"codetracer-wasm-instrumenter v1";
