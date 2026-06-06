// esbuild-plugin-codetracer-wasm
//
// Adds an onLoad hook for `.wasm` modules that shells out to
// `ct-instrument`. Mirrors the Vite/Webpack wrappers; the shared
// core is the Rust crate.
//
// Spec: M27 deliverable #3.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

const execFileAsync = promisify(execFile);

/**
 * Run the instrumenter and return the resulting bytes.
 * @param {string} binary
 * @param {string|undefined} configPath
 * @param {string} sourcePath
 * @returns {Promise<Buffer>}
 */
export async function instrumentWasmFile(binary, configPath, sourcePath) {
  const tmpdir = await fs.mkdtemp(path.join(os.tmpdir(), "ct-instrument-esbuild-"));
  try {
    const out = path.join(tmpdir, path.basename(sourcePath));
    const args = [sourcePath, "-o", out];
    if (configPath) args.push("--config", configPath);
    await execFileAsync(binary, args);
    return await fs.readFile(out);
  } finally {
    await fs.rm(tmpdir, { recursive: true, force: true });
  }
}

/**
 * esbuild plugin factory.
 * @param {{ binary?: string, configPath?: string }} [options]
 */
export default function codetracerWasm(options = {}) {
  const binary = options.binary ?? process.env.CT_INSTRUMENT_BIN ?? "ct-instrument";
  const configPath = options.configPath;
  return {
    name: "esbuild-plugin-codetracer-wasm",
    setup(build) {
      build.onLoad({ filter: /\.wasm$/ }, async (args) => {
        const bytes = await instrumentWasmFile(binary, configPath, args.path);
        return { contents: bytes, loader: "binary" };
      });
    },
  };
}
