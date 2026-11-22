use super::TargetError;
use std::sync::atomic::{AtomicU8, Ordering};

const PENDING: u8 = 0;
const WRITING: u8 = 1;
const ABORTED: u8 = 2;

/// A timeout and the worker must agree which side owns the write decision.
/// A separate bool/deadline check permits a paused worker to write after its
/// caller has already reported a definite failure.
#[derive(Default)]
pub(super) struct WriteRequest(AtomicU8);

impl WriteRequest {
    pub fn claim_write(&self) -> Result<(), TargetError> {
        self.0
            .compare_exchange(PENDING, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| TargetError::Busy)
    }

    pub fn timed_out(&self) -> TargetError {
        match self
            .0
            .compare_exchange(PENDING, ABORTED, Ordering::AcqRel, Ordering::Acquire)
        {
            Err(WRITING) => TargetError::Uncertain,
            _ => TargetError::Busy,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn worker_paused_after_validation_cannot_write_after_caller_timeout() {
        let request = Arc::new(WriteRequest::default());
        let validated = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let worker_request = request.clone();
        let worker_validated = validated.clone();
        let worker_resume = resume.clone();
        let worker = std::thread::spawn(move || {
            worker_validated.wait();
            worker_resume.wait();
            worker_request.claim_write()
        });
        validated.wait();
        assert_eq!(request.timed_out(), TargetError::Busy);
        resume.wait();
        assert_eq!(worker.join().unwrap(), Err(TargetError::Busy));
    }

    #[test]
    fn timeout_after_write_claim_is_uncertain() {
        let request = WriteRequest::default();
        request.claim_write().unwrap();
        assert_eq!(request.timed_out(), TargetError::Uncertain);
    }

    #[test]
    fn a_write_claim_is_one_shot() {
        let request = WriteRequest::default();
        request.claim_write().unwrap();
        assert_eq!(request.claim_write(), Err(TargetError::Busy));
    }
}
