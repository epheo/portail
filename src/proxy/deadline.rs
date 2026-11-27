//! Budgets on waits: a step that must progress, a phase that must end.

use std::pin::Pin;
use std::time::Duration;

use anyhow::Result;

/// Zero means disabled in the config; the streaming loops branch on Option.
#[inline]
pub(crate) fn idle_opt(d: Duration) -> Option<Duration> {
    (!d.is_zero()).then_some(d)
}

/// Await one streaming step under the optional per-step progress budget
/// (None = unbounded). `context` names the stalled party in the error.
/// Shared with the h2 bridge, whose steps are not io::Result-shaped.
#[inline]
pub(crate) async fn progress<T, F>(
    context: &'static str,
    idle: Option<Duration>,
    fut: F,
) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    match idle {
        Some(d) => tokio::time::timeout(d, fut)
            .await
            .map_err(|_| anyhow::anyhow!("{} stalled: no progress within {:?}", context, d)),
        None => Ok(fut.await),
    }
}

/// Await one body-streaming I/O step under the optional progress budget.
/// Only wraps the multi-read/write streaming loops — the common single-read
/// response never reaches these, so no timer is created on that path.
#[inline]
pub(super) async fn idle_guarded<T, F>(idle: Option<Duration>, fut: F) -> Result<T>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    Ok(progress("body stream", idle, fut).await??)
}

/// Clamp a phase budget by an optional absolute deadline.
/// Returns `min(budget, deadline - now)` if a deadline exists, else the budget.
#[inline]
pub(super) fn clamp_to_deadline(
    budget: Duration,
    deadline: Option<std::time::Instant>,
) -> Duration {
    match deadline {
        Some(d) => budget.min(d.saturating_duration_since(std::time::Instant::now())),
        None => budget,
    }
}

/// `budget` after `start`, or `deadline` when sooner. `None`, no bound, when
/// past the clock's range, as tokio's `timeout` takes such a budget.
pub(super) fn deadline_after(
    start: std::time::Instant,
    budget: Duration,
    deadline: Option<std::time::Instant>,
) -> Option<tokio::time::Instant> {
    let end = match (start.checked_add(budget), deadline) {
        (Some(end), Some(deadline)) => end.min(deadline),
        (end, deadline) => end.or(deadline)?,
    };
    Some(tokio::time::Instant::from_std(end))
}

/// `fut`'s output, or `None` once `deadline` passes first; no deadline, no
/// bound. tokio's `timeout_at` on a timer the connection keeps across its
/// requests: a fresh tokio timer registers in the runtime's one timer wheel
/// under its lock, and takes the lock again to cancel, on every request,
/// while moving a registered timer to a later deadline takes none.
pub(super) async fn within<F: std::future::Future>(
    mut timer: Pin<&mut tokio::time::Sleep>,
    deadline: Option<tokio::time::Instant>,
    fut: F,
) -> Option<F::Output> {
    use std::future::Future;
    use std::task::Poll;
    use tokio::task::coop::has_budget_remaining;
    let Some(deadline) = deadline else {
        return Some(fut.await);
    };
    timer.as_mut().reset(deadline);
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        let had_budget = has_budget_remaining();
        if let Poll::Ready(out) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        // As timeout_at does: a future that spent the task's budget still
        // has its deadline checked.
        let expired = if had_budget && !has_budget_remaining() {
            std::pin::pin!(tokio::task::coop::unconstrained(timer.as_mut())).poll(cx)
        } else {
            timer.as_mut().poll(cx)
        };
        expired.map(|()| None)
    })
    .await
}
