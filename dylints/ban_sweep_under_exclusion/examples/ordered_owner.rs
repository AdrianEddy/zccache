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
struct Owner {
    permit: PublicationReadPermit,
    cleanup: Cleanup,
}
fn checked(value: usize) {
    assert!(value == 0);
}
fn ordered(permit: PublicationReadPermit, cleanup: Cleanup, value: usize) {
    let _owner = Owner { permit, cleanup };
    checked(value);
}
fn nested(permit: PublicationReadPermit, cleanup: Cleanup, value: usize) {
    let _owner = (Owner { permit, cleanup }, 0usize);
    checked(value);
}
fn main() {}
