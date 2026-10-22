pub struct StagedCompilePlan;
impl StagedCompilePlan {
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all("private-output");
    }
}
pub struct Impostor;
impl Impostor {
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all("shared-cache");
    }
}
pub fn cleanup() {
    let _ = std::fs::remove_dir_all("shared-cache");
}
