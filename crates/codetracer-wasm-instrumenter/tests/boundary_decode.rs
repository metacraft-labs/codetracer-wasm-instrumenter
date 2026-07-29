//! An M37-shaped decoder, written against the recorded stream only.
//!
//! `tests/boundary_values.rs` asserts that the right values reach the
//! hooks. This file asks the harder question the re-execution contract
//! actually depends on (`Recording-Backends/WASM-Instrumentation-Layer.md`
//! §§ 5, 6): given nothing but the flat event stream and the sidecar
//! manifest's `boundaries` table — precisely what a replayer holds in
//! its hands — can the argument and result tuples be reconstructed
//! *unambiguously*, and do they equal what actually crossed?
//!
//! The decoder below is deliberately **not** the crate's own
//! `boundary_frames`. It re-derives the framing from the documented
//! contract and cross-checks every run against the manifest signature
//! (arity, element types, and slot numbering). A stream that satisfies
//! both the positional rule and the declared signature is one a
//! replayer can consume; a stream that satisfies only one of them is a
//! latent divergence.
//!
//! No mocks. The module under test is real instrumenter output, run
//! under `wasmi` through `codetracer-wasm-stub-host`'s embedder, with
//! real host implementations behind the imports.

use std::collections::HashMap;

use codetracer_wasm_instrumenter::{hooks, Pipeline, ScalarType};
use codetracer_wasm_stub_host::runtime::{
    run_module, ImportStub, RecordedValue, RuntimeEvent, RuntimeRecording,
};

const IMPORT: i32 = hooks::FUNC_KIND_IMPORT;
const EXPORT: i32 = hooks::FUNC_KIND_EXPORT;

/// One boundary's declared shape, as the manifest carries it: the
/// function's name plus its parameter and result types.
type Signature = (String, Vec<ScalarType>, Vec<ScalarType>);

/// The `boundaries` table, keyed the way the hooks report a crossing.
type SignatureTable = HashMap<(i32, u32), Signature>;

// ---------------------------------------------------------------------------
// The decoder an M37 replayer would have to be able to write
// ---------------------------------------------------------------------------

/// One reconstructed crossing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Crossing {
    fn_kind: i32,
    fn_index: u32,
    name: String,
    args: Vec<RecordedValue>,
    results: Vec<RecordedValue>,
}

fn type_of(v: RecordedValue) -> ScalarType {
    match v {
        RecordedValue::I32(_) => ScalarType::I32,
        RecordedValue::I64(_) => ScalarType::I64,
        RecordedValue::F32Bits(_) => ScalarType::F32,
        RecordedValue::F64Bits(_) => ScalarType::F64,
    }
}

/// Reconstruct every crossing from `events`, using `signatures` as the
/// only external knowledge.
///
/// Panics — loudly, with the offending stream — the moment the stream
/// cannot be decoded. That is the right behaviour for a replayer per
/// spec § 6 ("divergence is an error, never a warning"), and it is what
/// makes this a test rather than a best-effort parser.
fn decode(events: &[RuntimeEvent], signatures: &SignatureTable) -> Vec<Crossing> {
    let mut out: Vec<Crossing> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    // The run of value events since the last non-value event.
    let mut run: Vec<(i32, RecordedValue)> = Vec::new();

    let check = |label: &str, run: &[(i32, RecordedValue)], want: &[ScalarType]| {
        assert_eq!(
            run.len(),
            want.len(),
            "{label}: recorded {} values, the manifest signature declares {}",
            run.len(),
            want.len()
        );
        for (i, ((slot, value), ty)) in run.iter().zip(want.iter()).enumerate() {
            assert_eq!(
                *slot, i as i32,
                "{label}: slot {slot} out of position at index {i} — a replayer \
                 reassembles by slot, so a gap or a repeat is unrecoverable"
            );
            assert_eq!(
                type_of(*value),
                *ty,
                "{label}: slot {i} carries {:?}, the manifest declares {ty:?}",
                type_of(*value)
            );
        }
    };

    for event in events {
        match event {
            RuntimeEvent::Value { slot, value } => run.push((*slot, *value)),
            RuntimeEvent::Call { fn_kind, fn_index } => {
                assert!(
                    run.is_empty(),
                    "value run stranded before a call event: {run:?}"
                );
                if *fn_kind == hooks::FUNC_KIND_STORE {
                    // The experimental interior pass, retired in M36:
                    // not a boundary, not a replay input.
                    open.push(usize::MAX);
                    continue;
                }
                let (name, _, _) = signatures
                    .get(&(*fn_kind, *fn_index))
                    .unwrap_or_else(|| panic!("no manifest boundary for ({fn_kind}, {fn_index})"));
                out.push(Crossing {
                    fn_kind: *fn_kind,
                    fn_index: *fn_index,
                    name: name.clone(),
                    args: Vec::new(),
                    results: Vec::new(),
                });
                open.push(out.len() - 1);
            }
            RuntimeEvent::RealmBoundary {
                direction,
                fn_kind,
                fn_index,
                ..
            } => {
                if *direction == hooks::REALM_DIRECTION_ENTER {
                    // Contract: the run between the call event and the
                    // ENTER marker is the argument tuple.
                    let idx = *open.last().expect("ENTER marker with no open crossing");
                    let (name, params, _) = &signatures[&(*fn_kind, *fn_index)];
                    check(&format!("{name} args"), &run, params);
                    out[idx].args = std::mem::take(&mut run)
                        .into_iter()
                        .map(|(_, v)| v)
                        .collect();
                } else {
                    assert!(
                        run.is_empty(),
                        "value run stranded after a LEAVE marker: {run:?}"
                    );
                    run.clear();
                }
            }
            RuntimeEvent::Return { fn_kind, fn_index } => {
                let idx = open.pop().expect("return event with no open crossing");
                if idx == usize::MAX {
                    run.clear();
                    continue;
                }
                // Contract: the run between the call and the return
                // event is the result tuple.
                let (name, _, results) = &signatures[&(*fn_kind, *fn_index)];
                check(&format!("{name} results"), &run, results);
                out[idx].results = std::mem::take(&mut run)
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect();
            }
        }
    }
    assert!(open.is_empty(), "unbalanced crossings left open: {open:?}");
    assert!(
        run.is_empty(),
        "value run stranded at end of stream: {run:?}"
    );
    out
}

/// Instrument `wat`, run it, and decode the stream with the manifest.
fn record_and_decode(
    wat: &str,
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> (Vec<Crossing>, RuntimeRecording) {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    let (instrumented, manifest) = Pipeline::new()
        .run_bytes_with_manifest(&original, Some("t.rs"))
        .expect("instrumentation must succeed");
    let signatures: SignatureTable = manifest
        .boundaries
        .iter()
        .map(|b| {
            (
                (b.fn_kind, b.fn_index),
                (b.name.clone(), b.params.clone(), b.results.clone()),
            )
        })
        .collect();
    let run = run_module(&instrumented, export, args, stubs).expect("instrumented run");
    let crossings = decode(&run.events, &signatures);
    (crossings, run)
}

// ---------------------------------------------------------------------------
// 1. A rich signature, decoded with nothing but the manifest
// ---------------------------------------------------------------------------

/// Every scalar type on both edges, in both directions, decoded by a
/// replayer-shaped consumer and compared against ground truth.
#[test]
fn a_signature_only_decoder_reconstructs_every_tuple() {
    // `bounce` takes one of each type, calls an import with a
    // rearranged tuple, and returns a multi-value result that mixes
    // repeated types. Every number below is chosen so no two values
    // are equal — a decoder that transposes two slots cannot pass.
    let wat = r#"
        (module
          (import "env" "sink" (func $sink (param f64 i32 i64) (result i64 f32)))
          (func (export "bounce")
                (param i32) (param i64) (param f32) (param f64)
                (result i64 i64 f32 i32)
            local.get 3
            local.get 0
            local.get 1
            call $sink        ;; -> (i64, f32)
            local.set 2       ;; f32 result into the f32 param slot
            local.set 1       ;; i64 result into the i64 param slot
            local.get 1
            i64.const 11
            i64.add
            local.get 1
            i64.const 22
            i64.add
            local.get 2
            local.get 0))
    "#;
    let args = [
        RecordedValue::I32(-13),
        RecordedValue::I64(0x0102_0304_0506_0708),
        RecordedValue::f32(3.5),
        RecordedValue::f64(-9.75),
    ];
    let stub = ImportStub::returning(
        "env",
        "sink",
        vec![vec![
            RecordedValue::I64(7_000_000_000),
            RecordedValue::f32(0.125),
        ]],
    );
    let (crossings, run) = record_and_decode(wat, "bounce", &args, &[stub]);

    assert_eq!(
        crossings.len(),
        2,
        "one export and one import: {crossings:#?}"
    );

    let export = crossings
        .iter()
        .find(|c| c.fn_kind == EXPORT)
        .expect("export crossing");
    assert_eq!(export.name, "bounce");
    assert_eq!(
        export.args,
        args.to_vec(),
        "the decoder must recover the exact argument tuple the host passed"
    );
    assert_eq!(
        export.results,
        vec![
            RecordedValue::I64(7_000_000_011),
            RecordedValue::I64(7_000_000_022),
            RecordedValue::f32(0.125),
            RecordedValue::I32(-13),
        ],
        "two same-typed results in a row must not collapse or swap"
    );
    assert_eq!(
        export.results, run.results,
        "the decoded result tuple must equal what the engine actually returned"
    );

    let import = crossings
        .iter()
        .find(|c| c.fn_kind == IMPORT)
        .expect("import crossing");
    assert_eq!(import.name, "sink");
    assert_eq!(
        import.args,
        vec![
            RecordedValue::f64(-9.75),
            RecordedValue::I32(-13),
            RecordedValue::I64(0x0102_0304_0506_0708),
        ],
        "the import's arguments are recorded in call order, not in the caller's order"
    );
    assert_eq!(
        import.results,
        vec![RecordedValue::I64(7_000_000_000), RecordedValue::f32(0.125)],
        "the import results are the values replay must feed back (spec § 3.2)"
    );
}

// ---------------------------------------------------------------------------
// 2. One-sided and empty signatures: can a decoder tell them apart?
// ---------------------------------------------------------------------------

/// The four degenerate framings in one module, decoded together.
///
/// This is where a positional framing rule is most likely to be
/// ambiguous: a boundary with no results emits exactly one value run,
/// and so does a boundary with no parameters. If the stream cannot
/// distinguish them, a replayer silently feeds an argument tuple back
/// as a result.
#[test]
fn one_sided_and_empty_boundaries_decode_unambiguously() {
    let wat = r#"
        (module
          (import "env" "consume" (func $consume (param i32 i32)))
          (import "env" "produce" (func $produce (result i32 i64)))
          (import "env" "ping" (func $ping))
          (global $g (mut i32) (i32.const 0))
          (func (export "args_only") (param i32) (param i64)
            local.get 0
            local.get 0
            call $consume
            call $ping)
          (func (export "results_only") (result i32 i64)
            call $produce)
          (func (export "neither")
            i32.const 1
            global.set $g))
    "#;
    let stubs = [
        ImportStub::void("env", "consume"),
        ImportStub::returning(
            "env",
            "produce",
            vec![vec![RecordedValue::I32(41), RecordedValue::I64(-42)]],
        ),
        ImportStub::void("env", "ping"),
    ];

    // (a) parameters, no results — on both edges.
    let (crossings, _) = record_and_decode(
        wat,
        "args_only",
        &[RecordedValue::I32(5), RecordedValue::I64(6)],
        &stubs,
    );
    let export = crossings.iter().find(|c| c.name == "args_only").unwrap();
    assert_eq!(
        export.args,
        vec![RecordedValue::I32(5), RecordedValue::I64(6)]
    );
    assert!(
        export.results.is_empty(),
        "a void export must decode to an empty result tuple, not to its arguments"
    );
    let consume = crossings.iter().find(|c| c.name == "consume").unwrap();
    assert_eq!(
        consume.args,
        vec![RecordedValue::I32(5), RecordedValue::I32(5)]
    );
    assert!(consume.results.is_empty());
    let ping = crossings.iter().find(|c| c.name == "ping").unwrap();
    assert!(
        ping.args.is_empty() && ping.results.is_empty(),
        "a `() -> ()` import must decode to two empty tuples"
    );

    // (b) results, no parameters — on both edges.
    let (crossings, _) = record_and_decode(wat, "results_only", &[], &stubs);
    let export = crossings.iter().find(|c| c.name == "results_only").unwrap();
    assert!(
        export.args.is_empty(),
        "a nullary export must not adopt its results as arguments"
    );
    assert_eq!(
        export.results,
        vec![RecordedValue::I32(41), RecordedValue::I64(-42)]
    );
    let produce = crossings.iter().find(|c| c.name == "produce").unwrap();
    assert!(produce.args.is_empty());
    assert_eq!(
        produce.results,
        vec![RecordedValue::I32(41), RecordedValue::I64(-42)]
    );

    // (c) neither.
    let (crossings, _) = record_and_decode(wat, "neither", &[], &stubs);
    let export = crossings.iter().find(|c| c.name == "neither").unwrap();
    assert!(export.args.is_empty() && export.results.is_empty());
}

// ---------------------------------------------------------------------------
// 3. Ordering of same-typed multi-value results
// ---------------------------------------------------------------------------

/// Four results of two repeated types, each value distinct, on both
/// edges. A shared scratch local per type would return the last value
/// four times; a reversed restore would return them backwards.
#[test]
fn same_typed_multi_value_results_keep_their_order() {
    let wat = r#"
        (module
          (import "env" "quad" (func $quad (result i64 i64 f64 f64)))
          (func (export "passthrough") (result i64 i64 f64 f64)
            call $quad))
    "#;
    let stub = ImportStub::returning(
        "env",
        "quad",
        vec![vec![
            RecordedValue::I64(1),
            RecordedValue::I64(2),
            RecordedValue::f64(3.0),
            RecordedValue::f64(4.0),
        ]],
    );
    let (crossings, run) = record_and_decode(wat, "passthrough", &[], &[stub]);
    let want = vec![
        RecordedValue::I64(1),
        RecordedValue::I64(2),
        RecordedValue::f64(3.0),
        RecordedValue::f64(4.0),
    ];
    assert_eq!(
        run.results, want,
        "the engine itself must return them in order"
    );
    for crossing in &crossings {
        assert_eq!(
            crossing.results, want,
            "`{}` recorded its results out of order or with a collapsed slot",
            crossing.name
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Bit-exactness of the awkward float values
// ---------------------------------------------------------------------------

/// Signalling NaN, quiet NaN with a payload, and negative zero, on
/// both edges, compared **on bits**.
///
/// Spec § 7 makes a NaN payload mismatch a replay divergence, so a
/// capture that canonicalised a payload would produce a recording that
/// can never replay clean. `RecordedValue` holds floats as bit patterns
/// precisely so `assert_eq!` here cannot pass by `NaN != NaN` accident.
#[test]
fn nan_payloads_and_signed_zero_survive_both_edges_bit_exactly() {
    // f32 signalling NaN (quiet bit clear, payload 1) and f64 quiet NaN
    // with a distinctive payload.
    const F32_SNAN: u32 = 0x7F80_0001;
    const F64_QNAN_PAYLOAD: u64 = 0x7FF8_0000_DEAD_BEEF;

    let wat = r#"
        (module
          (import "env" "echo" (func $echo (param f32 f64) (result f32 f64)))
          (func (export "round") (param f32) (param f64) (result f32 f64)
            local.get 0
            local.get 1
            call $echo))
    "#;
    let args = [
        RecordedValue::F32Bits(F32_SNAN),
        RecordedValue::F64Bits(F64_QNAN_PAYLOAD),
    ];
    let stub = ImportStub::returning(
        "env",
        "echo",
        vec![vec![
            RecordedValue::F32Bits(0x8000_0000),           // -0.0f32
            RecordedValue::F64Bits(0x8000_0000_0000_0000), // -0.0f64
        ]],
    );
    let (crossings, _) = record_and_decode(wat, "round", &args, &[stub]);

    for crossing in &crossings {
        assert_eq!(
            crossing.args,
            vec![
                RecordedValue::F32Bits(F32_SNAN),
                RecordedValue::F64Bits(F64_QNAN_PAYLOAD),
            ],
            "`{}` lost a NaN payload on the argument edge",
            crossing.name
        );
        assert_eq!(
            crossing.results,
            vec![
                RecordedValue::F32Bits(0x8000_0000),
                RecordedValue::F64Bits(0x8000_0000_0000_0000),
            ],
            "`{}` lost the sign of a negative zero on the result edge",
            crossing.name
        );
    }

    // Guard the guard: `-0.0 == 0.0` in IEEE, so a value comparison
    // would have accepted a capture that dropped the sign bit. Prove
    // the assertions above are actually distinguishing bits.
    assert_ne!(
        RecordedValue::F64Bits(0x8000_0000_0000_0000),
        RecordedValue::f64(0.0),
        "RecordedValue must distinguish -0.0 from 0.0"
    );
    assert_eq!(-0.0f64, 0.0f64, "…which a plain float comparison does not");
}

// ---------------------------------------------------------------------------
// 5. The parity oracle must have teeth on memory, not only on results
// ---------------------------------------------------------------------------

/// Instrumentation must not perturb memory — and the check that says so
/// must be able to fail.
#[test]
fn the_parity_check_compares_memory_and_can_detect_a_difference() {
    let wat = |fill: &str| {
        format!(
            r#"
            (module
              (memory (export "mem") 1)
              (func (export "fill") (param i32) (result i32)
                (local i32)
                (local.set 1 (i32.const 0))
                (block $done
                  (loop $next
                    (br_if $done (i32.ge_s (local.get 1) (local.get 0)))
                    (i32.store8 (local.get 1) (i32.add (local.get 1) (i32.const {fill})))
                    (local.set 1 (i32.add (local.get 1) (i32.const 1)))
                    (br $next)))
                (local.get 1)))
        "#
        )
    };
    let a = wat::parse_str(wat("1")).unwrap();
    let b = wat::parse_str(wat("2")).unwrap();
    let a_instrumented = Pipeline::new().run_bytes(&a).unwrap();

    let args = [RecordedValue::I32(64)];
    let plain = run_module(&a, "fill", &args, &[]).unwrap();
    let instrumented = run_module(&a_instrumented, "fill", &args, &[]).unwrap();
    let different = run_module(&b, "fill", &args, &[]).unwrap();

    assert_eq!(plain.results, instrumented.results);
    assert_eq!(
        plain.memory, instrumented.memory,
        "instrumentation changed the bytes the module wrote"
    );
    assert!(
        plain
            .memory
            .as_ref()
            .is_some_and(|m| m.iter().any(|b| *b != 0)),
        "the fixture must actually write to memory, or the check proves nothing"
    );
    // The oracle's teeth: two modules that return the same value but
    // write different bytes must not compare equal.
    assert_eq!(
        plain.results, different.results,
        "the two fixtures must agree on the return value…"
    );
    assert_ne!(
        plain.memory, different.memory,
        "…so that this inequality proves the comparison is on memory, not on results"
    );
}

// ---------------------------------------------------------------------------
// 6. The known exit-path hole, pinned down as a fact
// ---------------------------------------------------------------------------

/// An exit taken by branching to the function's own label carries
/// neither the leave event nor the result capture.
///
/// This is a real hole in the export edge, documented rather than
/// fixed (see the follow-up milestone). It is pinned here for two
/// reasons: so it cannot be closed by accident without the test
/// noticing, and — more importantly — so the *shape* of the damage is
/// on record. The stream is left structurally unbalanced (a call and
/// an ENTER marker with no matching return or LEAVE), which is what
/// makes it a detectable missing record rather than a plausible wrong
/// one: the decoder above refuses such a stream outright.
#[test]
fn an_exit_by_branch_to_the_function_label_is_not_captured() {
    let wat = r#"
        (module
          (func (export "escape") (param i32) (result i32)
            local.get 0
            i32.const 100
            i32.add
            br 0
            unreachable))
    "#;
    let instrumented = instrument(wat);
    let run = run_module(&instrumented, "escape", &[RecordedValue::I32(1)], &[]).unwrap();
    assert_eq!(
        run.results,
        vec![RecordedValue::I32(101)],
        "the module still computes correctly — the loss is in the record, not the run"
    );

    let returns = run
        .events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Return { .. }))
        .count();
    let leaves = run
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                RuntimeEvent::RealmBoundary { direction, .. }
                    if *direction == hooks::REALM_DIRECTION_LEAVE
            )
        })
        .count();
    let values = run
        .events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Value { .. }))
        .count();

    assert_eq!(
        returns, 0,
        "known hole: no leave event on a br-to-label exit"
    );
    assert_eq!(leaves, 0, "known hole: no LEAVE marker either");
    assert_eq!(
        values, 1,
        "the parameter is still captured; only the result tuple is lost"
    );

    // The damage is detectable, not silent: the crossing never closes.
    let signatures = HashMap::from([(
        (EXPORT, 0u32),
        (
            "escape".to_string(),
            vec![ScalarType::I32],
            vec![ScalarType::I32],
        ),
    )]);
    let decoded = std::panic::catch_unwind(|| decode(&run.events, &signatures));
    assert!(
        decoded.is_err(),
        "a replayer-shaped decoder must reject the truncated stream rather than \
         accept a crossing with a fabricated empty result tuple"
    );
}

fn instrument(wat: &str) -> Vec<u8> {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    Pipeline::new().run_bytes(&original).expect("instrument")
}

// ---------------------------------------------------------------------------
// 7. Spec § 8: rejected, never silently degraded
// ---------------------------------------------------------------------------

/// Every unrepresentable boundary type, on both edges, in both
/// positions — and the whole module refused, not just the one function.
#[test]
fn unrepresentable_boundary_types_abort_the_whole_module() {
    // Each case is a module whose *only* defect is one boundary type.
    // A healthy exported function sits beside the offender so a
    // "skip the bad one, instrument the rest" implementation would be
    // caught: the run must fail outright, producing no module at all.
    let cases: [(&str, &str, &str); 5] = [
        (
            "externref export param",
            r#"(module
                 (func $good (export "good") (param i32) (result i32) local.get 0)
                 (func $bad (export "bad") (param externref)))"#,
            "bad",
        ),
        (
            "funcref export result",
            r#"(module
                 (func $good (export "good") (param i32) (result i32) local.get 0)
                 (func $bad (export "bad") (result funcref) ref.null func))"#,
            "bad",
        ),
        (
            "v128 export param",
            r#"(module
                 (func $good (export "good") (param i32) (result i32) local.get 0)
                 (func $bad (export "bad") (param v128)))"#,
            "bad",
        ),
        (
            "externref import param",
            r#"(module
                 (import "env" "sink" (func $sink (param externref)))
                 (func $good (export "good") (param i32) (result i32) local.get 0))"#,
            "sink",
        ),
        (
            "v128 import result",
            r#"(module
                 (import "env" "src" (func $src (result v128)))
                 (func $good (export "good") (param i32) (result i32) local.get 0))"#,
            "src",
        ),
    ];

    for (label, wat, offender) in cases {
        let original = wat::parse_str(wat).unwrap_or_else(|e| panic!("{label}: bad wat: {e}"));
        let err = Pipeline::new()
            .run_bytes(&original)
            .err()
            .unwrap_or_else(|| panic!("{label}: must be refused, not silently degraded"));
        let text = format!("{err:#}");
        assert!(
            text.contains(offender),
            "{label}: the diagnostic must name the offending function `{offender}`: {text}"
        );
        assert!(
            text.contains("reference type") || text.contains("v128"),
            "{label}: the diagnostic must name the construct: {text}"
        );
    }
}

/// A reference type *inside* the module — never crossing a boundary —
/// is not a reason to refuse. The rejection must be scoped to what the
/// value hooks actually have to carry.
#[test]
fn interior_reference_types_are_not_rejected() {
    let wat = r#"
        (module
          (table 1 funcref)
          (func $interior (param $r funcref) (result i32)
            (i32.const 7))
          (func (export "ok") (param i32) (result i32)
            (call $interior (ref.null func))))
    "#;
    let original = wat::parse_str(wat).expect("valid wat");
    let instrumented = Pipeline::new()
        .run_bytes(&original)
        .expect("an interior funcref is not a boundary value");
    let run = run_module(&instrumented, "ok", &[RecordedValue::I32(3)], &[])
        .expect("the instrumented module must still run");
    assert_eq!(run.results, vec![RecordedValue::I32(7)]);
}
