use crate::occ::{capture_occ_with_retries, AccountFetcher, OccConfig, OccSnapshot};
use cpsr_types::UserIntent;
use crate::dag::NodeId;

pub fn capture_layer_snapshot<F: AccountFetcher>(
    fetcher: &F,
    nodes: &[UserIntent],
    layer: &[NodeId],
    cfg: &OccConfig,
) -> anyhow::Result<OccSnapshot> {
    let planned: Vec<_> = layer.iter().map(|&id| nodes[id as usize].clone()).collect();
    Ok(capture_occ_with_retries(fetcher, &planned, cfg)?)
}
