//! M27 verification test #4:
//! `test_wasm_instrumenter_parity_with_interpreter_recorder`.
//!
//! Cross-modality parity check. Records the existing pure-WASM
//! fixtures under
//! `codetracer-wasm-recorder/cmd/wazero/testdata/recorder-golden/`
//! twice — once via the synthetic interpreter oracle, once via
//! `ct instrument` + the stub host — and asserts the two CTFS
//! event streams are equal modulo timestamps.
//!
//! The interpreter oracle is a static IR walk of the original
//! module that emits the same canonical Event vocabulary the
//! instrumented module emits at runtime. This is the structural
//! parity guarantee — when wired to a real WASM runtime in a
//! follow-on milestone, the runtime trace agrees byte-for-byte.

use std::path::{Path, PathBuf};

use codetracer_wasm_instrumenter::{Pipeline, PipelineConfig};
use codetracer_wasm_stub_host::{assert_parity, record_instrumented, record_interpreter};

fn golden_path(name: &str) -> Option<PathBuf> {
    // tests are run from the crate root; walk up to the
    // workspace root then sideways into the sibling wasm-recorder
    // checkout.
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

fn parity_one(path: &Path) {
    let bytes = std::fs::read(path).expect("read input wasm");
    let original_events = record_interpreter(&bytes).expect("oracle walk");
    // `record_interpreter` is an *interior-model* oracle: it walks the
    // original module and reports a `Write` for every store it finds.
    // The rewrite it is the oracle for is therefore the one that also
    // instruments stores, which M36 took off the default path (spec
    // §§ 2, 11) without removing. Running the default here instead
    // would not be a weaker parity check, it would be a comparison
    // between two different questions — the oracle would report writes
    // the module was never asked to report.
    //
    // Spec § 10 replaces this structural check with an end-to-end one
    // (materialised trace vs materialised trace) once the replayer of
    // M37 exists. Until then this remains the strongest available
    // statement that the rewriter preserves the event stream, and it
    // covers the boundary events of the default rewrite as a subset.
    let instrumented = Pipeline::with_config(PipelineConfig {
        instrument_stores: true,
        ..PipelineConfig::default()
    })
    .run_bytes(&bytes)
    .expect("instrument failed");
    let observed_events = record_instrumented(&instrumented).expect("observer walk");
    assert_parity(&original_events, &observed_events).expect("parity failed");
}

#[test]
fn test_wasm_instrumenter_parity_with_interpreter_recorder_collections() {
    let Some(path) = golden_path("collections.wasm") else {
        // M27 acceptance criteria allow narrow SKIP for fixtures
        // that aren't present (e.g. shallow clones of the
        // workspace). We log the skip rather than fail silently.
        eprintln!(
            "[skip] collections.wasm not found; \
             clone codetracer-wasm-recorder to enable the parity test"
        );
        return;
    };
    parity_one(&path);
}

#[test]
fn test_wasm_instrumenter_parity_with_interpreter_recorder_control_flow() {
    let Some(path) = golden_path("control_flow.wasm") else {
        eprintln!("[skip] control_flow.wasm not found");
        return;
    };
    parity_one(&path);
}

#[test]
fn test_wasm_instrumenter_parity_with_interpreter_recorder_nested_calls() {
    let Some(path) = golden_path("nested_calls.wasm") else {
        eprintln!("[skip] nested_calls.wasm not found");
        return;
    };
    parity_one(&path);
}

#[test]
fn test_wasm_instrumenter_parity_with_interpreter_recorder_panic_path() {
    let Some(path) = golden_path("panic_path.wasm") else {
        eprintln!("[skip] panic_path.wasm not found");
        return;
    };
    parity_one(&path);
}
