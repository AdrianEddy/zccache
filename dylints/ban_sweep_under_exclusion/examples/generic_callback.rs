#![allow(dead_code)]
use zccache_core::sync::PublicationReadPermit;
fn invoke<F: FnOnce()>(operation: F) {
    operation();
}
fn read(permit: PublicationReadPermit) {
    invoke(|| {
        let _ = std::fs::read("owned-cache");
    });
    drop(permit);
}
fn main() {}
