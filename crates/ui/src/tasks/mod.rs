//! Tasks: a Linear-style view of projects and issues, for this workspace's
//! own board and for what is pulled from Linear.

mod edit;
mod model;
mod sidebar;
mod view;
mod widgets;

pub use model::TasksModel;
pub use sidebar::TasksSidebar;
pub use view::TasksView;
pub(crate) use widgets::{avatar, short_date};
