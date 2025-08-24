use super::serializer::SerializedTx;

pub trait Executor: Send + Sync {
    fn submit(&mut self, txs: Vec<SerializedTx>) -> anyhow::Result<()>;
}

/// For now just logs to stdout
pub struct LogExecutor;
impl Executor for LogExecutor {
    fn submit(&mut self, txs: Vec<SerializedTx>) -> anyhow::Result<()> {
        for tx in txs { println!("[EXEC] node={} bytes={}", tx.node, tx.bytes.len()); }
        Ok(())
    }
}
