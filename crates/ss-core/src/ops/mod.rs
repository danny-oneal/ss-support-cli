//! The operations layer: the lenient [`Input`] every front-end produces, and
//! the closed [`View`] union operations return for renderers to visit.

mod input;
mod view;

pub use input::Input;
pub use view::{Node, OpResult, View};
