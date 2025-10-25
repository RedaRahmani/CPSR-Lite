/// E2E Integration test for Arcium MPC planner
/// Requires: ARCIUM_ENABLED=true and devnet deployment
/// Run with: cargo test --test mpc_e2e test_mpc_e2e_integration -- --ignored --nocapture

use bundler::config::ArciumConfig;
use bundler::pipeline::mpc::call_mpc_planner;
use cpsr_types::UserIntent;
use solana_sdk::{pubkey::Pubkey, system_instruction};

#[tokio::test]
#[ignore] // Requires deployed program on devnet
async fn test_mpc_e2e_integration() {
    // Read config from environment
    let config = ArciumConfig {
        enabled: std::env::var("ARCIUM_ENABLED").unwrap_or_default() == "true",
        program_id: std::env::var("ARCIUM_PROGRAM_ID")
            .unwrap_or_else(|_| "GvrPsn6Shsuwf7d5CYe5HSXc1kWKFay26g9TG7GoZ6Jz".to_string()),
        rpc_url: std::env::var("ARCIUM_RPC_URL")
            .unwrap_or_else(|_| "https://api.devnet.solana.com".to_string()),
        cluster: std::env::var("ARCIUM_CLUSTER")
            .unwrap_or_else(|_| "devnet".to_string()),
        bundler_js_path: "node bundler-js/dist/cli.js".to_string(),
        timeout_ms: std::env::var("ARCIUM_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30000),
    };

    assert!(config.enabled, "ARCIUM_ENABLED must be set to 'true'");

    println!("=== E2E MPC Integration Test ===");
    println!("Program ID: {}", config.program_id);
    println!("RPC URL: {}", config.rpc_url);
    println!("Cluster: {}", config.cluster);
    println!("Timeout: {}ms", config.timeout_ms);
    println!();

    // Create 5 test intents (matching privacy test)
    let mut intents = Vec::new();
    for i in 0..5 {
        let from = Pubkey::new_unique();
        let to = Pubkey::new_unique();
        let amount = (i + 1) as u64 * 1_000_000;
        
        let ix = system_instruction::transfer(&from, &to, amount);
        let intent = UserIntent {
            actor: from,
            ix: ix.clone(),
            accesses: UserIntent::accesses_from_ix(&ix),
            priority: ((i + 1) % 256) as u8,
            expires_at_slot: None,
        };
        
        intents.push(intent);
        println!("Intent {}: priority={}, amount={}", i, (i + 1) % 256, amount);
    }
    println!();

    // Call MPC planner
    println!("Calling MPC planner with {} intents...", intents.len());
    let start = std::time::Instant::now();
    
    let result = call_mpc_planner(&intents, &config)
        .await
        .expect("MPC planner call failed");
    
    let elapsed = start.elapsed();
    println!("MPC call completed in {:?}", elapsed);
    println!();

    // Verify result
    println!("=== Chunking Plan ===");
    println!("Chunks returned: {}", result.len());
    for (i, chunk) in result.iter().enumerate() {
        println!("Chunk {}: start={}, end={}", i, chunk.0, chunk.1);
    }
    println!();

    // Basic validation
    assert!(!result.is_empty(), "Should return at least one chunk");
    
    // Verify chunks cover all intents
    let covered: Vec<usize> = result.iter().flat_map(|(s, e)| *s..*e).collect();
    println!("Covered intent indices: {:?}", covered);
    
    // All intents should be covered
    for i in 0..intents.len() {
        assert!(
            covered.contains(&i),
            "Intent {} not covered in chunking plan",
            i
        );
    }

    println!("✅ E2E test passed!");
}
