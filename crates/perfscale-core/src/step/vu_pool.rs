//! Thread-per-core VU executor.
//!
//! VU tasks used to run on the process-wide multi-thread (work-stealing)
//! runtime. For an HTTP load generator that is the expensive choice: every
//! request crosses tasks (the VU task hands the request to hyper's
//! connection task over a channel) and work stealing spreads those two tasks
//! over different worker threads, so each request pays cross-thread wakeups
//! — futex/condvar park/unpark and shared-queue contention. Profiling a
//! 10-VU GET loop showed ~half the engine's CPU in park/unpark, and the same
//! load at 2 worker threads matched 10 workers' throughput on ~60% of the
//! CPU.
//!
//! Instead, `client_shard_count()` single-threaded runtimes run on dedicated
//! threads, one per HTTP client shard. VU `i` is spawned onto runtime
//! `shard(i)` — the same shard whose `reqwest::Client` (and therefore whose
//! keep-alive connections and their hyper connection tasks) it uses — so a
//! request and its connection task always share one thread and one event
//! loop, the gcannon/zrk model. Runtimes are created once per process and
//! live for its lifetime (the agent runs many tests back to back).
//!
//! The pool is used only when the caller runs on a multi-thread runtime (the
//! CLI's `#[tokio::main]`). A caller already on a current-thread runtime —
//! `#[tokio::test]`, an embedder's single-threaded loop — keeps its VUs
//! there: that is already one event loop, and it keeps such runs isolated
//! from each other instead of sharing the process-wide pool threads.
//!
//! `PERFSCALE_VU_RUNTIME=shared` restores the previous behaviour (VUs on
//! the ambient runtime) for comparison or as an escape hatch.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::Handle;
use tokio::task::JoinHandle;

fn pool() -> Option<&'static [Handle]> {
    static POOL: OnceLock<Option<Vec<Handle>>> = OnceLock::new();
    POOL.get_or_init(|| {
        if std::env::var("PERFSCALE_VU_RUNTIME").as_deref() == Ok("shared") {
            return None;
        }
        let n = super::actions::client_shard_count();
        let mut handles = Vec::with_capacity(n);
        for i in 0..n {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .thread_name(format!("perfscale-vu-{i}"))
                .build()
            {
                Ok(rt) => rt,
                // Out of threads/fds at start: degrade to the ambient
                // runtime rather than failing the run.
                Err(_) => return None,
            };
            handles.push(rt.handle().clone());
            let spawned = std::thread::Builder::new()
                .name(format!("perfscale-vu-{i}"))
                .spawn(move || rt.block_on(std::future::pending::<()>()));
            if spawned.is_err() {
                return None;
            }
        }
        Some(handles)
    })
    .as_deref()
}

/// Spawn a VU task on the event loop of HTTP client shard `shard`.
pub(crate) fn spawn_on_shard<F>(shard: usize, fut: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let ambient_is_single = Handle::try_current()
        .is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread);
    match pool() {
        Some(handles) if !ambient_is_single => handles[shard % handles.len()].spawn(fut),
        _ => tokio::spawn(fut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// From a multi-thread runtime, VU tasks land on the shard's dedicated
    /// pool thread — and the same shard always maps to the same thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_thread_callers_run_vus_on_the_shard_pool() {
        let name = |shard| {
            spawn_on_shard(shard, async {
                std::thread::current().name().unwrap_or("").to_owned()
            })
        };
        let a = name(0).await.unwrap();
        let b = name(0).await.unwrap();
        assert!(a.starts_with("perfscale-vu-"), "pool thread, got {a:?}");
        assert_eq!(a, b, "one shard, one event loop");
    }

    /// A current-thread caller keeps its VUs on its own loop.
    #[tokio::test]
    async fn current_thread_callers_keep_vus_local() {
        let caller = std::thread::current().id();
        let vu = spawn_on_shard(3, async { std::thread::current().id() })
            .await
            .unwrap();
        assert_eq!(vu, caller);
    }
}
