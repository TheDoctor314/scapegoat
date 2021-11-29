mod types;
pub(crate) use types::{ElemRefIter, ElemRefVec};

#[cfg(test)]
mod test;

mod arena;
#[cfg(fuzzing)]
pub use arena::NodeArena;

mod node;
#[cfg(fuzzing)]
pub use node::{Node, NodeGetHelper, NodeRebuildHelper};

mod iter;
pub use iter::{IntoIter, Iter, IterMut};

mod error;
pub use error::SGErr;

#[allow(clippy::module_inception)]
mod tree;
pub use tree::SGTree;

// TODO: Within this module, rename `I` to `U` and `C` to `N`, will be more clear!