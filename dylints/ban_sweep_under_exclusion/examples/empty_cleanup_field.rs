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
fn empty(value: usize) -> Holder {
    Holder {
        cleanup: None,
        value: checked(value),
    }
}
fn valid(permit: PublicationReadPermit, value: usize) {
    let holder = empty(value);
    drop(permit);
    drop(holder);
}
fn main() {}
