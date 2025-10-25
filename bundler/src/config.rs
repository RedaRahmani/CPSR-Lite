use std::env;
use serde::{Deserialize, Serialize};

/// Configuration for optional Arcium MPC privacy layer integration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArciumConfig {
    /// Enable MPC pre-chunking via Arcium confidential planner
    pub enabled: bool,
    
    /// Arcium program ID (confidential_planner deployment)
    pub program_id: String,
    
    /// RPC endpoint for Arcium operations
    pub rpc_url: String,
    
    /// Cluster identifier (devnet, testnet, mainnet-beta)
    pub cluster: String,
    
    /// Timeout for MPC computation in milliseconds
    pub timeout_ms: u64,
    
    /// Path to bundler-js CLI executable
    pub bundler_js_path: String,
}

impl ArciumConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let enabled = env::var("ARCIUM_ENABLED")
            .unwrap_or_else(|_| "false".to_string())
            .parse::<bool>()
            .unwrap_or(false);
        
        let program_id = env::var("ARCIUM_PROGRAM_ID")
            .unwrap_or_else(|_| "HXMYdSHXbovCAetoNR3Sju42rbfXXEBEWRLRTit1onaQ".to_string());
        
        let rpc_url = env::var("ARCIUM_RPC_URL")
            .unwrap_or_else(|_| "https://api.devnet.solana.com".to_string());
        
        let cluster = env::var("ARCIUM_CLUSTER")
            .unwrap_or_else(|_| "devnet".to_string());
        
        let timeout_ms = env::var("ARCIUM_TIMEOUT_MS")
            .unwrap_or_else(|_| "5000".to_string())
            .parse::<u64>()
            .unwrap_or(5000);
        
        let bundler_js_path = env::var("ARCIUM_BUNDLER_JS_PATH")
            .unwrap_or_else(|_| "node bundler-js/dist/cli.js".to_string());
        
        Self {
            enabled,
            program_id,
            rpc_url,
            cluster,
            timeout_ms,
            bundler_js_path,
        }
    }
    
    /// Create a disabled configuration (default)
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            program_id: "HXMYdSHXbovCAetoNR3Sju42rbfXXEBEWRLRTit1onaQ".to_string(),
            rpc_url: "https://api.devnet.solana.com".to_string(),
            cluster: "devnet".to_string(),
            timeout_ms: 5000,
            bundler_js_path: "node bundler-js/dist/cli.js".to_string(),
        }
    }
}

impl Default for ArciumConfig {
    fn default() -> Self {
        Self::from_env()
    }
}
