// Included inside `socket_server.rs`'s test module.
mod shutdown_admission_acceptance {
    use super::*;

    #[tokio::test]
    async fn global_close_during_victim_shutdown_cancels_candidate_before_bootstrap() {
        let daemon = Arc::new(GlobalDaemon::new_with_config(1, false));
        let victim_root = unique_test_root("shutdown-race-victim");
        let candidate_root = unique_test_root("shutdown-race-candidate");

        let victim_reservation = daemon
            .resource_budget
            .try_reserve("shutdown_race_victim", 1)
            .expect("reserve victim view");
        let victim = Arc::new(ShardEntry::pending(
            victim_root.clone(),
            Arc::clone(&daemon.index_work),
            victim_reservation,
        ));
        victim.bootstrapping.store(false, Ordering::Release);
        let (victim_started_tx, victim_started_rx) = tokio::sync::oneshot::channel();
        let (release_victim_tx, release_victim_rx) = tokio::sync::oneshot::channel();
        *lock_owned(&victim.bootstrap) = Some(tokio::spawn(async move {
            let _ = victim_started_tx.send(());
            let _ = release_victim_rx.await;
        }));
        daemon
            .shards
            .lock()
            .await
            .insert(shard_key(&victim_root), victim);
        victim_started_rx.await.expect("victim shutdown gate started");

        let candidate_daemon = Arc::clone(&daemon);
        let requested_candidate = candidate_root.clone();
        let candidate_request = tokio::spawn(async move {
            candidate_daemon
                .shard_for(requested_candidate, Vec::new(), Vec::new(), false)
                .await
        });

        let candidate = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(candidate) = daemon
                    .shards
                    .lock()
                    .await
                    .get(&shard_key(&candidate_root))
                    .cloned()
                {
                    break candidate;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("candidate inserted before victim shutdown completes");
        assert!(lock_owned(&candidate.bootstrap).is_none());
        assert!(lock_owned(&candidate.runtime).is_none());

        close_shard_admission(&daemon).await;
        shutdown_daemon_shards(&daemon).await;
        release_victim_tx.send(()).expect("release victim shutdown");

        let error = candidate_request
            .await
            .expect("candidate request joined")
            .err()
            .expect("closed daemon rejects candidate after victim shutdown");
        assert!(error.to_string().contains("shutting down"));
        assert!(daemon.shards.lock().await.is_empty());
        assert!(lock_owned(&candidate.bootstrap).is_none());
        assert!(lock_owned(&candidate.runtime).is_none());
        assert_eq!(candidate.active_connections.load(Ordering::Acquire), 0);

        let _ = std::fs::remove_dir_all(victim_root);
        let _ = std::fs::remove_dir_all(candidate_root);
    }
}
