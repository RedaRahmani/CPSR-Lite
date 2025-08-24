use crate::dag::{Dag, NodeId};
use cpsr_types::UserIntent;

pub struct Plan { pub dag: Dag, pub layers: Vec<Vec<NodeId>> }

pub fn plan(intents: Vec<UserIntent>) -> anyhow::Result<Plan> {
    let dag = Dag::build(intents);
    let layers = dag.layers()?;
    Ok(Plan { dag, layers })
}
