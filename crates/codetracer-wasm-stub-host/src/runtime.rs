//! A *real* WASM host for instrumented modules.
//!
//! The rest of this crate walks module IR statically. That is enough
//! to check which events an instrumented module *would* emit, but it
//! cannot see a single value: an argument arrives as `local.get 0`,
//! and its value exists only at runtime. M35 is entirely about values,
//! so verifying it needs an engine.
//!
//! This module instantiates a module under `wasmi`, supplies the
//! `__codetracer` hook surface with recording implementations, and
//! returns the resulting event stream together with the exported
//! call's own results and the final contents of the exported memory.
//!
//! Because [`run_module`] does not require the module to be
//! instrumented, the same function runs the *original* module too —
//! which is how the parity property is checked: same export, same
//! arguments, same import results, and the two runs must agree on the
//! returned values and on memory, byte for byte. That is a stronger
//! statement than "the instrumented module still validates", and it
//! is the property spec § 10 rests on.
//!
//! ## Why floats are carried as bit patterns
//!
//! [`RecordedValue`] stores `f32` / `f64` as their IEEE bits rather
//! than as Rust floats. `NaN != NaN` under `PartialEq`, so a float
//! comparison would report success for a capture that lost a NaN
//! payload — and spec § 7 makes exactly that a replay divergence.
//! Bits also make `-0.0` distinguishable from `0.0`.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use wasmi::core::{ValueType, F32, F64};
use wasmi::{Engine, Extern, Func, FuncType, Linker, Module, Store, Value};

use codetracer_wasm_instrumenter::hooks;

/// One boundary value, as observed at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedValue {
    /// `i32`.
    I32(i32),
    /// `i64`.
    I64(i64),
    /// `f32`, held as its IEEE-754 bit pattern.
    F32Bits(u32),
    /// `f64`, held as its IEEE-754 bit pattern.
    F64Bits(u64),
}

impl RecordedValue {
    /// Convenience constructor from a Rust `f32`.
    pub fn f32(v: f32) -> Self {
        RecordedValue::F32Bits(v.to_bits())
    }

    /// Convenience constructor from a Rust `f64`.
    pub fn f64(v: f64) -> Self {
        RecordedValue::F64Bits(v.to_bits())
    }

    /// The WASM value type this value carries.
    pub fn value_type(self) -> ValueType {
        match self {
            RecordedValue::I32(_) => ValueType::I32,
            RecordedValue::I64(_) => ValueType::I64,
            RecordedValue::F32Bits(_) => ValueType::F32,
            RecordedValue::F64Bits(_) => ValueType::F64,
        }
    }

    fn to_wasmi(self) -> Value {
        match self {
            RecordedValue::I32(v) => Value::I32(v),
            RecordedValue::I64(v) => Value::I64(v),
            RecordedValue::F32Bits(bits) => Value::F32(F32::from_bits(bits)),
            RecordedValue::F64Bits(bits) => Value::F64(F64::from_bits(bits)),
        }
    }

    fn from_wasmi(value: &Value) -> Result<Self> {
        Ok(match value {
            Value::I32(v) => RecordedValue::I32(*v),
            Value::I64(v) => RecordedValue::I64(*v),
            Value::F32(v) => RecordedValue::F32Bits(v.to_bits()),
            Value::F64(v) => RecordedValue::F64Bits(v.to_bits()),
            other => bail!("unsupported boundary value type: {other:?}"),
        })
    }
}

/// One event observed at runtime through the `__codetracer` imports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// `__ct_emit_call`.
    Call {
        /// `0` import, `1` export, `2` an experimental store event.
        fn_kind: i32,
        /// Index within the corresponding section.
        fn_index: u32,
    },
    /// `__ct_emit_return`.
    Return {
        /// `0` import, `1` export, `2` an experimental store event.
        fn_kind: i32,
        /// Index within the corresponding section.
        fn_index: u32,
    },
    /// `__ct_emit_realm_boundary`.
    RealmBoundary {
        /// `0` entering the foreign realm, `1` leaving it.
        direction: i32,
        /// `0` import, `1` export.
        fn_kind: i32,
        /// Index within the corresponding section.
        fn_index: u32,
        /// The correlation token handed out for this crossing.
        token: i64,
    },
    /// One of `__ct_emit_{i32,i64,f32,f64}`.
    Value {
        /// Position within the argument or result tuple.
        slot: i32,
        /// The value itself.
        value: RecordedValue,
    },
}

/// A stub for one of the module's own (non-hook) imports.
///
/// `results` is a *sequence*: the n-th call to the import returns
/// `results[n]`, with the final entry repeating once the sequence is
/// exhausted. That is what lets a test drive the same import from
/// several call sites and still tell the recorded results apart.
#[derive(Debug, Clone)]
pub struct ImportStub {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub name: String,
    /// Values returned by successive calls.
    pub results: Vec<Vec<RecordedValue>>,
    /// When set, the stub calls back *into* the module — invoking the
    /// named export with the given arguments and returning its
    /// results as its own.
    ///
    /// This is how the host re-entry case is exercised: an export
    /// calls an import, and the host services it by calling another
    /// export. The inner export's boundary capture then runs while
    /// the outer one is still open, which is exactly the situation a
    /// shared scratch-local pool would have to survive.
    pub reentry: Option<(String, Vec<RecordedValue>)>,
}

impl ImportStub {
    /// A stub returning nothing.
    pub fn void(module: &str, name: &str) -> Self {
        Self {
            module: module.to_string(),
            name: name.to_string(),
            results: vec![vec![]],
            reentry: None,
        }
    }

    /// A stub whose successive calls return the given tuples.
    pub fn returning(module: &str, name: &str, results: Vec<Vec<RecordedValue>>) -> Self {
        Self {
            module: module.to_string(),
            name: name.to_string(),
            results,
            reentry: None,
        }
    }

    /// A stub that services the call by re-entering `export`.
    pub fn reentering(module: &str, name: &str, export: &str, args: Vec<RecordedValue>) -> Self {
        Self {
            module: module.to_string(),
            name: name.to_string(),
            results: Vec::new(),
            reentry: Some((export.to_string(), args)),
        }
    }
}

/// Everything one run of a module produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRecording {
    /// Hook events, in the order the module emitted them. Empty for an
    /// un-instrumented module.
    pub events: Vec<RuntimeEvent>,
    /// The values the invoked export returned.
    pub results: Vec<RecordedValue>,
    /// Final contents of the module's exported memory, when it has
    /// one. Part of the parity comparison: instrumentation must not
    /// change what the module wrote.
    pub memory: Option<Vec<u8>>,
}

/// Instantiate `wasm` under `wasmi`, call `export` with `args`, and
/// return everything observed.
///
/// Works on both an instrumented and an un-instrumented module: the
/// hook imports are defined unconditionally and simply go unused when
/// the module does not import them.
pub fn run_module(
    wasm: &[u8],
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> Result<RuntimeRecording> {
    let engine = Engine::default();
    let module = Module::new(&engine, &mut &wasm[..]).context("wasmi rejected the module")?;
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);

    let events: Arc<Mutex<Vec<RuntimeEvent>>> = Arc::new(Mutex::new(Vec::new()));
    define_hooks(&mut linker, &mut store, &events)?;
    define_stubs(&mut linker, &mut store, &module, stubs)?;

    let instance = linker
        .instantiate(&mut store, &module)
        .context("instantiation failed")?
        .start(&mut store)
        .context("module start function trapped")?;

    let func = instance
        .get_export(&store, export)
        .and_then(Extern::into_func)
        .ok_or_else(|| anyhow!("module has no exported function `{export}`"))?;
    let ty = func.ty(&store);
    let inputs: Vec<Value> = args.iter().map(|v| v.to_wasmi()).collect();
    let mut outputs: Vec<Value> = ty.results().iter().map(|t| Value::default(*t)).collect();
    func.call(&mut store, &inputs, &mut outputs)
        .with_context(|| format!("calling `{export}` trapped"))?;

    // Memory is part of the parity comparison, so find it by kind
    // rather than by a conventional name: a module that exports its
    // memory as `memory` must be compared just as closely as one that
    // calls it `mem`.
    let memory_export = module
        .exports()
        .find_map(|e| matches!(e.ty(), wasmi::ExternType::Memory(_)).then(|| e.name().to_string()));
    let memory = memory_export
        .and_then(|name| instance.get_export(&store, &name))
        .and_then(Extern::into_memory)
        .map(|m| m.data(&store).to_vec());

    let results = outputs
        .iter()
        .map(RecordedValue::from_wasmi)
        .collect::<Result<Vec<_>>>()?;
    let events = std::mem::take(&mut *events.lock().expect("hook recorder mutex poisoned"));
    Ok(RuntimeRecording {
        events,
        results,
        memory,
    })
}

fn define_hooks(
    linker: &mut Linker<()>,
    store: &mut Store<()>,
    events: &Arc<Mutex<Vec<RuntimeEvent>>>,
) -> Result<()> {
    let host = hooks::DEFAULT_HOST_MODULE;

    let sink = Arc::clone(events);
    let emit_call = Func::new(
        &mut *store,
        FuncType::new([ValueType::I32, ValueType::I32], []),
        move |_caller, params, _results| {
            sink.lock().expect("poisoned").push(RuntimeEvent::Call {
                fn_kind: params[0].i32().unwrap_or(0),
                fn_index: params[1].i32().unwrap_or(0) as u32,
            });
            Ok(())
        },
    );
    linker.define(host, hooks::HOOK_CALL, emit_call)?;

    let sink = Arc::clone(events);
    let emit_return = Func::new(
        &mut *store,
        FuncType::new([ValueType::I32, ValueType::I32], []),
        move |_caller, params, _results| {
            sink.lock().expect("poisoned").push(RuntimeEvent::Return {
                fn_kind: params[0].i32().unwrap_or(0),
                fn_index: params[1].i32().unwrap_or(0) as u32,
            });
            Ok(())
        },
    );
    linker.define(host, hooks::HOOK_RETURN, emit_return)?;

    let sink = Arc::clone(events);
    let emit_realm = Func::new(
        &mut *store,
        FuncType::new(
            [
                ValueType::I32,
                ValueType::I32,
                ValueType::I32,
                ValueType::I64,
            ],
            [],
        ),
        move |_caller, params, _results| {
            sink.lock()
                .expect("poisoned")
                .push(RuntimeEvent::RealmBoundary {
                    direction: params[0].i32().unwrap_or(0),
                    fn_kind: params[1].i32().unwrap_or(0),
                    fn_index: params[2].i32().unwrap_or(0) as u32,
                    token: params[3].i64().unwrap_or(0),
                });
            Ok(())
        },
    );
    linker.define(host, hooks::HOOK_REALM_BOUNDARY, emit_realm)?;

    // Strictly monotonic, as the M25 bridge requires.
    let counter = Arc::new(Mutex::new(0i64));
    let token = Func::new(
        &mut *store,
        FuncType::new([], [ValueType::I64]),
        move |_caller, _params, results| {
            let mut guard = counter.lock().expect("poisoned");
            *guard += 1;
            results[0] = Value::I64(*guard);
            Ok(())
        },
    );
    linker.define(host, hooks::HOOK_CORRELATION_TOKEN, token)?;

    for (name, ty) in [
        (hooks::HOOK_EMIT_I32, ValueType::I32),
        (hooks::HOOK_EMIT_I64, ValueType::I64),
        (hooks::HOOK_EMIT_F32, ValueType::F32),
        (hooks::HOOK_EMIT_F64, ValueType::F64),
    ] {
        let sink = Arc::clone(events);
        let hook = Func::new(
            &mut *store,
            FuncType::new([ValueType::I32, ty], []),
            move |_caller, params, _results| {
                let value = RecordedValue::from_wasmi(&params[1])
                    .expect("value hooks only carry scalar types");
                sink.lock().expect("poisoned").push(RuntimeEvent::Value {
                    slot: params[0].i32().unwrap_or(0),
                    value,
                });
                Ok(())
            },
        );
        linker.define(host, name, hook)?;
    }
    Ok(())
}

fn define_stubs(
    linker: &mut Linker<()>,
    store: &mut Store<()>,
    module: &Module,
    stubs: &[ImportStub],
) -> Result<()> {
    for import in module.imports() {
        if import.module() == hooks::DEFAULT_HOST_MODULE {
            continue;
        }
        let func_ty = match import.ty() {
            wasmi::ExternType::Func(ty) => ty.clone(),
            other => bail!(
                "the runtime harness only stubs function imports; \
                 `{}::{}` is a {other:?}",
                import.module(),
                import.name(),
            ),
        };
        let stub = stubs
            .iter()
            .find(|s| s.module == import.module() && s.name == import.name())
            .ok_or_else(|| {
                anyhow!(
                    "no stub supplied for import `{}::{}`",
                    import.module(),
                    import.name()
                )
            })?;
        let sequence = stub.results.clone();
        let reentry = stub.reentry.clone();
        let calls = Arc::new(Mutex::new(0usize));
        let expected = func_ty.results().len();
        let name = format!("{}::{}", import.module(), import.name());
        let func = Func::new(&mut *store, func_ty, move |mut caller, _params, results| {
            let mut guard = calls.lock().expect("poisoned");
            let nth = *guard;
            *guard += 1;
            drop(guard);
            if let Some((export, args)) = &reentry {
                let callee = caller
                    .get_export(export)
                    .and_then(Extern::into_func)
                    .unwrap_or_else(|| panic!("re-entry target `{export}` is not an export"));
                let inputs: Vec<Value> = args.iter().map(|v| v.to_wasmi()).collect();
                // A trap inside the re-entered export is a real
                // failure of the module under test, so surface it
                // rather than swallowing it into a generic error.
                match callee.call(&mut caller, &inputs, results) {
                    Ok(()) => return Ok(()),
                    Err(wasmi::Error::Trap(trap)) => return Err(trap),
                    Err(other) => panic!("re-entering `{export}` failed: {other}"),
                }
            }
            let tuple = sequence
                .get(nth)
                .or_else(|| sequence.last())
                .cloned()
                .unwrap_or_default();
            assert_eq!(
                tuple.len(),
                expected,
                "stub for `{name}` returned {} values for a {expected}-result import",
                tuple.len(),
            );
            for (slot, value) in tuple.iter().enumerate() {
                results[slot] = value.to_wasmi();
            }
            Ok(())
        });
        linker.define(import.module(), import.name(), func)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Framing: turning the flat event stream back into per-boundary tuples
// ---------------------------------------------------------------------------

/// One boundary crossing, with its argument and result tuples
/// reassembled from the flat value stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryFrame {
    /// `0` import, `1` export, `2` an experimental store event.
    pub fn_kind: i32,
    /// Index within the corresponding section.
    pub fn_index: u32,
    /// Arguments, in slot order.
    pub args: Vec<RecordedValue>,
    /// Results, in slot order.
    pub results: Vec<RecordedValue>,
    /// Slot indices as they arrived, for the argument tuple.
    pub arg_slots: Vec<i32>,
    /// Slot indices as they arrived, for the result tuple.
    pub result_slots: Vec<i32>,
}

/// Reassemble boundary frames from a runtime event stream.
///
/// Applies the framing contract documented in
/// `codetracer_wasm_instrumenter::hooks`: the run of value events
/// immediately after `__ct_emit_call` is the argument tuple, the run
/// immediately before `__ct_emit_return` is the result tuple. A host
/// can therefore split the stream without the signature; the manifest
/// remains the authority for *typing* it.
pub fn boundary_frames(events: &[RuntimeEvent]) -> Vec<BoundaryFrame> {
    let mut frames: Vec<BoundaryFrame> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut pending: Vec<(i32, RecordedValue)> = Vec::new();
    // `Some(idx)` while the pending run is still the argument tuple of
    // frame `idx` — i.e. nothing but value events has happened since
    // that frame's `__ct_emit_call`.
    let mut pending_args_of: Option<usize> = None;

    let assign = |frame: &mut BoundaryFrame, run: Vec<(i32, RecordedValue)>, as_args: bool| {
        let (slots, values): (Vec<i32>, Vec<RecordedValue>) = run.into_iter().unzip();
        if as_args {
            frame.arg_slots = slots;
            frame.args = values;
        } else {
            frame.result_slots = slots;
            frame.results = values;
        }
    };

    for event in events {
        match event {
            RuntimeEvent::Value { slot, value } => pending.push((*slot, *value)),
            RuntimeEvent::Call { fn_kind, fn_index } => {
                if let (false, Some(idx)) = (pending.is_empty(), pending_args_of) {
                    assign(&mut frames[idx], std::mem::take(&mut pending), true);
                }
                pending.clear();
                frames.push(BoundaryFrame {
                    fn_kind: *fn_kind,
                    fn_index: *fn_index,
                    args: Vec::new(),
                    results: Vec::new(),
                    arg_slots: Vec::new(),
                    result_slots: Vec::new(),
                });
                open.push(frames.len() - 1);
                pending_args_of = Some(frames.len() - 1);
            }
            RuntimeEvent::Return { .. } => {
                if let Some(idx) = open.pop() {
                    if !pending.is_empty() {
                        let as_args = pending_args_of == Some(idx);
                        assign(&mut frames[idx], std::mem::take(&mut pending), as_args);
                    }
                }
                pending.clear();
                pending_args_of = None;
            }
            RuntimeEvent::RealmBoundary { .. } => {
                if let (false, Some(idx)) = (pending.is_empty(), pending_args_of) {
                    assign(&mut frames[idx], std::mem::take(&mut pending), true);
                }
                pending.clear();
                pending_args_of = None;
            }
        }
    }
    frames
}

/// Frames for one `(fn_kind, fn_index)` pair, in call order.
pub fn frames_for(events: &[RuntimeEvent], fn_kind: i32, fn_index: u32) -> Vec<BoundaryFrame> {
    boundary_frames(events)
        .into_iter()
        .filter(|f| f.fn_kind == fn_kind && f.fn_index == fn_index)
        .collect()
}
