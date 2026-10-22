//! Production cleanup entrypoints must retry previously detached namespaces.
use super::*;

#[tokio::test]
async fn clear_retries_a_previously_detached_exec_namespace() {
    retry_retired_namespace(true).await;
}

#[tokio::test]
async fn maintenance_retries_a_previously_detached_exec_namespace() {
    retry_retired_namespace(false).await;
}

async fn retry_retired_namespace(clear: bool) {
    crate::test_support::test_timeout(async {
        let directory = tempfile::tempdir().unwrap();
        let cache_dir = NormalizedPath::new(directory.path());
        let endpoint = crate::ipc::unique_test_endpoint();
        let mut server = DaemonServer::bind_with_cache_dir(&endpoint, &cache_dir).unwrap();
        let index_writer = tokio::spawn(run_index_writer(
            server.index_writer_rx.take().unwrap(),
            Arc::clone(&server.state.artifact_store),
            Arc::clone(&server.state.index_writer_shutdown),
        ));
        let key = crate::artifact::Key::from_hex(&"e".repeat(64)).unwrap();
        server
            .state
            .exec_store
            .put_async(
                handle_exec_probe::EXEC_PROBE_NAMESPACE,
                &key,
                b"abandoned namespace",
            )
            .await
            .unwrap();
        let state = Arc::clone(&server.state);
        state
            .launch_blocking({
                let state = Arc::clone(&state);
                move || {
                    drop(
                        state
                            .exec_store
                            .detach_namespace_for(
                                handle_exec_probe::EXEC_PROBE_NAMESPACE,
                                std::time::Duration::from_secs(2),
                            )
                            .unwrap(),
                    );
                }
            })
            .await
            .unwrap();
        let retired = cache_dir.join("kv/.retired");
        assert!(std::fs::read_dir(&retired).unwrap().next().is_some());
        if clear {
            assert!(matches!(
                crate::daemon::server::handle_clear::handle_clear(&state).await,
                Response::Cleared { .. }
            ));
        } else {
            maintain_state_disk(
                Arc::clone(&state),
                MaintenancePolicy::default(),
                MaintenanceKind::Full,
                None,
            )
            .await
            .unwrap();
        }
        server.state.index_writer_shutdown.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), index_writer)
            .await
            .unwrap()
            .unwrap();
        assert!(
            std::fs::read_dir(retired).unwrap().next().is_none(),
            "production cleanup never retried the abandoned namespace"
        );
    })
    .await;
}
