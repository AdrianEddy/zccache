# Bounded synchronization

`ban_unbounded_sync` rejects raw blocking waits and lock acquisition outside the product boundaries. `ban_guard_across_await` checks the compiler coroutine layout for retained data guards, including aliases guards nested in generic containers, and concrete local wrappers. Async state uses `zccache_core::sync::State`; unit-only coordination uses `sync::Gate` and `sync::Barrier`, whose raw primitives stay private.
