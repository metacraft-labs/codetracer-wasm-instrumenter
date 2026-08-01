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
// 6. Exits taken by branching to the function's own label (M35b)
// ---------------------------------------------------------------------------

/// Count the `Return`, LEAVE-marker and `Value` events in a stream.
fn event_counts(events: &[RuntimeEvent]) -> (usize, usize, usize) {
    let returns = events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Return { .. }))
        .count();
    let leaves = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                RuntimeEvent::RealmBoundary { direction, .. }
                    if *direction == hooks::REALM_DIRECTION_LEAVE
            )
        })
        .count();
    let values = events
        .iter()
        .filter(|e| matches!(e, RuntimeEvent::Value { .. }))
        .count();
    (returns, leaves, values)
}

/// A one-export signature table, as the manifest would carry it.
fn export_signature(name: &str, params: &[ScalarType], results: &[ScalarType]) -> SignatureTable {
    HashMap::from([(
        (EXPORT, 0u32),
        (name.to_string(), params.to_vec(), results.to_vec()),
    )])
}

/// An exit taken by branching to the function's own label carries the
/// leave event and the result capture, exactly as the fall-through
/// exit does.
///
/// This used to be a hole. An epilogue appended to the entry
/// instruction sequence is jumped clean over by a `br` that names the
/// function label — the branch targets the *end* of that sequence, so
/// it landed after the very instructions meant to record the exit, and
/// the stream was left structurally unbalanced (a call and an ENTER
/// marker with no matching return or LEAVE).
///
/// M35b closes it by moving the body into an inner block typed
/// `[] -> results` and re-pointing the branch at that block, so it now
/// lands *before* the epilogue. The test below is the inverse of the
/// one that used to pin the hole: the same fixture, the same computed
/// answer, and now a stream a replayer-shaped decoder accepts.
#[test]
fn an_exit_by_branch_to_the_function_label_is_captured() {
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
        "restructuring the body must not change what the module computes"
    );

    let (returns, leaves, values) = event_counts(&run.events);
    assert_eq!(returns, 1, "the br-to-label exit must emit a leave event");
    assert_eq!(leaves, 1, "…and its matching LEAVE marker");
    assert_eq!(
        values, 2,
        "one parameter and one result — the result tuple is no longer lost"
    );

    // And the record is not merely present but decodable: a replayer
    // holding nothing but the stream and the signature reconstructs
    // both tuples.
    let signatures = export_signature("escape", &[ScalarType::I32], &[ScalarType::I32]);
    let crossings = decode(&run.events, &signatures);
    assert_eq!(crossings.len(), 1, "{crossings:#?}");
    assert_eq!(crossings[0].args, vec![RecordedValue::I32(1)]);
    assert_eq!(crossings[0].results, vec![RecordedValue::I32(101)]);
    assert_eq!(
        crossings[0].results, run.results,
        "the decoded result tuple must equal what the engine actually returned"
    );
}

/// The three MVP branch forms, each exiting through the function
/// label, each also calling an import and writing to memory — run
/// instrumented and un-instrumented under `wasmi` and compared on both
/// the returned values and the final memory image.
///
/// Restructuring a function body is a far more invasive rewrite than
/// splicing instructions into it, so the event assertions above are
/// not enough on their own: they would still pass if the new inner
/// block had changed the order in which the body's side effects ran.
/// This is the oracle that says it did not. The import call and the
/// memory write are there so the comparison has something to be wrong
/// about — `the_parity_check_compares_memory_and_can_detect_a_difference`
/// proves the memory half of this comparison can fail.
#[test]
fn branch_exits_survive_the_parity_oracle() {
    // Every fixture: store the parameter into memory at a form-specific
    // address, call the import, then leave through the function label.
    let cases: [(&str, &str); 3] = [
        (
            "br",
            r#"
            (module
              (import "env" "note" (func $note (param i32) (result i32)))
              (memory (export "mem") 1)
              (func (export "escape") (param i32) (result i32)
                i32.const 0
                local.get 0
                i32.store
                local.get 0
                call $note
                local.get 0
                i32.add
                br 0
                unreachable))
            "#,
        ),
        (
            "br_if",
            r#"
            (module
              (import "env" "note" (func $note (param i32) (result i32)))
              (memory (export "mem") 1)
              (func (export "escape") (param i32) (result i32)
                i32.const 4
                local.get 0
                i32.store
                local.get 0
                call $note
                local.get 0
                i32.add
                local.get 0
                br_if 0
                i32.const 1000
                i32.add))
            "#,
        ),
        (
            "br_table",
            r#"
            (module
              (import "env" "note" (func $note (param i32) (result i32)))
              (memory (export "mem") 1)
              (func (export "escape") (param i32) (result i32)
                i32.const 8
                local.get 0
                i32.store
                local.get 0
                call $note
                local.get 0
                i32.add
                local.get 0
                br_table 0 0 0
                unreachable))
            "#,
        ),
    ];

    for (label, wat) in cases {
        let original = wat::parse_str(wat).unwrap_or_else(|e| panic!("{label}: bad wat: {e}"));
        let instrumented = Pipeline::new()
            .run_bytes(&original)
            .unwrap_or_else(|e| panic!("{label}: instrumentation failed: {e:#}"));

        // Both arms of the `br_if` fixture, so the conditional case is
        // compared on the taken *and* the fall-through path.
        for input in [7i32, 0i32] {
            let args = [RecordedValue::I32(input)];
            let stub =
                ImportStub::returning("env", "note", vec![vec![RecordedValue::I32(1_000_000)]]);
            let plain = run_module(&original, "escape", &args, std::slice::from_ref(&stub))
                .unwrap_or_else(|e| panic!("{label}/{input}: original run failed: {e:#}"));
            let recorded = run_module(&instrumented, "escape", &args, &[stub])
                .unwrap_or_else(|e| panic!("{label}/{input}: instrumented run failed: {e:#}"));

            assert_eq!(
                plain.results, recorded.results,
                "{label}/{input}: instrumentation changed the value the module returned"
            );
            assert_eq!(
                plain.memory, recorded.memory,
                "{label}/{input}: instrumentation changed the bytes the module wrote"
            );
            assert!(
                plain
                    .memory
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|b| *b != 0))
                    || input == 0,
                "{label}: the fixture must actually write to memory, or the check proves nothing"
            );
            assert!(
                plain.events.is_empty(),
                "{label}: the un-instrumented module must emit nothing"
            );

            // The instrumented run is also a *complete* record on every
            // path: the export crossing closes exactly once, however the
            // function left.
            let export_returns = recorded
                .events
                .iter()
                .filter(|e| matches!(e, RuntimeEvent::Return { fn_kind, .. } if *fn_kind == EXPORT))
                .count();
            let export_leaves = recorded
                .events
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        RuntimeEvent::RealmBoundary { direction, fn_kind, .. }
                            if *direction == hooks::REALM_DIRECTION_LEAVE && *fn_kind == EXPORT
                    )
                })
                .count();
            let (_, _, values) = event_counts(&recorded.events);
            assert_eq!(
                export_returns, 1,
                "{label}/{input}: exactly one export leave event"
            );
            assert_eq!(
                export_leaves, 1,
                "{label}/{input}: exactly one export LEAVE marker"
            );
            assert_eq!(
                values, 4,
                "{label}/{input}: export arg + import arg + import result + export result"
            );
        }
    }
}

/// The zero-result and multi-result block encodings, both reached by a
/// branch to the function label.
///
/// The inner block introduced by M35b is typed by the function's
/// results, so its block type is `Empty` for a void export, a plain
/// result type for one, and a real type-section entry
/// (`BlockType::FunctionType`, the multi-value proposal) for two or
/// more. All three encodings have to validate and all three have to
/// leave the stack exactly as the epilogue expects; `escape` above
/// covers the middle one, and these cover the ends.
#[test]
fn branch_exits_are_captured_for_zero_and_multi_result_exports() {
    // Zero results: the epilogue's capture group is empty, so the
    // crossing closes on the return event alone.
    // The function export is declared first so it is export index 0,
    // which is the index `export_signature` keys the boundary on.
    let void_wat = r#"
        (module
          (memory 1)
          (func (export "sink") (param i32)
            i32.const 0
            local.get 0
            i32.store
            br 0
            unreachable)
          (export "mem" (memory 0)))
    "#;
    let original = wat::parse_str(void_wat).unwrap();
    let instrumented = instrument(void_wat);
    let args = [RecordedValue::I32(0x2a)];
    let plain = run_module(&original, "sink", &args, &[]).unwrap();
    let run = run_module(&instrumented, "sink", &args, &[]).unwrap();
    assert!(run.results.is_empty(), "`sink` returns nothing");
    assert_eq!(
        plain.memory, run.memory,
        "a void export's branch exit must not perturb memory"
    );
    let (returns, leaves, values) = event_counts(&run.events);
    assert_eq!(returns, 1);
    assert_eq!(leaves, 1);
    assert_eq!(values, 1, "one parameter, no results");
    let crossings = decode(
        &run.events,
        &export_signature("sink", &[ScalarType::I32], &[]),
    );
    assert_eq!(crossings.len(), 1, "{crossings:#?}");
    assert_eq!(crossings[0].args, vec![RecordedValue::I32(0x2a)]);
    assert!(crossings[0].results.is_empty());

    // Three results of mixed type: `MultiValue`, and a result tuple
    // whose order the spill/restore has to preserve exactly.
    let multi_wat = r#"
        (module
          (func (export "triple") (param i32) (result i32 i64 f64)
            local.get 0
            i64.const -5
            f64.const 2.5
            br 0
            unreachable))
    "#;
    let run = run_module(
        &instrument(multi_wat),
        "triple",
        &[RecordedValue::I32(9)],
        &[],
    )
    .unwrap();
    assert_eq!(
        run.results,
        vec![
            RecordedValue::I32(9),
            RecordedValue::I64(-5),
            RecordedValue::f64(2.5),
        ],
        "the multi-value branch exit must return the tuple unchanged and in order"
    );
    let (returns, leaves, values) = event_counts(&run.events);
    assert_eq!(returns, 1);
    assert_eq!(leaves, 1);
    assert_eq!(values, 4, "one parameter plus three results");
    let crossings = decode(
        &run.events,
        &export_signature(
            "triple",
            &[ScalarType::I32],
            &[ScalarType::I32, ScalarType::I64, ScalarType::F64],
        ),
    );
    assert_eq!(crossings.len(), 1, "{crossings:#?}");
    assert_eq!(
        crossings[0].results, run.results,
        "the decoded tuple must equal what the engine returned"
    );
}

/// When the branch is conditional, both exits are reachable — and each
/// records exactly once.
///
/// This is the assertion that catches a double epilogue. The
/// fall-through path leaves the inner block normally and then runs the
/// epilogue that follows it; if the rewrite had left the original
/// appended epilogue in place as well, the fall-through run would
/// report its results twice and the decoder would reject the stream.
#[test]
fn a_conditional_branch_exit_records_exactly_once_on_either_path() {
    let wat = r#"
        (module
          (func (export "pick") (param i32) (result i32)
            i32.const 100
            local.get 0
            br_if 0
            drop
            i32.const 200))
    "#;
    let instrumented = instrument(wat);
    let signatures = export_signature("pick", &[ScalarType::I32], &[ScalarType::I32]);

    // 1 -> the branch is taken; 0 -> the body falls out of the block.
    for (input, expected) in [(1i32, 100i32), (0i32, 200i32)] {
        let run = run_module(&instrumented, "pick", &[RecordedValue::I32(input)], &[]).unwrap();
        assert_eq!(
            run.results,
            vec![RecordedValue::I32(expected)],
            "input {input}: the conditional must still pick the same answer"
        );
        let (returns, leaves, values) = event_counts(&run.events);
        assert_eq!(
            returns, 1,
            "input {input}: exactly one leave event, not two"
        );
        assert_eq!(
            leaves, 1,
            "input {input}: exactly one LEAVE marker, not two"
        );
        assert_eq!(
            values, 2,
            "input {input}: one parameter and one result — a second epilogue \
             would report the result tuple twice"
        );
        let crossings = decode(&run.events, &signatures);
        assert_eq!(crossings.len(), 1, "{crossings:#?}");
        assert_eq!(crossings[0].results, vec![RecordedValue::I32(expected)]);
    }
}

/// An explicit `return` nested inside the restructured body is still
/// found and still wrapped.
///
/// Moving the body into an inner block relocates every `return` in the
/// function along with it. The pass that wraps them walks from the
/// entry sequence, so it only reaches them because the `Instr::Block`
/// naming the inner sequence is already in place by the time it runs.
/// Get that ordering wrong and explicit returns silently stop being
/// recorded in exactly the functions this milestone restructures —
/// which is a regression the branch-exit tests above cannot see.
#[test]
fn an_explicit_return_is_still_wrapped_inside_a_restructured_body() {
    let wat = r#"
        (module
          (func (export "both") (param i32) (result i32)
            (block $inner
              (br_if $inner (i32.eqz (local.get 0)))
              (return (i32.const 111)))
            i32.const 222
            local.get 0
            br_if 0
            drop
            i32.const 333))
    "#;
    let instrumented = instrument(wat);
    let signatures = export_signature("both", &[ScalarType::I32], &[ScalarType::I32]);

    // 1 -> the explicit `return`; 0 -> out of `$inner`, then the
    // `br_if 0` is not taken either, so the fall-through exit runs.
    for (input, expected) in [(1i32, 111i32), (0i32, 333i32)] {
        let run = run_module(&instrumented, "both", &[RecordedValue::I32(input)], &[]).unwrap();
        assert_eq!(
            run.results,
            vec![RecordedValue::I32(expected)],
            "input {input}: restructuring must not change which exit is taken"
        );
        let (returns, leaves, values) = event_counts(&run.events);
        assert_eq!(returns, 1, "input {input}: exactly one leave event");
        assert_eq!(leaves, 1, "input {input}: exactly one LEAVE marker");
        assert_eq!(values, 2, "input {input}: one parameter and one result");
        let crossings = decode(&run.events, &signatures);
        assert_eq!(crossings.len(), 1, "{crossings:#?}");
        assert_eq!(crossings[0].results, vec![RecordedValue::I32(expected)]);
    }
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
