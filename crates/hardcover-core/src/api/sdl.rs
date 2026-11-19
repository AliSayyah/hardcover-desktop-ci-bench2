//! Validates GraphQL documents against Hardcover's schema with apollo-compiler.
//!
//! Shared by the unit tests (against the committed `schema/hardcover.graphql`)
//! and the `api_check` example (against a freshly introspected schema), which
//! includes this file with `#[path]` because apollo-compiler is a dev-dependency.

#![allow(dead_code)]

use apollo_compiler::validation::Valid;
use apollo_compiler::{ExecutableDocument, Schema};

/// The committed schema, parsed once.
pub fn schema() -> &'static Valid<Schema> {
    static SCHEMA: std::sync::OnceLock<Valid<Schema>> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        parse(include_str!("../../../../schema/hardcover.graphql"))
            .unwrap_or_else(|e| panic!("schema/hardcover.graphql is invalid:\n{e}"))
    })
}

/// Parses SDL in the format of `schema/hardcover.graphql`, adapting it for
/// apollo-compiler: the introspected SDL re-declares the built-in scalars and
/// names its roots `query_root`/`mutation_root`/`subscription_root`.
pub fn parse(raw: &str) -> Result<Valid<Schema>, String> {
    let mut sdl: String = raw
        .lines()
        .filter(|l| {
            !matches!(
                l.trim(),
                "scalar Boolean" | "scalar Float" | "scalar Int" | "scalar String" | "scalar ID"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    sdl.push_str(
        "\nschema { query: query_root mutation: mutation_root subscription: subscription_root }\n",
    );
    Schema::parse_and_validate(sdl, "hardcover.graphql").map_err(|e| e.errors.to_string())
}

pub fn validate(schema: &Valid<Schema>, name: &str, doc: &str) -> Result<(), String> {
    ExecutableDocument::parse_and_validate(schema, doc, format!("{name}.graphql"))
        .map(|_| ())
        .map_err(|e| e.errors.to_string())
}

/// Names of the top-level fields a document selects (one per document in
/// practice), for checking them against the OAuth scope map.
pub fn top_level_fields(schema: &Valid<Schema>, name: &str, doc: &str) -> Vec<String> {
    let Ok(doc) = ExecutableDocument::parse_and_validate(schema, doc, format!("{name}.graphql"))
    else {
        return Vec::new();
    };
    doc.operations
        .iter()
        .flat_map(|op| op.selection_set.selections.iter())
        .filter_map(|s| s.as_field().map(|f| f.name.to_string()))
        .collect()
}
