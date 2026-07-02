//! perfscale-core — generic load-testing engine.
//!
//! Three ways to run a load test, unified behind [`runner::execute`]:
//! - `k6` — shells out to an existing `k6` installation
//! - `locust` — shells out to an existing `locust` installation
//! - native steps — pure-Rust step engine (`std/http`, `std/check`, `std/sleep`, `std/log`)
//!
//! Test/config files are plain YAML, deserialized straight into [`step::TestDef`]
//! and [`yaml::ConfigFile`] via `serde`.

pub mod lint;
pub mod models;
pub mod runner;
pub mod schema;
pub mod step;
pub mod yaml;
