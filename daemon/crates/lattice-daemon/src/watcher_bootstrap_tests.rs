use super::*;
use lattice_core::graph::CodeGraph;

fn native_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn bootstrap_test_watcher(root: PathBuf) -> (FileWatcher, Arc<Mutex<Indexer>>, Arc<WatcherHealth>) {
    let graph = Arc::new(CodeGraph::new());
    let engine = Arc::new(Mutex::new(QueryEngine::new_shared(graph, None)));
    let indexer = Arc::new(Mutex::new(Indexer::new(root.clone())));
    let graph_store = Arc::new(Mutex::new(GraphStore::open_in_memory().unwrap()));
    let health = Arc::new(WatcherHealth::default());
    let readiness = Arc::new(IndexReadiness::default());
    let watcher = FileWatcher::new(
        root.clone(),
        None,
        Some(Arc::clone(&indexer)),
        None,
        graph_store,
        engine,
        Arc::new(OnceLock::new()),
        None,
        Arc::new(Mutex::new(RepoStateTracker::new(&root))),
        Arc::new(AtomicBool::new(false)),
        IndexWorkCoordinator::new(1),
        readiness,
        Arc::clone(&health),
        Arc::new(IndexHealth::default()),
        None,
        None,
        "bootstrap-test".to_string(),
    );
    (watcher, indexer, health)
}

fn bootstrap_test_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "lattice-{name}-{}",
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn root_event_diff_returns_only_changed_and_deleted_sources() {
    let root = bootstrap_test_root("watch-root-diff");
    let source_dir = root.join("src");
    std::fs::create_dir_all(&source_dir).unwrap();
    for index in 0..64 {
        std::fs::write(
            source_dir.join(format!("module_{index}.rs")),
            format!("pub fn module_{index}() {{}}\n"),
        )
        .unwrap();
    }
    let changed = source_dir.join("module_7.rs");
    let deleted = source_dir.join("module_8.rs");
    let ignored = root.join("node_modules/ignored.rs");
    let (watcher, _, _) = bootstrap_test_watcher(root.clone());
    watcher.capture_source_baseline().unwrap();

    std::fs::write(
        &changed,
        "pub fn module_7_changed() { println!(\"changed\"); }\n",
    )
    .unwrap();
    std::fs::remove_file(&deleted).unwrap();
    std::fs::create_dir_all(ignored.parent().unwrap()).unwrap();
    std::fs::write(&ignored, "pub fn ignored() {}\n").unwrap();
    let mut paths = Vec::new();
    watcher.handle_event(
        Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(root.clone()),
        &mut paths,
    );

    assert_eq!(paths, vec![changed, deleted]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn uncommitted_directory_diff_retries_and_preserves_newer_mutation() {
    let root = bootstrap_test_root("watch-diff-retry");
    let source = root.join("module.rs");
    std::fs::write(&source, "pub fn value() -> u32 { 1 }\n").unwrap();
    let (watcher, _, _) = bootstrap_test_watcher(root.clone());
    watcher.capture_source_baseline().unwrap();

    std::fs::write(&source, "pub fn value() -> u32 { 22 }\n").unwrap();
    assert_eq!(
        watcher.diff_source_snapshot_subtree(&root).unwrap(),
        vec![source.clone()]
    );
    assert_eq!(
        watcher.diff_source_snapshot_subtree(&root).unwrap(),
        vec![source.clone()]
    );

    std::fs::write(&source, "pub fn value() -> u32 { 333 }\n").unwrap();
    watcher.commit_source_snapshot_paths(std::slice::from_ref(&source));
    assert_eq!(
        watcher.diff_source_snapshot_subtree(&root).unwrap(),
        vec![source]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn metadata_inventory_skips_external_and_cyclic_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let root = bootstrap_test_root("watch-symlink-inventory");
    let external = bootstrap_test_root("watch-symlink-external");
    std::fs::write(external.join("outside.rs"), "pub fn outside() {}\n").unwrap();
    std::fs::write(root.join("inside.rs"), "pub fn inside() {}\n").unwrap();
    symlink(&external, root.join("external-link")).unwrap();
    symlink(&root, root.join("self-cycle")).unwrap();
    let (watcher, _, _) = bootstrap_test_watcher(root.clone());

    let snapshot = watcher.poll_snapshot().unwrap();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot.contains_key(&root.join("inside.rs")));
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(external);
}

#[tokio::test]
async fn native_directory_rename_replaces_indexed_paths() {
    let _native_guard = native_test_lock().lock().await;
    let root = bootstrap_test_root("watch-directory-rename");
    let old_file = root.join("old/module.rs");
    std::fs::create_dir_all(old_file.parent().unwrap()).unwrap();
    std::fs::write(&old_file, "pub fn renamed() {}\n").unwrap();
    let (watcher, indexer, _) = bootstrap_test_watcher(root.clone());
    let watcher = watcher.with_forced_poll_interval(Duration::from_millis(10));
    watcher.index_readiness.mark_ready();
    watcher.process_changes(vec![old_file.clone()]).await;
    assert!(indexer
        .lock()
        .await
        .parsed_files()
        .contains_key("old/module.rs"));
    let watcher = Arc::new(watcher);
    let (registered_tx, registered_rx) = oneshot::channel();
    let task = {
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { watcher.run_with_registration(registered_tx).await })
    };
    tokio::time::timeout(Duration::from_secs(2), registered_rx)
        .await
        .unwrap()
        .unwrap();

    std::fs::rename(root.join("old"), root.join("new")).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let indexer = indexer.lock().await;
            if indexer.parsed_files().contains_key("new/module.rs")
                && !indexer.parsed_files().contains_key("old/module.rs")
            {
                break;
            }
            drop(indexer);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("native directory rename must replace indexed paths");

    task.abort();
    let _ = task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn registered_native_watch_queues_edit_until_startup_is_ready() {
    let _native_guard = native_test_lock().lock().await;
    let root = bootstrap_test_root("watch-bootstrap-barrier");
    let source = root.join("src/module_0.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "pub fn value() -> u32 { 1 }\n").unwrap();

    let (mut watcher, indexer, _) = bootstrap_test_watcher(root.clone());
    watcher.forced_poll_interval = Some(Duration::from_millis(10));
    let readiness = Arc::new(IndexReadiness::default());
    watcher.index_readiness = Arc::clone(&readiness);
    let watcher = Arc::new(watcher);
    let (registered_tx, registered_rx) = oneshot::channel();
    let watch_task = {
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { watcher.run_with_registration(registered_tx).await })
    };

    tokio::time::timeout(Duration::from_secs(2), registered_rx)
        .await
        .expect("watch registration must complete")
        .expect("watcher must retain registration sender");
    std::fs::write(&source, "pub fn value() -> u32 { 2 }\n").unwrap();
    let source_event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if watcher.observed_source_event_count_for_test() > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        source_event.is_ok(),
        "native backend did not deliver a classifiable source edit; observed {:?}",
        watcher.observed_events_for_test()
    );
    assert_eq!(indexer.lock().await.file_count(), 0);

    readiness.mark_ready();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if indexer.lock().await.file_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued native edit must publish after startup readiness");

    watch_task.abort();
    let _ = watch_task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn degraded_polling_establishes_baseline_before_releasing_bootstrap() {
    let root = bootstrap_test_root("watch-bootstrap-polling");
    std::fs::write(root.join("module.rs"), "pub fn value() {}\n").unwrap();
    let (watcher, indexer, health) = bootstrap_test_watcher(root.clone());
    watcher.index_readiness.mark_ready();
    let watcher = watcher
        .with_forced_watch_failure("registration fixture failure")
        .with_forced_poll_interval(Duration::from_millis(10));
    let (registered_tx, registered_rx) = oneshot::channel();
    let task = tokio::spawn(async move { watcher.run_with_registration(registered_tx).await });

    tokio::time::timeout(Duration::from_secs(2), registered_rx)
        .await
        .expect("polling registration must complete")
        .expect("polling watcher must retain registration sender");
    let snapshot = health.snapshot();
    assert!(snapshot.watch_degraded);
    assert_eq!(
        snapshot.reason.as_deref(),
        Some("registration fixture failure")
    );
    assert!(snapshot.last_poll_epoch_secs.is_some());

    std::fs::write(root.join("module.rs"), "pub fn changed() {}\n").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if indexer.lock().await.file_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("polling must index an edit made after its registered baseline");

    task.abort();
    let _ = task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn polling_settles_metadata_noop_and_unindexed_deletion() {
    let root = bootstrap_test_root("watch-polling-noop");
    let indexed = root.join("indexed.rs");
    let unindexed = root.join("unindexed.rs");
    std::fs::write(&indexed, "pub fn indexed() {}\n").unwrap();
    std::fs::write(&unindexed, "pub fn never_indexed() {}\n").unwrap();
    let (watcher, indexer, _) = bootstrap_test_watcher(root.clone());
    watcher.index_readiness.mark_ready();
    watcher.process_changes(vec![indexed.clone()]).await;
    let watcher = Arc::new(
        watcher
            .with_forced_watch_failure("polling no-op fixture")
            .with_forced_poll_interval(Duration::from_millis(10)),
    );
    let (registered_tx, registered_rx) = oneshot::channel();
    let task = {
        let watcher = Arc::clone(&watcher);
        tokio::spawn(async move { watcher.run_with_registration(registered_tx).await })
    };
    tokio::time::timeout(Duration::from_secs(2), registered_rx)
        .await
        .unwrap()
        .unwrap();

    std::fs::write(&indexed, "pub fn indexed() {}\n").unwrap();
    std::fs::remove_file(&unindexed).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if watcher
                .diff_source_snapshot_subtree(&root)
                .unwrap()
                .is_empty()
                && watcher.pending_source_snapshot.lock().unwrap().is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("successful polling no-ops must commit and settle");
    assert_eq!(indexer.lock().await.file_count(), 1);

    task.abort();
    let _ = task.await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn polling_inventory_failure_retries_change_without_another_edit() {
    let root = bootstrap_test_root("watch-polling-retry");
    let hidden = root.with_extension("temporarily-unavailable");
    let source = root.join("module.rs");
    std::fs::write(&source, "pub fn before() {}\n").unwrap();
    let (watcher, indexer, health) = bootstrap_test_watcher(root.clone());
    watcher.index_readiness.mark_ready();
    watcher.process_changes(vec![source.clone()]).await;
    let watcher = watcher
        .with_forced_watch_failure("polling retry fixture")
        .with_forced_poll_interval(Duration::from_millis(10));
    let (registered_tx, registered_rx) = oneshot::channel();
    let task = tokio::spawn(async move { watcher.run_with_registration(registered_tx).await });
    tokio::time::timeout(Duration::from_secs(2), registered_rx)
        .await
        .unwrap()
        .unwrap();

    std::fs::rename(&root, &hidden).unwrap();
    std::fs::write(hidden.join("module.rs"), "pub fn after() {}\n").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if health
                .snapshot()
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("inventory failed"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unavailable root must report degraded inventory");
    std::fs::rename(&hidden, &root).unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let parsed = indexer.lock().await;
            if parsed
                .parsed_files()
                .get("module.rs")
                .is_some_and(|file| file.symbols.iter().any(|symbol| symbol.name == "after"))
            {
                break;
            }
            drop(parsed);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("polling must retry the same changed metadata after inventory recovery");

    task.abort();
    let _ = task.await;
    let _ = std::fs::remove_dir_all(root);
}
