//! Bounded rich replies retained together with delivery status after lost ACK.
use serde::{Deserialize, Serialize};
pub(crate) const MAX_REPLY: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Delivery {
    Rejected,
    Written,
    Indeterminate,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Submission {
    Submitted,
    Unverified,
    Late,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Reply {
    Unit,
    Submission(Submission),
    Cleared(Option<String>),
    Error(ErrorCode),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ErrorCode {
    Busy,
    Exited,
    Attached,
    HumanInputRecent,
    WriteTimeout,
    SubmitUncertain,
    LateSubmitAbandoned,
    Invalid,
    Unavailable,
    ResultTooLarge,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Completion {
    pub delivery: Delivery,
    pub reply: Reply,
}
impl Completion {
    pub fn fits(&self) -> bool {
        serde_json::to_writer(Budget(0), self).is_ok()
    }
    /// An oversized result after an effect is NOT a safe rejection/replay.
    pub fn bounded(self) -> Self {
        if self.fits() {
            self
        } else {
            Self {
                delivery: Delivery::Indeterminate,
                reply: Reply::Error(ErrorCode::ResultTooLarge),
            }
        }
    }
}
// Count serialization without allocating an oversized temporary result buffer.
struct Budget(usize);
impl std::io::Write for Budget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_REPLY.saturating_sub(self.0) {
            return Err(std::io::Error::other("host result byte budget exceeded"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rich_results_do_not_collapse_delivery_and_submission() {
        let result = Completion {
            delivery: Delivery::Written,
            reply: Reply::Submission(Submission::Unverified),
        };
        let bytes = serde_json::to_vec(&result).unwrap();
        assert_eq!(
            serde_json::from_slice::<Completion>(&bytes).unwrap(),
            result
        );
    }
    #[test]
    fn oversized_result_remains_uncertain_and_bounded() {
        let result = Completion {
            delivery: Delivery::Written,
            reply: Reply::Cleared(Some("x".repeat(MAX_REPLY))),
        }
        .bounded();
        assert_eq!(result.delivery, Delivery::Indeterminate);
        assert_eq!(result.reply, Reply::Error(ErrorCode::ResultTooLarge));
        assert!(serde_json::to_vec(&result).unwrap().len() <= MAX_REPLY);
    }
}
