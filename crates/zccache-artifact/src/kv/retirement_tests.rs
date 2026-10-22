//! Clear retires a namespace before later recursive cleanup.
use super::*;
use std::time::Duration;

#[test]
fn retired_namespace_sweep_preserves_a_fresh_put() {
    let directory = tempfile::tempdir().unwrap();
    let store = KvStore::open(directory.path()).unwrap();
    let key = Key::from_hex(&"a".repeat(64)).unwrap();
    store.put("exec-probe-v1", &key, b"old").unwrap();
    let detached = store
        .detach_namespace_for("exec-probe-v1", Duration::from_secs(2))
        .unwrap();
    assert!(store.get("exec-probe-v1", &key).unwrap().is_none());
    store.put("exec-probe-v1", &key, b"new").unwrap();
    let report = detached.sweep_for(Duration::from_secs(2)).unwrap();
    assert_eq!(report.failures, 0);
    assert_eq!(store.get("exec-probe-v1", &key).unwrap().unwrap(), b"new");
}

#[test]
fn dropped_retired_namespace_is_recovered_without_touching_current_values() {
    let directory = tempfile::tempdir().unwrap();
    let store = KvStore::open(directory.path()).unwrap();
    let key = Key::from_hex(&"b".repeat(64)).unwrap();
    store.put("exec-probe-v1", &key, b"old").unwrap();
    drop(
        store
            .detach_namespace_for("exec-probe-v1", Duration::from_secs(2))
            .unwrap(),
    );
    let reopened = KvStore::open(directory.path()).unwrap();
    reopened.put("exec-probe-v1", &key, b"current").unwrap();
    let recovered = reopened
        .sweep_retired_namespaces_for(Duration::from_secs(2))
        .unwrap();
    assert_eq!(recovered.failures, 0);
    assert!(recovered.reclaimed_bytes > 0);
    assert_eq!(
        reopened.get("exec-probe-v1", &key).unwrap().unwrap(),
        b"current"
    );
}
