use adapter_protocol::{AdapterError, Result};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct RequestContext {
    pub cancellation: CancellationToken,
    pub deadline: Instant,
}

impl RequestContext {
    pub fn new(timeout: Duration) -> Result<Self> {
        Self::with_cancellation(timeout, CancellationToken::new())
    }

    pub fn with_cancellation(timeout: Duration, cancellation: CancellationToken) -> Result<Self> {
        if timeout.is_zero() {
            return Err(AdapterError::invalid("Request timeout must be positive."));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| AdapterError::invalid("Request timeout exceeds the clock range."))?;
        Ok(Self {
            cancellation,
            deadline,
        })
    }

    pub fn check(&self) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(AdapterError::new(
                499,
                "cancelled",
                "The client disconnected.",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(AdapterError::new(
                504,
                "deadline_exceeded",
                "The request deadline expired.",
            ));
        }
        Ok(())
    }

    pub async fn bounded<T, F>(&self, future: F) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        self.check()?;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(AdapterError::new(499, "cancelled", "The client disconnected.")),
            _ = tokio::time::sleep_until(self.deadline) => Err(AdapterError::new(504, "deadline_exceeded", "The request deadline expired.")),
            result = future => result,
        }
    }

    pub fn cancel_on_drop(&self) -> CancelOnDrop {
        CancelOnDrop(self.cancellation.clone())
    }

    pub fn child(&self) -> Self {
        Self {
            cancellation: self.cancellation.child_token(),
            deadline: self.deadline,
        }
    }
}

pub struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_and_absolute_deadlines_interrupt_pending_work() {
        let context = RequestContext::new(Duration::from_secs(10)).unwrap();
        context.cancellation.cancel();
        let result: Result<()> = context.bounded(std::future::pending()).await;
        assert_eq!(result.unwrap_err().code, "cancelled");
        let context = RequestContext::new(Duration::from_millis(5)).unwrap();
        let result: Result<()> = context.bounded(std::future::pending()).await;
        assert_eq!(result.unwrap_err().code, "deadline_exceeded");
    }

    #[test]
    fn owned_guard_cancels_its_request_without_touching_another() {
        let first = RequestContext::new(Duration::from_secs(1)).unwrap();
        let other = RequestContext::new(Duration::from_secs(1)).unwrap();
        drop(first.cancel_on_drop());
        assert!(first.cancellation.is_cancelled());
        assert!(!other.cancellation.is_cancelled());
    }

    #[test]
    fn operation_scopes_inherit_deadlines_and_cancel_only_downward() {
        let request = RequestContext::new(Duration::from_secs(10)).unwrap();
        let completed_operation = request.child();
        assert_eq!(completed_operation.deadline, request.deadline);
        drop(completed_operation.cancel_on_drop());
        assert!(completed_operation.cancellation.is_cancelled());
        assert!(!request.cancellation.is_cancelled());
        let active_operation = request.child();
        request.cancellation.cancel();
        assert!(active_operation.cancellation.is_cancelled());
    }
}
