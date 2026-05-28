// Legacy in-process CLI helpers from phases 0~1. The daemon path uses a
// subset (page::navigate_existing, eval::evaluate_value, ax::traverse_visible_nodes
// via Phase 2 handlers); the rest will get pulled back in as Tier 3~5 daemon
// actions land in phase 3c~3e. Silence dead-code warnings for the whole group
// rather than annotating each function individually.
#![allow(dead_code)]

pub mod ax;
pub mod eval;
pub mod fetch_text;
pub mod find_all;
pub mod get_text;
pub mod navigate;
pub mod page;
pub mod query_all;
