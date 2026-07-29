//! M35 verification: boundary value capture at the WASM
//! import/export edges.
//!
//! `Recording-Backends/WASM-Instrumentation-Layer.md` §§ 3, 5, 7, 8.
//!
//! Every assertion here is made against **executed** modules, not
//! against module IR. A static walk can prove that a value hook was
//! inserted; only a run can prove it carried the right number. The
//! engine is `wasmi`, driven through
//! `codetracer_wasm_stub_host::runtime`.
//!
//! No mocks are used. The "stub host" is a real WASM embedder
//! supplying real implementations of the `__codetracer` imports and of
//! the module's own imports — the same role a browser page plays. The
//! module under test is the real instrumenter output.

use std::path::PathBuf;

use codetracer_wasm_instrumenter::{hooks, ManifestBoundary, Pipeline, PipelineConfig, ScalarType};
use codetracer_wasm_stub_host::runtime::{
    boundary_frames, frames_for, run_module, BoundaryFrame, ImportStub, RecordedValue, RuntimeEvent,
};

const FN_KIND_IMPORT: i32 = hooks::FUNC_KIND_IMPORT;
const FN_KIND_EXPORT: i32 = hooks::FUNC_KIND_EXPORT;

fn instrument(wat: &str) -> Vec<u8> {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    Pipeline::new()
        .run_bytes(&original)
        .expect("pipeline must succeed")
}

/// Instrument, run, and hand back the frames — the shape almost every
/// test below wants.
fn record(
    wat: &str,
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> (Vec<BoundaryFrame>, Vec<RecordedValue>, Vec<RuntimeEvent>) {
    let instrumented = instrument(wat);
    let run = run_module(&instrumented, export, args, stubs).expect("instrumented run");
    (boundary_frames(&run.events), run.results, run.events)
}

/// Run the original and the instrumented module side by side and
/// require them to agree on everything observable.
///
/// Both rewrite configurations are checked, and the store one is not
/// optional. Non-interference is the property that does not care which
/// pass is enabled — it is a statement about the rewriter, not about
/// what gets reported — and the store pass is the *more* invasive of
/// the two: it reloads the old value at every store site, which is
/// exactly the kind of splice that could disturb the operand stack or
/// clobber memory. Until M36 this function inherited the store pass
/// from `PipelineConfig::default()`; when that default flipped to
/// boundary-only the coverage would have quietly disappeared with it,
/// which is why the configurations are named here rather than
/// inherited.
fn assert_computes_identically(
    wat: &str,
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    let before = run_module(&original, export, args, stubs).expect("original run");
    assert!(
        before.events.is_empty(),
        "the original module must emit no hook events"
    );

    let configs = [
        ("boundary-only (the default)", PipelineConfig::default()),
        (
            "boundary + the interior store pass",
            PipelineConfig {
                instrument_stores: true,
                ..PipelineConfig::default()
            },
        ),
    ];

    for (label, config) in configs {
        let instrumented = Pipeline::with_config(config)
            .run_bytes(&original)
            .expect("instrument");
        let after = run_module(&instrumented, export, args, stubs).expect("instrumented run");

        assert_eq!(
            before.results, after.results,
            "instrumentation changed the value `{export}` returned ({label})"
        );
        assert_eq!(
            before.memory, after.memory,
            "instrumentation changed what `{export}` wrote to memory ({label})"
        );
    }
}

// ---------------------------------------------------------------------------
// Verification: export arguments and results
// ---------------------------------------------------------------------------

/// `(i32, i64, f32, f64) -> i64`: all four arguments and the result
/// recorded with exact values *and* exact types.
#[test]
fn verify_export_arguments_and_results_are_recorded() {
    // The two float parameters are deliberately unused by the
    // computation. A capture that read the operand stack instead of
    // the parameter locals would have nothing to find for them.
    let wat = r#"
        (module
          (func (export "mix") (param i32) (param i64) (param f32) (param f64) (result i64)
            local.get 1
            local.get 0
            i64.extend_i32_s
            i64.add))
    "#;
    let args = [
        RecordedValue::I32(-7),
        RecordedValue::I64(1_000_000_000_000),
        RecordedValue::f32(1.5),
        RecordedValue::f64(-2.25),
    ];
    let (frames, results, _) = record(wat, "mix", &args, &[]);

    let export_frames: Vec<_> = frames
        .iter()
        .filter(|f| f.fn_kind == FN_KIND_EXPORT)
        .collect();
    assert_eq!(export_frames.len(), 1, "one export crossing: {frames:#?}");
    let frame = export_frames[0];

    assert_eq!(
        frame.args,
        vec![
            RecordedValue::I32(-7),
            RecordedValue::I64(1_000_000_000_000),
            RecordedValue::F32Bits(1.5f32.to_bits()),
            RecordedValue::F64Bits((-2.25f64).to_bits()),
        ],
        "every parameter, in declaration order, with its own type"
    );
    assert_eq!(
        frame.arg_slots,
        vec![0, 1, 2, 3],
        "slots must be the positional index within the tuple"
    );
    assert_eq!(
        frame.results,
        vec![RecordedValue::I64(1_000_000_000_000 - 7)],
        "the result, with its exact value"
    );
    assert_eq!(frame.result_slots, vec![0]);
    assert_eq!(results, vec![RecordedValue::I64(1_000_000_000_000 - 7)]);
}

/// The recorded types are the module's declared types, not whatever
/// the host happened to widen them to. Checked by asserting the
/// *variant*, which is what carries the type.
#[test]
fn verify_export_value_types_match_the_declared_signature() {
    let wat = r#"
        (module
          (func (export "id_f32") (param f32) (result f32)
            local.get 0))
    "#;
    let (frames, _, _) = record(wat, "id_f32", &[RecordedValue::f32(0.1)], &[]);
    let frame = &frames[0];
    assert!(
        matches!(frame.args[0], RecordedValue::F32Bits(_)),
        "an f32 argument must arrive through the f32 hook, not widened: {:?}",
        frame.args[0]
    );
    assert!(matches!(frame.results[0], RecordedValue::F32Bits(_)));
    // 0.1f32 widened to f64 and narrowed back is still 0.1f32, so the
    // bit check below is what actually rules widening out: 0.1f32's
    // bits are not 0.1f64's bits.
    assert_eq!(frame.args[0], RecordedValue::F32Bits(0.1f32.to_bits()));
}

// ---------------------------------------------------------------------------
// Verification: import results — the replay inputs
// ---------------------------------------------------------------------------

/// An imported call's arguments *and* return values appear in the log.
/// The results are the whole point: they are the non-determinism M37
/// feeds back in place of calling the real host.
#[test]
fn verify_import_results_are_recorded() {
    let wat = r#"
        (module
          (import "env" "clock" (func $clock (result i64)))
          (import "env" "scale" (func $scale (param i32) (param f64) (result f64)))
          (func (export "run") (result f64)
            i32.const 7
            f64.const 2.5
            call $scale
            call $clock
            f64.convert_i64_u
            f64.add))
    "#;
    let stubs = [
        ImportStub::returning(
            "env",
            "clock",
            vec![vec![RecordedValue::I64(0x0123_4567_89ab_cdef)]],
        ),
        ImportStub::returning("env", "scale", vec![vec![RecordedValue::f64(17.5)]]),
    ];
    let (_, _, events) = record(wat, "run", &[], &stubs);

    // Import indices are the module's own, assigned before the hook
    // imports were registered: clock = 0, scale = 1.
    let clock = frames_for(&events, FN_KIND_IMPORT, 0);
    assert_eq!(clock.len(), 1, "one call to env.clock");
    assert!(clock[0].args.is_empty(), "env.clock takes no arguments");
    assert_eq!(
        clock[0].results,
        vec![RecordedValue::I64(0x0123_4567_89ab_cdef)],
        "without this the replayer has no clock value to feed back"
    );

    let scale = frames_for(&events, FN_KIND_IMPORT, 1);
    assert_eq!(scale.len(), 1, "one call to env.scale");
    assert_eq!(
        scale[0].args,
        vec![RecordedValue::I32(7), RecordedValue::f64(2.5)],
        "arguments let the replay assert it reached the same call"
    );
    assert_eq!(scale[0].results, vec![RecordedValue::f64(17.5)]);
}

/// The same import reached from two call sites records two frames with
/// each call's own result — a single-slot capture would collapse them.
#[test]
fn verify_import_results_are_recorded_per_call_site() {
    let wat = r#"
        (module
          (import "env" "next" (func $next (result i32)))
          (func (export "twice") (result i32)
            call $next
            call $next
            i32.add))
    "#;
    let stubs = [ImportStub::returning(
        "env",
        "next",
        vec![vec![RecordedValue::I32(10)], vec![RecordedValue::I32(32)]],
    )];
    let (_, results, events) = record(wat, "twice", &[], &stubs);
    let calls = frames_for(&events, FN_KIND_IMPORT, 0);
    assert_eq!(calls.len(), 2, "two call sites, two frames");
    assert_eq!(calls[0].results, vec![RecordedValue::I32(10)]);
    assert_eq!(calls[1].results, vec![RecordedValue::I32(32)]);
    assert_eq!(results, vec![RecordedValue::I32(42)]);

    assert_computes_identically(wat, "twice", &[], &stubs);
}

// ---------------------------------------------------------------------------
// Verification: multi-value returns
// ---------------------------------------------------------------------------

/// Three results of three different types, recorded in declaration
/// order with ascending slots.
#[test]
fn verify_multi_value_returns_are_captured_in_order() {
    let wat = r#"
        (module
          (func (export "three") (result i32 i64 f32)
            i32.const 11
            i64.const 22
            f32.const 3.5))
    "#;
    let (frames, results, _) = record(wat, "three", &[], &[]);
    let frame = &frames[0];
    assert_eq!(
        frame.results,
        vec![
            RecordedValue::I32(11),
            RecordedValue::I64(22),
            RecordedValue::F32Bits(3.5f32.to_bits()),
        ],
        "results must be reported in declaration order"
    );
    assert_eq!(frame.result_slots, vec![0, 1, 2]);
    assert_eq!(
        results,
        vec![
            RecordedValue::I32(11),
            RecordedValue::I64(22),
            RecordedValue::F32Bits(3.5f32.to_bits()),
        ],
        "and the module must still return them"
    );

    assert_computes_identically(wat, "three", &[], &[]);
}

/// Two results of the *same* type, deliberately swapped relative to
/// the parameters. Sharing one scratch local per type would report
/// (and return) the same value twice; distinct locals are the only way
/// this passes.
#[test]
fn verify_multi_value_same_type_results_use_distinct_scratch_locals() {
    let wat = r#"
        (module
          (func (export "swap") (param i64) (param i64) (result i64 i64)
            local.get 1
            local.get 0))
    "#;
    let args = [RecordedValue::I64(100), RecordedValue::I64(200)];
    let (frames, results, _) = record(wat, "swap", &args, &[]);
    assert_eq!(
        frames[0].results,
        vec![RecordedValue::I64(200), RecordedValue::I64(100)],
        "a shared scratch local would report one value twice"
    );
    assert_eq!(
        results,
        vec![RecordedValue::I64(200), RecordedValue::I64(100)],
        "and would also corrupt what the module returns"
    );

    assert_computes_identically(wat, "swap", &args, &[]);
}

/// A multi-result *import*: the results the replayer must feed back
/// are a tuple, not a scalar.
#[test]
fn verify_multi_value_import_results_are_captured_in_order() {
    let wat = r#"
        (module
          (import "env" "split" (func $split (param i32) (result i32 i32)))
          (func (export "run") (param i32) (result i32)
            local.get 0
            call $split
            i32.sub))
    "#;
    let stubs = [ImportStub::returning(
        "env",
        "split",
        vec![vec![RecordedValue::I32(90), RecordedValue::I32(8)]],
    )];
    let args = [RecordedValue::I32(5)];
    let (_, results, events) = record(wat, "run", &args, &stubs);
    let split = frames_for(&events, FN_KIND_IMPORT, 0);
    assert_eq!(
        split[0].results,
        vec![RecordedValue::I32(90), RecordedValue::I32(8)]
    );
    assert_eq!(split[0].result_slots, vec![0, 1]);
    assert_eq!(results, vec![RecordedValue::I32(82)]);

    assert_computes_identically(wat, "run", &args, &stubs);
}

// ---------------------------------------------------------------------------
// Verification: the capture does not perturb the computation
// ---------------------------------------------------------------------------

/// The instrumented module must compute exactly what the original
/// computes — same returned values, same memory — across a corpus that
/// covers every shape the capture code has a branch for, plus the
/// golden fixtures.
#[test]
fn verify_operand_stack_is_undisturbed_by_capture() {
    // (wat, export, args, stubs)
    let cases: Vec<(&str, &str, Vec<RecordedValue>, Vec<ImportStub>)> = vec![
        (r#"(module (func (export "nop")))"#, "nop", vec![], vec![]),
        (
            r#"(module (func (export "k") (result i32) i32.const 5))"#,
            "k",
            vec![],
            vec![],
        ),
        (
            r#"(module (func (export "sink") (param i32) (param f64)))"#,
            "sink",
            vec![RecordedValue::I32(3), RecordedValue::f64(9.5)],
            vec![],
        ),
        (
            // Explicit `return` from inside nested control flow: the
            // epilogue is spliced before the `return`, where the
            // results are also on the stack.
            r#"
            (module
              (func (export "early") (param i32) (result i32 i32)
                block
                  local.get 0
                  i32.eqz
                  br_if 0
                  i32.const 1
                  local.get 0
                  return
                end
                i32.const 0
                i32.const 0))
            "#,
            "early",
            vec![RecordedValue::I32(41)],
            vec![],
        ),
        (
            // A loop, so the same capture code runs many times.
            r#"
            (module
              (memory (export "mem") 1)
              (func (export "sum") (param i32) (result i64)
                (local $i i32) (local $acc i64)
                block
                  loop
                    local.get $i
                    local.get 0
                    i32.ge_s
                    br_if 1
                    local.get $acc
                    local.get $i
                    i64.extend_i32_s
                    i64.add
                    local.set $acc
                    local.get $i
                    i64.const 7
                    i64.store
                    local.get $i
                    i32.const 8
                    i32.add
                    local.set $i
                    br 0
                  end
                end
                local.get $acc))
            "#,
            "sum",
            vec![RecordedValue::I32(64)],
            vec![],
        ),
        (
            // Imports on both sides of the boundary, with results fed
            // back into the computation.
            r#"
            (module
              (import "env" "a" (func $a (param i32) (result i64)))
              (import "env" "b" (func $b (param i64) (param f32) (result f32)))
              (func (export "chain") (param i32) (result f32)
                local.get 0
                call $a
                f32.const 0.5
                call $b))
            "#,
            "chain",
            vec![RecordedValue::I32(9)],
            vec![
                ImportStub::returning("env", "a", vec![vec![RecordedValue::I64(1234)]]),
                ImportStub::returning("env", "b", vec![vec![RecordedValue::f32(-3.75)]]),
            ],
        ),
    ];

    for (wat, export, args, stubs) in cases {
        assert_computes_identically(wat, export, &args, &stubs);
    }

    // The golden corpus cannot be *executed* here (the fixtures are
    // Rust-compiled programs with their own host expectations), so it
    // is checked the strongest way that does not need a host: a real
    // engine must accept the instrumented module. A capture that
    // spilled the wrong arity, or left the stack unbalanced, is a type
    // error the validator rejects — it cannot pass this check and
    // still be wrong about the stack.
    let mut checked = 0;
    for name in [
        "collections.wasm",
        "control_flow.wasm",
        "nested_calls.wasm",
        "panic_path.wasm",
    ] {
        let Some(path) = golden_path(name) else {
            eprintln!("[skip] {name} not found; clone codetracer-wasm-recorder to enable");
            continue;
        };
        let bytes = std::fs::read(&path).expect("read golden fixture");
        let instrumented = Pipeline::new().run_bytes(&bytes).expect("instrument");
        wasmi::Module::new(&wasmi::Engine::default(), &mut &instrumented[..]).unwrap_or_else(|e| {
            panic!("a real engine rejected the instrumented {name}: {e}");
        });
        checked += 1;
    }
    assert_eq!(
        checked, 4,
        "the golden corpus must be present for this check to mean anything"
    );
}

fn golden_path(name: &str) -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        manifest_dir.join(format!(
            "../../../codetracer-wasm-recorder/cmd/wazero/testdata/recorder-golden/{name}"
        )),
        manifest_dir.join(format!(
            "../../codetracer-wasm-recorder/cmd/wazero/testdata/recorder-golden/{name}"
        )),
    ];
    candidates.into_iter().find(|c| c.exists())
}

// ---------------------------------------------------------------------------
// Verification: the withdrawn write hook
// ---------------------------------------------------------------------------

/// Spec § 5 withdraws the per-store write hook. Asserted three ways:
/// it is not in the declared surface, no instrumented module imports
/// it, and the name does not occur anywhere under `crates/`.
///
/// The name is assembled at runtime precisely so that this file does
/// not itself become a hit for the source scan below.
#[test]
fn verify_emit_write_hook_is_gone() {
    let withdrawn = format!("__ct_{}{}", "emit_", "write");

    assert!(
        !hooks::ALL_HOOKS.contains(&withdrawn.as_str()),
        "the withdrawn hook is still part of the declared surface"
    );

    let wat = r#"
        (module
          (memory (export "mem") 1)
          (func (export "w") (param i32) (param i32)
            local.get 0
            local.get 1
            i32.store))
    "#;
    // Two configurations, because the store pass is the one most
    // likely to bring the hook back and M36 took it off the default
    // path — checking only the default would stop exercising it.
    let store_pass_on = PipelineConfig {
        instrument_stores: true,
        ..PipelineConfig::default()
    };
    assert!(
        store_pass_on.instrument_stores && !PipelineConfig::default().instrument_stores,
        "M36: boundary-only by default, the store pass reachable behind the flag"
    );
    for config in [PipelineConfig::default(), store_pass_on] {
        let with_stores = config.instrument_stores;
        let original = wat::parse_str(wat).expect("input WAT must compile");
        let instrumented = Pipeline::with_config(config)
            .run_bytes(&original)
            .expect("pipeline must succeed");
        let module = walrus::Module::from_buffer(&instrumented).expect("re-parse");
        let imported: Vec<String> = module.imports.iter().map(|i| i.name.clone()).collect();
        assert!(
            !imported.contains(&withdrawn),
            "an instrumented module still imports the withdrawn hook \
             (instrument_stores={with_stores}): {imported:?}"
        );
    }

    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ is the parent of this crate")
        .to_path_buf();
    let mut hits = Vec::new();
    scan_rust_sources(&crates_dir, &withdrawn, &mut hits);
    assert!(
        hits.is_empty(),
        "the withdrawn hook name still occurs in: {hits:?}"
    );
}

fn scan_rust_sources(dir: &std::path::Path, needle: &str, hits: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            scan_rust_sources(&path, needle, hits);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if text.contains(needle) {
                    hits.push(path.display().to_string());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The awkward cases
// ---------------------------------------------------------------------------

/// A zero-parameter, zero-result export still produces a framed
/// crossing — with empty tuples, and no value events at all.
#[test]
fn zero_parameter_zero_result_export_emits_no_values() {
    let wat = r#"(module (func (export "nop")))"#;
    let (frames, results, events) = record(wat, "nop", &[], &[]);
    assert!(results.is_empty());
    assert_eq!(frames.len(), 1);
    assert!(frames[0].args.is_empty());
    assert!(frames[0].results.is_empty());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Value { .. })),
        "nothing crosses the boundary, so nothing should be emitted"
    );
}

/// Only-parameters and only-results are the two cases where a host
/// sees a single run of values and must still label it correctly. The
/// framing contract in `hooks` is what makes them distinguishable.
#[test]
fn one_sided_signatures_are_framed_correctly() {
    let sink = r#"(module (func (export "sink") (param i32) (param i32)))"#;
    let (frames, _, _) = record(
        sink,
        "sink",
        &[RecordedValue::I32(4), RecordedValue::I32(5)],
        &[],
    );
    assert_eq!(
        frames[0].args,
        vec![RecordedValue::I32(4), RecordedValue::I32(5)],
        "a zero-result export's single value run is the argument tuple"
    );
    assert!(frames[0].results.is_empty());

    let source = r#"(module (func (export "source") (result i32) i32.const 6))"#;
    let (frames, _, _) = record(source, "source", &[], &[]);
    assert!(
        frames[0].args.is_empty(),
        "a zero-parameter export's single value run is the result tuple"
    );
    assert_eq!(frames[0].results, vec![RecordedValue::I32(6)]);
}

/// A function that already fills its local index space densely.
/// Walrus assigns local indices at emit time from the set the body
/// references, so the scratch pool cannot collide — this is the test
/// that says so out loud.
#[test]
fn densely_used_locals_do_not_collide_with_the_scratch_pool() {
    let wat = r#"
        (module
          (func (export "dense") (param i32) (param i64) (result i64 i64)
            (local $a i32) (local $b i32) (local $c i64) (local $d i64)
            (local $e f32) (local $f f64) (local $g i32)
            local.get 0
            local.set $a
            local.get 0
            i32.const 2
            i32.mul
            local.set $b
            local.get 1
            local.set $c
            local.get 1
            i64.const 3
            i64.mul
            local.set $d
            f32.const 1.25
            local.set $e
            f64.const 2.5
            local.set $f
            local.get $a
            local.get $b
            i32.add
            local.set $g
            local.get $c
            local.get $g
            i64.extend_i32_s
            i64.add
            local.get $d))
    "#;
    let args = [RecordedValue::I32(10), RecordedValue::I64(4)];
    let (frames, results, _) = record(wat, "dense", &args, &[]);
    assert_eq!(
        frames[0].args,
        vec![RecordedValue::I32(10), RecordedValue::I64(4)]
    );
    // c + (a + b) = 4 + (10 + 20) = 34 ; d = 12
    assert_eq!(
        frames[0].results,
        vec![RecordedValue::I64(34), RecordedValue::I64(12)]
    );
    assert_eq!(
        results,
        vec![RecordedValue::I64(34), RecordedValue::I64(12)]
    );

    assert_computes_identically(wat, "dense", &args, &[]);
}

/// NaN payloads and negative zero must survive capture bit-for-bit:
/// spec § 7 makes a NaN payload mismatch a replay *divergence*, so a
/// capture that canonicalised one would produce a recording that
/// cannot be replayed.
#[test]
fn nan_payloads_and_negative_zero_are_captured_bit_exactly() {
    let wat = r#"
        (module
          (import "env" "echo32" (func $echo32 (param f32) (result f32)))
          (func (export "pass") (param f32) (param f64) (result f32 f64)
            local.get 0
            call $echo32
            local.get 1))
    "#;
    // A *signalling* NaN with a non-zero payload, and -0.0. Both are
    // values a widening or a naive float round-trip would destroy.
    let snan_bits: u32 = 0x7f80_0001;
    let payload_nan_bits: u64 = 0x7ff8_0000_dead_beef;
    let stub_bits: u32 = 0xffc0_1234;

    let stubs = [ImportStub::returning(
        "env",
        "echo32",
        vec![vec![RecordedValue::F32Bits(stub_bits)]],
    )];
    let args = [
        RecordedValue::F32Bits(snan_bits),
        RecordedValue::F64Bits(payload_nan_bits),
    ];
    let (frames, results, events) = record(wat, "pass", &args, &stubs);

    let export = frames
        .iter()
        .find(|f| f.fn_kind == FN_KIND_EXPORT)
        .expect("export frame");
    assert_eq!(
        export.args,
        vec![
            RecordedValue::F32Bits(snan_bits),
            RecordedValue::F64Bits(payload_nan_bits),
        ],
        "argument NaN bits must be recorded exactly"
    );

    let echo = frames_for(&events, FN_KIND_IMPORT, 0);
    assert_eq!(
        echo[0].args,
        vec![RecordedValue::F32Bits(snan_bits)],
        "the signalling NaN must reach the import hook unchanged"
    );
    assert_eq!(
        echo[0].results,
        vec![RecordedValue::F32Bits(stub_bits)],
        "and the host's NaN result must come back unchanged"
    );
    assert_eq!(
        results,
        vec![
            RecordedValue::F32Bits(stub_bits),
            RecordedValue::F64Bits(payload_nan_bits),
        ]
    );

    // Negative zero: `-0.0 == 0.0` numerically, so only the bits can
    // tell them apart.
    let neg_zero = r#"
        (module (func (export "id") (param f64) (result f64) local.get 0))
    "#;
    let (frames, results, _) = record(neg_zero, "id", &[RecordedValue::f64(-0.0)], &[]);
    assert_eq!(
        frames[0].args,
        vec![RecordedValue::F64Bits(0x8000_0000_0000_0000)]
    );
    assert_eq!(results, vec![RecordedValue::F64Bits(0x8000_0000_0000_0000)]);
}

/// An export calls an import; the host services it by calling *back*
/// into another export. The inner crossing's capture runs while the
/// outer one is still open — the case a shared scratch pool would have
/// to survive, and it does, because WASM locals are per-invocation.
#[test]
fn nested_export_import_reentry_captures_every_level() {
    let wat = r#"
        (module
          (import "env" "trampoline" (func $t (param i32) (result i32)))
          (func (export "inner") (param i32) (result i32)
            local.get 0
            i32.const 100
            i32.mul)
          (func (export "outer") (param i32) (result i32)
            local.get 0
            call $t
            i32.const 1
            i32.add))
    "#;
    let stubs = [ImportStub::reentering(
        "env",
        "trampoline",
        "inner",
        vec![RecordedValue::I32(5)],
    )];
    let args = [RecordedValue::I32(3)];
    let (frames, results, events) = record(wat, "outer", &args, &stubs);

    // outer is export index 1, inner is export index 0.
    let outer = frames_for(&events, FN_KIND_EXPORT, 1);
    assert_eq!(outer.len(), 1);
    assert_eq!(outer[0].args, vec![RecordedValue::I32(3)]);
    assert_eq!(outer[0].results, vec![RecordedValue::I32(501)]);

    let inner = frames_for(&events, FN_KIND_EXPORT, 0);
    assert_eq!(inner.len(), 1, "the re-entered export is captured too");
    assert_eq!(inner[0].args, vec![RecordedValue::I32(5)]);
    assert_eq!(inner[0].results, vec![RecordedValue::I32(500)]);

    let import = frames_for(&events, FN_KIND_IMPORT, 0);
    assert_eq!(import[0].args, vec![RecordedValue::I32(3)]);
    assert_eq!(import[0].results, vec![RecordedValue::I32(500)]);

    assert_eq!(results, vec![RecordedValue::I32(501)]);
    // Frames must nest, not interleave: inner opens after the import
    // and closes before it.
    let kinds: Vec<(i32, u32)> = frames.iter().map(|f| (f.fn_kind, f.fn_index)).collect();
    assert_eq!(
        kinds,
        vec![
            (FN_KIND_EXPORT, 1),
            (FN_KIND_IMPORT, 0),
            (FN_KIND_EXPORT, 0)
        ],
        "frames are opened outer-first"
    );

    assert_computes_identically(wat, "outer", &args, &stubs);
}

/// A recursive export re-enters its own capture code. Locals are
/// per-invocation, so each level reports its own values.
#[test]
fn recursive_export_captures_each_invocation_separately() {
    let wat = r#"
        (module
          (func $fact (export "fact") (param i64) (result i64)
            local.get 0
            i64.eqz
            if (result i64)
              i64.const 1
            else
              local.get 0
              local.get 0
              i64.const 1
              i64.sub
              call $fact
              i64.mul
            end))
    "#;
    let (_, results, events) = record(wat, "fact", &[RecordedValue::I64(5)], &[]);
    assert_eq!(results, vec![RecordedValue::I64(120)]);
    let frames = frames_for(&events, FN_KIND_EXPORT, 0);
    assert_eq!(frames.len(), 6, "one frame per invocation, 5 down to 0");
    assert_eq!(frames[0].args, vec![RecordedValue::I64(5)]);
    assert_eq!(frames[0].results, vec![RecordedValue::I64(120)]);
    assert_eq!(frames[5].args, vec![RecordedValue::I64(0)]);
    assert_eq!(frames[5].results, vec![RecordedValue::I64(1)]);

    assert_computes_identically(wat, "fact", &[RecordedValue::I64(5)], &[]);
}

// ---------------------------------------------------------------------------
// The manifest's boundary table
// ---------------------------------------------------------------------------

/// The manifest records each boundary's signature, so the replayer can
/// decode the flat value stream without re-parsing the `.wasm`.
#[test]
fn manifest_records_every_boundary_signature() {
    let wat = r#"
        (module
          (import "env" "log" (func $log (param i32) (param f64)))
          (import "env" "now" (func $now (result i64)))
          (memory (export "mem") 1)
          (func (export "mix") (param i32) (param f32) (result i64 i64)
            i64.const 1
            i64.const 2))
    "#;
    let original = wat::parse_str(wat).expect("valid wat");
    let (_, manifest) = Pipeline::new()
        .run_bytes_with_manifest(&original, Some("src/lib.rs"))
        .expect("instrument");

    let find = |kind: i32, index: u32| -> &ManifestBoundary {
        manifest
            .boundaries
            .iter()
            .find(|b| b.fn_kind == kind && b.fn_index == index)
            .unwrap_or_else(|| {
                panic!(
                    "no boundary for ({kind}, {index}): {:#?}",
                    manifest.boundaries
                )
            })
    };

    let log = find(FN_KIND_IMPORT, 0);
    assert_eq!(log.module, "env");
    assert_eq!(log.name, "log");
    assert_eq!(log.params, vec![ScalarType::I32, ScalarType::F64]);
    assert!(log.results.is_empty());

    let now = find(FN_KIND_IMPORT, 1);
    assert_eq!(now.name, "now");
    assert!(now.params.is_empty());
    assert_eq!(now.results, vec![ScalarType::I64]);

    // Export index 0 is the memory export, so `mix` is index 1 — the
    // same index the runtime hooks report.
    let mix = find(FN_KIND_EXPORT, 1);
    assert_eq!(mix.name, "mix");
    assert_eq!(mix.params, vec![ScalarType::I32, ScalarType::F32]);
    assert_eq!(mix.results, vec![ScalarType::I64, ScalarType::I64]);
    assert!(mix.unsupported_type.is_none());

    // The JSON the embedder ships must carry the table.
    let json = manifest.to_json().expect("serialise");
    assert!(json.contains("\"boundaries\""), "manifest JSON: {json}");
    assert!(json.contains("\"fnKind\""), "camelCase, like the rest");
}

/// The manifest's signature must agree with what the hooks actually
/// emit — otherwise the replayer decodes the stream against the wrong
/// shape. Cross-checked by running the module.
#[test]
fn manifest_signatures_agree_with_the_emitted_slots() {
    let wat = r#"
        (module
          (func (export "f") (param i32) (param f64) (result f32 i64)
            f32.const 1.5
            i64.const 9))
    "#;
    let original = wat::parse_str(wat).expect("valid wat");
    let (instrumented, manifest) = Pipeline::new()
        .run_bytes_with_manifest(&original, None)
        .expect("instrument");
    let entry = manifest
        .boundaries
        .iter()
        .find(|b| b.fn_kind == FN_KIND_EXPORT && b.name == "f")
        .expect("boundary entry");

    let run = run_module(
        &instrumented,
        "f",
        &[RecordedValue::I32(1), RecordedValue::f64(2.0)],
        &[],
    )
    .expect("run");
    let frame = &boundary_frames(&run.events)[0];

    let observed_params: Vec<ScalarType> = frame
        .args
        .iter()
        .map(|v| match v {
            RecordedValue::I32(_) => ScalarType::I32,
            RecordedValue::I64(_) => ScalarType::I64,
            RecordedValue::F32Bits(_) => ScalarType::F32,
            RecordedValue::F64Bits(_) => ScalarType::F64,
        })
        .collect();
    let observed_results: Vec<ScalarType> = frame
        .results
        .iter()
        .map(|v| match v {
            RecordedValue::I32(_) => ScalarType::I32,
            RecordedValue::I64(_) => ScalarType::I64,
            RecordedValue::F32Bits(_) => ScalarType::F32,
            RecordedValue::F64Bits(_) => ScalarType::F64,
        })
        .collect();
    assert_eq!(entry.params, observed_params);
    assert_eq!(entry.results, observed_results);
}

// ---------------------------------------------------------------------------
// Rejection rather than silent degradation (spec § 8)
// ---------------------------------------------------------------------------

/// A boundary carrying a reference type is refused with a diagnostic
/// naming the function, rather than recorded with a gap in its value
/// stream.
#[test]
fn reference_typed_boundaries_are_rejected_with_a_diagnostic() {
    let wat = r#"
        (module
          (func $take (export "take") (param externref)))
    "#;
    let original = wat::parse_str(wat).expect("valid wat");
    let err = Pipeline::new()
        .run_bytes(&original)
        .expect_err("a reference-typed boundary must be refused");
    let text = format!("{err:#}");
    assert!(
        text.contains("take") && text.contains("reference type"),
        "the diagnostic must name the function and the offending type: {text}"
    );

    // With value capture off there is nothing to omit, so the same
    // module instruments fine — the refusal is about the recording
    // being incomplete, not about the module being unsupported.
    let config = PipelineConfig {
        capture_boundary_values: false,
        ..PipelineConfig::default()
    };
    Pipeline::with_config(config)
        .run_bytes(&original)
        .expect("call-shape-only instrumentation stays available");
}
