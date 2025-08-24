use cpsr_types::UserIntent;

pub trait IntentSource: Send + Sync {
    fn fetch_batch(&mut self, max: usize) -> anyhow::Result<Vec<UserIntent>>;
}

/// Simple in‑memory source (great for demos/tests)
pub struct VecSource { items: Vec<UserIntent> }
impl VecSource { pub fn new(items: Vec<UserIntent>) -> Self { Self { items } } }
impl IntentSource for VecSource {
    fn fetch_batch(&mut self, max: usize) -> anyhow::Result<Vec<UserIntent>> {
        let take = max.min(self.items.len());
        let keep = self.items.len().saturating_sub(take);
        Ok(self.items.split_off(keep)) // returns last `take` items
    }
}
