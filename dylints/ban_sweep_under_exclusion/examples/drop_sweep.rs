#![allow(dead_code)]
use std::time::Duration;
use zccache_artifact::DetachedKvNamespace;
use zccache_core::sync::PublicationReadPermit;
struct Cleanup(Option<DetachedKvNamespace>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(retired) = self.0.take() {
            let _ = retired.sweep_for(Duration::from_secs(1));
        }
    }
}
fn explicit(permit: PublicationReadPermit, cleanup: Cleanup) {
    drop(cleanup);
    drop(permit);
}
fn implicit(permit: PublicationReadPermit, cleanup: Cleanup) {
    {
        let _cleanup = cleanup;
    }
    drop(permit);
}
fn released(permit: PublicationReadPermit, cleanup: Cleanup) {
    drop(permit);
    drop(cleanup);
}
fn main() {}
