struct Other;
impl Other { fn remove_dir_all(&self) {} fn remove_dir(&self) {} fn drain_for(&self) {} }
fn main() {
    let _ = std::fs::metadata("owned-cache");
    Other.remove_dir_all();
    Other.remove_dir();
    Other.drain_for();
}
