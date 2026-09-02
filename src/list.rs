// Copyright 2023-2024 Martin Pool

//! List mutants and files as text or json.

#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use itertools::Itertools;
use serde::{Serialize, Serializer};
use serde_json::{Value, json};

use crate::Options;
use crate::mutant::Mutant;
use crate::path::Utf8PathSlashes;
use crate::source::SourceFile;

/// Return a string representation of a list of mutants.
///
/// The format is controlled by the `emit_json`, `emit_diffs`, `show_line_col`, and `colors` options.
pub fn list_mutants(mutants: &[Mutant], options: &Options) -> String {
    if options.emit_json {
        mutants_to_json_string(mutants)
    } else {
        // TODO: Do we need to check this? Could the console library strip them if they're not
        // supported?
        let colors = options.colors.active_stdout();
        let mut out = String::with_capacity(200 * mutants.len());
        for mutant in mutants {
            if colors {
                out.push_str(&mutant.to_styled_string(options.show_line_col));
            } else {
                mutant.write_name(options.show_line_col, &mut out);
            }
            out.push('\n');
            if options.emit_diffs() {
                out.push_str(&mutant.diff());
                out.push('\n');
            }
        }
        out
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
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter().map(Mutant::to_json))
    }
}

/// Convert a slice of mutants to a pretty-printed JSON string.
///
/// Each mutant includes its diff. Used for `--list --json`; `mutants.json` is
/// written straight to the file instead, without going through a string.
pub fn mutants_to_json_string(mutants: &[Mutant]) -> String {
    serde_json::to_string_pretty(&MutantsJson(mutants)).expect("Serialize mutants")
}
