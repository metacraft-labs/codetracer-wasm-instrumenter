//! M27 verification test #5:
//! `test_wasm_generic_host_module_factory_logs_all_calls`.
//!
//! Given a target host module's import list described in TOML,
//! the framework produces a [`PassThroughPlan`] that the embedder
//! (wazero recorder, wasmtime, browser host) translates into one
//! pass-through stub per import. Each stub logs every call with
//! its arguments and return value before forwarding to the real
//! module.
//!
//! This test exercises the framework's V1 surface by:
//!
//! 1. Loading a TOML config that describes five distinct host
//!    imports (mirroring the wazero-recorder `vm_hooks` Stylus
//!    module's surface, scaled down for the test).
//! 2. Driving 5 simulated calls through [`format_call_log`].
//! 3. Asserting one log line per call appears with the expected
//!    argument formatting + the per-call auto-correlation marker
//!    boundary id.

use codetracer_wasm_host_module_framework::{
    format_call_log, plan_from_toml, CallValue, PassThroughFunction,
};

const CONFIG: &str = r#"
    [host_module]
    name = "vm_hooks"
    auto_correlation_markers = true

    [[function]]
    name    = "read_args"
    params  = ["i32"]
    results = []
    note    = "Stylus calldata read"

    [[function]]
    name    = "storage_load_bytes32"
    params  = ["i32", "i32"]
    results = []
    note    = "EVM SLOAD"

    [[function]]
    name    = "storage_store_bytes32"
    params  = ["i32", "i32"]
    results = []
    note    = "EVM SSTORE"

    [[function]]
    name    = "msg_value"
    params  = ["i32"]
    results = []
    note    = "Tx value (wei)"

    [[function]]
    name    = "msg_sender"
    params  = ["i32"]
    results = []
    note    = "Tx sender (20 bytes)"
"#;

#[test]
fn test_wasm_generic_host_module_factory_logs_all_calls() {
    let plan = plan_from_toml(CONFIG).expect("config must parse");
    assert_eq!(plan.module, "vm_hooks");
    assert!(plan.auto_correlation_markers);
    assert_eq!(plan.functions.len(), 5);

    // Every function gets a marker boundary id.
    for f in &plan.functions {
        let boundary = f
            .boundary_id
            .as_deref()
            .expect("every function must carry an auto-marker boundary_id");
        assert_eq!(boundary, format!("vm_hooks.{}", f.name));
    }

    // Simulate one call per function and assert the log line shape.
    let cases: Vec<(&PassThroughFunction, Vec<CallValue>, &str)> = vec![
        (
            &plan.functions[0],
            vec![CallValue::I32(0x1000)],
            "call vm_hooks.read_args(i32:4096)",
        ),
        (
            &plan.functions[1],
            vec![CallValue::I32(0x2000), CallValue::I32(0x3000)],
            "call vm_hooks.storage_load_bytes32(i32:8192, i32:12288)",
        ),
        (
            &plan.functions[2],
            vec![CallValue::I32(0x4000), CallValue::I32(0x5000)],
            "call vm_hooks.storage_store_bytes32(i32:16384, i32:20480)",
        ),
        (
            &plan.functions[3],
            vec![CallValue::I32(0x6000)],
            "call vm_hooks.msg_value(i32:24576)",
        ),
        (
            &plan.functions[4],
            vec![CallValue::I32(0x7000)],
            "call vm_hooks.msg_sender(i32:28672)",
        ),
    ];
    for (f, args, expected) in cases {
        let line = format_call_log(&plan, &f.name, &args);
        assert_eq!(line, expected, "log line mismatch for {}", f.name);
    }
}
