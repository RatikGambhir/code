//! GitHub: sign in, then browse your repositories' code, pull requests,
//! issues and Actions runs.

mod model;
mod sidebar;
mod view;
mod widgets;

pub use model::GitHubModel;
pub use sidebar::GitHubSidebar;
pub use view::GitHubView;
