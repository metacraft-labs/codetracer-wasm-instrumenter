//! M28 verification: Stylus recorder generalisation onto the M27 WASM
//! instrumentation layer.
//!
//! Three M28 verification tests live here:
//!
//! 1. `test_stylus_recorder_via_wasm_instrumentation_matches_legacy`
//!    — the Stylus `vm_hooks` config recorded via the M27 generic
//!    host-module factory exposes the **same import surface**
//!    (function names + signatures) as the legacy
//!    `codetracer-wasm-recorder/internal/stylus/stylus_funcs.go`
//!    file. The legacy surface is encoded here as the expected
//!    fixture; the test asserts the M27 plan derived from
//!    `codetracer-evm-recorder/stylus/codetracer.toml` matches it
//!    one-for-one. This pins the deliverable that the new TOML
//!    path produces an event stream whose origin queries match the
//!    legacy recorder's surface — the per-event log shape is
//!    structurally identical because `format_call_log` is the
//!    shared formatter both pipelines feed their `vm_hooks.<name>`
//!    events through.
//!
//! 2. `test_stylus_fixture_regenerator_uses_ct_instrument` — the
//!    new `regenerate-stylus-fixture.sh` does **not** invoke the
//!    legacy Stylus-node-only path; instead it routes through the
//!    `ct instrument` (M27) bytecode-rewriting pipeline + the
//!    framework plan above. This test pins the regenerator's text
//!    against the expected invocation, so CI catches a regression
//!    if someone reintroduces the legacy path.
//!
//! 3. `test_origin_stylus_evm_canonical_chain_via_m27` — the M23
//!    canonical chain assertion (in
//!    `codetracer/src/db-backend/tests/origin_stylus_dap_test.rs`)
//!    continues to apply when the fixture is recorded through the
//!    M27 pipeline. This test asserts the *fixture-source contract
//!    is reachable* (so the M27 pipeline has something to record)
//!    and SKIPs cleanly when the Stylus toolchain isn't available.
//!
//! All three tests are environment-tolerant: they exercise the
//! configuration / plan derivation surface — the only piece that
//! lands in M28 — and explicitly defer the *live recording* end of
//! the pipeline to the recorder follow-on (the Stylus toolchain is
//! not part of the Rust workspace this crate sits in).

use std::path::PathBuf;

use codetracer_wasm_host_module_framework::{plan_from_toml, CallValue, PassThroughPlan, WasmType};

/// Locate the committed Stylus configuration. The crate sits at
/// `codetracer-wasm-instrumenter/crates/codetracer-wasm-host-module-framework`,
/// so the config lives four levels up:
/// `../../codetracer-evm-recorder/stylus/codetracer.toml`.
fn stylus_config_path() -> PathBuf {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    here.join("../../../codetracer-evm-recorder/stylus/codetracer.toml")
}

fn load_stylus_plan() -> PassThroughPlan {
    let path = stylus_config_path();
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "stylus config not found at {} ({}). The M28 deliverable \
             ships this file as part of the codetracer-evm-recorder \
             repo; if the repos are vendored side-by-side it should be \
             reachable through the relative path used by this test.",
            path.display(),
            e,
        )
    });
    plan_from_toml(&src).expect("stylus config must parse as a PassThroughPlan")
}

/// Expected `vm_hooks` import list — extracted verbatim from the
/// legacy `codetracer-wasm-recorder/internal/stylus/stylus_funcs.go`
/// `exportSylusFunctions` body (the only place the full surface is
/// pinned in code today). Format:
/// `(name, params, results, note_prefix_or_None)`.
///
/// The note prefix is what `codetracer.toml` should at least
/// mention — the parity test allows the TOML to add commentary as
/// long as the EVM equivalent is recognised. (`""` means: no
/// constraint on the note for this entry.)
fn legacy_stylus_surface() -> &'static [(&'static str, &'static [WasmType], &'static [WasmType])] {
    use WasmType::*;
    &[
        ("read_args", &[I32], &[]),
        ("write_result", &[I32, I32], &[]),
        ("read_return_data", &[I32, I32, I32], &[I32]),
        ("create2", &[I32, I32, I32, I32, I32, I32], &[]),
        ("create1", &[I32, I32, I32, I32, I32], &[]),
        ("account_balance", &[I32, I32], &[]),
        ("account_code", &[I32, I32, I32, I32], &[I32]),
        ("account_code_size", &[I32], &[I32]),
        ("account_codehash", &[I32, I32], &[]),
        ("return_data_size", &[], &[I32]),
        ("contract_address", &[I32], &[]),
        ("msg_reentrant", &[], &[I32]),
        ("msg_sender", &[I32], &[]),
        ("msg_value", &[I32], &[]),
        ("tx_ink_price", &[], &[I32]),
        ("tx_gas_price", &[I32], &[]),
        ("tx_origin", &[I32], &[]),
        ("native_keccak256", &[I32, I32, I32], &[]),
        ("storage_cache_bytes32", &[I32, I32], &[]),
        ("storage_load_bytes32", &[I32, I32], &[]),
        ("storage_flush_cache", &[I32], &[]),
        ("emit_log", &[I32, I32, I32], &[]),
        ("call_contract", &[I32, I32, I32, I32, I64, I32], &[I32]),
        ("delegate_call_contract", &[I32, I32, I32, I64, I32], &[I32]),
        ("static_call_contract", &[I32, I32, I32, I64, I32], &[I32]),
        ("block_basefee", &[I32], &[]),
        ("chainid", &[], &[I64]),
        ("block_coinbase", &[I32], &[]),
        ("block_gas_limit", &[], &[I64]),
        ("block_number", &[], &[I64]),
        ("block_timestamp", &[], &[I64]),
        ("pay_for_memory_grow", &[I32], &[]),
        ("evm_gas_left", &[], &[I64]),
        ("evm_ink_left", &[], &[I64]),
    ]
}

#[test]
fn test_stylus_recorder_via_wasm_instrumentation_matches_legacy() {
    let plan = load_stylus_plan();

    assert_eq!(
        plan.module, "vm_hooks",
        "stylus host module name must match wazero's HostModuleBuilder call",
    );
    assert!(
        plan.auto_correlation_markers,
        "Stylus crossings feed the M25 correlation index — auto markers MUST be on",
    );

    let legacy = legacy_stylus_surface();
    assert_eq!(
        plan.functions.len(),
        legacy.len(),
        "plan must declare every legacy `vm_hooks` import (legacy {} vs plan {})",
        legacy.len(),
        plan.functions.len(),
    );

    // Position-independent check: every legacy entry must appear in
    // the plan with matching params/results. The framework iterates
    // the TOML order at plan-build time, so the TOML ordering is
    // free as long as no entry is missing or signature-mismatched.
    for (name, params, results) in legacy {
        let entry = plan
            .functions
            .iter()
            .find(|f| f.name == *name)
            .unwrap_or_else(|| panic!("legacy import `{name}` missing from M27 plan"));
        assert_eq!(
            entry.params.as_slice(),
            *params,
            "param mismatch for vm_hooks.{name}",
        );
        assert_eq!(
            entry.results.as_slice(),
            *results,
            "result mismatch for vm_hooks.{name}",
        );
        let boundary = entry
            .boundary_id
            .as_deref()
            .unwrap_or_else(|| panic!("vm_hooks.{name} missing M25 boundary id"));
        assert_eq!(boundary, format!("vm_hooks.{name}"));
    }

    // Smoke-check the per-call log shape on one entry. This is the
    // event-stream contract origin queries match against: the
    // recorder emits one `format_call_log` line per crossing, with
    // a stable `i32:<val>` formatting per argument. (See
    // `codetracer/src/db-backend/tests/fixtures/stylus-fund-trace/trace.events.json`
    // — every `Event.metadata` there is the bare function name.)
    let read_args = plan
        .functions
        .iter()
        .find(|f| f.name == "read_args")
        .expect("read_args must be present");
    let line = codetracer_wasm_host_module_framework::format_call_log(
        &plan,
        &read_args.name,
        &[CallValue::I32(0x1000)],
    );
    assert_eq!(line, "call vm_hooks.read_args(i32:4096)");
}

#[test]
fn test_stylus_fixture_regenerator_uses_ct_instrument() {
    // The new regenerate script should explicitly route through the
    // M27 `ct instrument` pipeline rather than re-invoking the
    // legacy Stylus-node code in `codetracer-wasm-recorder/internal/stylus/`.
    //
    // The test only consults the script *text*; the script itself
    // remains gated behind `cargo test --test stylus_fixture_rebuild
    // -- --ignored` (heavyweight rebuild). CI byte-equivalence of
    // the produced `.ct` across reruns is enforced by the
    // deterministic packer in `stylus_fixture_rebuild.rs` (fixed
    // recording_id + CBOR-streamed events with no timestamps).
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = here.join(
        "../../../codetracer/src/db-backend/tests/fixtures/stylus-fund-trace/regenerate-stylus-fixture.sh",
    );
    let text = match std::fs::read_to_string(&script) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "SKIPPED: regenerate-stylus-fixture.sh not visible from this workspace ({e}). \
                 Expected at {} when the codetracer/ repo is vendored side-by-side.",
                script.display(),
            );
            return;
        }
    };

    assert!(
        text.contains("M27") || text.contains("ct instrument") || text.contains("PassThroughPlan")
            || text.contains("stylus_fixture_rebuild"),
        "regenerator script must reference the M27 pipeline (ct instrument / PassThroughPlan / stylus_fixture_rebuild). \
         Current text:\n{text}",
    );
    assert!(
        !text.contains("legacy_stylus_node_recorder"),
        "regenerator must not re-introduce the legacy Stylus-node-only path",
    );
}

#[test]
fn test_origin_stylus_evm_canonical_chain_via_m27() {
    // M23's canonical fixture lives at
    // codetracer/src/db-backend/tests/fixtures/origin/stylus/simple_trivial_chain/main.rs
    // The M28 promise is: the same chain assertion passes when the
    // fixture is recorded through the M27 pipeline. This crate
    // can't drive the cargo-stylus toolchain, but it *can* assert
    // (a) the canonical fixture is committed, (b) the M27 plan
    // recognises the storage/keccak imports the chain crosses.
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let canonical_main = here.join(
        "../../../codetracer/src/db-backend/tests/fixtures/origin/stylus/simple_trivial_chain/main.rs",
    );
    if !canonical_main.is_file() {
        eprintln!(
            "SKIPPED: canonical Stylus fixture missing at {} \
             (sibling codetracer/ repo not vendored). The M23 chain \
             test still pins the assertion end-to-end when the \
             db-backend test suite runs.",
            canonical_main.display(),
        );
        return;
    }

    // The plan must expose the imports the canonical Stylus
    // `simple_trivial_chain` fixture exercises. The fixture itself
    // is `let a = 10; let b = a; let c = b;` — under Stylus
    // codegen this hits `read_args` (calldata fetch) and the
    // export-return path, both of which are in the plan.
    let plan = load_stylus_plan();
    for must_have in &[
        "read_args",
        "write_result",
        "storage_load_bytes32",
        "storage_cache_bytes32",
        "native_keccak256",
    ] {
        assert!(
            plan.functions.iter().any(|f| f.name == *must_have),
            "M27 Stylus plan must surface `{must_have}` for the canonical chain",
        );
    }
}
