//! `kind: agent` executor — autonomous in-process sub-agents as in-step
//! "intelligent hands" (SPEC §33.x; plan: okay-the-skill-executor-joyful-falcon).
//!
//! This crate holds **agent logic only** — no storage engine. The agent runs
//! in-process on the rig runner (no praxec write handle, no subprocess spawn),
//! returns a structured result via the schema-enforced `final_answer` contract,
//! and the runtime projects it through the step's existing `output:` mapping.
//! Core stays a plain governed substrate; all autonomy lives here.

pub mod breaker;
pub mod config;
pub mod config_doctor;
pub mod error;
pub mod executor;
pub mod file_tools;
pub mod orchestrator;
pub mod park;
pub mod rig_runner;
pub mod session;
pub mod spill;
pub mod tool_budget;
