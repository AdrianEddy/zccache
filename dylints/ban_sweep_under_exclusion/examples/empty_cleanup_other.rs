#![allow(dead_code)]
use zccache_artifact::DetachedKvNamespace;
use zccache_core::sync::PublicationReadPermit;
struct Cleanup(Option<DetachedKvNamespace>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(retired) = self.0.take() {
            let _ = retired.sweep_for(std::time::Duration::from_secs(1));
        }
    }
}
struct Holder {
    cleanup: Option<Cleanup>,
    value: usize,
}
fn checked(value: usize) -> usize {
    assert!(value == 0);
    value
}
fn populated(cleanup: Cleanup, value: usize) -> Holder {
    Holder {
        cleanup: Some(cleanup),
        value: checked(value),
    }
}
fn install(slot: &mut Option<Cleanup>, cleanup: Cleanup) {
    *slot = Some(cleanup);
}
fn changed(cleanup: Cleanup, value: usize) {
    let mut slot = None;
    install(&mut slot, cleanup);
    checked(value);
}
fn invalid_populated(permit: PublicationReadPermit, cleanup: Cleanup, value: usize) {
    let holder = populated(cleanup, value);
    drop(permit);
    drop(holder);
}
fn invalid_changed(permit: PublicationReadPermit, cleanup: Cleanup, value: usize) {
    changed(cleanup, value);
    drop(permit);
}
fn main() {}
