//! M27 verification tests #2 + #3:
//!
//! - `test_wasm_recorder_runtime_emits_store_events` — a module
//!   executing a known sequence of `i32.store` instructions emits
//!   one store event per store.
//! - `test_wasm_recorder_runtime_emits_realm_crossing_events` —
//!   every JS↔WASM call site produces a recorded boundary event
//!   with a correlation token.
//!
//! The verification is performed against the `codetracer-wasm-stub-host`
//! event recorder which walks the instrumented module's IR
//! deterministically. A follow-on test bundle runs the same
//! modules through a real WASM runtime — see
//! `crates/codetracer-wasm-stub-host/tests/parity.rs` for the
//! cross-modality parity check (#4).

use codetracer_wasm_instrumenter::Pipeline;
use codetracer_wasm_stub_host::{record_instrumented, Event};

fn instrument_wat(wat: &str) -> Vec<u8> {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    Pipeline::new()
        .run_bytes(&original)
        .expect("pipeline must succeed")
}

#[test]
fn test_wasm_recorder_runtime_emits_store_events() {
    // Four distinct i32.store instructions: the module writes
    // four i32 values to ascending byte offsets in the default
    // memory.
    let wat = r#"
        (module
          (memory (export "mem") 1)
          (func (export "fill")
            ;; *(addr=0)  = 0xaa
            i32.const 0
            i32.const 0xaa
            i32.store

            ;; *(addr=4)  = 0xbb
            i32.const 4
            i32.const 0xbb
            i32.store

            ;; *(addr=8)  = 0xcc
            i32.const 8
            i32.const 0xcc
            i32.store

            ;; *(addr=12) = 0xdd
            i32.const 12
            i32.const 0xdd
            i32.store))
    "#;
    let instrumented = instrument_wat(wat);
    let events = record_instrumented(&instrumented).expect("walk");

    let writes: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::Write { .. }))
        .collect();
    assert_eq!(
        writes.len(),
        4,
        "expected one store event per store; saw {:?}",
        writes
    );
    for w in writes {
        assert!(
            matches!(w, Event::Write { size: 4, .. }),
            "expected size=4 for i32.store; saw {w:?}"
        );
    }
}

#[test]
fn test_wasm_recorder_runtime_emits_store_events_mixed_widths() {
    // Mixed-width stores: i32, i64, f32, f64, and sized variants.
    let wat = r#"
        (module
          (memory (export "mem") 1)
          (func (export "mixed")
            i32.const 0
            i32.const 0x55
            i32.store8         ;; size 1

            i32.const 0
            i32.const 0x5555
            i32.store16        ;; size 2

            i32.const 0
            i64.const 0x55555555_55555555
            i64.store          ;; size 8

            i32.const 0
            f32.const 1.5
            f32.store          ;; size 4

            i32.const 0
            f64.const 2.5
            f64.store))        ;; size 8
    "#;
    let instrumented = instrument_wat(wat);
    let events = record_instrumented(&instrumented).expect("walk");
    let sizes: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            Event::Write { size, .. } => Some(*size),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, vec![1, 2, 8, 4, 8], "mixed-width store sizes wrong");
}

#[test]
fn test_wasm_recorder_runtime_emits_realm_crossing_events() {
    // One imported function (`env.callback`); one exported
    // function (`run`) that calls it twice. The expected event
    // shape per call to the import is:
    //
    //   Call(import, 0) → RealmBoundary(enter, import, 0)
    //   Return(import, 0) → RealmBoundary(leave, import, 0)
    //
    // The exported function wrap adds:
    //
    //   Call(export, 0) → RealmBoundary(enter, export, 0) [entry]
    //   Return(export, 0) → RealmBoundary(leave, export, 0) [exit]
    let wat = r#"
        (module
          (import "env" "callback" (func $cb (param i32)))
          (func (export "run") (param $arg i32)
            local.get $arg
            call $cb
            local.get $arg
            call $cb))
    "#;
    let instrumented = instrument_wat(wat);
    let events = record_instrumented(&instrumented).expect("walk");

    // Every JS↔WASM call produces a RealmBoundary pair. We assert
    // both halves of every boundary appear, paired in order.
    let boundaries: Vec<&Event> = events
        .iter()
        .filter(|e| matches!(e, Event::RealmBoundary { .. }))
        .collect();

    // Expected boundary pairs (in DFS order):
    // 1. export enter, 2. import enter, 3. import leave,
    // 4. import enter, 5. import leave, 6. export leave.
    let expected: Vec<Event> = vec![
        Event::RealmBoundary {
            direction: 0,
            fn_kind: 1,
            fn_index: 0,
        }, // export enter "run"
        Event::RealmBoundary {
            direction: 0,
            fn_kind: 0,
            fn_index: 0,
        }, // import enter $cb
        Event::RealmBoundary {
            direction: 1,
            fn_kind: 0,
            fn_index: 0,
        }, // import leave $cb
        Event::RealmBoundary {
            direction: 0,
            fn_kind: 0,
            fn_index: 0,
        }, // second $cb enter
        Event::RealmBoundary {
            direction: 1,
            fn_kind: 0,
            fn_index: 0,
        }, // second $cb leave
        Event::RealmBoundary {
            direction: 1,
            fn_kind: 1,
            fn_index: 0,
        }, // export leave "run"
    ];
    let actual: Vec<Event> = boundaries.into_iter().cloned().collect();
    assert_eq!(actual, expected, "realm-crossing event sequence mismatch");
}

#[test]
fn test_wasm_recorder_runtime_realm_boundary_carries_correlation_token() {
    // We can't observe the *runtime* value of the correlation
    // token from a static IR walk, but we *can* assert the
    // instrumented module declares the
    // `__ct_correlation_token() -> i64` import and calls it
    // exactly once per `__ct_emit_realm_boundary` invocation
    // (i.e. the bridge between M27 and M25 is structurally
    // present).
    let wat = r#"
        (module
          (import "env" "callback" (func $cb (param i32)))
          (func (export "run") (param $arg i32)
            local.get $arg
            call $cb))
    "#;
    let instrumented = instrument_wat(wat);
    let module = walrus::Module::from_buffer(&instrumented).expect("re-parse");

    let token_id = module
        .imports
        .iter()
        .find(|i| i.name == codetracer_wasm_instrumenter::hooks::HOOK_CORRELATION_TOKEN)
        .and_then(|i| match i.kind {
            walrus::ImportKind::Function(f) => Some(f),
            _ => None,
        })
        .expect("correlation token import must be declared");
    let realm_id = module
        .imports
        .iter()
        .find(|i| i.name == codetracer_wasm_instrumenter::hooks::HOOK_REALM_BOUNDARY)
        .and_then(|i| match i.kind {
            walrus::ImportKind::Function(f) => Some(f),
            _ => None,
        })
        .expect("realm boundary import must be declared");

    let mut token_calls = 0u32;
    let mut realm_calls = 0u32;
    for (_, lf) in module.funcs.iter_local() {
        let entry = lf.entry_block();
        // Walk all blocks (entry plus nested) so the boundary
        // hooks emitted inside nested control flow also count.
        let mut stack = vec![entry];
        while let Some(id) = stack.pop() {
            for (instr, _) in &lf.block(id).instrs {
                use walrus::ir::Instr;
                match instr {
                    Instr::Call(walrus::ir::Call { func }) => {
                        if *func == token_id {
                            token_calls += 1;
                        }
                        if *func == realm_id {
                            realm_calls += 1;
                        }
                    }
                    Instr::Block(walrus::ir::Block { seq })
                    | Instr::Loop(walrus::ir::Loop { seq }) => stack.push(*seq),
                    Instr::IfElse(walrus::ir::IfElse {
                        consequent,
                        alternative,
                    }) => {
                        stack.push(*consequent);
                        stack.push(*alternative);
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(realm_calls > 0, "no realm-boundary calls were inserted");
    assert_eq!(
        token_calls, realm_calls,
        "every __ct_emit_realm_boundary must be preceded by a fresh \
         __ct_correlation_token() (M25 bridge); saw {token_calls} tokens \
         for {realm_calls} boundary calls",
    );
}
