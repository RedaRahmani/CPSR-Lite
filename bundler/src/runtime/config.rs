use std::time::Duration;
use crate::occ::OccConfig;

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub poll_interval: Duration,
    pub max_batch: usize,
    pub occ: OccConfig,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(300),
            max_batch: 64,
            occ: OccConfig::default(),
        }
    }
}
