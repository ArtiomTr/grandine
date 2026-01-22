mod proof;

pub use proof::{EXECUTION_PROOF_TYPE_COUNT, ExecutionProof, ExecutionProofId};

/// Minimum number of execution proofs required from different proof types
/// before marking an execution payload as available in ZK-VM mode.
///
/// This provides client diversity - nodes wait for proofs from K different
/// zkVM+EL combinations before considering an execution payload available.
pub const DEFAULT_MIN_PROOFS_REQUIRED: usize = 2;

/// Maximum number of execution proofs that can be requested or stored.
/// This corresponds to the maximum number of proof types (zkVM+EL combinations)
/// that can be supported, which is currently 8 (ExecutionProofId is 0-7).
pub const MAX_PROOFS: usize = 8;
