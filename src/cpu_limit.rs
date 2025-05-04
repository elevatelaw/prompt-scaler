//! Tools for limiting the number of concurrent CPU-bound tasks.

use std::sync::LazyLock;

use tokio::sync::Semaphore;

use crate::prelude::*;

/// Semaphore used to limit the number of concurrent `pdfseparate` and `pdfcairo`
/// processes.
static CPU_SEMAPHORE: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(num_cpus::get()));

/// Call an async function while holding a permit from the CPU semaphore.
///
/// We do this to limit the number of external processes that are each trying
/// to use 100% of a CPU core.
///
/// You don't need to do this for in-process CPU-bound tasks, as long as you're
/// using
/// [`async_utils::blocking_iter_streams::spawn_blocking_propagating_panics`].
/// But you should use for expensive external processes.
///
/// We may want alternative versions of this function if we need to run any
/// heavily multithreaded external commands. This would allow us to register
/// that we're multiple CPUs, or all the CPUs available.
#[instrument(level = "trace", skip_all)]
pub async fn with_cpu_semaphore<Func, Fut, R>(f: Func) -> Result<R>
where
    Func: FnOnce() -> Fut,
    Fut: Future<Output = Result<R>>,
{
    // Acquire a permit from the semaphore.
    let permit = CPU_SEMAPHORE
        .acquire()
        .await
        .context("Could not acquire CPU permit")?;
    // Run the function while holding the permit.
    let result = f().await;
    // Release the permit.
    drop(permit);
    result
}
