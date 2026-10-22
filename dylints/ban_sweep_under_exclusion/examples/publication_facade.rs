#![allow(dead_code)]
use std::time::Duration;
use zccache_core::sync::{PublicationBarrier, PublicationCommitPermit, PublicationDrainPermit};
fn commit(barrier: &PublicationBarrier) {
    let permit: PublicationCommitPermit = barrier.blocking_write_for(Duration::from_secs(1));
    drop(permit);
}
async fn drain(barrier: &PublicationBarrier) {
    let permit: PublicationDrainPermit = barrier.drain_for(Duration::from_secs(1)).await;
    drop(permit);
}
fn main() {}
