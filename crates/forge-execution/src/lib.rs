//! Execution providers: native local process execution with approval
//! gating, and a recording mock for tests/BDD.

mod mock;
mod native;

pub use mock::MockExecution;
pub use native::{ApprovalChannel, NativeExecution};
