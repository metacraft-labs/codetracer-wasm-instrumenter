//! `codetracer-wasm-instrumenter` — the bytecode-rewriting WASM
//! instrumentation pipeline used by CodeTracer to record `.wasm`
//! modules running inside an unmodified host runtime (V8 in a
//! browser, wasmtime, JSC, …).
//!
//! See:
//! - `codetracer-specs/Planned-Features/Value-Origin-Tracking.milestones.org`
//!   § M27 (deliverables and verification);
//! - `codetracer-specs/GUI/Debugging-Features/Value-Origin-Tracking.md`
//!   § 14.5 (the prose specification);
//! - `codetracer-specs/Recording-Backends/WASM-Instrumentation-Layer.md`
//!   (the dedicated trade-off doc, authored as part of M27).
//!
//! ## What the pipeline does
//!
//! Given an input `.wasm` module, [`Pipeline::run`] produces a
//! self-contained instrumented `.wasm` module that, when executed
//! under a host runtime which provides the documented
//! [`hooks::HOOK_*`] imports, emits a deterministic sequence of
//! CodeTracer events covering:
//!
//! 1. **Imported call boundaries** — every `call` whose target is
//!    an imported function is preceded by `__ct_emit_call(0,
//!    fn_index)` and followed by `__ct_emit_return(0, fn_index)`.
//!    These are the natural realm-crossing points in a browser
//!    embedding (host calls from WASM into JS).
//!
//! 2. **Exported function boundaries** — every function reachable
//!    via an `export` entry receives an entry-prologue
//!    `__ct_emit_call(1, fn_index)` and a return-epilogue
//!    `__ct_emit_return(1, fn_index)` (the export index is the
//!    index of the matching entry in the original `export` section
//!    — stable across re-instrumentation). These are the natural
//!    realm-crossing points in the reverse direction (calls from
//!    JS into WASM).
//!
//! 3. **The values that cross those boundaries** — every argument
//!    and every result, reported one at a time through the typed
//!    `__ct_emit_{i32,i64,f32,f64}(slot, value)` hooks (M35). This
//!    is what makes the log a *re-execution input* rather than a
//!    call graph: without an import's recorded results the replayer
//!    of spec § 6 has nothing to feed back in place of the real
//!    host. Values are read without disturbing the computation by
//!    spilling the operands into scratch locals, emitting, and
//!    pushing them back — see [`push_value_capture_group`].
//!
//! 4. **Realm-crossing correlation tokens** — both directions also
//!    emit a paired
//!    `__ct_emit_realm_boundary(direction, fn_kind, fn_index, token)`
//!    event where `token` is a strictly monotonic 64-bit counter
//!    sourced from a freshly-declared host import
//!    `__ct_correlation_token() -> i64`. The token is the
//!    M27 → M25 bridge: it rides on the standard correlation marker
//!    machinery at the JS↔WASM granularity.
//!
//! The pipeline is **idempotent up to instrumentation**: running it
//! twice on the same input produces the same byte output
//! (modulo timestamps / counter state, which are runtime concerns
//! not embedded in the module).
//!
//! An experimental interior pass behind
//! [`PipelineConfig::instrument_stores`] additionally reports every
//! memory store. It is **off by default** and **not** part of any
//! production path — spec §§ 2 and 11 withdraw the interior model,
//! and M36 retired it from the default rewrite. The pass survives
//! only as a reachable experiment; nothing in the browser pipeline
//! consumes its events. It reports through the surviving hook
//! surface (see [`hooks::FUNC_KIND_STORE`]) rather than the
//! withdrawn per-store write hook.
//!
//! ## What the pipeline does **not** do (deferred, documented)
//!
//! - Multi-memory write tracking (the `memory_id` field is captured
//!   in the IR but only memory 0 is currently passed through to the
//!   hook; multi-memory support widens the hook signature, see
//!   `Recording-Backends/WASM-Instrumentation-Layer.md`).
//! - SIMD `v128.store` and the lane-store variants — captured in
//!   the IR as `Store { kind: V128 }` or `LoadStoreLane` but only
//!   reported as a call/size event (`old`/`new` set to zero) in
//!   V1; the full 128-bit shadow is a follow-on item.
//! - Atomic stores: instrumented like regular stores but the
//!   pre-read is non-atomic — sufficient for single-threaded
//!   browser pages, insufficient for shared-memory threads.
//! - Boundary signatures carrying `externref` / `funcref` / `v128`:
//!   the module is **rejected** with a diagnostic naming the
//!   function rather than recorded with a hole in its value stream
//!   (spec § 8).
//! - **An exception that unwinds *past* an open crossing leaves it
//!   open.** The hook surface has no "on unwind" event, so if a
//!   `throw` propagates out of an instrumented export, or out of an
//!   imported call whose `__ct_emit_call` has already fired, the
//!   matching `__ct_emit_return` and LEAVE never fire. This is
//!   inherent to the surface, not to the walk — it is equally true of
//!   a function that throws without using a `try_table` at all — and
//!   the failure is loud rather than silent: the stream is left
//!   structurally unbalanced, which a § 6 replayer refuses outright
//!   rather than replaying past. Recording an unwind is a hook-surface
//!   change and belongs to whichever milestone takes on replaying
//!   exceptions.
//!
//! The exit-site coverage of the export edge is complete for MVP
//! control flow as of M35b, and for the exception-handling and GC
//! proposals as of M35c: the fall-through exit, an explicit `return`
//! wherever it is nested, an exit taken by branching to the function's
//! own label (`br` / `br_if` / `br_table`), the GC label-carrying
//! forms (`br_on_null`, `br_on_non_null`, `br_on_cast`,
//! `br_on_cast_fail`), and a `try_table` catch clause that names the
//! function label all emit the leave event and the result capture.
//!
//! Two mechanisms do that work, and they are separate. Branching to
//! the function's own label used to jump clean over an epilogue
//! appended to the entry sequence; M35b closed it by moving the body
//! into an inner block typed `[] -> results` and re-pointing those
//! branches at that block, so they land before the epilogue rather
//! than after it (see `wrap_local_function_boundary`, and
//! `visit_branch_targets` for the set of forms that count as a
//! branch). The restructuring is applied only to functions that
//! actually contain such a branch, which is what keeps every other
//! module byte-identical.
//!
//! Nesting is the other half. Until M35c the crate's single traversal,
//! `collect_block_ids`, descended into `block` / `loop` / `if`-`else`
//! and nothing else, so **nothing inside a `try_table` body, a legacy
//! `try` body or a legacy catch handler was seen by any pass** — an
//! ordinary explicit `return` there was not wrapped, and an imported
//! call there was not captured, even though `try_table` is exactly
//! what `-fwasm-exceptions` emits. `collect_block_ids` now descends
//! into all of them.

#![deny(rust_2018_idioms, unused_must_use)]
#![warn(missing_docs)]

use anyhow::{bail, Context, Result};
use std::path::Path;
use walrus::ir::{BinaryOp, ExtendedLoad, Instr, InstrSeqId, LoadKind, MemArg, StoreKind, UnaryOp};
use walrus::{FunctionId, FunctionKind, LocalId, MemoryId, Module, ModuleConfig, ValType};

pub mod config;
pub mod dwarf;
pub mod hooks;
pub mod manifest;

use hooks::{
    FUNC_KIND_EXPORT, FUNC_KIND_IMPORT, FUNC_KIND_STORE, REALM_DIRECTION_ENTER,
    REALM_DIRECTION_LEAVE,
};

pub use config::PipelineConfig;
pub use manifest::{
    BoundarySignature, ManifestBoundary, ManifestFunction, ManifestSite, ModuleManifest, ScalarType,
};

/// One-shot bytecode-rewriting pipeline.
///
/// Construct with [`Pipeline::new`] (or [`Pipeline::with_config`] to
/// override the defaults) and drive with [`Pipeline::run_bytes`] or
/// [`Pipeline::run_files`].
///
/// The pipeline is intentionally *stateless across calls* — the
/// `Pipeline` value owns nothing that survives a single
/// instrumentation. This is the contract the bundler plugins rely
/// on: every HMR re-instrument reuses the same `Pipeline`
/// instance.
#[derive(Debug, Default, Clone)]
pub struct Pipeline {
    config: PipelineConfig,
}

impl Pipeline {
    /// Construct a pipeline with [`PipelineConfig::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a pipeline with the supplied config.
    pub fn with_config(config: PipelineConfig) -> Self {
        Self { config }
    }

    /// Read `input` as WASM bytes, instrument, and return the
    /// instrumented bytes. The input is fully decoded with
    /// [`walrus::Module::from_buffer`] so a malformed module is
    /// rejected up-front (V1 returns the underlying walrus error).
    pub fn run_bytes(&self, input: &[u8]) -> Result<Vec<u8>> {
        Ok(self.run_bytes_with_manifest(input, None)?.0)
    }

    /// Instrument `input` and additionally return the sidecar
    /// [`ModuleManifest`] describing the module's export table.
    ///
    /// The manifest is what turns the runtime's bare `fn_index` integers
    /// back into names and source paths; an embedder that records a
    /// module without shipping it produces a trace whose frames are
    /// anonymous. `source_path` names the source the module was compiled
    /// from (see [`ModuleManifest::from_module`] for why it cannot be
    /// inferred).
    ///
    /// The manifest is built from the module **before** instrumentation
    /// registers the hook imports, so the indices it records are the ones
    /// the original module — and therefore the injected hook calls —
    /// refer to.
    pub fn run_bytes_with_manifest(
        &self,
        input: &[u8],
        source_path: Option<&str>,
    ) -> Result<(Vec<u8>, ModuleManifest)> {
        let mut module = Module::from_buffer_with_config(input, &walrus_config())
            .context("failed to parse input WASM module")?;
        let module_name = module
            .name
            .clone()
            .unwrap_or_else(|| "module.wasm".to_string());
        let manifest = ModuleManifest::from_module(&module, input, source_path, &module_name);
        self.instrument_module(&mut module)?;
        Ok((module.emit_wasm(), manifest))
    }

    /// Read `input` from disk, instrument, and write to `output`.
    pub fn run_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, input: P, output: Q) -> Result<()> {
        self.run_files_with_manifest(input, output, None::<&Path>, None)
    }

    /// [`Pipeline::run_files`] plus sidecar-manifest emission.
    ///
    /// When `manifest_output` is `Some`, the manifest JSON is written
    /// there. `source_path` is forwarded to
    /// [`ModuleManifest::from_module`].
    pub fn run_files_with_manifest<P: AsRef<Path>, Q: AsRef<Path>, M: AsRef<Path>>(
        &self,
        input: P,
        output: Q,
        manifest_output: Option<M>,
        source_path: Option<&str>,
    ) -> Result<()> {
        let bytes = std::fs::read(input.as_ref())
            .with_context(|| format!("failed to read {}", input.as_ref().display()))?;
        // Without an explicit source path, fall back to the input
        // module's own filename so the manifest still names something
        // real rather than a fabricated source file.
        let fallback_name = input
            .as_ref()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let effective_source = source_path.or(fallback_name.as_deref());
        let (instrumented, manifest) = self.run_bytes_with_manifest(&bytes, effective_source)?;
        std::fs::write(output.as_ref(), instrumented)
            .with_context(|| format!("failed to write {}", output.as_ref().display()))?;
        if let Some(manifest_path) = manifest_output {
            let json = manifest
                .to_json()
                .context("failed to serialise the instrumentation manifest")?;
            std::fs::write(manifest_path.as_ref(), json)
                .with_context(|| format!("failed to write {}", manifest_path.as_ref().display()))?;
        }
        Ok(())
    }

    /// Borrowed config (the plugin wrappers pin this).
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    fn instrument_module(&self, module: &mut Module) -> Result<()> {
        // Snapshot the original module's import / export view
        // *before* we register the hook imports — the indices we
        // report to the hooks must match the index space the
        // original module sees, not the post-instrumentation one
        // (where the hooks live at low indices).
        let imported_funcs = collect_imported_function_indices(module);
        let exported_targets = collect_exported_function_targets(module);

        // Spec § 8: refuse a module whose boundary signatures carry a
        // type the value hooks cannot transport, rather than emitting
        // a recording with a hole in it. A silently incomplete
        // recording replays fine right up until the missing input
        // matters, and then diverges at a point unrelated to the
        // cause — the most expensive failure mode there is.
        if self.config.capture_boundary_values {
            reject_unrepresentable_boundaries(module, &imported_funcs, &exported_targets)?;
        }

        let hook_ids = HookFunctionIds::register(module, &self.config);
        let memories = collect_memory_ids(module);

        // `iter_local_mut` / `funcs.get_mut` borrow the function arena
        // exclusively, so every local this run needs has to exist
        // before the walk starts. Size the pool to the widest boundary
        // tuple in the module: that is what makes the spill correct
        // for a multi-value return, where several results of the same
        // type are live at once and must not share one slot.
        let scratch = if self.config.capture_boundary_values {
            let mut widths = TypeCounts::default();
            for sig in imported_funcs
                .signatures()
                .chain(exported_targets.iter().map(|(_, _, sig)| sig))
            {
                widths.take_max(&TypeCounts::of(&sig.params));
                widths.take_max(&TypeCounts::of(&sig.results));
            }
            ScratchPool::pre_allocate(module, &widths)
        } else {
            ScratchPool::default()
        };
        let values = self.config.capture_boundary_values.then_some(&scratch);

        if self.config.instrument_stores {
            self.instrument_stores(module, &hook_ids, &memories)?;
        }
        if self.config.instrument_imported_calls {
            self.instrument_imported_calls(module, &hook_ids, &imported_funcs, values)?;
        }
        if self.config.instrument_exported_functions {
            self.instrument_exported_functions(module, &hook_ids, &exported_targets, values)?;
        }

        // Record a build-time marker so downstream tooling can
        // detect an already-instrumented module and refuse to
        // double-instrument (a guard the bundler plugins consult).
        module.customs.add(walrus::RawCustomSection {
            name: hooks::CUSTOM_SECTION_NAME.to_string(),
            data: hooks::CUSTOM_SECTION_BODY.to_vec(),
        });
        Ok(())
    }

    fn instrument_stores(
        &self,
        module: &mut Module,
        hook_ids: &HookFunctionIds,
        memories: &MemoryHandles,
    ) -> Result<()> {
        // `iter_local_mut` borrows the function arena exclusively, so
        // we cannot touch `module.locals` from within the closure.
        // We therefore pre-allocate enough generic scratch locals
        // per type before the walk.
        let scratch = ScratchLocals::pre_allocate(module);

        for (_func_id, local_func) in module.funcs.iter_local_mut() {
            let mut rewriter = StoreRewriter {
                hook_ids,
                memories,
                scratch: &scratch,
            };
            rewriter.rewrite(local_func);
        }
        Ok(())
    }

    fn instrument_imported_calls(
        &self,
        module: &mut Module,
        hook_ids: &HookFunctionIds,
        imported_funcs: &ImportedFuncSet,
        scratch: Option<&ScratchPool>,
    ) -> Result<()> {
        for (_func_id, local_func) in module.funcs.iter_local_mut() {
            ImportedCallRewriter {
                hook_ids,
                imported_funcs,
                scratch,
            }
            .rewrite(local_func);
        }
        Ok(())
    }

    fn instrument_exported_functions(
        &self,
        module: &mut Module,
        hook_ids: &HookFunctionIds,
        exports: &[(FunctionId, u32, BoundarySignature)],
        scratch: Option<&ScratchPool>,
    ) -> Result<()> {
        for (func_id, export_index, sig) in exports {
            // Only local functions can be wrapped — re-exporting an
            // imported function would mean the call dispatches
            // straight to the host, so the boundary is already
            // recorded by `instrument_imported_calls`.
            let kind = &module.funcs.get(*func_id).kind;
            if matches!(kind, FunctionKind::Local(_)) {
                wrap_local_function_boundary(
                    module,
                    *func_id,
                    *export_index,
                    hook_ids,
                    sig,
                    scratch,
                );
            }
        }
        Ok(())
    }
}

/// Spec § 8: a boundary whose signature mentions a type the value
/// hooks cannot carry is a hard error naming the offending function.
fn reject_unrepresentable_boundaries(
    module: &Module,
    imports: &ImportedFuncSet,
    exports: &[(FunctionId, u32, BoundarySignature)],
) -> Result<()> {
    let describe = |func_id: FunctionId| -> String {
        module
            .funcs
            .get(func_id)
            .name
            .clone()
            .unwrap_or_else(|| "<anonymous>".to_string())
    };
    for (func_id, _, sig) in &imports.entries {
        if let Some(bad) = sig.unrepresentable {
            bail!(
                "imported function `{}` has a boundary signature containing `{}`, \
                 which the value-capture hooks cannot transport; \
                 recording it would silently omit a replay input \
                 (WASM-Instrumentation-Layer.md § 8)",
                describe(*func_id),
                bad,
            );
        }
    }
    for (func_id, _, sig) in exports {
        if let Some(bad) = sig.unrepresentable {
            bail!(
                "exported function `{}` has a boundary signature containing `{}`, \
                 which the value-capture hooks cannot transport; \
                 recording it would silently omit a replay input \
                 (WASM-Instrumentation-Layer.md § 8)",
                describe(*func_id),
                bad,
            );
        }
    }
    Ok(())
}

fn walrus_config() -> ModuleConfig {
    let mut config = ModuleConfig::new();
    // Walrus by default strips the producers section; we preserve
    // it so the instrumented module still reports the original
    // toolchain in tooling like `wasm-tools`.
    config.generate_producers_section(false);
    // Generate dwarf only when present (don't fabricate).
    config.generate_dwarf(true);
    config
}

// ---------------------------------------------------------------------------
// Hook function registration
// ---------------------------------------------------------------------------

/// Resolved `FunctionId`s for each imported hook the instrumenter
/// inserts calls to. Held by value during a single pipeline run.
#[derive(Debug, Clone, Copy)]
struct HookFunctionIds {
    emit_call: FunctionId,
    emit_return: FunctionId,
    emit_realm_boundary: FunctionId,
    correlation_token: FunctionId,
    emit_i32: FunctionId,
    emit_i64: FunctionId,
    emit_f32_bits: FunctionId,
    emit_f64_bits: FunctionId,
}

impl HookFunctionIds {
    /// The hook that transports one value of `ty`, and the
    /// reinterpretation the module must apply before calling it.
    ///
    /// Floats travel as their integer bit pattern (M52): the module
    /// does the `reinterpret` itself so no float ever crosses into the
    /// host, where a JavaScript embedder's `Number` conversion would
    /// leave a NaN's payload implementation-defined. See `hooks.rs`,
    /// "Why the float hooks carry integers".
    fn value_hook(&self, ty: ScalarType) -> (FunctionId, Option<UnaryOp>) {
        match ty {
            ScalarType::I32 => (self.emit_i32, None),
            ScalarType::I64 => (self.emit_i64, None),
            ScalarType::F32 => (self.emit_f32_bits, Some(UnaryOp::I32ReinterpretF32)),
            ScalarType::F64 => (self.emit_f64_bits, Some(UnaryOp::I64ReinterpretF64)),
        }
    }

    fn register(module: &mut Module, config: &PipelineConfig) -> Self {
        // __ct_emit_call(fn_kind: i32, fn_index: i32)
        let call_ty = module.types.add(&[ValType::I32, ValType::I32], &[]);
        let (emit_call, _) =
            module.add_import_func(&config.host_module_name, hooks::HOOK_CALL, call_ty);

        // __ct_emit_return(fn_kind: i32, fn_index: i32)
        let return_ty = module.types.add(&[ValType::I32, ValType::I32], &[]);
        let (emit_return, _) =
            module.add_import_func(&config.host_module_name, hooks::HOOK_RETURN, return_ty);

        // __ct_emit_realm_boundary(direction: i32, fn_kind: i32, fn_index: i32, token: i64)
        let realm_ty = module.types.add(
            &[ValType::I32, ValType::I32, ValType::I32, ValType::I64],
            &[],
        );
        let (emit_realm_boundary, _) = module.add_import_func(
            &config.host_module_name,
            hooks::HOOK_REALM_BOUNDARY,
            realm_ty,
        );

        // __ct_correlation_token() -> i64
        let tok_ty = module.types.add(&[], &[ValType::I64]);
        let (correlation_token, _) = module.add_import_func(
            &config.host_module_name,
            hooks::HOOK_CORRELATION_TOKEN,
            tok_ty,
        );

        // __ct_emit_i32(slot: i32, value: i32), and one sibling per
        // value type. Typed rather than universal because a single
        // hook would have to widen `f32` to `f64`, which is not
        // bit-preserving for signalling NaNs — and spec § 7 makes a
        // NaN payload mismatch a replay divergence.
        let mut value_hook = |name: &str, ty: ValType| {
            let hook_ty = module.types.add(&[ValType::I32, ty], &[]);
            module
                .add_import_func(&config.host_module_name, name, hook_ty)
                .0
        };
        let emit_i32 = value_hook(hooks::HOOK_EMIT_I32, ValType::I32);
        let emit_i64 = value_hook(hooks::HOOK_EMIT_I64, ValType::I64);
        // The float hooks take the value's *bit pattern*, not the
        // float — see `hooks.rs`, "Why the float hooks carry integers
        // (M52)". The reinterpret happens inside the module.
        let emit_f32_bits = value_hook(hooks::HOOK_EMIT_F32_BITS, ValType::I32);
        let emit_f64_bits = value_hook(hooks::HOOK_EMIT_F64_BITS, ValType::I64);

        HookFunctionIds {
            emit_call,
            emit_return,
            emit_realm_boundary,
            correlation_token,
            emit_i32,
            emit_i64,
            emit_f32_bits,
            emit_f64_bits,
        }
    }
}

// ---------------------------------------------------------------------------
// Scratch locals for boundary value capture
// ---------------------------------------------------------------------------

/// A count of values per [`ScalarType`], used to size the scratch pool.
#[derive(Debug, Default, Clone, Copy)]
struct TypeCounts([usize; 4]);

impl TypeCounts {
    fn of(types: &[ScalarType]) -> Self {
        let mut out = Self::default();
        for ty in types {
            out.0[ty.pool_slot()] += 1;
        }
        out
    }

    fn take_max(&mut self, other: &Self) {
        for (mine, theirs) in self.0.iter_mut().zip(other.0.iter()) {
            *mine = (*mine).max(*theirs);
        }
    }
}

/// Scratch locals the boundary passes spill operands into.
///
/// Walrus assigns a function's local indices at emit time from the set
/// of locals its body actually references, so a pool allocated here can
/// never collide with a local the input module already uses, however
/// densely it packs its index space. Locals are per-invocation in WASM,
/// so a recursive or re-entrant function gets its own copies and the
/// shared pool cannot alias across frames.
#[derive(Debug, Default, Clone)]
struct ScratchPool {
    /// One vector per [`ScalarType`], indexed by `ScalarType::pool_slot`.
    by_type: [Vec<LocalId>; 4],
}

impl ScratchPool {
    fn pre_allocate(module: &mut Module, widths: &TypeCounts) -> Self {
        let mut pool = Self::default();
        for (slot, count) in widths.0.iter().enumerate() {
            let val_ty = ScalarType::from_pool_slot(slot).val_type();
            pool.by_type[slot] = (0..*count).map(|_| module.locals.add(val_ty)).collect();
        }
        pool
    }

    /// The `nth` scratch local of type `ty`.
    fn local(&self, ty: ScalarType, nth: usize) -> LocalId {
        self.by_type[ty.pool_slot()][nth]
    }
}

/// Emit the value-capture sequence for one tuple of operands sitting on
/// top of the operand stack (last element on top), leaving the stack
/// exactly as it was found.
///
/// This is the whole mechanism of M35 and the only reason capture is
/// non-perturbing: an operand cannot be read in place, so each is
/// spilled into a scratch local, reported, and pushed back in the
/// original order. Each element gets its *own* local — sharing one per
/// type would corrupt a multi-value return of two same-typed results.
fn push_value_capture_group(
    out: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
    loc: walrus::ir::InstrLocId,
    hook_ids: &HookFunctionIds,
    scratch: &ScratchPool,
    types: &[ScalarType],
) {
    if types.is_empty() {
        return;
    }
    let mut used = TypeCounts::default();
    let mut locals: Vec<LocalId> = Vec::with_capacity(types.len());
    for ty in types {
        let nth = used.0[ty.pool_slot()];
        used.0[ty.pool_slot()] += 1;
        locals.push(scratch.local(*ty, nth));
    }

    // Spill: the tuple's last element is on top, so pop in reverse.
    for local in locals.iter().rev() {
        out.push((Instr::LocalSet(walrus::ir::LocalSet { local: *local }), loc));
    }
    // Report, in declaration order, with the positional slot index.
    for (slot, (local, ty)) in locals.iter().zip(types.iter()).enumerate() {
        push_value_emit(out, loc, hook_ids, slot as i32, *local, *ty);
    }
    // Restore in declaration order, so the top of the stack is again
    // the tuple's last element.
    for local in locals.iter() {
        out.push((Instr::LocalGet(walrus::ir::LocalGet { local: *local }), loc));
    }
}

/// `i32.const slot; local.get value; [reinterpret;] call __ct_emit_<ty>`.
///
/// The optional reinterpret is what makes a float boundary value exact
/// on a JavaScript host (M52): `f32`/`f64` become `i32`/`i64` here, in
/// the module, so the host is handed a bit pattern rather than a number
/// it might not be able to represent. The spill local keeps its float
/// type — the operand is pushed back unchanged, so the module under
/// observation computes exactly as it did before.
fn push_value_emit(
    out: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
    loc: walrus::ir::InstrLocId,
    hook_ids: &HookFunctionIds,
    slot: i32,
    local: LocalId,
    ty: ScalarType,
) {
    let (func, reinterpret) = hook_ids.value_hook(ty);
    out.push((
        Instr::Const(walrus::ir::Const {
            value: walrus::ir::Value::I32(slot),
        }),
        loc,
    ));
    out.push((Instr::LocalGet(walrus::ir::LocalGet { local }), loc));
    if let Some(op) = reinterpret {
        out.push((Instr::Unop(walrus::ir::Unop { op }), loc));
    }
    out.push((Instr::Call(walrus::ir::Call { func }), loc));
}

// ---------------------------------------------------------------------------
// Memory handles & scratch locals
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct MemoryHandles {
    /// Memory id of memory 0 (V1 instruments stores against the
    /// default memory only).
    default_memory: Option<MemoryId>,
}

fn collect_memory_ids(module: &Module) -> MemoryHandles {
    MemoryHandles {
        default_memory: module.memories.iter().next().map(|m| m.id()),
    }
}

#[derive(Debug, Clone, Copy)]
struct ScratchLocals {
    addr_i32: LocalId,
    old_i32: LocalId,
    new_i32: LocalId,
    old_i64: LocalId,
    new_i64: LocalId,
    old_f32: LocalId,
    new_f32: LocalId,
    old_f64: LocalId,
    new_f64: LocalId,
}

impl ScratchLocals {
    fn pre_allocate(module: &mut Module) -> Self {
        Self {
            addr_i32: module.locals.add(ValType::I32),
            old_i32: module.locals.add(ValType::I32),
            new_i32: module.locals.add(ValType::I32),
            old_i64: module.locals.add(ValType::I64),
            new_i64: module.locals.add(ValType::I64),
            old_f32: module.locals.add(ValType::F32),
            new_f32: module.locals.add(ValType::F32),
            old_f64: module.locals.add(ValType::F64),
            new_f64: module.locals.add(ValType::F64),
        }
    }
}

// ---------------------------------------------------------------------------
// Imported / exported function discovery
// ---------------------------------------------------------------------------

fn collect_imported_function_indices(module: &Module) -> ImportedFuncSet {
    let mut set = ImportedFuncSet::default();
    let mut idx = 0u32;
    for import in module.imports.iter() {
        if let walrus::ImportKind::Function(func_id) = import.kind {
            // Skip the instrumenter's own hooks — wrapping them
            // with __ct_emit_call / __ct_emit_return would produce
            // infinite recursion and isn't what M27 is reporting
            // on. The "stable index" still has to count every
            // function import (so downstream tooling can correlate
            // to the WASM function space), so we just refuse to
            // include them in the wrap-target set; the synthetic
            // index counter still ticks.
            if !is_codetracer_hook_import(import) {
                let sig = BoundarySignature::of_function(module, func_id);
                set.entries.push((func_id, idx, sig));
            }
            idx += 1;
        }
    }
    set
}

/// Does this import belong to the instrumenter's own hook surface?
///
/// Matched by prefix rather than by an exact name list so that a
/// module produced by an older pipeline — which imported hooks this
/// version no longer declares — is still recognised and not wrapped
/// recursively.
fn is_codetracer_hook_import(import: &walrus::Import) -> bool {
    let name = import.name.as_str();
    name.starts_with("__ct_emit_") || name == hooks::HOOK_CORRELATION_TOKEN
}

#[derive(Debug, Default, Clone)]
struct ImportedFuncSet {
    /// In-order list of imported function ids that are eligible for
    /// import-call wrapping, paired with their stable "imported
    /// function index" in the WASM import section and the boundary
    /// signature whose values M35 captures. The instrumenter's own
    /// hooks are deliberately omitted from this list so they don't
    /// get wrapped recursively, but their section indices are still
    /// skipped so the indices we report match the index space the
    /// original module sees.
    entries: Vec<(FunctionId, u32, BoundarySignature)>,
}

impl ImportedFuncSet {
    fn lookup(&self, id: FunctionId) -> Option<(u32, &BoundarySignature)> {
        self.entries
            .iter()
            .find_map(|(fid, idx, sig)| (*fid == id).then_some((*idx, sig)))
    }

    fn signatures(&self) -> impl Iterator<Item = &BoundarySignature> {
        self.entries.iter().map(|(_, _, sig)| sig)
    }
}

fn collect_exported_function_targets(module: &Module) -> Vec<(FunctionId, u32, BoundarySignature)> {
    let mut out = Vec::new();
    for (idx, export) in module.exports.iter().enumerate() {
        if let walrus::ExportItem::Function(func_id) = export.item {
            out.push((
                func_id,
                idx as u32,
                BoundarySignature::of_function(module, func_id),
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Store instrumentation
// ---------------------------------------------------------------------------

struct StoreRewriter<'a> {
    hook_ids: &'a HookFunctionIds,
    memories: &'a MemoryHandles,
    scratch: &'a ScratchLocals,
}

impl<'a> StoreRewriter<'a> {
    fn rewrite(&mut self, local_func: &mut walrus::LocalFunction) {
        let entry = local_func.entry_block();
        // Collect all reachable InstrSeq ids before mutating.
        let block_ids = collect_block_ids(local_func, entry);
        for id in block_ids {
            self.rewrite_block(local_func, id);
        }
    }

    fn rewrite_block(&mut self, local_func: &mut walrus::LocalFunction, id: InstrSeqId) {
        // Walk the block in reverse order so insertions don't
        // shift indices we haven't processed yet.
        let original_len = local_func.block(id).instrs.len();
        for idx in (0..original_len).rev() {
            let (instr_clone, _) = local_func.block(id).instrs[idx].clone();
            if let Instr::Store(walrus::ir::Store { memory, kind, arg }) = instr_clone {
                // V1: only instrument the default memory. A
                // multi-memory module gets its non-default stores
                // left alone (logged via a sentinel custom section
                // entry — TODO once the spec doc lands the surface).
                if self.memories.default_memory != Some(memory) {
                    continue;
                }
                self.replace_store_with_instrumented(local_func, id, idx, memory, kind, arg);
            }
        }
    }

    fn replace_store_with_instrumented(
        &mut self,
        local_func: &mut walrus::LocalFunction,
        block_id: InstrSeqId,
        idx: usize,
        memory: MemoryId,
        kind: StoreKind,
        arg: MemArg,
    ) {
        // Build the replacement instruction sequence into a
        // throw-away vector, then splice it back into the block.
        // This is cleaner than driving an `InstrSeqBuilder` over
        // an existing position (which is not part of the public
        // walrus API at 0.26).
        let mut replacement: Vec<(Instr, walrus::ir::InstrLocId)> = Vec::with_capacity(24);
        // Use the same loc id as the original store for all the
        // synthetic instructions so source-map debuggers see them
        // grouped with the originating instruction.
        let loc = local_func.block(block_id).instrs[idx].1;
        let push = |v: &mut Vec<(Instr, walrus::ir::InstrLocId)>, instr: Instr| {
            v.push((instr, loc));
        };

        let scratch = self.scratch;
        let mem = memory;
        let size = kind.width() as i32;

        // Stack on entry to the original `store`: [addr, value].
        // We save value to a typed scratch local, tee-save addr,
        // re-load the existing memory contents, save as `old`,
        // restore addr+value, store, then push the hook call with
        // `(eff_addr, size, old, new)`.
        match kind {
            StoreKind::I32 { .. } => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_i32, ValType::I32);
                // Reload old.
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: LoadKind::I32 { atomic: false },
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_i32,
                    }),
                );
                // Actual store: re-push addr & new.
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                // Emit hook: eff_addr, size, old (i64 widen), new (i64 widen).
                self.emit_hook_write_i32(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_i32,
                    scratch.new_i32,
                );
            }
            StoreKind::I32_8 { .. } => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_i32, ValType::I32);
                // For sized stores we read back the same width as the original.
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: LoadKind::I32_8 {
                            kind: ExtendedLoad::ZeroExtend,
                        },
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                self.emit_hook_write_i32(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_i32,
                    scratch.new_i32,
                );
            }
            StoreKind::I32_16 { .. } => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_i32, ValType::I32);
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: LoadKind::I32_16 {
                            kind: ExtendedLoad::ZeroExtend,
                        },
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                self.emit_hook_write_i32(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_i32,
                    scratch.new_i32,
                );
            }
            StoreKind::I64 { .. } => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_i64, ValType::I64);
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: LoadKind::I64 { atomic: false },
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_i64,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_i64,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                self.emit_hook_write_i64_direct(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_i64,
                    scratch.new_i64,
                );
            }
            StoreKind::I64_8 { .. } | StoreKind::I64_16 { .. } | StoreKind::I64_32 { .. } => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_i64, ValType::I64);
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                let load_kind = match kind {
                    StoreKind::I64_8 { .. } => LoadKind::I64_8 {
                        kind: ExtendedLoad::ZeroExtend,
                    },
                    StoreKind::I64_16 { .. } => LoadKind::I64_16 {
                        kind: ExtendedLoad::ZeroExtend,
                    },
                    StoreKind::I64_32 { .. } => LoadKind::I64_32 {
                        kind: ExtendedLoad::ZeroExtend,
                    },
                    _ => unreachable!(),
                };
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: load_kind,
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_i64,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_i64,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                self.emit_hook_write_i64_direct(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_i64,
                    scratch.new_i64,
                );
            }
            StoreKind::F32 => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_f32, ValType::F32);
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: LoadKind::F32,
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_f32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_f32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                self.emit_hook_write_f32(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_f32,
                    scratch.new_f32,
                );
            }
            StoreKind::F64 => {
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_f64, ValType::F64);
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Load(walrus::ir::Load {
                        memory: mem,
                        kind: LoadKind::F64,
                        arg,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalSet(walrus::ir::LocalSet {
                        local: scratch.old_f64,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.new_f64,
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                self.emit_hook_write_f64(
                    &mut replacement,
                    &push,
                    arg.offset as i32,
                    size,
                    scratch.addr_i32,
                    scratch.old_f64,
                    scratch.new_f64,
                );
            }
            StoreKind::V128 => {
                // V1: keep the store, log a write event with size=16
                // and old/new=0 (a placeholder until the v128 hook
                // signature lands). See the module-level docs.
                self.emit_save_new_addr(&mut replacement, &push, scratch.new_i64, ValType::V128);
                // For V128 we don't track old/new bytes yet; just
                // re-emit the store and the hook with zero
                // payloads.
                push(
                    &mut replacement,
                    Instr::LocalGet(walrus::ir::LocalGet {
                        local: scratch.addr_i32,
                    }),
                );
                // We saved `new` as i64 by reinterpret; that's
                // lossy for v128 so just drop it back as zero —
                // the placeholder. The store needs the original
                // value back on stack, which we lost: in V1 we
                // cannot reconstruct it. Therefore, for SIMD
                // stores, fall back to emitting the original
                // store untouched followed by a *non-paired*
                // event entry. We undo the work above by clearing
                // the replacement and re-emitting the original
                // store + a sentinel hook.
                replacement.clear();
                push(
                    &mut replacement,
                    Instr::Store(walrus::ir::Store {
                        memory: mem,
                        kind,
                        arg,
                    }),
                );
                // Sentinel store event: addr=static offset, size=16,
                // old=new=0.
                self.push_store_group_open(&mut replacement, &push, size);
                for (slot, value) in [
                    (0i32, walrus::ir::Value::I32(arg.offset as i32)),
                    (1, walrus::ir::Value::I64(0)),
                    (2, walrus::ir::Value::I64(0)),
                ] {
                    push(
                        &mut replacement,
                        Instr::Const(walrus::ir::Const {
                            value: walrus::ir::Value::I32(slot),
                        }),
                    );
                    push(&mut replacement, Instr::Const(walrus::ir::Const { value }));
                    push(
                        &mut replacement,
                        Instr::Call(walrus::ir::Call {
                            func: if slot == 0 {
                                self.hook_ids.emit_i32
                            } else {
                                self.hook_ids.emit_i64
                            },
                        }),
                    );
                }
                self.push_store_group_close(&mut replacement, &push, size);
            }
        }

        // Splice into the block: remove original at idx, insert
        // replacement in-place.
        let block = local_func.block_mut(block_id);
        block.instrs.splice(idx..=idx, replacement);
    }

    fn emit_save_new_addr(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        new_local: LocalId,
        _new_ty: ValType,
    ) {
        // Stack on entry to the synthetic block: [addr, value].
        push(
            replacement,
            Instr::LocalSet(walrus::ir::LocalSet { local: new_local }),
        );
        push(
            replacement,
            Instr::LocalSet(walrus::ir::LocalSet {
                local: self.scratch.addr_i32,
            }),
        );
    }

    /// Open a store-event group: `__ct_emit_call(FUNC_KIND_STORE, size)`.
    ///
    /// The withdrawn per-store write hook carried `(addr, size, old,
    /// new)` in one call. That hook is gone from the surface (spec
    /// § 5), so this experimental pass now reports the same four
    /// fields through the hooks that remain: the group header carries
    /// `size`, and the three values follow as a typed tuple.
    fn push_store_group_open(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        size: i32,
    ) {
        push(
            replacement,
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(FUNC_KIND_STORE),
            }),
        );
        push(
            replacement,
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(size),
            }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_call,
            }),
        );
    }

    /// Close the group opened by [`Self::push_store_group_open`].
    fn push_store_group_close(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        size: i32,
    ) {
        push(
            replacement,
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(FUNC_KIND_STORE),
            }),
        );
        push(
            replacement,
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(size),
            }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_return,
            }),
        );
    }

    /// Slot 0 of a store group: the effective address (base + static
    /// offset), as an `i32`.
    fn push_store_address(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        offset_const: i32,
        addr_local: LocalId,
    ) {
        push(
            replacement,
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(0),
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: addr_local }),
        );
        push(
            replacement,
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(offset_const),
            }),
        );
        push(
            replacement,
            Instr::Binop(walrus::ir::Binop {
                op: BinaryOp::I32Add,
            }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_i32,
            }),
        );
    }

    /// Slots 1 and 2 of a store group: the previous and the new value,
    /// each widened into the `i64` slot the way the withdrawn write
    /// hook did (zero-extend for narrower integers, bit-reinterpret
    /// for floats so the receiver sees a stable bit pattern).
    fn push_store_values(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        old_local: LocalId,
        new_local: LocalId,
        widen: &[UnaryOp],
    ) {
        for (slot, local) in [(1i32, old_local), (2i32, new_local)] {
            push(
                replacement,
                Instr::Const(walrus::ir::Const {
                    value: walrus::ir::Value::I32(slot),
                }),
            );
            push(replacement, Instr::LocalGet(walrus::ir::LocalGet { local }));
            for op in widen {
                push(replacement, Instr::Unop(walrus::ir::Unop { op: *op }));
            }
            push(
                replacement,
                Instr::Call(walrus::ir::Call {
                    func: self.hook_ids.emit_i64,
                }),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_hook_write_i32(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        offset_const: i32,
        size: i32,
        addr_local: LocalId,
        old_local: LocalId,
        new_local: LocalId,
    ) {
        self.push_store_group_open(replacement, push, size);
        self.push_store_address(replacement, push, offset_const, addr_local);
        self.push_store_values(
            replacement,
            push,
            old_local,
            new_local,
            &[UnaryOp::I64ExtendUI32],
        );
        self.push_store_group_close(replacement, push, size);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_hook_write_i64_direct(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        offset_const: i32,
        size: i32,
        addr_local: LocalId,
        old_local: LocalId,
        new_local: LocalId,
    ) {
        self.push_store_group_open(replacement, push, size);
        self.push_store_address(replacement, push, offset_const, addr_local);
        self.push_store_values(replacement, push, old_local, new_local, &[]);
        self.push_store_group_close(replacement, push, size);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_hook_write_f32(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        offset_const: i32,
        size: i32,
        addr_local: LocalId,
        old_local: LocalId,
        new_local: LocalId,
    ) {
        self.push_store_group_open(replacement, push, size);
        self.push_store_address(replacement, push, offset_const, addr_local);
        // f32 -> i32 (reinterpret) -> i64 (zero-extend)
        self.push_store_values(
            replacement,
            push,
            old_local,
            new_local,
            &[UnaryOp::I32ReinterpretF32, UnaryOp::I64ExtendUI32],
        );
        self.push_store_group_close(replacement, push, size);
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_hook_write_f64(
        &self,
        replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
        push: &impl Fn(&mut Vec<(Instr, walrus::ir::InstrLocId)>, Instr),
        offset_const: i32,
        size: i32,
        addr_local: LocalId,
        old_local: LocalId,
        new_local: LocalId,
    ) {
        self.push_store_group_open(replacement, push, size);
        self.push_store_address(replacement, push, offset_const, addr_local);
        self.push_store_values(
            replacement,
            push,
            old_local,
            new_local,
            &[UnaryOp::I64ReinterpretF64],
        );
        self.push_store_group_close(replacement, push, size);
    }
}

/// Every instruction sequence reachable from `root`, `root` included.
///
/// This is the one traversal the whole crate walks: every pass that
/// has to find something *inside* a function — the explicit-`return`
/// sites the export epilogue is spliced before, the imported-call
/// sites, the store sites of the retired interior pass, and the
/// branch-target scan the M35b restructuring is driven by — reaches
/// its work through here. A sequence this function does not yield is
/// a sequence no pass instruments, silently.
///
/// That is exactly how the exception-handling gap arose (M35, review
/// of 2026-08-01): until M35c only `block` / `loop` / `if`-`else` were
/// descended into, so a plain `return` inside a `try_table` body was
/// never wrapped and the crossing it took never closed. The nesting
/// forms of both exception-handling proposals are therefore descended
/// into here as well:
///
/// - `try_table`'s body (`seq`). Its *catch clauses* are not nesting —
///   a `TryTableCatch`'s `label` names an enclosing block, which makes
///   it a branch target, so it is handled by [`visit_branch_targets`]
///   instead.
/// - the legacy `try`'s body (`seq`) **and** its `catch` / `catch_all`
///   handler sequences, which unlike a `try_table` catch clause really
///   are nested sequences owned by the instruction. A legacy
///   `delegate` carries a relative depth rather than a sequence id and
///   so has nothing to descend into.
///
/// GC adds no new *nesting* forms — `br_on_cast` and friends carry a
/// label, not a body — so they need nothing here; they too are
/// [`visit_branch_targets`]'s business.
///
/// The order of the returned ids is load-bearing in one narrow but
/// important sense: it fixes the order the passes rewrite sequences
/// in, and therefore the bytes they emit. The arms below are additive,
/// so a function containing no exception-handling instruction yields
/// exactly the sequence, in exactly the order, it yielded before —
/// which is what keeps the rewrite byte-identical for every module
/// that does not use these proposals.
fn collect_block_ids(local_func: &walrus::LocalFunction, root: InstrSeqId) -> Vec<InstrSeqId> {
    let mut out = vec![root];
    let mut work = vec![root];
    while let Some(id) = work.pop() {
        for (instr, _) in &local_func.block(id).instrs {
            match instr {
                Instr::Block(walrus::ir::Block { seq }) | Instr::Loop(walrus::ir::Loop { seq }) => {
                    out.push(*seq);
                    work.push(*seq);
                }
                Instr::IfElse(walrus::ir::IfElse {
                    consequent,
                    alternative,
                }) => {
                    out.push(*consequent);
                    out.push(*alternative);
                    work.push(*consequent);
                    work.push(*alternative);
                }
                Instr::TryTable(walrus::ir::TryTable { seq, .. }) => {
                    out.push(*seq);
                    work.push(*seq);
                }
                Instr::Try(walrus::ir::Try { seq, catches }) => {
                    out.push(*seq);
                    work.push(*seq);
                    for catch in catches {
                        match catch {
                            walrus::ir::LegacyCatch::Catch { handler, .. }
                            | walrus::ir::LegacyCatch::CatchAll { handler } => {
                                out.push(*handler);
                                work.push(*handler);
                            }
                            // `delegate` re-throws to an enclosing
                            // block by relative depth; there is no
                            // handler sequence to walk.
                            walrus::ir::LegacyCatch::Delegate { .. } => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Imported-call boundary instrumentation
// ---------------------------------------------------------------------------

struct ImportedCallRewriter<'a> {
    hook_ids: &'a HookFunctionIds,
    imported_funcs: &'a ImportedFuncSet,
    scratch: Option<&'a ScratchPool>,
}

impl<'a> ImportedCallRewriter<'a> {
    fn rewrite(&mut self, local_func: &mut walrus::LocalFunction) {
        let entry = local_func.entry_block();
        let block_ids = collect_block_ids(local_func, entry);
        for id in block_ids {
            self.rewrite_block(local_func, id);
        }
    }

    fn rewrite_block(&mut self, local_func: &mut walrus::LocalFunction, id: InstrSeqId) {
        let original_len = local_func.block(id).instrs.len();
        for idx in (0..original_len).rev() {
            let (instr, loc) = local_func.block(id).instrs[idx].clone();
            if let Instr::Call(walrus::ir::Call { func }) = instr {
                if let Some((import_index, sig)) = self.imported_funcs.lookup(func) {
                    let sig = sig.clone();
                    self.replace_imported_call(local_func, id, idx, func, import_index, &sig, loc);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_imported_call(
        &mut self,
        local_func: &mut walrus::LocalFunction,
        block_id: InstrSeqId,
        idx: usize,
        target: FunctionId,
        import_index: u32,
        sig: &BoundarySignature,
        loc: walrus::ir::InstrLocId,
    ) {
        let mut replacement: Vec<(Instr, walrus::ir::InstrLocId)> = Vec::with_capacity(8);
        // PRE: emit_call(0, import_index); the argument tuple;
        //      realm_boundary(0, 0, import_index, token).
        //
        // The arguments go *between* the call event and the realm
        // marker so a host can split the flat value stream without
        // the signature: everything between `__ct_emit_call` and the
        // next non-value event is the argument tuple. See the framing
        // contract in `hooks`.
        push_call_event(
            &mut replacement,
            loc,
            self.hook_ids.emit_call,
            FUNC_KIND_IMPORT,
            import_index,
        );
        if let Some(scratch) = self.scratch {
            push_value_capture_group(&mut replacement, loc, self.hook_ids, scratch, &sig.params);
        }
        push_realm_boundary(
            &mut replacement,
            loc,
            self.hook_ids,
            REALM_DIRECTION_ENTER,
            FUNC_KIND_IMPORT,
            import_index,
        );
        // The original call.
        replacement.push((Instr::Call(walrus::ir::Call { func: target }), loc));
        // POST: the result tuple — *the* non-determinism this whole
        // design exists to capture, since replay feeds these back in
        // place of calling the real host (spec §§ 3.2, 6) — then
        // emit_return(0, import_index); realm_boundary(1, ...).
        if let Some(scratch) = self.scratch {
            push_value_capture_group(&mut replacement, loc, self.hook_ids, scratch, &sig.results);
        }
        push_call_event(
            &mut replacement,
            loc,
            self.hook_ids.emit_return,
            FUNC_KIND_IMPORT,
            import_index,
        );
        push_realm_boundary(
            &mut replacement,
            loc,
            self.hook_ids,
            REALM_DIRECTION_LEAVE,
            FUNC_KIND_IMPORT,
            import_index,
        );

        let block = local_func.block_mut(block_id);
        block.instrs.splice(idx..=idx, replacement);
    }
}

fn push_call_event(
    replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
    loc: walrus::ir::InstrLocId,
    hook: FunctionId,
    fn_kind: i32,
    fn_index: u32,
) {
    replacement.push((
        Instr::Const(walrus::ir::Const {
            value: walrus::ir::Value::I32(fn_kind),
        }),
        loc,
    ));
    replacement.push((
        Instr::Const(walrus::ir::Const {
            value: walrus::ir::Value::I32(fn_index as i32),
        }),
        loc,
    ));
    replacement.push((Instr::Call(walrus::ir::Call { func: hook }), loc));
}

fn push_realm_boundary(
    replacement: &mut Vec<(Instr, walrus::ir::InstrLocId)>,
    loc: walrus::ir::InstrLocId,
    hooks: &HookFunctionIds,
    direction: i32,
    fn_kind: i32,
    fn_index: u32,
) {
    replacement.push((
        Instr::Const(walrus::ir::Const {
            value: walrus::ir::Value::I32(direction),
        }),
        loc,
    ));
    replacement.push((
        Instr::Const(walrus::ir::Const {
            value: walrus::ir::Value::I32(fn_kind),
        }),
        loc,
    ));
    replacement.push((
        Instr::Const(walrus::ir::Const {
            value: walrus::ir::Value::I32(fn_index as i32),
        }),
        loc,
    ));
    replacement.push((
        Instr::Call(walrus::ir::Call {
            func: hooks.correlation_token,
        }),
        loc,
    ));
    replacement.push((
        Instr::Call(walrus::ir::Call {
            func: hooks.emit_realm_boundary,
        }),
        loc,
    ));
}

// ---------------------------------------------------------------------------
// Exported function entry/leave instrumentation
// ---------------------------------------------------------------------------

fn wrap_local_function_boundary(
    module: &mut Module,
    target: FunctionId,
    export_index: u32,
    hooks: &HookFunctionIds,
    sig: &BoundarySignature,
    scratch: Option<&ScratchPool>,
) {
    // M35b: an exit taken by branching to the function's *own* label
    // (`br`/`br_if`/`br_table` naming the entry sequence) jumps to the
    // end of the entry sequence — i.e. past an epilogue appended
    // there. Such an exit would carry neither the leave event nor the
    // result capture. The fix moves the body into a fresh inner block
    // typed `[] -> results` and re-points those branches at it, so a
    // branch that named the function label now lands *before* the
    // epilogue instead of after it.
    //
    // Deliberately scan first and restructure only when at least one
    // branch actually names the entry label. The vast majority of
    // functions have no such branch, and leaving them on the original
    // code path keeps their instrumented bytes identical to what the
    // golden/parity fixtures pinned — the new shape stays confined to
    // the one case it exists for, which is also the only case it has
    // been reasoned about for.
    if function_branches_to_entry(module, target) {
        reroot_body_into_inner_block(module, target);
    }

    let kind = match &mut module.funcs.get_mut(target).kind {
        FunctionKind::Local(lf) => lf,
        _ => return,
    };
    let entry_block_id = kind.entry_block();
    // Parameters arrive as the function's leading locals, so the
    // argument tuple needs no spill at all — it is already
    // addressable, and reading it at entry (before any `local.set`
    // can overwrite a parameter slot) reports what the caller passed.
    let params: Vec<LocalId> = kind.args.clone();

    // Pre-build the entry prologue + exit epilogue as two
    // separate instruction sequences, then splice them in.
    let mut prologue: Vec<(Instr, walrus::ir::InstrLocId)> = Vec::new();
    let mut epilogue: Vec<(Instr, walrus::ir::InstrLocId)> = Vec::new();
    let zero_loc = walrus::ir::InstrLocId::new(0);

    push_call_event(
        &mut prologue,
        zero_loc,
        hooks.emit_call,
        FUNC_KIND_EXPORT,
        export_index,
    );
    if scratch.is_some() {
        for (slot, (local, ty)) in params.iter().zip(sig.params.iter()).enumerate() {
            push_value_emit(&mut prologue, zero_loc, hooks, slot as i32, *local, *ty);
        }
    }
    push_realm_boundary(
        &mut prologue,
        zero_loc,
        hooks,
        REALM_DIRECTION_ENTER,
        FUNC_KIND_EXPORT,
        export_index,
    );

    if let Some(scratch) = scratch {
        push_value_capture_group(&mut epilogue, zero_loc, hooks, scratch, &sig.results);
    }
    push_call_event(
        &mut epilogue,
        zero_loc,
        hooks.emit_return,
        FUNC_KIND_EXPORT,
        export_index,
    );
    push_realm_boundary(
        &mut epilogue,
        zero_loc,
        hooks,
        REALM_DIRECTION_LEAVE,
        FUNC_KIND_EXPORT,
        export_index,
    );

    let block = kind.block_mut(entry_block_id);
    block.instrs.splice(0..0, prologue);
    block.instrs.extend(epilogue);

    // Also wrap any explicit `return` instructions that appear
    // inside any block of this function.
    wrap_returns_in_function(module, target, export_index, hooks, sig, scratch);
}

/// Does any `br` / `br_if` / `br_table` in `target` name the
/// function's own entry label?
///
/// Such a branch is an exit: it leaves the function with the results
/// on the stack, exactly as a fall-through or an explicit `return`
/// does. It is also the only exit shape that an epilogue appended to
/// the entry sequence cannot see, which is why it gets its own scan.
fn function_branches_to_entry(module: &Module, target: FunctionId) -> bool {
    let local_func = match &module.funcs.get(target).kind {
        FunctionKind::Local(lf) => lf,
        _ => return false,
    };
    let entry = local_func.entry_block();
    collect_block_ids(local_func, entry).into_iter().any(|id| {
        local_func.block(id).instrs.iter().any(|(instr, _)| {
            let mut hit = false;
            visit_branch_targets(instr, |block| hit |= *block == entry);
            hit
        })
    })
}

/// Move `target`'s body into a fresh inner block typed
/// `[] -> results`, re-pointing every branch that named the function
/// label at that block, and leave the entry sequence holding just the
/// `Instr::Block` that runs it.
///
/// The rewrite is stack-neutral: the inner block's result type is the
/// function's result type, so it leaves exactly the values the removed
/// fall-through left, in the same order, for an epilogue appended
/// after it. Nothing about the computation changes — only where a
/// label-targeting branch lands.
///
/// Re-pointing is a pure `InstrSeqId` substitution. walrus stores
/// branch targets symbolically and computes the relative depth a `br`
/// encodes only at emit time (by searching the enclosing block stack),
/// so introducing a new enclosing block renumbers every other depth in
/// the function for free and no depth arithmetic is needed here.
fn reroot_body_into_inner_block(module: &mut Module, target: FunctionId) {
    // `InstrSeqType::new` picks the encoding by arity: `Simple(None)`
    // for a void export, `Simple(Some(ty))` for the single-result
    // case, and `MultiValue(TypeId)` — a real entry in the type
    // section, encoded as `BlockType::FunctionType` — for two or more.
    //
    // The types come from the module's own type table rather than from
    // the `BoundarySignature`. The two agree whenever the signature is
    // representable, but a signature mentioning `externref`, `funcref`
    // or `v128` carries *empty* param and result vectors by design
    // (see `BoundarySignature::unrepresentable`), and with value
    // capture switched off such a module is instrumented rather than
    // refused. Typing the inner block from that empty vector would
    // emit a block that drops the function's results on the floor and
    // a module that no longer validates.
    let results: Vec<ValType> = module.types.results(module.funcs.get(target).ty()).to_vec();
    let seq_ty = walrus::ir::InstrSeqType::new(&mut module.types, &[], &results);

    let local_func = match &mut module.funcs.get_mut(target).kind {
        FunctionKind::Local(lf) => lf,
        _ => return,
    };
    let entry = local_func.entry_block();
    let inner = local_func.builder_mut().dangling_instr_seq(seq_ty).id();

    let body = std::mem::take(&mut local_func.block_mut(entry).instrs);
    local_func.block_mut(inner).instrs = body;

    // The body now lives under `inner`, so walk from there. The entry
    // sequence is empty at this point.
    for id in collect_block_ids(local_func, inner) {
        for (instr, _) in local_func.block_mut(id).instrs.iter_mut() {
            visit_branch_targets_mut(instr, |block| {
                if *block == entry {
                    *block = inner;
                }
            });
        }
    }

    local_func.block_mut(entry).instrs.push((
        Instr::Block(walrus::ir::Block { seq: inner }),
        walrus::ir::InstrLocId::new(0),
    ));
}

/// Apply `f` to every branch-target label `instr` carries.
///
/// Every label-carrying form is covered, not just the three MVP
/// branches: the GC proposal's `br_on_null` / `br_on_non_null` /
/// `br_on_cast` / `br_on_cast_fail`, and a `try_table`'s catch
/// clauses, whose `label` is a branch target in the enclosing scope
/// rather than a nested handler. (The *legacy* `try`'s `catch`
/// handlers are nested sequences, not labels — [`collect_block_ids`]
/// descends into them; `delegate` carries a relative depth and no
/// label at all.)
///
/// Two callers depend on this being exhaustive, and they fail
/// differently if it is not. [`function_branches_to_entry`] would miss
/// an exit and record it short; [`reroot_body_into_inner_block`] would
/// leave a re-rooted branch pointing at the now-empty entry sequence.
/// The substitution those two perform is type-safe for every form here
/// because the entry and inner labels are given identical result
/// types, so a branch that type-checked against one type-checks
/// against the other whatever values it carries.
fn visit_branch_targets(instr: &Instr, mut f: impl FnMut(&InstrSeqId)) {
    match instr {
        Instr::Br(walrus::ir::Br { block })
        | Instr::BrIf(walrus::ir::BrIf { block })
        | Instr::BrOnNull(walrus::ir::BrOnNull { block })
        | Instr::BrOnNonNull(walrus::ir::BrOnNonNull { block })
        | Instr::BrOnCast(walrus::ir::BrOnCast { block, .. })
        | Instr::BrOnCastFail(walrus::ir::BrOnCastFail { block, .. }) => f(block),
        Instr::BrTable(walrus::ir::BrTable { blocks, default }) => {
            for block in blocks.iter() {
                f(block);
            }
            f(default);
        }
        Instr::TryTable(walrus::ir::TryTable { catches, .. }) => {
            for catch in catches {
                match catch {
                    walrus::ir::TryTableCatch::Catch { label, .. }
                    | walrus::ir::TryTableCatch::CatchRef { label, .. }
                    | walrus::ir::TryTableCatch::CatchAll { label }
                    | walrus::ir::TryTableCatch::CatchAllRef { label } => f(label),
                }
            }
        }
        _ => {}
    }
}

/// [`visit_branch_targets`], by mutable reference.
fn visit_branch_targets_mut(instr: &mut Instr, mut f: impl FnMut(&mut InstrSeqId)) {
    match instr {
        Instr::Br(walrus::ir::Br { block })
        | Instr::BrIf(walrus::ir::BrIf { block })
        | Instr::BrOnNull(walrus::ir::BrOnNull { block })
        | Instr::BrOnNonNull(walrus::ir::BrOnNonNull { block })
        | Instr::BrOnCast(walrus::ir::BrOnCast { block, .. })
        | Instr::BrOnCastFail(walrus::ir::BrOnCastFail { block, .. }) => f(block),
        Instr::BrTable(walrus::ir::BrTable { blocks, default }) => {
            for block in blocks.iter_mut() {
                f(block);
            }
            f(default);
        }
        Instr::TryTable(walrus::ir::TryTable { catches, .. }) => {
            for catch in catches.iter_mut() {
                match catch {
                    walrus::ir::TryTableCatch::Catch { label, .. }
                    | walrus::ir::TryTableCatch::CatchRef { label, .. }
                    | walrus::ir::TryTableCatch::CatchAll { label }
                    | walrus::ir::TryTableCatch::CatchAllRef { label } => f(label),
                }
            }
        }
        _ => {}
    }
}

fn wrap_returns_in_function(
    module: &mut Module,
    target: FunctionId,
    export_index: u32,
    hooks: &HookFunctionIds,
    sig: &BoundarySignature,
    scratch: Option<&ScratchPool>,
) {
    let kind = match &mut module.funcs.get_mut(target).kind {
        FunctionKind::Local(lf) => lf,
        _ => return,
    };
    let entry = kind.entry_block();
    let block_ids = collect_block_ids(kind, entry);
    for bid in block_ids {
        let block = kind.block_mut(bid);
        let len = block.instrs.len();
        for idx in (0..len).rev() {
            if matches!(block.instrs[idx].0, Instr::Return(_)) {
                let loc = block.instrs[idx].1;
                let mut prefix: Vec<(Instr, walrus::ir::InstrLocId)> = Vec::new();
                // An explicit `return` has the function's results on
                // top of the stack, exactly as the fall-through exit
                // does, so the same spill/restore applies.
                if let Some(scratch) = scratch {
                    push_value_capture_group(&mut prefix, loc, hooks, scratch, &sig.results);
                }
                push_call_event(
                    &mut prefix,
                    loc,
                    hooks.emit_return,
                    FUNC_KIND_EXPORT,
                    export_index,
                );
                push_realm_boundary(
                    &mut prefix,
                    loc,
                    hooks,
                    REALM_DIRECTION_LEAVE,
                    FUNC_KIND_EXPORT,
                    export_index,
                );
                block.instrs.splice(idx..idx, prefix);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest possible round-trip: an empty module instruments
    /// to an empty module (plus the host-import declarations and
    /// the custom marker section).
    #[test]
    fn empty_module_round_trip() {
        let wat = "(module)";
        let input = wat::parse_str(wat).unwrap();
        let out = Pipeline::new().run_bytes(&input).unwrap();
        // Re-parse the output to confirm structural validity.
        let module = Module::from_buffer(&out).unwrap();
        // The hook surface has a size, stated here as a literal rather
        // than as `ALL_HOOKS.len()`. Comparing the emitted imports
        // against `ALL_HOOKS` alone would be satisfied by a surface
        // that lost a hook in both places at once; this line is what
        // makes dropping one a test failure.
        assert_eq!(
            hooks::ALL_HOOKS.len(),
            8,
            "the hook surface changed size — spec § 5 lists exactly four \
             control hooks, the token source, and one value hook per WASM \
             scalar type"
        );
        // One import per declared hook, and nothing else.
        assert_eq!(module.imports.iter().count(), hooks::ALL_HOOKS.len());
        let declared: Vec<&str> = module.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(declared, hooks::ALL_HOOKS);
        // The custom marker section is present.
        assert!(module
            .customs
            .iter()
            .any(|(_, c)| c.name() == hooks::CUSTOM_SECTION_NAME));
    }

    /// A module that performs one `i32.store` gets exactly one store
    /// event group injected — one group header, and a group carrying
    /// the full `(addr, old, new)` tuple.
    ///
    /// The withdrawn write hook carried those three fields as its own
    /// arguments, so counting hook calls was enough to prove the record
    /// was complete. Now the fields arrive as separate value hooks, so
    /// the count of headers alone would pass for a group that reported
    /// an address and nothing else; the tuple is checked explicitly
    /// below to keep the assertion as strong as the one it replaced.
    ///
    /// The pass is off by default from M36 on (spec §§ 2, 11), so this
    /// test asks for it explicitly. It is still worth keeping: the
    /// pass stays reachable behind the flag, and a reachable pass that
    /// nothing exercises rots.
    #[test]
    fn single_i32_store_instrumented_once() {
        let wat = r#"
            (module
              (memory (export "mem") 1)
              (func (export "write") (param $addr i32) (param $val i32)
                local.get $addr
                local.get $val
                i32.store))
        "#;
        let input = wat::parse_str(wat).unwrap();
        let out = Pipeline::with_config(PipelineConfig {
            instrument_stores: true,
            ..PipelineConfig::default()
        })
        .run_bytes(&input)
        .unwrap();
        let module = Module::from_buffer(&out).unwrap();

        // A store now reports through the group header
        // `__ct_emit_call(FUNC_KIND_STORE, size)` rather than a
        // dedicated write hook, so count those headers.
        let call_id = module
            .imports
            .iter()
            .find(|i| i.name == hooks::HOOK_CALL)
            .and_then(|i| match i.kind {
                walrus::ImportKind::Function(f) => Some(f),
                _ => None,
            })
            .expect("hook import missing");

        let return_id = module
            .imports
            .iter()
            .find(|i| i.name == hooks::HOOK_RETURN)
            .and_then(|i| match i.kind {
                walrus::ImportKind::Function(f) => Some(f),
                _ => None,
            })
            .expect("hook import missing");
        let value_hooks: Vec<FunctionId> = module
            .imports
            .iter()
            .filter(|i| {
                [
                    hooks::HOOK_EMIT_I32,
                    hooks::HOOK_EMIT_I64,
                    hooks::HOOK_EMIT_F32_BITS,
                    hooks::HOOK_EMIT_F64_BITS,
                ]
                .contains(&i.name.as_str())
            })
            .filter_map(|i| match i.kind {
                walrus::ImportKind::Function(f) => Some(f),
                _ => None,
            })
            .collect();

        let mut store_groups = 0u32;
        let mut tuple_widths: Vec<usize> = Vec::new();
        for (_, lf) in module.funcs.iter_local() {
            let entry = lf.entry_block();
            for bid in collect_block_ids(lf, entry) {
                let instrs = &lf.block(bid).instrs;
                for (i, (instr, _)) in instrs.iter().enumerate() {
                    if let Instr::Call(walrus::ir::Call { func }) = instr {
                        if *func == call_id
                            && preceding_const_i32(instrs, i, 1) == Some(FUNC_KIND_STORE)
                        {
                            store_groups += 1;
                            // Count the value hooks between this header
                            // and the group's closing marker.
                            let mut width = 0usize;
                            for (later, _) in instrs.iter().skip(i + 1) {
                                match later {
                                    Instr::Call(walrus::ir::Call { func: f })
                                        if *f == return_id =>
                                    {
                                        break
                                    }
                                    Instr::Call(walrus::ir::Call { func: f })
                                        if value_hooks.contains(f) =>
                                    {
                                        width += 1
                                    }
                                    _ => {}
                                }
                            }
                            tuple_widths.push(width);
                        }
                    }
                }
            }
        }
        assert_eq!(
            store_groups, 1,
            "expected exactly one store event group per store"
        );
        assert_eq!(
            tuple_widths,
            vec![3],
            "a store group must carry the full (addr, old, new) tuple, \
             which is what the withdrawn write hook reported in one call"
        );
    }

    /// The `n`-th `i32.const` preceding `at`, scanning backwards.
    fn preceding_const_i32(
        instrs: &[(Instr, walrus::ir::InstrLocId)],
        at: usize,
        n: usize,
    ) -> Option<i32> {
        let mut seen = 0;
        for j in (0..at).rev() {
            if let Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(v),
            }) = &instrs[j].0
            {
                if seen == n {
                    return Some(*v);
                }
                seen += 1;
            }
        }
        None
    }
}
