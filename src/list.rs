// Copyright 2023-2024 Martin Pool

//! List mutants and files as text or json.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::io::Write;

use itertools::Itertools;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::mutant::Mutant;
use crate::path::Utf8PathSlashes;
use crate::source::SourceFile;
use crate::{Context, Options, Result};

/// Write a list of mutants to `out`.
///
/// The format is controlled by the `emit_json`, `emit_diffs`, `show_line_col`, and `colors` options.
///
/// Written straight to `out` rather than returned as a string: the JSON
/// rendering of a large tree runs to many megabytes, and building it as a
/// string first costs that again plus the slack of growing it.
pub fn write_mutants(out: &mut impl Write, mutants: &[Mutant], options: &Options) -> Result<()> {
    if options.emit_json {
        serde_json::to_writer_pretty(out, &MutantsJson(mutants)).context("serialize mutants")
    } else {
        // TODO: Do we need to check this? Could the console library strip them if they're not
        // supported?
        let colors = options.colors.active_stdout();
        // One buffer, reused for every mutant, so that the names and diffs
        // don't each allocate on their way to `out`.
        let mut line = String::new();
        for mutant in mutants {
            line.clear();
            if colors {
                line.push_str(&mutant.to_styled_string(options.show_line_col));
            } else {
                mutant.write_name(options.show_line_col, &mut line);
            }
            line.push('\n');
            if options.emit_diffs() {
                line.push_str(&mutant.diff());
                line.push('\n');
            }
            out.write_all(line.as_bytes())?;
        }
        Ok(())
    }
}

/// List the source files as json or text.
pub fn list_files(source_files: &[SourceFile], options: &Options) -> String {
    if options.emit_json {
        let json_list = Value::Array(
            source_files
                .iter()
                .map(|source_file| {
                    json!({
                        "path": source_file.tree_relative_path.to_slash_path(),
                        "package": source_file.package.name,
                    })
                })
                .collect(),
        );
        serde_json::to_string_pretty(&json_list).expect("Serialize source files")
    } else {
        source_files
            .iter()
            .map(|file| file.tree_relative_path.to_slash_path() + "\n")
            .join("")
    }
}

/// A slice of mutants, serialized as the array that `--list --json` prints and
/// that `mutants.out/mutants.json` contains.
///
/// Each mutant is turned into a [`serde_json::Value`] and serialized on its
/// own, so only one mutant's JSON tree is alive at a time instead of the whole
/// list. That `Value` step is also what sorts each mutant's keys, and the
/// sorted order is part of the published format, so it can't be skipped in
/// favour of serializing `Mutant` directly.
pub struct MutantsJson<'a>(pub &'a [Mutant]);

impl Serialize for MutantsJson<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(Mutant::to_json))
    }
}
