// Included by mcp.rs's test module.  MCP transport is exercised with the
// real handler; controlled timestamps are used only for the core sweep because
// the production lifecycle worker correctly owns wall-clock scheduling.
mod retention_delivery_acceptance {
    use super::*;
    use lattice_core::memory::retention::{self, RetentionPolicy};
    use serde_json::json;

    fn payload(result: Value) -> Value {
        parse_wrapped_tool_payload(
            result["content"][0]["text"]
                .as_str()
                .expect("wrapped public MCP result"),
        )
        .expect("structured public MCP payload")
    }

    async fn call(handler: &McpHandler, name: &str, arguments: Value) -> Value {
        handler
            .handle("tools/call", json!({"name": name, "arguments": arguments}))
            .await
            .expect("public MCP tools/call succeeds")
    }

    fn policy() -> RetentionPolicy {
        RetentionPolicy {
            stale_after_secs: 90 * retention::DAY,
            purge_after_secs: 180 * retention::DAY,
            sweep_interval_secs: 1,
            receipt_retention_secs: 7 * 86_400,
            max_receipts: 8,
            batch_size: 8,
        }
    }

    #[tokio::test]
    async fn mcp_delivery_requires_exact_ack_and_explicitly_discovers_retention_stale() {
        let (handler, memory_store, workspace_root) = build_memory_test_handler("retention-ack");
        let saved = payload(call(
            &handler,
            "remember",
            json!({
                "kind": "durable", "content": "retention delivery fixture exact content",
                "scope": "repo", "memory_class": "constraint", "confidence": 1.0,
                "confidence_reason": "MCP retention delivery acceptance fixture.",
                "freshness_policy": "repo_scoped"
            }),
        ).await);
        let memory_id = saved["memory_id"].as_str().expect("saved memory id").to_string();
        {
            let store = memory_store.lock().await;
            store.with_connection(|connection| {
                connection.execute("UPDATE memories SET verification_status='verified' WHERE id=?1", [&memory_id])
                    .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
                Ok(())
            }).expect("promote fixture record for delivery");
        }

        let delivered = payload(call(&handler, "recall", json!({
            "query": "retention delivery fixture", "mode": "search", "render_mode": "full"
        })).await);
        let receipt = delivered["memory_deliveries"][0].clone();
        assert_eq!(delivered["memories"][0]["id"], format!("repository:{}:{memory_id}", workspace_root.to_string_lossy()));

        let mut forged = receipt.clone();
        forged["payload_hash"] = json!("sha256:forged-payload");
        let rejected = handler.handle("tools/call", json!({"name":"recall", "arguments": {
            "mode": "acknowledge_delivery", "authority": forged["authority"],
            "delivery_id": forged["delivery_id"], "payload_hash": forged["payload_hash"]
        }})).await;
        assert!(rejected.is_err(), "forged acknowledgement must fail public MCP transport");
        {
            let store = memory_store.lock().await;
            let recalled: Option<i64> = store.with_connection(|connection| {
                connection.query_row("SELECT last_recalled_at FROM memories WHERE id=?1", [&memory_id], |row| row.get(0))
                    .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))
            }).expect("read unrenewed memory");
            assert_eq!(recalled, None, "forged receipt must not renew retention");
        }

        let acknowledged = payload(call(&handler, "recall", json!({
            "mode": "acknowledge_delivery", "authority": receipt["authority"],
            "delivery_id": receipt["delivery_id"], "payload_hash": receipt["payload_hash"]
        })).await);
        assert_eq!(acknowledged["acknowledged_count"], 1);
        {
            let store = memory_store.lock().await;
            store.with_connection(|connection| {
                connection.execute("UPDATE memories SET retention_stale=1 WHERE id=?1", [&memory_id])
                    .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
                Ok(())
            }).expect("introduce retention staleness after acknowledgement");
        }
        let replay = payload(call(&handler, "recall", json!({
            "mode": "acknowledge_delivery", "authority": receipt["authority"],
            "delivery_id": receipt["delivery_id"], "payload_hash": receipt["payload_hash"]
        })).await);
        assert_eq!(replay["acknowledged_count"], 0, "replay cannot renew or clear trust drift");

        let hidden = payload(call(&handler, "recall", json!({"query": "retention delivery fixture", "mode": "search"})).await);
        assert_eq!(hidden["count"], 0);
        let explicit = payload(call(&handler, "recall", json!({
            "query": "retention delivery fixture", "mode": "search", "include_retention_stale": true
        })).await);
        assert_eq!(explicit["count"], 1);
        assert_eq!(explicit["memories"][0]["retention_stale"], true);
        assert_eq!(explicit["memories"][0]["verification_status"], "verified");
        {
            let store = memory_store.lock().await;
            store.with_connection(|connection| {
                connection.execute("UPDATE memories SET is_stale=1, verification_status='stale' WHERE id=?1", [&memory_id])
                    .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
                Ok(())
            }).expect("introduce independent evidence drift");
        }
        let replay_after_drift = payload(call(&handler, "recall", json!({
            "mode": "acknowledge_delivery", "authority": receipt["authority"],
            "delivery_id": receipt["delivery_id"], "payload_hash": receipt["payload_hash"]
        })).await);
        assert_eq!(replay_after_drift["acknowledged_count"], 0);
        let evidence_stale = payload(call(&handler, "recall", json!({
            "query": "retention delivery fixture", "mode": "search", "include_retention_stale": true
        })).await);
        assert_eq!(evidence_stale["count"], 0, "retention discovery cannot override evidence drift");
        assert!(memory_store.lock().await.get_by_id(&memory_id).unwrap().unwrap().is_stale);
        let _ = std::fs::remove_dir_all(workspace_root);
    }

    #[tokio::test]
    async fn controlled_sweep_purges_an_unrenewed_mcp_memory_and_blocks_replay() {
        let (handler, memory_store, workspace_root) = build_memory_test_handler("retention-sweep");
        let saved = payload(call(&handler, "remember", json!({
            "kind": "durable", "content": "controlled expiry fixture payload",
            "scope": "repo", "memory_class": "constraint", "confidence": 1.0,
            "confidence_reason": "Controlled retention acceptance fixture.", "freshness_policy": "repo_scoped"
        })).await);
        let memory_id = saved["memory_id"].as_str().expect("saved memory id").to_string();
        {
            let store = memory_store.lock().await;
            store.with_connection(|connection| {
                connection.execute("UPDATE memories SET verification_status='verified' WHERE id=?1", [&memory_id])
                    .map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
                Ok(())
            }).expect("promote fixture record for expiry");
        }
        let _attempted = payload(call(&handler, "recall", json!({
            "query": "controlled expiry fixture", "mode": "search", "render_mode": "full"
        })).await);

        let store = memory_store.lock().await;
        store.with_connection(|connection| {
            // This is the sole controlled-clock seam: real MCP delivery above
            // proves attempted delivery did not set last_recalled_at.
            let recalled: Option<i64> = connection.query_row("SELECT last_recalled_at FROM memories WHERE id=?1", [&memory_id], |row| row.get(0)).map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
            assert_eq!(recalled, None, "attempted delivery alone must not renew retention");
            connection.execute("UPDATE memories SET created_at=100,retention_grace_until=0 WHERE id=?1", [&memory_id]).map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
            let stale_at = 100 + 90 * retention::DAY;
            let purge_at = 100 + 180 * retention::DAY;
            assert_eq!(retention::sweep(connection, stale_at - 1, &policy())?.stale, 0);
            assert_eq!(retention::sweep(connection, stale_at, &policy())?.stale, 1);
            assert_eq!(retention::sweep(connection, purge_at - 1, &policy())?.purged, 0);
            assert_eq!(retention::sweep(connection, purge_at, &policy())?.purged, 1);
            let memories: i64 = connection.query_row("SELECT COUNT(*) FROM memories WHERE id=?1", [&memory_id], |row| row.get(0)).map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
            let fts: i64 = connection.query_row("SELECT COUNT(*) FROM memories_fts WHERE memory_id=?1", [&memory_id], |row| row.get(0)).map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
            let receipt: i64 = connection.query_row("SELECT COUNT(*) FROM memory_deletion_receipts WHERE memory_id=?1", [&memory_id], |row| row.get(0)).map_err(|error| lattice_core::error::LatticeError::Storage(error.to_string()))?;
            assert_eq!((memories, fts, receipt), (0, 0, 1));
            let replay = connection.execute("INSERT INTO memories(id,content,memory_type) VALUES(?1,'replayed','fact')", [&memory_id]);
            assert!(replay.unwrap_err().to_string().contains("purged memory replay forbidden"), "purge receipt trigger must forbid replay resurrection");
            Ok(())
        }).expect("controlled retention sweep");
        drop(store);
        let _ = std::fs::remove_dir_all(workspace_root);
    }
}
