//! Source-location recovery from a WASM module's DWARF sections.
//!
//! # Why this exists
//!
//! Without it, a recorded WASM frame is addressable only by function:
//! the debugger can say "the value came from `compute_balance`" but not
//! *where inside it*, and an origin chain that crosses into the module
//! has nowhere to point. Every step in the recording collapses onto
//! line 0.
//!
//! Rust emits DWARF into WASM custom sections even for release builds
//! (`.debug_line`, `.debug_info`, `.debug_abbrev`, `.debug_str`), and
//! walrus parses them into [`walrus::Module::debug`] rather than
//! discarding them. So the information is already in the module we hold
//! — it just needs reading.
//!
//! # What it does not do
//!
//! It reports only what DWARF states. A module built without debug info
//! yields no locations at all and callers fall back to naming the
//! module; nothing is inferred from function names, source text, or
//! ordering. That matches the instrumentation-layer spec's rule that
//! the rewriter never invents source locations
//! (`Recording-Backends/WASM-Instrumentation-Layer.md` §10).

use std::collections::{BTreeMap, HashMap};

/// A resolved source position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// Source path exactly as DWARF records it.
    pub file: String,
    /// 1-based line, or 0 when the line program marks the row as having
    /// no meaningful line.
    pub line: i64,
    /// 1-based column, or 0 when unknown.
    pub column: i64,
}

/// Where each function is *declared*, keyed by name.
///
/// The line table answers "what source position does this instruction
/// belong to", which for a function's first instruction is often inside
/// an inlined callee — a prologue that begins by unwrapping a `Result`
/// reports a location in the standard library, not in the user's file.
///
/// `DW_TAG_subprogram`'s `DW_AT_decl_file` / `DW_AT_decl_line` answer
/// the question actually being asked: where the developer wrote this
/// function. That is what belongs in a stack frame.
#[derive(Debug, Default)]
pub struct DeclarationTable {
    by_name: HashMap<String, SourceLocation>,
}

impl DeclarationTable {
    /// Look up a function's declaration site by name.
    pub fn lookup(&self, name: &str) -> Option<&SourceLocation> {
        self.by_name.get(name)
    }

    /// Number of declarations recovered.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Is the table empty?
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

/// Address-ordered map of code offsets to source positions.
///
/// Lookups take the greatest recorded address not exceeding the query,
/// which is how DWARF line tables are meant to be read: a row applies
/// from its address up to the next row's.
#[derive(Debug, Default)]
pub struct LineTable {
    rows: BTreeMap<u64, SourceLocation>,
}

impl LineTable {
    /// Is the table empty (no debug info in the module)?
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Number of rows recovered — useful for diagnostics.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Resolve a code offset to the source position covering it.
    pub fn lookup(&self, address: u64) -> Option<&SourceLocation> {
        self.rows.range(..=address).next_back().map(|(_, loc)| loc)
    }

    /// Build the table from the module's original bytes.
    ///
    /// Deliberately reads the input `.wasm` rather than
    /// [`walrus::Module::debug`]: walrus moves each `.debug_*` section's
    /// bytes out of the module while building its own view, and the
    /// per-unit line programs that survive that move come back empty.
    /// Parsing the bytes we were handed keeps this independent of a
    /// third party's internal bookkeeping.
    ///
    /// Returns an empty table — not an error — when the module carries
    /// no debug info or a line program fails to parse. A module without
    /// usable DWARF is an ordinary situation (any stripped build), and
    /// refusing to instrument it would be far worse than instrumenting
    /// it without source lines.
    pub fn from_wasm_bytes(input: &[u8]) -> Self {
        Self::parse(input).0
    }

    /// Parse both the line table and the function-declaration table in
    /// one pass over the DWARF, since they read the same units.
    pub fn parse_all(input: &[u8]) -> (Self, DeclarationTable) {
        Self::parse(input)
    }

    fn parse(input: &[u8]) -> (Self, DeclarationTable) {
        let mut sections: HashMap<String, Vec<u8>> = HashMap::new();
        for payload in wasmparser::Parser::new(0).parse_all(input).flatten() {
            if let wasmparser::Payload::CustomSection(reader) = payload {
                if reader.name().starts_with(".debug") {
                    sections.insert(reader.name().to_string(), reader.data().to_vec());
                }
            }
        }
        if !sections.contains_key(".debug_line") {
            return (Self::default(), DeclarationTable::default());
        }

        let empty: Vec<u8> = Vec::new();
        let load = |id: gimli::SectionId| -> Result<gimli::EndianSlice<'_, gimli::LittleEndian>, gimli::Error> {
            let data = sections.get(id.name()).unwrap_or(&empty);
            Ok(gimli::EndianSlice::new(data, gimli::LittleEndian))
        };
        let Ok(dwarf) = gimli::Dwarf::load(load) else {
            return (Self::default(), DeclarationTable::default());
        };

        let mut rows: BTreeMap<u64, SourceLocation> = BTreeMap::new();
        let mut declarations: HashMap<String, SourceLocation> = HashMap::new();
        let mut headers = dwarf.units();
        while let Ok(Some(header)) = headers.next() {
            let Ok(unit) = dwarf.unit(header) else {
                continue;
            };
            let Some(program) = unit.line_program.clone() else {
                continue;
            };
            let comp_dir = unit
                .comp_dir
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();

            // Function declarations, from the unit's DIE tree.
            collect_declarations(&dwarf, &unit, &comp_dir, &mut declarations);

            let mut state = program.rows();
            while let Ok(Some((header, row))) = state.next_row() {
                // An end-of-sequence row marks the address just past the
                // last instruction; it carries no position of its own.
                if row.end_sequence() {
                    continue;
                }
                let file = row
                    .file(header)
                    .and_then(|entry| {
                        dwarf
                            .attr_string(&unit, entry.path_name())
                            .ok()
                            .map(|s| s.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default();
                // DWARF file names may be relative to the compilation
                // directory. Joining keeps the recorded path resolvable
                // from outside the build tree.
                let file = if file.starts_with('/') || comp_dir.is_empty() {
                    file
                } else {
                    format!("{comp_dir}/{file}")
                };
                rows.insert(
                    row.address(),
                    SourceLocation {
                        file,
                        line: row.line().map(|l| l.get() as i64).unwrap_or(0),
                        column: match row.column() {
                            gimli::ColumnType::Column(c) => c.get() as i64,
                            gimli::ColumnType::LeftEdge => 0,
                        },
                    },
                );
            }
        }

        (
            Self { rows },
            DeclarationTable {
                by_name: declarations,
            },
        )
    }
}

/// Record every `DW_TAG_subprogram`'s declared name and source position.
fn collect_declarations(
    dwarf: &gimli::Dwarf<gimli::EndianSlice<'_, gimli::LittleEndian>>,
    unit: &gimli::Unit<gimli::EndianSlice<'_, gimli::LittleEndian>>,
    comp_dir: &str,
    out: &mut HashMap<String, SourceLocation>,
) {
    let Some(program) = unit.line_program.as_ref() else {
        return;
    };
    let header = program.header();
    let mut entries = unit.entries();
    while let Ok(Some((_, entry))) = entries.next_dfs() {
        if entry.tag() != gimli::DW_TAG_subprogram {
            continue;
        }
        let Ok(Some(name_attr)) = entry.attr(gimli::DW_AT_name) else {
            continue;
        };
        let Ok(name) = dwarf.attr_string(unit, name_attr.value()) else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();

        let line = entry
            .attr(gimli::DW_AT_decl_line)
            .ok()
            .flatten()
            .and_then(|a| a.udata_value())
            .unwrap_or(0) as i64;
        let file = entry
            .attr(gimli::DW_AT_decl_file)
            .ok()
            .flatten()
            .and_then(|a| a.udata_value())
            .and_then(|idx| header.file(idx))
            .and_then(|f| {
                dwarf
                    .attr_string(unit, f.path_name())
                    .ok()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if file.is_empty() || line == 0 {
            continue;
        }
        let file = if file.starts_with('/') || comp_dir.is_empty() {
            file
        } else {
            format!("{comp_dir}/{file}")
        };
        out.entry(name).or_insert(SourceLocation {
            file,
            line,
            column: 0,
        });
    }
}
