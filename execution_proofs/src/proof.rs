use anyhow::Result;
use ssz::{H256, Ssz, SszSize};

/// Number of execution proofs
/// Each proof represents adifferent zkVM+EL combination
pub const EXECUTION_PROOF_TYPE_COUNT: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Ssz)]
pub struct ExecutionProofId(u8);

impl ExecutionProofId {
    pub fn new(id: u8) -> Result<Self> {
        Ok(Self(id))
    }
}

#[derive(Ssz, Clone, Debug)]
pub struct ExecutionProof {
    pub proof_id: ExecutionProofId,

    pub block_root: H256,
}

impl ExecutionProof {
    pub const fn min_size() -> usize {
        ExecutionProofId::SIZE.get()
    }

    pub const fn max_size() -> usize {
        0
    }
}
