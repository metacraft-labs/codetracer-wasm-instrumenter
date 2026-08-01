//! M35c: the export edge's exit coverage inside exception-handling and
//! GC instruction sequences.
//!
//! `Recording-Backends/WASM-Instrumentation-Layer.md` §§ 3, 5, 6, 8.
//!
//! Until M35c the rewriter's one traversal, `collect_block_ids`,
//! descended into `block` / `loop` / `if`-`else` and nothing else. Two
//! consequences, and the second was the one that kept M35 open:
//!
//! 1. a label-carrying GC/EH instruction (`br_on_cast`, `br_on_null`,
//!    a `try_table` catch clause) naming the function's own label was
//!    not recognised as an exit; and
//! 2. an ordinary explicit `return` nested inside a `try_table` body
//!    was **not wrapped at all**, so that exit emitted no leave event
//!    and no result tuple — despite being an entirely ordinary exit
//!    shape, and despite `try_table` being exactly what
//!    `-fwasm-exceptions` emits.
//!
//! The gap was silent: walrus parses and re-emits such a module
//! without complaint, and the module still computes correctly. Only
//! the record was short.
//!
//! ## Why these tests run under V8 rather than `wasmi`
//!
//! Every other value-level suite in this crate uses the in-process
//! `wasmi` oracle. It cannot serve here: `wasmi` 0.31 hard-codes
//! `exceptions: false` in its engine configuration and exposes no
//! setter, so it refuses a `try_table` module before executing a
//! single instruction. That is precisely why this gap survived a
//! milestone with no coverage of any kind.
//!
//! `codetracer_wasm_stub_host::v8` therefore runs the same modules
//! under the host V8 through `node`, returning the same
//! `RuntimeRecording`. This is not a weaker oracle than `wasmi` — it
//! is a real engine, it is the engine spec § 1's deployment target
//! actually names, and it is the one that ships the final exception
//! handling and WasmGC proposals. `the_two_oracles_agree_on_a_module_
//! both_can_run` pins the two against each other on a module neither
//! has to refuse, so a V8-only assertion below is not resting on an
//! unvalidated harness.
//!
//! No mocks: the host supplies real implementations of the
//! `__codetracer` imports and of the module's own imports, which is
//! the role an embedding page plays, and the module under test is real
//! instrumenter output.

use codetracer_wasm_instrumenter::{hooks, Pipeline, PipelineConfig};
use codetracer_wasm_stub_host::runtime::{
    boundary_frames, run_module, BoundaryFrame, ImportStub, RecordedValue, RuntimeEvent,
    RuntimeRecording,
};
use codetracer_wasm_stub_host::v8::{run_module_under_v8, run_module_under_v8_expecting_failure};

const FN_KIND_IMPORT: i32 = hooks::FUNC_KIND_IMPORT;
const FN_KIND_EXPORT: i32 = hooks::FUNC_KIND_EXPORT;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An explicit `return` nested inside a `try_table` body, plus a
/// fall-through exit, so both paths are live in one module.
///
/// This is the shape the 2026-08-01 review measured. `f(9)` takes the
/// `return`; `f(1)` falls out of the `try_table` and returns 42.
const RETURN_INSIDE_TRY_TABLE: &str = r#"
    (module
      (tag $e)
      (memory (export "mem") 1)
      (func (export "f") (param i32) (result i32)
        (i32.store (i32.const 0) (i32.const 55))
        (block $catch
          (try_table (catch_all $catch)
            local.get 0
            i32.const 5
            i32.gt_s
            (if (then i32.const 101 return))))
        i32.const 42))
"#;

/// The twin of [`RETURN_INSIDE_TRY_TABLE`] with the `try_table`
/// replaced by a plain `block`, and nothing else changed.
///
/// The review's measurement was that these two grow by different
/// amounts under instrumentation — 390 against 416 bytes, the 26-byte
/// difference being exactly the one return-site epilogue the
/// `try_table` twin never received. Keeping both fixtures lets that be
/// re-asserted as a *behavioural* equality rather than a byte count.
const RETURN_INSIDE_PLAIN_BLOCK: &str = r#"
    (module
      (tag $e)
      (memory (export "mem") 1)
      (func (export "f") (param i32) (result i32)
        (i32.store (i32.const 0) (i32.const 55))
        (block $catch
          (block
            local.get 0
            i32.const 5
            i32.gt_s
            (if (then i32.const 101 return))))
        i32.const 42))
"#;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn instrument(wat: &str) -> Vec<u8> {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    Pipeline::new()
        .run_bytes(&original)
        .expect("pipeline must succeed")
}

fn record_v8(
    wat: &str,
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> (Vec<BoundaryFrame>, RuntimeRecording) {
    let instrumented = instrument(wat);
    validates_with_all_proposals(&instrumented, "instrumented module");
    let run = run_module_under_v8(&instrumented, export, args, stubs)
        .expect("V8 must instantiate and run the instrumented module");
    (boundary_frames(&run.events), run)
}

/// Assert the module is well-formed WASM under the full feature set,
/// using the same `wasmparser` validator wasmtime uses.
///
/// This is a genuinely independent check on the rewrite and not a
/// restatement of "V8 accepted it": an inner block typed against the
/// wrong result vector, an unbalanced spill, or a branch left pointing
/// at a sequence that is no longer in scope are all caught here, with
/// a byte offset, rather than as a bare instantiation failure.
fn validates_with_all_proposals(wasm: &[u8], what: &str) {
    let mut features = wasmparser::WasmFeatures::default();
    features.insert(wasmparser::WasmFeatures::EXCEPTIONS);
    features.insert(wasmparser::WasmFeatures::LEGACY_EXCEPTIONS);
    features.insert(wasmparser::WasmFeatures::GC);
    features.insert(wasmparser::WasmFeatures::FUNCTION_REFERENCES);
    wasmparser::Validator::new_with_features(features)
        .validate_all(wasm)
        .unwrap_or_else(|e| panic!("{what} does not validate: {e}"));
}

/// The export-edge frames of a recording, which is what every
/// assertion below is really about.
fn export_frames(frames: &[BoundaryFrame]) -> Vec<&BoundaryFrame> {
    frames
        .iter()
        .filter(|f| f.fn_kind == FN_KIND_EXPORT)
        .collect()
}

fn count_export(events: &[RuntimeEvent], want_return: bool) -> usize {
    events
        .iter()
        .filter(|e| match e {
            RuntimeEvent::Return { fn_kind, .. } if want_return => *fn_kind == FN_KIND_EXPORT,
            RuntimeEvent::Call { fn_kind, .. } if !want_return => *fn_kind == FN_KIND_EXPORT,
            _ => false,
        })
        .count()
}

/// Run the original and the instrumented module side by side under V8
/// and require them to agree on the returned values and on memory.
///
/// Both rewrite configurations are checked for the same reason
/// `boundary_values.rs` checks both: non-interference is a statement
/// about the rewriter, not about which pass is enabled, and the
/// interior store pass is the more invasive of the two. Since M35c
/// that pass also descends into `try_table` bodies, so a store there
/// is now rewritten and this is the only thing that would notice if
/// the rewrite disturbed the operand stack in an EH sequence.
fn assert_computes_identically_under_v8(
    wat: &str,
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) {
    let original = wat::parse_str(wat).expect("input WAT must compile");
    let before = run_module_under_v8(&original, export, args, stubs).expect("original run");
    assert!(
        before.events.is_empty(),
        "the original module must emit no hook events"
    );

    for (label, config) in [
        ("boundary-only (the default)", PipelineConfig::default()),
        (
            "boundary + the interior store pass",
            PipelineConfig {
                instrument_stores: true,
                ..PipelineConfig::default()
            },
        ),
    ] {
        let instrumented = Pipeline::with_config(config)
            .run_bytes(&original)
            .expect("instrument");
        validates_with_all_proposals(&instrumented, label);
        let after = run_module_under_v8(&instrumented, export, args, stubs)
            .expect("instrumented run under V8");
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
// The oracle itself
// ---------------------------------------------------------------------------

/// The V8 oracle and the `wasmi` oracle must agree, or nothing a
/// V8-only test asserts below means anything.
///
/// The module is deliberately one both engines can run — no
/// exceptions, no GC — and it exercises every part of the bridge that
/// could disagree: an export with an argument and a result, an
/// imported call with a stubbed return value, a memory write, and the
/// full event stream including correlation tokens.
#[test]
fn the_two_oracles_agree_on_a_module_both_can_run() {
    let wat = r#"
        (module
          (import "env" "h" (func $h (param i32) (result i64)))
          (memory (export "mem") 1)
          (func (export "f") (param i32) (result i64)
            (i32.store (i32.const 8) (local.get 0))
            local.get 0
            call $h))
    "#;
    let original = wat::parse_str(wat).expect("wat");
    let instrumented = Pipeline::new().run_bytes(&original).expect("instrument");
    let args = [RecordedValue::I32(21)];
    let stubs = [ImportStub::returning(
        "env",
        "h",
        vec![vec![RecordedValue::I64(4242)]],
    )];

    let via_wasmi = run_module(&instrumented, "f", &args, &stubs).expect("wasmi run");
    let via_v8 = run_module_under_v8(&instrumented, "f", &args, &stubs).expect("V8 run");

    assert_eq!(
        via_wasmi.events, via_v8.events,
        "the two engines disagree on the recorded event stream"
    );
    assert_eq!(via_wasmi.results, via_v8.results, "returned values differ");
    assert_eq!(via_wasmi.memory, via_v8.memory, "final memory differs");
}

/// The oracle must be able to fail. A test whose engine silently
/// swallows a bad module proves nothing.
#[test]
fn the_v8_oracle_reports_a_module_it_cannot_run() {
    // A module whose only export traps immediately.
    let wat = r#"(module (func (export "f") unreachable))"#;
    let wasm = wat::parse_str(wat).expect("wat");
    let err = run_module_under_v8(&wasm, "f", &[], &[]).expect_err("a trap must surface");
    let text = format!("{err:#}");
    assert!(
        text.contains("unreachable") || text.contains("RuntimeError"),
        "the trap must be reported, not swallowed: {text}"
    );

    let err = run_module_under_v8(&wasm, "nope", &[], &[])
        .expect_err("a missing export must be an error");
    assert!(
        format!("{err:#}").contains("nope"),
        "the missing export must be named"
    );
}

/// `wasmi` really cannot serve this suite — recorded so that nobody
/// "simplifies" these tests back onto the in-process oracle and
/// silently loses the coverage again.
#[test]
fn the_wasmi_oracle_still_refuses_exception_handling_modules() {
    let instrumented = instrument(RETURN_INSIDE_TRY_TABLE);
    let err = run_module(&instrumented, "f", &[RecordedValue::I32(9)], &[])
        .expect_err("wasmi 0.31 has no way to enable the exceptions proposal");
    let text = format!("{err:#}").to_lowercase();
    assert!(
        text.contains("exception"),
        "the refusal should name the proposal: {text}"
    );
}

// ---------------------------------------------------------------------------
// The milestone blocker: a plain `return` inside a `try_table` body
// ---------------------------------------------------------------------------

/// An ordinary explicit `return` nested in a `try_table` body records
/// its leave event and its result tuple, on a real engine.
///
/// This is the assertion M35 was held open for. Before M35c the
/// `f(9)` run below produced `[Call, Value(9), RealmBoundary(ENTER)]`
/// and stopped there: no result value, no `Return`, no LEAVE — the
/// crossing simply never closed, and a § 6 replayer refuses such a
/// stream outright.
#[test]
fn an_explicit_return_inside_a_try_table_body_is_captured() {
    // The `return` path.
    let (frames, run) = record_v8(RETURN_INSIDE_TRY_TABLE, "f", &[RecordedValue::I32(9)], &[]);
    assert_eq!(
        run.results,
        vec![RecordedValue::I32(101)],
        "computed answer"
    );

    let exports = export_frames(&frames);
    assert_eq!(exports.len(), 1, "exactly one export crossing: {frames:#?}");
    assert_eq!(
        exports[0].args,
        vec![RecordedValue::I32(9)],
        "the parameter, captured at entry"
    );
    assert_eq!(
        exports[0].results,
        vec![RecordedValue::I32(101)],
        "the result tuple of the exit taken inside the try_table — this is \
         what was missing entirely before M35c"
    );

    assert_eq!(
        count_export(&run.events, true),
        1,
        "exactly one export-kind Return: a double epilogue is as wrong as none"
    );
    assert_eq!(
        run.events
            .iter()
            .filter(|e| matches!(
                e,
                RuntimeEvent::RealmBoundary {
                    direction,
                    fn_kind,
                    ..
                } if *direction == hooks::REALM_DIRECTION_LEAVE && *fn_kind == FN_KIND_EXPORT
            ))
            .count(),
        1,
        "the crossing must close with exactly one LEAVE"
    );

    // The fall-through path through the same module, which was already
    // captured before M35c and must stay that way.
    let (frames, run) = record_v8(RETURN_INSIDE_TRY_TABLE, "f", &[RecordedValue::I32(1)], &[]);
    assert_eq!(run.results, vec![RecordedValue::I32(42)]);
    let exports = export_frames(&frames);
    assert_eq!(exports.len(), 1);
    assert_eq!(exports[0].args, vec![RecordedValue::I32(1)]);
    assert_eq!(exports[0].results, vec![RecordedValue::I32(42)]);
    assert_eq!(count_export(&run.events, true), 1);
}

/// The `try_table` twin and the plain-`block` twin now record
/// identically — which is the 26-byte gap, re-expressed as the thing
/// it actually cost.
///
/// The review's evidence for the gap was a byte measurement: two
/// modules with identical bodies grew by 390 and 416 bytes, the
/// difference being one return-site epilogue. A byte count is fragile
/// as a regression test (any unrelated change to the epilogue moves
/// it), so it is asserted here as the equality it stood for: the two
/// twins must produce the *same event stream* on the same input. A
/// missing epilogue on either side breaks it, and so does a spurious
/// extra one.
///
/// The byte measurement is kept as a second, weaker assertion because
/// it is the one that fails if the two rewrites diverge in a way the
/// event stream happens not to observe.
#[test]
fn the_try_table_and_plain_block_twins_record_identically() {
    for input in [1, 9] {
        let args = [RecordedValue::I32(input)];
        let (_, via_try) = record_v8(RETURN_INSIDE_TRY_TABLE, "f", &args, &[]);
        let (_, via_block) = record_v8(RETURN_INSIDE_PLAIN_BLOCK, "f", &args, &[]);
        assert_eq!(
            via_try.results, via_block.results,
            "the twins must compute the same answer for f({input})"
        );
        assert_eq!(
            via_try.events, via_block.events,
            "the twins must RECORD the same thing for f({input}); a difference here \
             is an exit inside the try_table that the plain block captured and the \
             try_table did not"
        );
    }

    // The same property at the byte level, which is how the gap was
    // originally found.
    let growth = |wat: &str| {
        let original = wat::parse_str(wat).expect("wat");
        instrument(wat).len() as i64 - original.len() as i64
    };
    assert_eq!(
        growth(RETURN_INSIDE_TRY_TABLE),
        growth(RETURN_INSIDE_PLAIN_BLOCK),
        "the two twins must grow by the same number of bytes under \
         instrumentation; an inequality is a missing epilogue"
    );
}

/// Instrumenting an exception-handling module must not change what it
/// computes or what it writes — including under the interior store
/// pass, which since M35c also rewrites stores inside `try_table`
/// bodies.
#[test]
fn instrumenting_a_try_table_module_does_not_perturb_it() {
    for input in [1, 9] {
        assert_computes_identically_under_v8(
            RETURN_INSIDE_TRY_TABLE,
            "f",
            &[RecordedValue::I32(input)],
            &[],
        );
    }
}

// ---------------------------------------------------------------------------
// Branch exits from inside exception-handling sequences
// ---------------------------------------------------------------------------

/// M35b's restructuring must reach a branch that names the function
/// label from *inside* a `try_table` body.
///
/// Two things have to be true at once for this to pass, and they are
/// in different functions. `function_branches_to_entry` has to see the
/// branch (it walks `collect_block_ids`, so it needs the descent), and
/// `reroot_body_into_inner_block` has to re-point it (it walks the
/// same traversal and substitutes through `visit_branch_targets_mut`).
/// Miss either and the exit lands past the epilogue exactly as it did
/// before M35b.
#[test]
fn a_branch_to_the_function_label_from_inside_a_try_table_is_captured() {
    let wat = r#"
        (module
          (tag $e)
          (memory (export "mem") 1)
          (func (export "f") (param i32) (result i32)
            (i32.store (i32.const 0) (i32.const 55))
            (block $catch
              (try_table (catch_all $catch)
                local.get 0
                i32.const 5
                i32.gt_s
                (if (then i32.const 77 br 3))))
            i32.const 42))
    "#;

    let (frames, run) = record_v8(wat, "f", &[RecordedValue::I32(9)], &[]);
    assert_eq!(run.results, vec![RecordedValue::I32(77)], "computed answer");
    let exports = export_frames(&frames);
    assert_eq!(exports.len(), 1, "one export crossing: {frames:#?}");
    assert_eq!(exports[0].args, vec![RecordedValue::I32(9)]);
    assert_eq!(
        exports[0].results,
        vec![RecordedValue::I32(77)],
        "the branch exit's result tuple"
    );
    assert_eq!(count_export(&run.events, true), 1, "exactly one Return");

    // The fall-through path, in the same module.
    let (frames, run) = record_v8(wat, "f", &[RecordedValue::I32(1)], &[]);
    assert_eq!(run.results, vec![RecordedValue::I32(42)]);
    assert_eq!(
        export_frames(&frames)[0].results,
        vec![RecordedValue::I32(42)]
    );
    assert_eq!(count_export(&run.events, true), 1);

    assert_computes_identically_under_v8(wat, "f", &[RecordedValue::I32(9)], &[]);
    assert_computes_identically_under_v8(wat, "f", &[RecordedValue::I32(1)], &[]);
}

/// A `try_table` **catch clause** naming the function's own label is
/// an exit, and it is one no `br` instruction appears for.
///
/// This is the form `visit_branch_targets` had to be widened for: the
/// label lives in the `catches` vector of the `try_table` instruction,
/// not in a branch instruction, so a scan that only looks at
/// `Br`/`BrIf`/`BrTable` cannot see it at all. The exception carries
/// the tag's `i32` payload, which becomes the function's result.
#[test]
fn a_try_table_catch_clause_naming_the_function_label_is_an_exit() {
    let wat = r#"
        (module
          (tag $e (param i32))
          (memory (export "mem") 1)
          (func (export "f") (param i32) (result i32)
            (i32.store (i32.const 0) (i32.const 55))
            (try_table (catch $e 0)
              local.get 0
              i32.const 5
              i32.gt_s
              (if (then i32.const 88 throw $e)))
            i32.const 42))
    "#;

    // The throwing path: the catch clause branches straight to the
    // function label, carrying 88 as the result.
    let (frames, run) = record_v8(wat, "f", &[RecordedValue::I32(9)], &[]);
    assert_eq!(run.results, vec![RecordedValue::I32(88)], "computed answer");
    let exports = export_frames(&frames);
    assert_eq!(exports.len(), 1, "one export crossing: {frames:#?}");
    assert_eq!(exports[0].args, vec![RecordedValue::I32(9)]);
    assert_eq!(
        exports[0].results,
        vec![RecordedValue::I32(88)],
        "the value the catch clause carried out of the function"
    );
    assert_eq!(count_export(&run.events, true), 1, "exactly one Return");

    // The non-throwing path.
    let (frames, run) = record_v8(wat, "f", &[RecordedValue::I32(1)], &[]);
    assert_eq!(run.results, vec![RecordedValue::I32(42)]);
    assert_eq!(
        export_frames(&frames)[0].results,
        vec![RecordedValue::I32(42)]
    );
    assert_eq!(count_export(&run.events, true), 1);

    assert_computes_identically_under_v8(wat, "f", &[RecordedValue::I32(9)], &[]);
    assert_computes_identically_under_v8(wat, "f", &[RecordedValue::I32(1)], &[]);
}

/// The GC half of the widening: `br_on_null` naming the function
/// label.
///
/// The export's own signature stays scalar on purpose — a boundary
/// signature mentioning a reference type is refused outright by
/// `reject_unrepresentable_boundaries` (spec § 8), so the reference
/// has to live *inside* the function, which is also where a real
/// WasmGC producer puts it. The module reports through memory rather
/// than through a result so that the branch exit carries an empty
/// tuple, exercising the `Simple(None)` inner-block encoding on a
/// GC-form exit.
#[test]
fn a_br_on_null_naming_the_function_label_is_an_exit() {
    let wat = r#"
        (module
          (type $s (struct (field i32)))
          (memory (export "mem") 1)
          (func (export "f") (param i32)
            (local $r (ref null $s))
            (if (local.get 0)
              (then (local.set $r (struct.new $s (i32.const 7)))))
            (i32.store (i32.const 0) (i32.const 55))
            i32.const 4
            local.get $r
            br_on_null 0
            struct.get $s 0
            i32.store))
    "#;

    for (input, expect_field) in [(0, 0u8), (1, 7u8)] {
        let (frames, run) = record_v8(wat, "f", &[RecordedValue::I32(input)], &[]);
        let memory = run.memory.as_ref().expect("the module exports memory");
        assert_eq!(memory[0], 55, "the unconditional store must have happened");
        assert_eq!(
            memory[4],
            expect_field,
            "f({input}) must have taken the {} path",
            if input == 0 { "null" } else { "non-null" }
        );

        let exports = export_frames(&frames);
        assert_eq!(exports.len(), 1, "one export crossing: {frames:#?}");
        assert_eq!(exports[0].args, vec![RecordedValue::I32(input)]);
        assert!(
            exports[0].results.is_empty(),
            "a void export has no result tuple"
        );
        assert_eq!(
            count_export(&run.events, true),
            1,
            "the leave event must be recorded on the br_on_null exit too"
        );

        assert_computes_identically_under_v8(wat, "f", &[RecordedValue::I32(input)], &[]);
    }
}

// ---------------------------------------------------------------------------
// The import edge inside an exception-handling sequence
// ---------------------------------------------------------------------------

/// An imported call inside a `try_table` body is captured on both
/// halves.
///
/// The import edge is walked through the same traversal as the export
/// exits, so before M35c a call to an imported function inside a
/// `try_table` produced no crossing at all — the replay input M37
/// feeds back was simply absent from the log.
#[test]
fn an_imported_call_inside_a_try_table_body_is_captured() {
    let wat = r#"
        (module
          (import "env" "h" (func $h (param i32) (result i64)))
          (tag $e)
          (memory (export "mem") 1)
          (func (export "f") (param i32) (result i64)
            (block $catch
              (try_table (catch_all $catch)
                local.get 0
                call $h
                return))
            i64.const -1))
    "#;
    let stubs = [ImportStub::returning(
        "env",
        "h",
        vec![vec![RecordedValue::I64(4242)]],
    )];
    let (frames, run) = record_v8(wat, "f", &[RecordedValue::I32(21)], &stubs);
    assert_eq!(run.results, vec![RecordedValue::I64(4242)]);

    let imports: Vec<_> = frames
        .iter()
        .filter(|f| f.fn_kind == FN_KIND_IMPORT)
        .collect();
    assert_eq!(
        imports.len(),
        1,
        "the import crossing inside the try_table must appear: {frames:#?}"
    );
    assert_eq!(
        imports[0].args,
        vec![RecordedValue::I32(21)],
        "the argument, captured before the call"
    );
    assert_eq!(
        imports[0].results,
        vec![RecordedValue::I64(4242)],
        "the import's result — the value an M37 replay has to feed back"
    );

    let exports = export_frames(&frames);
    assert_eq!(exports.len(), 1);
    assert_eq!(exports[0].results, vec![RecordedValue::I64(4242)]);
    assert_eq!(count_export(&run.events, true), 1);

    assert_computes_identically_under_v8(wat, "f", &[RecordedValue::I32(21)], &stubs);
}

// ---------------------------------------------------------------------------
// The legacy exception-handling proposal
// ---------------------------------------------------------------------------

/// The legacy `try` / `catch` form, whose handler blocks are nested
/// sequences rather than labels.
///
/// Browsers still serve modules produced by older Emscripten
/// toolchains, so the legacy form is not hypothetical.
/// `collect_block_ids` descends into the try body *and* into every
/// `catch` / `catch_all` handler, which is a different code path from
/// the `try_table` descent — a handler is owned by the instruction,
/// whereas a `try_table` catch clause names an enclosing label.
///
/// Asserted structurally rather than by execution: V8 has retired the
/// legacy opcodes, so there is no engine here that will run one. What
/// *is* checked is the property that matters — the return sites in
/// both the try body and the catch handler are wrapped, so the module
/// records the same number of exits as its `block`-shaped twin — plus
/// full re-validation under `wasmparser`'s legacy-exceptions feature.
#[test]
fn returns_inside_a_legacy_try_and_its_catch_handler_are_wrapped() {
    let wat = r#"
        (module
          (tag $e)
          (func (export "f") (param i32) (result i32)
            try (result i32)
              local.get 0
              i32.const 5
              i32.gt_s
              if
                i32.const 101
                return
              end
              i32.const 1
            catch_all
              i32.const 202
              return
            end))
    "#;
    let original = wat::parse_str(wat).expect("legacy try WAT must compile");
    let instrumented = Pipeline::new().run_bytes(&original).expect("instrument");
    validates_with_all_proposals(&instrumented, "instrumented legacy-try module");

    // Two `return`s, both nested in legacy EH sequences, plus the
    // fall-through exit: three epilogues. Counted through the stub
    // host's static walk, which is the tool for a module no engine
    // here will execute.
    let events = codetracer_wasm_stub_host::record_instrumented(&instrumented)
        .expect("the stub host must be able to walk the instrumented module");
    let returns = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                codetracer_wasm_stub_host::Event::Return { fn_kind, .. }
                    if *fn_kind == FN_KIND_EXPORT
            )
        })
        .count();
    assert_eq!(
        returns, 3,
        "the fall-through exit, the `return` in the try body and the `return` in \
         the catch_all handler must each carry an epilogue; before M35c only the \
         fall-through did. Events: {events:#?}"
    );
}

// ---------------------------------------------------------------------------
// The residual: an exception that unwinds past an open crossing
// ---------------------------------------------------------------------------

/// **Pins a known gap rather than a fixed behaviour.** An exception
/// that propagates out of an instrumented export leaves the crossing
/// open, because the § 5 hook surface has no unwind event.
///
/// This is deliberately *not* an exception-handling-walk problem, and
/// widening the walk cannot address it: the same thing happens to a
/// function that throws without a `try_table` anywhere in it, which is
/// the second half of this test. What is missing is a hook, so closing
/// it is a § 5 change and belongs with whichever milestone replays
/// exceptions.
///
/// It does not violate spec § 8's "rejected rather than silently
/// degraded" rule, and that is the substance of what is asserted here:
/// the stream is left structurally **unbalanced** — the ENTER half
/// with no LEAVE — which a § 6 replayer refuses outright rather than
/// replaying past. A silent divergence would be an open Call matched
/// by a fabricated Return; that is what this test proves does not
/// happen.
///
/// The test is written to fail the day the gap is closed: it asserts
/// the export does not return normally and that the LEAVE is absent.
/// Whoever adds an unwind hook must come here and invert it.
#[test]
fn an_exception_unwinding_out_of_an_export_leaves_the_crossing_open() {
    for (label, wat) in [
        (
            "with a try_table in the function",
            r#"
            (module
              (tag $e)
              (func (export "f") (param i32) (result i32)
                (block $catch
                  (try_table (catch_all $catch)
                    i32.const 0
                    drop))
                throw $e))
        "#,
        ),
        (
            "with no exception-handling instruction at all",
            r#"
            (module
              (tag $e)
              (func (export "f") (param i32) (result i32)
                throw $e))
        "#,
        ),
    ] {
        let instrumented = instrument(wat);
        validates_with_all_proposals(&instrumented, label);
        let failure = run_module_under_v8_expecting_failure(
            &instrumented,
            "f",
            &[RecordedValue::I32(1)],
            &[],
        )
        .unwrap_or_else(|e| panic!("{label}: {e:#}"));

        let enters = failure
            .events
            .iter()
            .filter(|e| {
                matches!(e, RuntimeEvent::RealmBoundary { direction, fn_kind, .. }
                    if *direction == hooks::REALM_DIRECTION_ENTER && *fn_kind == FN_KIND_EXPORT)
            })
            .count();
        let leaves = failure
            .events
            .iter()
            .filter(|e| {
                matches!(e, RuntimeEvent::RealmBoundary { direction, fn_kind, .. }
                    if *direction == hooks::REALM_DIRECTION_LEAVE && *fn_kind == FN_KIND_EXPORT)
            })
            .count();

        assert_eq!(
            enters, 1,
            "{label}: the crossing must still have been opened: {:#?}",
            failure.events
        );
        assert_eq!(
            leaves, 0,
            "{label}: KNOWN GAP — there is no unwind hook, so the crossing cannot \
             close. If this now reads 1, an unwind event has been added and this \
             test should be inverted rather than deleted. Events: {:#?}",
            failure.events
        );
        assert_eq!(
            count_export(&failure.events, true),
            0,
            "{label}: and no Return may be fabricated for an exit that did not \
             produce results — an unbalanced stream a § 6 replayer refuses is the \
             correct failure, a matched-but-invented one would be a silent divergence"
        );
    }
}
