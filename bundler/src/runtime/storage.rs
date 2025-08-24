use crate::occ::OccSnapshot;

#[derive(Default)]
pub struct MemoryStore { pub occ_snaps: Vec<OccSnapshot> }
impl MemoryStore { pub fn record_snapshot(&mut self, s: OccSnapshot) { self.occ_snaps.push(s); } }
