//! MPC CLI JSON schema tests
//! Tests deserialization of bundler-js CLI output

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PlanOutput {
        chunk_count: u16,
        chunks: Vec<PlanChunk>,
    }

    #[derive(Debug, Deserialize)]
    struct PlanChunk {
        start: u16,
        end: u16,
    }

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

    #[test]
    fn test_cli_output_without_metrics() {
        let json = r#"{
            "ok": true,
            "plan": {
                "chunkCount": 1,
                "chunks": [{"start": 0, "end": 5}]
            },
            "offset": "123456789",
            "finalizeTx": "5jQQQ..."
        }"#;

        let output: CliOutput = serde_json::from_str(json).expect("Failed to parse CLI output");
        assert!(output.ok);
        assert!(output.plan.is_some());
        let plan = output.plan.unwrap();
        assert_eq!(plan.chunk_count, 1);
        assert_eq!(plan.chunks.len(), 1);
        assert_eq!(plan.chunks[0].start, 0);
        assert_eq!(plan.chunks[0].end, 5);
        assert!(output.metrics.is_none());
    }

    #[test]
    fn test_cli_output_with_metrics() {
        let json = r#"{
            "ok": true,
            "plan": {
                "chunkCount": 2,
                "chunks": [
                    {"start": 0, "end": 3},
                    {"start": 3, "end": 5}
                ]
            },
            "offset": "987654321",
            "finalizeTx": "7kRRR...",
            "metrics": {
                "encryptMs": 100,
                "queueMs": 200,
                "awaitMs": 3000,
                "decryptMs": 50,
                "totalMs": 3350
            }
        }"#;

        let output: CliOutput = serde_json::from_str(json).expect("Failed to parse CLI output");
        assert!(output.ok);
        let metrics = output.metrics.expect("Metrics should be present");
        assert_eq!(metrics.encrypt_ms, 100);
        assert_eq!(metrics.total_ms, 3350);
    }

    #[test]
    fn test_cli_output_error() {
        let json = r#"{
            "ok": false,
            "error": "MPC timeout"
        }"#;

        let output: CliOutput = serde_json::from_str(json).expect("Failed to parse error output");
        assert!(!output.ok);
        assert_eq!(output.error, Some("MPC timeout".to_string()));
    }

    #[test]
    fn test_plan_output_no_dropped_field() {
        // Ensure we correctly parse output WITHOUT the 'dropped' field
        let json = r#"{
            "chunkCount": 1,
            "chunks": [{"start": 0, "end": 10}]
        }"#;

        let plan: PlanOutput = serde_json::from_str(json).expect("Failed to parse PlanOutput");
        assert_eq!(plan.chunk_count, 1);
        assert_eq!(plan.chunks[0].start, 0);
        assert_eq!(plan.chunks[0].end, 10);
    }

    #[test]
    fn test_empty_chunks() {
        let json = r#"{
            "ok": true,
            "plan": {
                "chunkCount": 0,
                "chunks": []
            },
            "offset": "0",
            "finalizeTx": "sig"
        }"#;

        let output: CliOutput = serde_json::from_str(json).expect("Failed to parse empty chunks");
        assert!(output.ok);
        let plan = output.plan.unwrap();
        assert_eq!(plan.chunk_count, 0);
        assert_eq!(plan.chunks.len(), 0);
    }

    #[test]
    fn test_max_intents_chunks() {
        // Create JSON with 64 chunks (MAX_INTENTS)
        let chunks: Vec<String> = (0..64)
            .map(|i| format!(r#"{{"start": {}, "end": {}}}"#, i, i + 1))
            .collect();
        let chunks_json = chunks.join(",");
        
        let json = format!(
            r#"{{
                "ok": true,
                "plan": {{
                    "chunkCount": 64,
                    "chunks": [{}]
                }},
                "offset": "123",
                "finalizeTx": "sig"
            }}"#,
            chunks_json
        );

        let output: CliOutput = serde_json::from_str(&json).expect("Failed to parse 64 chunks");
        assert!(output.ok);
        let plan = output.plan.unwrap();
        assert_eq!(plan.chunk_count, 64);
        assert_eq!(plan.chunks.len(), 64);
    }

    #[test]
    fn test_cli_output_with_all_fields() {
        let json = r#"{
            "ok": true,
            "plan": {
                "chunkCount": 3,
                "chunks": [
                    {"start": 0, "end": 2},
                    {"start": 2, "end": 5},
                    {"start": 5, "end": 7}
                ]
            },
            "offset": "456789123",
            "finalizeTx": "9xTTT...",
            "metrics": {
                "encryptMs": 150,
                "queueMs": 250,
                "awaitMs": 2500,
                "decryptMs": 75,
                "totalMs": 2975
            }
        }"#;

        let output: CliOutput = serde_json::from_str(json).expect("Failed to parse full output");
        assert!(output.ok);
        assert!(output.plan.is_some());
        assert_eq!(output.offset, Some("456789123".to_string()));
        assert_eq!(output.finalize_tx, Some("9xTTT...".to_string()));
        assert!(output.metrics.is_some());
        
        let plan = output.plan.unwrap();
        assert_eq!(plan.chunk_count, 3);
        assert_eq!(plan.chunks.len(), 3);
    }
}
