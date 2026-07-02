//! Praxec Mission Control cockpit — library surface.
//!
//! Separate from the chat surface (`praxec-tui` / wisp). Mission Control
//! is the home: a two-mode (Build/Run) cockpit that reads the same gateway
//! affordance surface the model sees, so the human and the model share one
//! view of the legal next actions.

pub mod agent;
pub mod app;
pub mod chat_catalog;
pub mod gateway;
pub mod llm;
pub mod map;
pub mod mediator;
pub mod model;
pub mod nav;
pub mod op;
pub mod priorities;
pub mod snapshot;
pub mod theme;
pub mod ui;
pub mod view;
