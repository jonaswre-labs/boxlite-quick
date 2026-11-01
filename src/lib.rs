//! boxq: run an Agent against a Workspace inside an isolated Sandbox.

pub mod auth;
pub mod claude;
pub mod claude_config;
pub mod config;
mod digest;
pub mod egress;
pub mod log;
pub mod reap;
pub mod runtime;
pub mod sandbox;
pub mod session;
pub mod setup;
pub mod spec;
mod workspace;

pub use workspace::Workspace;
