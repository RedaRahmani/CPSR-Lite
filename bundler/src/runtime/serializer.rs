use crate::dag::{Dag, NodeId};

#[derive(Debug, Clone)]
pub struct SerializedTx { pub node: NodeId, pub bytes: Vec<u8> }

pub fn serialize_chunk(dag: &Dag, chunk: &[NodeId]) -> anyhow::Result<Vec<SerializedTx>> {
    Ok(chunk.iter().map(|&nid| {
        let ui = &dag.nodes[nid as usize];
        SerializedTx { node: nid, bytes: ui.ix.data.clone() }
    }).collect())
}
