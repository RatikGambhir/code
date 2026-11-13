//! GitHub for the GitHub view: signing in (browser device flow, the GitHub
//! CLI's login, or a pasted token), keeping the token, and a REST
//! client. Plain data and blocking I/O; callers run the I/O off the UI thread.

pub mod api;
pub mod auth;
pub mod model;
pub mod token;
pub mod write;

pub use api::Client;
pub use model::*;
