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

    Pipeline::with_config(config).run_files(&cli.input, &output)?;
    println!(
        "ct-instrument: {} -> {}",
        cli.input.display(),
        output.display()
    );
    Ok(())
}
