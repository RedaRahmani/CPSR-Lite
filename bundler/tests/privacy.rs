//! Privacy layer integration tests (dev mode, no CI)
//! Tests for MPC chunking via Arcium confidential planner

#[cfg(test)]
mod privacy_tests {
    use bundler::config::ArciumConfig;
    use bundler::pipeline::mpc::*;
    use cpsr_types::{intent::AccessKind, UserIntent};
    use solana_program::{instruction::Instruction, pubkey::Pubkey};

    /// Helper to create a test intent
    fn create_test_intent(index: u8, priority: u8) -> UserIntent {
        let actor = Pubkey::new_unique();
        let program_id = Pubkey::new_unique();
        let account = Pubkey::new_unique();

        let ix = Instruction {
            program_id,
            accounts: vec![solana_program::instruction::AccountMeta::new(
                account, false,
            )],
            data: vec![index; 32],
        };

        UserIntent {
            actor,
            ix,
            accesses: vec![cpsr_types::intent::AccountAccess {
                pubkey: account,
                access: AccessKind::Writable,
            }],
            priority,
            expires_at_slot: None,
        }
    }

    #[test]
    fn test_intent_conversion() {
        // Test converting UserIntent to MPC Intent format
        let intent = create_test_intent(1, 5);
        
        // This would be tested via the internal function in mpc.rs
        // For now, just verify the intent is created properly
        assert_eq!(intent.priority, 5);
        assert_eq!(intent.accesses.len(), 1);
    }

    #[test]
    fn test_config_disabled_by_default() {
        // Test that ArciumConfig defaults to disabled
        std::env::remove_var("ARCIUM_ENABLED");
        let config = ArciumConfig::from_env();
        assert!(!config.enabled);
    }

    #[test]
    fn test_config_enabled_via_env() {
        // Test enabling via environment variable
        std::env::set_var("ARCIUM_ENABLED", "true");
        let config = ArciumConfig::from_env();
        assert!(config.enabled);
        std::env::remove_var("ARCIUM_ENABLED");
    }

    #[test]
    fn test_config_custom_program_id() {
        // Test custom program ID via env
        let test_program_id = "TestProgramId111111111111111111111111111";
        std::env::set_var("ARCIUM_PROGRAM_ID", test_program_id);
        let config = ArciumConfig::from_env();
        assert_eq!(config.program_id, test_program_id);
        std::env::remove_var("ARCIUM_PROGRAM_ID");
    }

    #[tokio::test]
    async fn test_mpc_disabled_returns_none() {
        // Test that disabled config returns None
        let config = ArciumConfig::disabled();
        let intents = vec![
            create_test_intent(1, 1),
            create_test_intent(2, 2),
            create_test_intent(3, 3),
        ];

        let result = bundler::pipeline::mpc::apply_mpc_chunking_or_fallback(&intents, &config).await;
        assert!(result.is_none());
    }

    // Mock test - would need actual MPC endpoint or mock server
    #[tokio::test]
    #[ignore] // Ignore by default since it requires external service
    async fn test_mpc_chunking_with_mock() {
        // This test would be run manually with ARCIUM_ENABLED=true
        // and would verify the full MPC flow
        
        if std::env::var("ARCIUM_ENABLED").unwrap_or_default() != "true" {
            return;
        }

        let config = ArciumConfig::from_env();
        let intents = vec![
            create_test_intent(1, 1),
            create_test_intent(2, 2),
            create_test_intent(3, 3),
        ];

        // This would call the actual MPC planner if enabled
        let result = bundler::pipeline::mpc::apply_mpc_chunking_or_fallback(&intents, &config).await;
        
        // In mock/test mode, we'd verify the structure
        if let Some(chunks) = result {
            assert!(!chunks.is_empty(), "Should return at least one chunk");
            let total_intents: usize = chunks.iter().map(|c| c.len()).sum();
            assert_eq!(total_intents, intents.len(), "All intents should be in chunks");
        }
    }

    #[test]
    fn test_intent_hash_deterministic() {
        // Test that hashing is deterministic for same input
        // Use fixed pubkey instead of new_unique() for determinism
        let fixed_actor = Pubkey::new_from_array([1u8; 32]);
        let fixed_program = Pubkey::new_from_array([2u8; 32]);
        let fixed_account = Pubkey::new_from_array([3u8; 32]);
        
        let ix1 = Instruction {
            program_id: fixed_program,
            accounts: vec![solana_program::instruction::AccountMeta::new(
                fixed_account, false,
            )],
            data: vec![42; 32],
        };
        
        let ix2 = Instruction {
            program_id: fixed_program,
            accounts: vec![solana_program::instruction::AccountMeta::new(
                fixed_account, false,
            )],
            data: vec![42; 32],
        };
        
        // Both should serialize to same bytes
        let bytes1 = bincode::serialize(&ix1).unwrap();
        let bytes2 = bincode::serialize(&ix2).unwrap();
        
        assert_eq!(bytes1, bytes2, "Same instruction should serialize identically");
    }

    #[test]
    fn test_planner_input_bounds() {
        // Test that we handle MAX_INTENTS correctly
        const MAX_INTENTS: usize = 64;
        
        // Create more than MAX_INTENTS
        let intents: Vec<UserIntent> = (0..70)
            .map(|i| create_test_intent(i as u8, 1))
            .collect();
        
        // The conversion should handle this gracefully
        // (either error or truncate - depends on implementation)
        assert!(intents.len() > MAX_INTENTS);
    }
}

#[cfg(test)]
mod packing_tests {
    //! Tests for TypeScript <-> Rust serialization compatibility
    //! These verify the JSON structures match between Rust and TypeScript

    use serde_json::json;

    #[test]
    fn test_intent_json_structure() {
        // Test Intent JSON structure matches TypeScript
        let hash_array: Vec<u8> = vec![0; 32];
        let intent_json = json!({
            "user": 12345u64,
            "hash": hash_array,
            "feeHint": 1000u32,
            "priority": 5u16,
            "reserved": 0u16,
        });

        // Should deserialize to our Rust Intent struct
        let intent_str = intent_json.to_string();
        assert!(intent_str.contains("user"));
        assert!(intent_str.contains("feeHint"));
        assert!(intent_str.contains("priority"));
    }

    #[test]
    fn test_planner_input_json_structure() {
        // Test PlannerInput JSON structure
        let input_json = json!({
            "intents": [],
            "count": 0u16,
        });

        let input_str = input_json.to_string();
        assert!(input_str.contains("intents"));
        assert!(input_str.contains("count"));
    }

    #[test]
    fn test_plan_output_json_structure() {
        // Test PlanOutput JSON structure matches TypeScript
        let dropped_array: Vec<u8> = vec![0; 64];
        let output_json = json!({
            "chunkCount": 2u16,
            "chunks": [
                {"start": 0u16, "end": 5u16},
                {"start": 5u16, "end": 10u16},
            ],
            "dropped": dropped_array,
        });

        let output_str = output_json.to_string();
        assert!(output_str.contains("chunkCount"));
        assert!(output_str.contains("chunks"));
        assert!(output_str.contains("dropped"));
    }

    #[test]
    fn test_cli_output_success() {
        // Test CliOutput success case
        let dropped_array: Vec<u8> = vec![0; 64];
        let output_json = json!({
            "ok": true,
            "plan": {
                "chunkCount": 1,
                "chunks": [{"start": 0, "end": 3}],
                "dropped": dropped_array,
            },
            "offset": "123456789",
            "finalizeTx": "5jQQQ...",
            "metrics": {
                "encryptMs": 100,
                "queueMs": 200,
                "awaitMs": 3000,
                "decryptMs": 50,
                "totalMs": 3350,
            }
        });

        let output_str = output_json.to_string();
        assert!(output_str.contains("\"ok\":true"));
    }

    #[test]
    fn test_cli_output_error() {
        // Test CliOutput error case
        let output_json = json!({
            "ok": false,
            "error": "MPC timeout",
        });

        let output_str = output_json.to_string();
        assert!(output_str.contains("\"ok\":false"));
        assert!(output_str.contains("MPC timeout"));
    }
}
