use crate::Graphs;
use std::fmt::{self, Debug, Display, Formatter};

/// Information generated from the PDG proper that is queried by static analysis.
///
/// Eventually this will include information about what kinds of [`Node`]s the [`Node`] is an ancestor of,
/// as well as its ability to be used as a `&mut`.
///
/// [`Node`]: crate::graph::Node
#[derive(Hash, Clone, PartialEq, Eq, Debug)]
pub struct NodeInfo {}

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "")
    }
}

/// Initialize [`Node::info`] for each [`Node`].
///
/// For now, this is empty, because the current [`NodeInfo`] is empty.
///
/// [`Node`]: crate::graph::Node
/// [`Node::info`]: crate::graph::Node::info
pub fn add_info(pdg: &mut Graphs) {
    for g in &mut pdg.graphs {
        for mut node in &mut g.nodes {
            node.info = Some(NodeInfo {});
        }
    }
}
