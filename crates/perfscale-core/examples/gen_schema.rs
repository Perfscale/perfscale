//! Regenerate `schema/*.json` at the repo root.
//!
//! Run after changing `TestDef`, `Step`, `RunConfig`, or `ConfigFile`:
//! `cargo run -p perfscale-core --example gen_schema`

use std::fs;

fn main() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schema");
    fs::create_dir_all(root).expect("create schema/ dir");

    let test_schema = serde_json::to_string_pretty(&perfscale_core::schema::test_schema()).unwrap();
    let config_schema =
        serde_json::to_string_pretty(&perfscale_core::schema::config_schema()).unwrap();

    fs::write(format!("{root}/test.schema.json"), test_schema).expect("write test.schema.json");
    fs::write(format!("{root}/config.schema.json"), config_schema)
        .expect("write config.schema.json");

    println!("wrote {root}/test.schema.json and {root}/config.schema.json");
}
