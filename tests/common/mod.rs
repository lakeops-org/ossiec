//! Shared by the correctness tier (DuckDB) and the engine tier (Trino,
//! StarRocks, ClickHouse, Postgres, MySQL): the shop seed data, the request
//! cases with their reference SQL, and result normalization.
#![allow(dead_code)]

mod cases;
mod duck;
mod engines;
mod layer2_cases;
mod shop;
mod site;

// Each test crate uses part of this module, so a re-export can go unused in it.
#[allow(unused_imports)]
pub use cases::*;
#[allow(unused_imports)]
pub use duck::*;
#[allow(unused_imports)]
pub use engines::*;
#[allow(unused_imports)]
pub use layer2_cases::*;
#[allow(unused_imports)]
pub use shop::*;
#[allow(unused_imports)]
pub use site::*;
