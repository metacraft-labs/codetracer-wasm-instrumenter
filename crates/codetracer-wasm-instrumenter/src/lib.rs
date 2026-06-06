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
//! 1. **Memory stores** — every `i32.store` / `i64.store` /
//!    `f32.store` / `f64.store` (and the sized variants like
//!    `i32.store8`) emits an `__ct_emit_write(addr, size, old, new)`
//!    call right after the store completes. The hook receives the
//!    effective address (i.e. base + static offset), the byte size
//!    of the store, and both the **previous** and the **new** value
//!    in the same 64-bit slot (zero-extended for narrower stores,
//!    bit-reinterpreted for floats so the receiver sees a stable
//!    bit pattern).
//!
//! 2. **Imported call boundaries** — every `call` whose target is
//!    an imported function is preceded by `__ct_emit_call(0,
//!    fn_index)` and followed by `__ct_emit_return(0, fn_index)`.
//!    These are the natural realm-crossing points in a browser
//!    embedding (host calls from WASM into JS).
//!
//! 3. **Exported function boundaries** — every function reachable
//!    via an `export` entry receives an entry-prologue
//!    `__ct_emit_call(1, fn_index)` and a return-epilogue
//!    `__ct_emit_return(1, fn_index)` (the export index is the
//!    index of the matching entry in the original `export` section
//!    — stable across re-instrumentation). These are the natural
//!    realm-crossing points in the reverse direction (calls from
//!    JS into WASM).
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

#![deny(rust_2018_idioms, unused_must_use)]
#![warn(missing_docs)]

use anyhow::{Context, Result};
use std::path::Path;
use walrus::ir::{BinaryOp, ExtendedLoad, Instr, InstrSeqId, LoadKind, MemArg, StoreKind, UnaryOp};
use walrus::{FunctionId, FunctionKind, LocalId, MemoryId, Module, ModuleConfig, ValType};

pub mod config;
pub mod hooks;

pub use config::PipelineConfig;

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
        let mut module = Module::from_buffer_with_config(input, &walrus_config())
            .context("failed to parse input WASM module")?;
        self.instrument_module(&mut module)?;
        Ok(module.emit_wasm())
    }

    /// Read `input` from disk, instrument, and write to `output`.
    pub fn run_files<P: AsRef<Path>, Q: AsRef<Path>>(&self, input: P, output: Q) -> Result<()> {
        let bytes = std::fs::read(input.as_ref())
            .with_context(|| format!("failed to read {}", input.as_ref().display()))?;
        let instrumented = self.run_bytes(&bytes)?;
        std::fs::write(output.as_ref(), instrumented)
            .with_context(|| format!("failed to write {}", output.as_ref().display()))?;
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

        let hook_ids = HookFunctionIds::register(module, &self.config);
        let memories = collect_memory_ids(module);

        if self.config.instrument_stores {
            self.instrument_stores(module, &hook_ids, &memories)?;
        }
        if self.config.instrument_imported_calls {
            self.instrument_imported_calls(module, &hook_ids, &imported_funcs)?;
        }
        if self.config.instrument_exported_functions {
            self.instrument_exported_functions(module, &hook_ids, &exported_targets)?;
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
    ) -> Result<()> {
        for (_func_id, local_func) in module.funcs.iter_local_mut() {
            ImportedCallRewriter {
                hook_ids,
                imported_funcs,
            }
            .rewrite(local_func);
        }
        Ok(())
    }

    fn instrument_exported_functions(
        &self,
        module: &mut Module,
        hook_ids: &HookFunctionIds,
        exports: &[(FunctionId, u32)],
    ) -> Result<()> {
        for &(func_id, export_index) in exports {
            // Only local functions can be wrapped — re-exporting an
            // imported function would mean the call dispatches
            // straight to the host, so the boundary is already
            // recorded by `instrument_imported_calls`.
            let kind = &module.funcs.get(func_id).kind;
            if matches!(kind, FunctionKind::Local(_)) {
                wrap_local_function_boundary(module, func_id, export_index, hook_ids);
            }
        }
        Ok(())
    }
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
    emit_write: FunctionId,
    emit_call: FunctionId,
    emit_return: FunctionId,
    emit_realm_boundary: FunctionId,
    correlation_token: FunctionId,
}

impl HookFunctionIds {
    fn register(module: &mut Module, config: &PipelineConfig) -> Self {
        // __ct_emit_write(addr: i32, size: i32, old: i64, new: i64)
        let write_ty = module.types.add(
            &[ValType::I32, ValType::I32, ValType::I64, ValType::I64],
            &[],
        );
        let (emit_write, _) =
            module.add_import_func(&config.host_module_name, hooks::HOOK_WRITE, write_ty);

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

        HookFunctionIds {
            emit_write,
            emit_call,
            emit_return,
            emit_realm_boundary,
            correlation_token,
        }
    }
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
                set.push(func_id, idx);
            }
            idx += 1;
        }
    }
    set
}

fn is_codetracer_hook_import(import: &walrus::Import) -> bool {
    matches!(
        import.name.as_str(),
        hooks::HOOK_WRITE
            | hooks::HOOK_CALL
            | hooks::HOOK_RETURN
            | hooks::HOOK_REALM_BOUNDARY
            | hooks::HOOK_CORRELATION_TOKEN
    )
}

#[derive(Debug, Default, Clone)]
struct ImportedFuncSet {
    /// In-order list of imported function ids that are eligible for
    /// import-call wrapping, paired with their stable "imported
    /// function index" in the WASM import section. The
    /// instrumenter's own hooks are deliberately omitted from this
    /// list so they don't get wrapped recursively, but their
    /// section indices are still skipped so the indices we report
    /// match the index space the original module sees.
    entries: Vec<(FunctionId, u32)>,
}

impl ImportedFuncSet {
    fn push(&mut self, id: FunctionId, index: u32) {
        self.entries.push((id, index));
    }

    fn index_of(&self, id: FunctionId) -> Option<u32> {
        self.entries
            .iter()
            .find_map(|(fid, idx)| (*fid == id).then_some(*idx))
    }
}

fn collect_exported_function_targets(module: &Module) -> Vec<(FunctionId, u32)> {
    let mut out = Vec::new();
    for (idx, export) in module.exports.iter().enumerate() {
        if let walrus::ExportItem::Function(func_id) = export.item {
            out.push((func_id, idx as u32));
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
                // Sentinel write event: addr=0, size=16, old=0, new=0.
                push(
                    &mut replacement,
                    Instr::Const(walrus::ir::Const {
                        value: walrus::ir::Value::I32(arg.offset as i32),
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Const(walrus::ir::Const {
                        value: walrus::ir::Value::I32(size),
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Const(walrus::ir::Const {
                        value: walrus::ir::Value::I64(0),
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Const(walrus::ir::Const {
                        value: walrus::ir::Value::I64(0),
                    }),
                );
                push(
                    &mut replacement,
                    Instr::Call(walrus::ir::Call {
                        func: self.hook_ids.emit_write,
                    }),
                );
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
        // eff_addr = addr + offset_const
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
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(size),
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: old_local }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I64ExtendUI32,
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: new_local }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I64ExtendUI32,
            }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_write,
            }),
        );
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
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(size),
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: old_local }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: new_local }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_write,
            }),
        );
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
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(size),
            }),
        );
        // old: f32 -> i32 (reinterpret) -> i64 (zero-extend)
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: old_local }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I32ReinterpretF32,
            }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I64ExtendUI32,
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: new_local }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I32ReinterpretF32,
            }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I64ExtendUI32,
            }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_write,
            }),
        );
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
            Instr::Const(walrus::ir::Const {
                value: walrus::ir::Value::I32(size),
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: old_local }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I64ReinterpretF64,
            }),
        );
        push(
            replacement,
            Instr::LocalGet(walrus::ir::LocalGet { local: new_local }),
        );
        push(
            replacement,
            Instr::Unop(walrus::ir::Unop {
                op: UnaryOp::I64ReinterpretF64,
            }),
        );
        push(
            replacement,
            Instr::Call(walrus::ir::Call {
                func: self.hook_ids.emit_write,
            }),
        );
    }
}

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
                if let Some(import_index) = self.imported_funcs.index_of(func) {
                    self.replace_imported_call(local_func, id, idx, func, import_index, loc);
                }
            }
        }
    }

    fn replace_imported_call(
        &mut self,
        local_func: &mut walrus::LocalFunction,
        block_id: InstrSeqId,
        idx: usize,
        target: FunctionId,
        import_index: u32,
        loc: walrus::ir::InstrLocId,
    ) {
        let mut replacement: Vec<(Instr, walrus::ir::InstrLocId)> = Vec::with_capacity(8);
        // PRE: emit_call(0, import_index); realm_boundary(0, 0, import_index, token)
        push_call_event(
            &mut replacement,
            loc,
            self.hook_ids.emit_call,
            FUNC_KIND_IMPORT,
            import_index,
        );
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
        // POST: emit_return(0, import_index); realm_boundary(1, 0, import_index, token)
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

const FUNC_KIND_IMPORT: i32 = 0;
const FUNC_KIND_EXPORT: i32 = 1;
const REALM_DIRECTION_ENTER: i32 = 0;
const REALM_DIRECTION_LEAVE: i32 = 1;

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
) {
    let kind = match &mut module.funcs.get_mut(target).kind {
        FunctionKind::Local(lf) => lf,
        _ => return,
    };
    let entry_block_id = kind.entry_block();

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
    push_realm_boundary(
        &mut prologue,
        zero_loc,
        hooks,
        REALM_DIRECTION_ENTER,
        FUNC_KIND_EXPORT,
        export_index,
    );

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
    wrap_returns_in_function(module, target, export_index, hooks);
}

fn wrap_returns_in_function(
    module: &mut Module,
    target: FunctionId,
    export_index: u32,
    hooks: &HookFunctionIds,
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
        // Five imports (the five __ct_emit_* hooks).
        assert_eq!(module.imports.iter().count(), 5);
        // The custom marker section is present.
        assert!(module
            .customs
            .iter()
            .any(|(_, c)| c.name() == hooks::CUSTOM_SECTION_NAME));
    }

    /// A module that performs one `i32.store` gets one
    /// `__ct_emit_write` injected.
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
        let out = Pipeline::new().run_bytes(&input).unwrap();
        let module = Module::from_buffer(&out).unwrap();

        // Count `Call` instructions targeting the __ct_emit_write hook.
        let write_id = module
            .imports
            .iter()
            .find(|i| i.name == hooks::HOOK_WRITE)
            .and_then(|i| match i.kind {
                walrus::ImportKind::Function(f) => Some(f),
                _ => None,
            })
            .expect("hook import missing");

        let mut write_calls = 0u32;
        for (_, lf) in module.funcs.iter_local() {
            let entry = lf.entry_block();
            for bid in collect_block_ids(lf, entry) {
                for (instr, _) in &lf.block(bid).instrs {
                    if let Instr::Call(walrus::ir::Call { func }) = instr {
                        if *func == write_id {
                            write_calls += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(
            write_calls, 1,
            "expected exactly one __ct_emit_write per store"
        );
    }
}
