//! M27 verification test #1:
//! `test_wasm_instrumenter_cli_produces_valid_module`.
//!
//! Drives the `ct-instrument` binary against a known WAT source,
//! asserts the produced output is a structurally valid WASM module
//! (parseable by both `walrus` and `wasmparser`), and asserts the
//! expected `__ct_emit_*` imports appear in the result.

use std::path::PathBuf;
use std::process::Command;

fn binary_path() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<binname> for the test target.
    PathBuf::from(env!("CARGO_BIN_EXE_ct-instrument"))
}

#[test]
fn test_wasm_instrumenter_cli_produces_valid_module() {
    // 1. Compile a small "hello"-equivalent WAT to a temporary
    //    .wasm input the CLI can consume.
    let tmp = tempdir_via_env();
    let input_path = tmp.join("hello.wasm");
    let output_path = tmp.join("hello.instrumented.wasm");

    let wat_source = r#"
        (module
          (memory (export "mem") 1)
          (func (export "hello") (param $addr i32) (param $val i32)
            local.get $addr
            local.get $val
            i32.store))
    "#;
    let bytes = wat::parse_str(wat_source).expect("input WAT must compile");
    std::fs::write(&input_path, &bytes).expect("write input");

    // 2. Invoke the CLI.
    let output = Command::new(binary_path())
        .arg(&input_path)
        .arg("-o")
        .arg(&output_path)
        .output()
        .expect("ct-instrument binary must be invocable");
    assert!(
        output.status.success(),
        "ct-instrument failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output_path.exists(), "output WASM not written");

    // 3. Verify the output is structurally valid via wasmparser
    //    (a clean-room walker independent of the walrus
    //    round-trip we already exercised in unit tests).
    let instrumented = std::fs::read(&output_path).expect("read output");
    let mut validator = wasmparser::Validator::new();
    validator
        .validate_all(&instrumented)
        .expect("instrumented module must validate per WASM 2.0 spec");

    // 4. Verify the expected hook imports appear.
    let parser = wasmparser::Parser::new(0);
    let mut hooks_seen = std::collections::BTreeSet::new();
    for payload in parser.parse_all(&instrumented) {
        if let Ok(wasmparser::Payload::ImportSection(reader)) = payload {
            // wasmparser 0.245 groups imports via the compact-imports
            // proposal — flatten with `into_imports_with_offsets`.
            for entry in reader.into_imports_with_offsets() {
                let (_, import) = entry.expect("well-formed import");
                if let wasmparser::TypeRef::Func(_) = import.ty {
                    hooks_seen.insert(import.name.to_string());
                }
            }
        }
    }
    for expected in [
        "__ct_emit_call",
        "__ct_emit_return",
        "__ct_emit_realm_boundary",
        "__ct_correlation_token",
        "__ct_emit_i32",
        "__ct_emit_i64",
        // M52: floats cross as their integer bit pattern, so a
        // JavaScript host never performs a lossy `Number` conversion
        // on a NaN payload.  See `hooks.rs`, "Why the float hooks
        // carry integers".
        "__ct_emit_f32_bits",
        "__ct_emit_f64_bits",
    ] {
        assert!(
            hooks_seen.contains(expected),
            "hook import missing: {expected}; saw: {hooks_seen:?}"
        );
    }
}

fn tempdir_via_env() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "codetracer-wasm-instrumenter-cli-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create temp dir");
    base
}
