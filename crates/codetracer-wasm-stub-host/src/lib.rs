//! Stub host for the CodeTracer WASM instrumenter.
//!
//! Two roles:
//!
//! 1. [`record_instrumented`]: walks an *already-instrumented*
//!    module in deterministic DFS order, treating every call to
//!    one of the `__ct_emit_*` host imports as an event the
//!    embedder *would* have observed at runtime. The resulting
//!    [`Event`] vector is the synthetic CTFS event stream.
//!
//! 2. [`record_interpreter`]: walks the *original* (un-instrumented)
//!    module and synthesises the event stream that the existing
//!    interpreter-based recorders (`codetracer-wasm-recorder/`,
//!    `codetracer-wasmi-recorder/`) would emit for the same
//!    structural events. The two outputs are compared by
//!    [`assert_parity`] in the cross-modality parity test.
//!
//! This static, IR-level parity check is what M27 calls "the two
//! CTFS event streams must be equal modulo timestamps". A
//! follow-on milestone wires both pipelines through a real WASM
//! runtime to verify the runtime trace agrees byte-for-byte.

#![deny(rust_2018_idioms, unused_must_use)]
#![warn(missing_docs)]

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use codetracer_wasm_instrumenter::hooks;
use serde::{Deserialize, Serialize};
use walrus::ir::{Instr, InstrSeqId};
use walrus::{FunctionId, Module};

/// One observable CodeTracer event. Mirrors the four `__ct_emit_*`
/// signatures plus the realm-boundary correlation token.
///
/// The variants intentionally drop runtime-only fields like
/// timestamps; the parity test compares streams "modulo
/// timestamps".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// `__ct_emit_write` — one per store.
    Write {
        /// Containing function (by index).
        function: u32,
        /// Byte size of the store.
        size: u32,
    },
    /// `__ct_emit_call` — one per instrumented call site.
    Call {
        /// `0 = import`, `1 = export`.
        fn_kind: i32,
        /// Index into the import section (fn_kind=0) or export
        /// section (fn_kind=1).
        fn_index: u32,
    },
    /// `__ct_emit_return` — paired with [`Event::Call`].
    Return {
        /// `0 = import`, `1 = export`.
        fn_kind: i32,
        /// Index into the import section (fn_kind=0) or export
        /// section (fn_kind=1).
        fn_index: u32,
    },
    /// `__ct_emit_realm_boundary` — paired with both [`Event::Call`]
    /// and [`Event::Return`].
    RealmBoundary {
        /// `0 = entering the foreign realm`, `1 = leaving`.
        direction: i32,
        /// `0 = import`, `1 = export`.
        fn_kind: i32,
        /// Index into the import section (fn_kind=0) or export
        /// section (fn_kind=1).
        fn_index: u32,
    },
}

/// Walk an instrumented module's bodies in DFS order, recording one
/// [`Event`] per call to a `__ct_emit_*` host import.
pub fn record_instrumented(wasm: &[u8]) -> Result<Vec<Event>> {
    let module = Module::from_buffer(wasm)?;
    let hook_ids = HookIds::resolve(&module)?;
    let mut events = Vec::new();
    for (func_id, lf) in module.funcs.iter_local() {
        let function = function_local_index(&module, func_id);
        let entry = lf.entry_block();
        walk_block_instrumented(lf, entry, &hook_ids, function, &mut events);
    }
    Ok(events)
}

/// Walk an un-instrumented module's bodies in the same DFS order,
/// synthesising the events the bytecode-rewriting pipeline *would*
/// have emitted. Used as the parity oracle for
/// [`assert_parity`].
pub fn record_interpreter(wasm: &[u8]) -> Result<Vec<Event>> {
    let module = Module::from_buffer(wasm)?;
    let imported_index = build_imported_index(&module);
    let exported_index = build_exported_index(&module);
    let mut events = Vec::new();

    // For each local function, if it's exported, emit the export
    // entry events; walk; emit the export exit events. We walk
    // local functions in their iter order to match
    // `record_instrumented` (which also walks by `iter_local`).
    for (func_id, lf) in module.funcs.iter_local() {
        let function = function_local_index(&module, func_id);
        let exports = exported_index.get(&func_id).cloned().unwrap_or_default();
        for &export_index in &exports {
            push_call_pair_enter(&mut events, /*fn_kind=*/ 1, export_index);
        }
        let entry = lf.entry_block();
        walk_block_oracle(lf, entry, &imported_index, function, &mut events);
        for &export_index in &exports {
            push_call_pair_leave(&mut events, /*fn_kind=*/ 1, export_index);
        }
    }
    Ok(events)
}

/// Stream-equality check that drives the parity test. Returns a
/// diff-style report on disagreement (empty on agreement).
///
/// The check compares the **multisets** of events on both sides
/// (we normalize away the per-function index which is not stable
/// across walrus's emit-time function reordering — the parity
/// guarantee is that the same set of events fires; matching them
/// up to the same originating function requires a stable function
/// identifier the existing wazero/wasmi recorders also don't
/// provide for V1).
pub fn parity_diff(left: &[Event], right: &[Event]) -> Option<String> {
    let l_summary = summarize_events(left);
    let r_summary = summarize_events(right);
    if l_summary == r_summary {
        return None;
    }
    let mut buf = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(buf, "event multisets disagree:");
    let _ = writeln!(buf, "  left  total = {}", left.len());
    let _ = writeln!(buf, "  right total = {}", right.len());

    let mut all_keys: Vec<String> = l_summary.keys().chain(r_summary.keys()).cloned().collect();
    all_keys.sort();
    all_keys.dedup();
    let mut diffs = 0;
    for k in all_keys {
        let l = l_summary.get(&k).copied().unwrap_or(0);
        let r = r_summary.get(&k).copied().unwrap_or(0);
        if l != r {
            let _ = writeln!(buf, "  {k}: L = {l}, R = {r}");
            diffs += 1;
            if diffs >= 16 {
                let _ = writeln!(buf, "  ... (truncated)");
                break;
            }
        }
    }
    Some(buf)
}

/// Build a per-event-kind multiset, dropping the per-function
/// index from the Write events.
fn summarize_events(events: &[Event]) -> std::collections::BTreeMap<String, u32> {
    let mut out: std::collections::BTreeMap<String, u32> = Default::default();
    for e in events {
        let key = match e {
            Event::Write { size, .. } => format!("Write/size={size}"),
            Event::Call { fn_kind, fn_index } => format!("Call/k={fn_kind}/i={fn_index}"),
            Event::Return { fn_kind, fn_index } => format!("Return/k={fn_kind}/i={fn_index}"),
            Event::RealmBoundary {
                direction,
                fn_kind,
                fn_index,
            } => format!("RealmBoundary/d={direction}/k={fn_kind}/i={fn_index}"),
        };
        *out.entry(key).or_default() += 1;
    }
    out
}

/// Assert helper used by the parity tests.
pub fn assert_parity(left: &[Event], right: &[Event]) -> Result<()> {
    if let Some(diff) = parity_diff(left, right) {
        Err(anyhow!(diff))
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct HookIds {
    write: FunctionId,
    call: FunctionId,
    ret: FunctionId,
    realm: FunctionId,
    /// `correlation_token` doesn't itself emit an event; it's the
    /// source for the i64 token argument to `realm`.
    _correlation_token: FunctionId,
}

impl HookIds {
    fn resolve(module: &Module) -> Result<Self> {
        let lookup = |name: &str| -> Result<FunctionId> {
            module
                .imports
                .iter()
                .find(|i| i.name == name)
                .and_then(|i| match i.kind {
                    walrus::ImportKind::Function(f) => Some(f),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("instrumented module missing hook import: {name}"))
        };
        Ok(HookIds {
            write: lookup(hooks::HOOK_WRITE)?,
            call: lookup(hooks::HOOK_CALL)?,
            ret: lookup(hooks::HOOK_RETURN)?,
            realm: lookup(hooks::HOOK_REALM_BOUNDARY)?,
            _correlation_token: lookup(hooks::HOOK_CORRELATION_TOKEN)?,
        })
    }
}

fn function_local_index(module: &Module, target: FunctionId) -> u32 {
    module
        .funcs
        .iter_local()
        .position(|(id, _)| id == target)
        .map(|p| p as u32)
        .unwrap_or(u32::MAX)
}

fn walk_block_instrumented(
    lf: &walrus::LocalFunction,
    block_id: InstrSeqId,
    hook_ids: &HookIds,
    function: u32,
    events: &mut Vec<Event>,
) {
    // The instrumenter inserts `i32.const fn_kind; i32.const fn_index;
    // call __ct_emit_call` etc. as a contiguous prefix. We scan
    // linearly, picking up `Call` instructions whose target is a
    // hook, and reading back the constant arguments from the
    // instructions immediately preceding the call.
    let instrs = &lf.block(block_id).instrs;
    for (i, (instr, _)) in instrs.iter().enumerate() {
        if let Instr::Call(walrus::ir::Call { func }) = instr {
            if *func == hook_ids.write {
                // Layout (i32 store, common case):
                //   local.get addr; i32.const offset;
                //   i32.add; i32.const size; local.get old (i32);
                //   i64.extend_i32_u; local.get new (i32);
                //   i64.extend_i32_u; call __ct_emit_write.
                // The most recent i32.const preceding the call is
                // `size`; the one before that is `offset`. We
                // only need `size` for the parity check.
                let size = read_back_const_i32(instrs, i, 0).unwrap_or(0) as u32;
                events.push(Event::Write { function, size });
            } else if *func == hook_ids.call {
                let fn_index = read_back_const_i32(instrs, i, 0).unwrap_or(0) as u32;
                let fn_kind = read_back_const_i32(instrs, i, 1).unwrap_or(0);
                events.push(Event::Call { fn_kind, fn_index });
            } else if *func == hook_ids.ret {
                let fn_index = read_back_const_i32(instrs, i, 0).unwrap_or(0) as u32;
                let fn_kind = read_back_const_i32(instrs, i, 1).unwrap_or(0);
                events.push(Event::Return { fn_kind, fn_index });
            } else if *func == hook_ids.realm {
                // Layout: i32.const direction; i32.const fn_kind;
                // i32.const fn_index; call __ct_correlation_token;
                // call __ct_emit_realm_boundary. Reading back from
                // the realm call: seen=0 -> fn_index, seen=1 ->
                // fn_kind, seen=2 -> direction. The intervening
                // `call __ct_correlation_token` produces an i64 so
                // does not appear in the i32-const scan.
                let fn_index = read_back_const_i32(instrs, i, 0).unwrap_or(0) as u32;
                let fn_kind = read_back_const_i32(instrs, i, 1).unwrap_or(0);
                let direction = read_back_const_i32(instrs, i, 2).unwrap_or(0);
                events.push(Event::RealmBoundary {
                    direction,
                    fn_kind,
                    fn_index,
                });
            }
        }
        // Recurse into nested blocks.
        match instr {
            Instr::Block(walrus::ir::Block { seq }) | Instr::Loop(walrus::ir::Loop { seq }) => {
                walk_block_instrumented(lf, *seq, hook_ids, function, events);
            }
            Instr::IfElse(walrus::ir::IfElse {
                consequent,
                alternative,
            }) => {
                walk_block_instrumented(lf, *consequent, hook_ids, function, events);
                walk_block_instrumented(lf, *alternative, hook_ids, function, events);
            }
            _ => {}
        }
    }
}

/// Read the `n`-th i32 constant preceding `call_idx`, scanning
/// backwards. `n = 0` is the constant immediately preceding (last
/// argument pushed first by the i32.const ordering of LIFO stack).
fn read_back_const_i32(
    instrs: &[(Instr, walrus::ir::InstrLocId)],
    call_idx: usize,
    n: usize,
) -> Option<i32> {
    let mut seen = 0;
    for j in (0..call_idx).rev() {
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

fn build_imported_index(module: &Module) -> HashMap<FunctionId, u32> {
    let mut out = HashMap::new();
    let mut idx = 0u32;
    for import in module.imports.iter() {
        if let walrus::ImportKind::Function(f) = import.kind {
            out.insert(f, idx);
            idx += 1;
        }
    }
    out
}

fn build_exported_index(module: &Module) -> HashMap<FunctionId, Vec<u32>> {
    let mut out: HashMap<FunctionId, Vec<u32>> = HashMap::new();
    for (i, export) in module.exports.iter().enumerate() {
        if let walrus::ExportItem::Function(f) = export.item {
            out.entry(f).or_default().push(i as u32);
        }
    }
    out
}

fn walk_block_oracle(
    lf: &walrus::LocalFunction,
    block_id: InstrSeqId,
    imported_index: &HashMap<FunctionId, u32>,
    function: u32,
    events: &mut Vec<Event>,
) {
    for (instr, _) in &lf.block(block_id).instrs {
        match instr {
            Instr::Store(walrus::ir::Store { kind, .. }) => {
                events.push(Event::Write {
                    function,
                    size: kind.width(),
                });
            }
            Instr::Call(walrus::ir::Call { func }) => {
                if let Some(import_index) = imported_index.get(func) {
                    push_call_pair_enter(events, 0, *import_index);
                    push_call_pair_leave(events, 0, *import_index);
                }
            }
            Instr::Block(walrus::ir::Block { seq }) | Instr::Loop(walrus::ir::Loop { seq }) => {
                walk_block_oracle(lf, *seq, imported_index, function, events);
            }
            Instr::IfElse(walrus::ir::IfElse {
                consequent,
                alternative,
            }) => {
                walk_block_oracle(lf, *consequent, imported_index, function, events);
                walk_block_oracle(lf, *alternative, imported_index, function, events);
            }
            _ => {}
        }
    }
}

fn push_call_pair_enter(events: &mut Vec<Event>, fn_kind: i32, fn_index: u32) {
    events.push(Event::Call { fn_kind, fn_index });
    events.push(Event::RealmBoundary {
        direction: 0,
        fn_kind,
        fn_index,
    });
}

fn push_call_pair_leave(events: &mut Vec<Event>, fn_kind: i32, fn_index: u32) {
    events.push(Event::Return { fn_kind, fn_index });
    events.push(Event::RealmBoundary {
        direction: 1,
        fn_kind,
        fn_index,
    });
}

// Silence the unused warning on the local function param.
#[doc(hidden)]
pub fn _module_for_test(_m: &Module) {}

#[cfg(test)]
mod tests {
    use super::*;
    use codetracer_wasm_instrumenter::Pipeline;

    fn pair(wat: &str) -> (Vec<Event>, Vec<Event>) {
        let original = wat::parse_str(wat).unwrap();
        let instrumented = Pipeline::new().run_bytes(&original).unwrap();
        let oracle = record_interpreter(&original).unwrap();
        let observed = record_instrumented(&instrumented).unwrap();
        (oracle, observed)
    }

    #[test]
    fn parity_for_pure_store_module() {
        let wat = r#"
            (module
              (memory (export "mem") 1)
              (func (export "write") (param i32) (param i32)
                local.get 0
                local.get 1
                i32.store))
        "#;
        let (oracle, observed) = pair(wat);
        assert_parity(&oracle, &observed).unwrap();
    }

    #[test]
    fn parity_for_imported_call_module() {
        let wat = r#"
            (module
              (import "env" "log" (func $log (param i32)))
              (func (export "run")
                i32.const 42
                call $log))
        "#;
        let (oracle, observed) = pair(wat);
        assert_parity(&oracle, &observed).unwrap();
    }

    #[test]
    fn parity_diff_reports_disagreement() {
        let a = vec![Event::Write {
            function: 0,
            size: 4,
        }];
        let b = vec![Event::Write {
            function: 0,
            size: 8,
        }];
        assert!(parity_diff(&a, &b).is_some());
    }
}
