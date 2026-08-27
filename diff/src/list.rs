mod balances;
mod participation;
mod position_set;
mod positional;
mod queue;
mod validators;
mod vector;

use typenum::U10000000000000000000;

pub use balances::BalancesPatch;
pub use participation::ParticipationPatch;
pub use positional::PositionalPatch;
pub use queue::QueuePatch;
pub use validators::ValidatorListPatch;
pub use vector::VectorPatch;

/// A large value, used as "unlimited" value for containers.
pub(crate) type Unlimited = U10000000000000000000;
