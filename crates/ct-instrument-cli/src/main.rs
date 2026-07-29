//! `ct instrument <input.wasm>` — thin CLI wrapper over
//! [`codetracer_wasm_instrumenter::Pipeline`].
//!
//! Two ways to invoke:
//!
//! - directly: `ct-instrument <input.wasm> -o <output.wasm>`;
//! - as a `ct` subcommand: the `ct` umbrella binary in the
//!   codetracer repo dispatches to this binary as `ct instrument`.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use codetracer_wasm_instrumenter::{Pipeline, PipelineConfig};

/// CLI definition.
#[derive(Parser, Debug)]
#[command(
    name = "ct-instrument",
    about = "Bytecode-rewriting WASM instrumenter for CodeTracer (M27)",
    version
)]
struct Cli {
    /// Input WASM module.
    input: PathBuf,

    /// Output path. Defaults to `<input>.instrumented.wasm`.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Optional TOML configuration overriding the
    /// [`PipelineConfig`] defaults.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Where to write the sidecar instrumentation manifest (JSON).
    ///
    /// The manifest maps the `fn_index` integers the injected runtime
    /// hooks report back to function names and a source path. An
    /// embedder that records the module must ship it to the recording
    /// daemon, otherwise the resulting trace has anonymous frames.
    /// Defaults to `<output>.manifest.json`; pass `--no-manifest` to
    /// suppress it.
    #[arg(long)]
    manifest: Option<PathBuf>,

    /// Suppress sidecar-manifest emission.
    #[arg(long, conflicts_with = "manifest")]
    no_manifest: bool,

    /// Source file the module was compiled from, recorded verbatim as
    /// the manifest's source path.
    ///
    /// A release-profile `.wasm` generally carries no DWARF, so this
    /// cannot be inferred from the module; it is a build input in the
    /// same sense as `--output`. Defaults to the input module's
    /// filename.
    #[arg(long)]
    source_path: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let output = cli.output.clone().unwrap_or_else(|| {
        let mut p = cli.input.clone();
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "module".to_string());
        p.set_file_name(format!("{stem}.instrumented.wasm"));
        p
    });

    let config = if let Some(path) = cli.config.as_ref() {
        let body = std::fs::read_to_string(path)?;
        PipelineConfig::from_toml(&body)?
    } else {
        PipelineConfig::default()
    };

    // Default the manifest next to the instrumented module so the
    // common case needs no extra flag: an embedder that just runs
    // `ct-instrument foo.wasm` gets both artefacts it needs.
    let manifest_output: Option<PathBuf> = if cli.no_manifest {
        None
    } else {
        Some(cli.manifest.clone().unwrap_or_else(|| {
            let mut p = output.clone();
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "module.wasm".to_string());
            p.set_file_name(format!("{name}.manifest.json"));
            p
        }))
    };

    Pipeline::with_config(config).run_files_with_manifest(
        &cli.input,
        &output,
        manifest_output.as_ref(),
        cli.source_path.as_deref(),
    )?;
    match manifest_output.as_ref() {
        Some(manifest_path) => println!(
            "ct-instrument: {} -> {} (manifest: {})",
            cli.input.display(),
            output.display(),
            manifest_path.display()
        ),
        None => println!(
            "ct-instrument: {} -> {}",
            cli.input.display(),
            output.display()
        ),
    }
    Ok(())
}
