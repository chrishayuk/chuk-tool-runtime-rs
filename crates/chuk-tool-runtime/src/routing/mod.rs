//! Multi-server routing: a first-wins [`ToolRegistry`] and a [`Router`] that
//! dispatches tool calls to the owning server's invoker. The `Router` is the
//! innermost invoker; the policy layers wrap it.

mod registry;
mod router;

pub use registry::ToolRegistry;
pub use router::{Router, NO_SERVER_PREFIX};
