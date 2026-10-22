//! Owned cleanup of displaced cached records on the blocking lane.
use super::*;

// Schedule before returning the future: an unpolled/cancelled waiter must
// never inherit recursive record cleanup on an async worker.
pub(super) fn retire_for(
    retired: Vec<CachedArtifact>,
    bound: std::time::Duration,
) -> impl std::future::Future<Output = ()> {
    let task = if retired.is_empty() {
        None
    } else {
        Some(kernal_api::async_engine::launch_blocking(move || drop(retired)).detach_on_drop())
    };
    async move {
        if let Some(task) = task {
            match kernal_api::async_engine::timeout(bound, task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(%error, "retired artifact cleanup task failed"),
                Err(_) => tracing::warn!(
                    "retired artifact cleanup exceeded its wait budget; owned cleanup continues"
                ),
            }
        }
    }
}
