//! The GitHub views for code, pull requests, issues and Actions runs.

mod edit;
mod model;
mod sidebar;
mod view;
mod widgets;

pub use model::GitHubModel;
pub use sidebar::GitHubSidebar;
pub use view::GitHubView;
