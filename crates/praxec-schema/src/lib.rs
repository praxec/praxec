//! Typify-generated types for praxec JSON schemas.
//!
//! The schemas in `/schemas` are the source of truth. Build-time generation
//! produces strongly-typed views over the gateway config and workflow response
//! shapes. The runtime in `praxec-core` operates on `serde_json::Value`
//! for flexibility; these types are convenience for callers that want them.

#![allow(clippy::all)]
#![allow(dead_code)]
#![allow(unused_imports)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

pub use serde_json;

include!(concat!(env!("OUT_DIR"), "/types.rs"));
