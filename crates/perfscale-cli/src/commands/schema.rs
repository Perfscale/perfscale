//! `perfscale schema` — print the JSON Schema for test or config YAML.
//!
//! Prints the exact schema `perfscale lint` validates against, as pretty
//! JSON on stdout, so editors and agent tooling (e.g. the perfscale MCP
//! server) can consume it without depending on perfscale-core.

use perfscale_core::schema::{config_schema, test_schema};

use crate::cli::{SchemaArgs, SchemaDumpKind};
use crate::error::CliError;

pub async fn run(args: SchemaArgs) -> Result<(), CliError> {
    let schema = match args.kind {
        SchemaDumpKind::Test => test_schema(),
        SchemaDumpKind::Config => config_schema(),
    };
    // Schemas are generated in-process; serialization cannot fail.
    println!(
        "{}",
        serde_json::to_string_pretty(&schema).expect("schema serializes")
    );
    Ok(())
}
