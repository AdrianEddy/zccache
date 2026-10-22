//! #1964: recursive cleanup must not retain exclusion from publishers.

use super::*;
use std::time::Duration;

#[test]
fn maintenance_sweep_allows_unrelated_publication_after_detach() {
    publication_while_sweeping(false);
}

#[test]
fn clear_sweep_allows_unrelated_publication_after_detach() {
    publication_while_sweeping(true);
}

fn publication_while_sweeping(clear: bool) {
    use super::super::{persist_staged_artifact_paths, StagedHookGuard, StagedHookPoint};
    let directory = tempfile::tempdir().unwrap();
    let artifact_dir = directory.path().join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let source = directory.path().join("source");
    fs::write(&source, b"immutable staged payload").unwrap();
    let sources = vec![crate::core::path::NormalizedPath::from(source)];
    let old_key = "a".repeat(64);
    let new_key = "b".repeat(64);
    persist_staged_artifact_paths(&artifact_dir, &old_key, &sources).unwrap();
    let expected = HashMap::from([(
        old_key.clone(),
        Some(
            fs::read_to_string(pointer_path(&artifact_dir, &old_key))
                .unwrap()
                .trim()
                .to_string(),
        ),
    )]);
    let hook = StagedHookGuard::arm(&artifact_dir, StagedHookPoint::TreeSweep);
    let cleaner_dir = artifact_dir.clone();
    let cleaner = std::thread::spawn(move || {
        if clear {
            clear_staged_artifacts(&cleaner_dir).map(|_| ())
        } else {
            evict_staged_artifact_keys_if_unchanged(&cleaner_dir, &expected).map(|_| ())
        }
    });
    hook.wait_until_reached_for(Duration::from_secs(10));
    let detached_before_sweep = !staged_root(&artifact_dir).join(&old_key).exists()
        && !pointer_path(&artifact_dir, &old_key).exists();
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    let publisher_dir = artifact_dir.clone();
    let publisher_key = new_key.clone();
    let publisher = std::thread::spawn(move || {
        let result = persist_staged_artifact_paths(&publisher_dir, &publisher_key, &sources);
        crate::core::wait::send_for(&done_tx, result, Duration::from_secs(5)).unwrap();
    });
    let published = crate::core::wait::recv_for(&done_rx, Duration::from_secs(1));
    let published_before_resume = published.is_ok();
    // Release and drain both workers even on the baseline failure path.
    hook.resume();
    let result = published.unwrap_or_else(|_| {
        crate::core::wait::recv_for(&done_rx, Duration::from_secs(10)).unwrap()
    });
    result.unwrap();
    crate::core::wait::join_for(publisher, Duration::from_secs(10))
        .unwrap()
        .unwrap();
    crate::core::wait::join_for(cleaner, Duration::from_secs(10))
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        published_before_resume,
        "paused recursive cleanup retained store/publication exclusion"
    );
    assert!(
        detached_before_sweep,
        "evicted entries were reachable when recursive cleanup began"
    );
    assert!(
        pointer_path(&artifact_dir, &new_key).exists(),
        "Clear swept a new publication"
    );
}
