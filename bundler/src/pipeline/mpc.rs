use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::ArciumConfig;
use cpsr_types::UserIntent;

/// Input JSON structure for bundler-js CLI
#[derive(Debug, Serialize)]
struct CliInput {
    #[serde(rename = "rpcUrl")]
    rpc_url: String,
    #[serde(rename = "programId")]
    program_id: String,
    cluster: String,
    dag: PlannerInput,
    #[serde(rename = "timeoutMs")]
    timeout_ms: u64,
}

/// Output JSON structure from bundler-js CLI
#[derive(Debug, Deserialize)]
struct CliOutput {
    ok: bool,
    plan: Option<PlanOutput>,
    offset: Option<String>,
    #[serde(rename = "finalizeTx")]
    finalize_tx: Option<String>,
    error: Option<String>,
    metrics: Option<Metrics>,
}

#[derive(Debug, Deserialize)]
struct Metrics {
    #[serde(rename = "encryptMs")]
    encrypt_ms: u64,
    #[serde(rename = "queueMs")]
    queue_ms: u64,
    #[serde(rename = "awaitMs")]
    await_ms: u64,
    #[serde(rename = "decryptMs")]
    decrypt_ms: u64,
    #[serde(rename = "totalMs")]
    total_ms: u64,
}

/// Intent structure matching TypeScript Intent
#[derive(Debug, Serialize)]
struct Intent {
    user: u64,
    hash: [u8; 32],
    #[serde(rename = "feeHint")]
    fee_hint: u32,
    priority: u16,
    reserved: u16,
}

/// Planner input matching TypeScript PlannerInput
#[derive(Debug, Serialize)]
struct PlannerInput {
    intents: Vec<Intent>,
    count: u16,
}

/// Plan chunk structure matching TypeScript PlanChunk
#[derive(Debug, Deserialize)]
struct PlanChunk {
    start: u16,
    end: u16,
}

/// Plan output matching TypeScript PlanOutput
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanOutput {
    chunk_count: u16,
    chunks: Vec<PlanChunk>,
}

/// Convert UserIntent to MPC Intent format
fn user_intent_to_mpc_intent(intent: &UserIntent, index: usize) -> Intent {
    // Hash the intent instruction data for privacy
    let serialized = bincode::serialize(&intent.ix).unwrap_or_default();
    let hash_bytes = cpsr_types::hash::dhash(b"MPC_INTENT", &[&serialized]);
    
    Intent {
        user: index as u64, // Use index as user ID for dev mode
        hash: hash_bytes,
        fee_hint: 1000, // Default fee hint
        priority: intent.priority as u16,
        reserved: 0,
    }
}

/// Call Arcium MPC planner via bundler-js CLI
pub async fn call_mpc_planner(
    intents: &[UserIntent],
    config: &ArciumConfig,
) -> Result<Vec<(usize, usize)>> {
    let start = std::time::Instant::now();
    
    info!(
        target: "mpc",
        intent_count = intents.len(),
        cluster = %config.cluster,
        program_id = %config.program_id,
        "Starting Arcium MPC pre-chunking"
    );
    
    eprintln!("[MPC] ⚙️  Starting MPC chunking for {} intents", intents.len());
    eprintln!("[MPC] 🌐 Cluster: {} (offset: check config)", config.cluster);
    eprintln!("[MPC] 📍 Program: {}", config.program_id);

    // Convert intents to MPC format
    let mpc_intents: Vec<Intent> = intents
        .iter()
        .enumerate()
        .map(|(i, intent)| user_intent_to_mpc_intent(intent, i))
        .collect();

    let planner_input = PlannerInput {
        intents: mpc_intents,
        count: intents.len() as u16,
    };

    let cli_input = CliInput {
        rpc_url: config.rpc_url.clone(),
        program_id: config.program_id.clone(),
        cluster: config.cluster.clone(),
        dag: planner_input,
        timeout_ms: config.timeout_ms,
    };

    let input_json = serde_json::to_string(&cli_input)
        .context("Failed to serialize MPC input")?;

    debug!(
        target: "mpc",
        input_json_len = input_json.len(),
        nonce_endianness = "LE",
        chosen_option = "A",
        "Serialized MPC input JSON"
    );

    // Parse bundler-js path (supports compiled JS or source with npx)
    let parts: Vec<&str> = config.bundler_js_path.split_whitespace().collect();
    let (program, args) = if parts.is_empty() {
        return Err(anyhow!("Empty bundler_js_path in config"));
    } else {
        (parts[0], &parts[1..])
    };

    eprintln!("[MPC DEBUG] Spawning CLI: {} {:?}", program, args);
    eprintln!("[MPC DEBUG] Full Input JSON:\n{}", input_json);

    // Determine workspace root (go up from bundler/ to workspace root)
    let workspace_root = std::env::current_dir()
        .ok()
        .and_then(|d| {
            // If we're in bundler/, go up one level
            if d.ends_with("bundler") {
                d.parent().map(|p| p.to_path_buf())
            } else {
                Some(d)
            }
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    eprintln!("[MPC DEBUG] Working directory: {:?}", workspace_root);

    // Spawn bundler-js CLI process
    let mut child = Command::new(program)
        .args(args)
        .current_dir(&workspace_root)  // Set working directory
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context(format!(
            "Failed to spawn bundler-js CLI process: {} {}. \
            Make sure bundler-js is built (cd bundler-js && npm run build) or \
            dependencies are installed (npm install) if using npx ts-node",
            program,
            args.join(" ")
        ))?;

    // Write input JSON to stdin
    let mut stdin = child.stdin.take().expect("Failed to get stdin");
    stdin
        .write_all(input_json.as_bytes())
        .await
        .context("Failed to write to bundler-js stdin")?;
    stdin
        .shutdown()
        .await
        .context("Failed to close bundler-js stdin")?;

    // Read output
    let output = child
        .wait_with_output()
        .await
        .context("Failed to wait for bundler-js process")?;

    eprintln!("[MPC] ✅ CLI exited with status: {:?}", output.status);
    eprintln!("[MPC] 📊 Output sizes - stdout: {} bytes, stderr: {} bytes", 
        output.stdout.len(), output.stderr.len());

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    if !stderr_str.is_empty() {
        eprintln!("[MPC] 📋 CLI stderr output:");
        eprintln!("{}", stderr_str);
        debug!(target: "mpc", stderr = %stderr_str, "bundler-js stderr");
    }

    if !output.status.success() {
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        eprintln!("[MPC] ❌ CLI stdout (error case):");
        eprintln!("{}", stdout_str);
        
        // Check for common MPC worker issues
        if stderr_str.contains("timeout") || stdout_str.contains("timeout") {
            eprintln!("[MPC] ⚠️  TIMEOUT: No MPC workers responded");
            eprintln!("[MPC] 💡 Possible causes:");
            eprintln!("[MPC]    1. No ARX nodes running for this cluster offset");
            eprintln!("[MPC]    2. Cluster offset mismatch");
            eprintln!("[MPC]    3. Network connectivity issues");
            eprintln!("[MPC] 🔧 Solutions:");
            eprintln!("[MPC]    - Run: bash bundler-js/scripts/setup_arx_node.sh");
            eprintln!("[MPC]    - Verify: arcium arx-active");
            eprintln!("[MPC]    - Check logs: tail -f ~/.arcium/logs/arx.log");
        }
        
        warn!(
            target: "mpc",
            exit_code = ?output.status.code(),
            stderr_len = stderr_str.len(),
            stdout_len = stdout_str.len(),
            stderr = %stderr_str,
            stdout = %stdout_str,
            "bundler-js CLI failed"
        );
        return Err(anyhow!(
            "bundler-js CLI failed with exit code {:?}:\nstderr: {}\nstdout: {}",
            output.status.code(),
            stderr_str,
            stdout_str
        ));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let cli_output: CliOutput = serde_json::from_str(&stdout_str)
        .context(format!("Failed to parse bundler-js output: {}", stdout_str))?;

    if !cli_output.ok {
        let error_msg = cli_output.error.unwrap_or_else(|| "unknown".to_string());
        eprintln!("[MPC] ❌ MPC planner error: {}", error_msg);
        return Err(anyhow!("MPC planner returned error: {}", error_msg));
    }

    let plan = cli_output
        .plan
        .ok_or_else(|| anyhow!("MPC planner returned no plan"))?;

    let metrics = cli_output.metrics.as_ref();
    let elapsed = start.elapsed();
    
    eprintln!("[MPC] ✅ MPC computation completed successfully!");
    eprintln!("[MPC] 📦 Chunks returned: {}", plan.chunk_count);
    if let Some(m) = metrics {
        eprintln!("[MPC] ⏱️  Timing breakdown:");
        eprintln!("[MPC]    - Encrypt: {}ms", m.encrypt_ms);
        eprintln!("[MPC]    - Queue:   {}ms", m.queue_ms);
        eprintln!("[MPC]    - Await:   {}ms", m.await_ms);
        eprintln!("[MPC]    - Decrypt: {}ms", m.decrypt_ms);
        eprintln!("[MPC]    - Total:   {}ms", m.total_ms);
    }

    info!(
        target: "mpc",
        input_intents = intents.len(),
        output_chunk_count = plan.chunk_count,
        encrypt_ms = metrics.map(|m| m.encrypt_ms).unwrap_or(0),
        queue_ms = metrics.map(|m| m.queue_ms).unwrap_or(0),
        await_ms = metrics.map(|m| m.await_ms).unwrap_or(0),
        decrypt_ms = metrics.map(|m| m.decrypt_ms).unwrap_or(0),
        total_ms = metrics.map(|m| m.total_ms).unwrap_or(0),
        elapsed_ms = elapsed.as_millis() as u64,
        nonce_endianness = "LE",
        "Arcium MPC pre-chunking completed"
    );

    // Convert chunks to (start, end) tuples
    let mut chunk_ranges = Vec::new();
    for chunk in &plan.chunks[0..plan.chunk_count as usize] {
        debug!(
            target: "mpc",
            start = chunk.start,
            end = chunk.end,
            "MPC chunk"
        );
        chunk_ranges.push((chunk.start as usize, chunk.end as usize));
    }

    Ok(chunk_ranges)
}

/// Apply MPC chunks to intents, or fallback on error
pub async fn apply_mpc_chunking_or_fallback(
    intents: &[UserIntent],
    config: &ArciumConfig,
) -> Option<Vec<Vec<UserIntent>>> {
    if !config.enabled {
        return None;
    }

    match call_mpc_planner(intents, config).await {
        Ok(chunk_ranges) => {
            // Convert ranges to actual intent chunks
            let mut chunks = Vec::new();
            for (start, end) in chunk_ranges {
                if start >= intents.len() || end > intents.len() || start > end {
                    warn!(
                        target: "mpc",
                        start,
                        end,
                        total = intents.len(),
                        "Invalid MPC chunk range, falling back to OCC"
                    );
                    return None;
                }
                chunks.push(intents[start..end].to_vec());
            }
            
            info!(
                target: "mpc",
                chunks_count = chunks.len(),
                "MPC chunking applied successfully"
            );
            Some(chunks)
        }
        Err(err) => {
            warn!(
                target: "mpc",
                error = %err,
                "MPC planner failed, falling back to plain OCC/chunking"
            );
            None
        }
    }
}
