use ssz::{SszReadDefault, SszWrite};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    CrossPhaseDiff,
    PatchPhaseMismatch,
    PatchIndexOutOfBounds,
    PatchListLimitExceeded,
    PatchLengthMismatch,
    UnsupportedDiff,
    InvalidPatchEncoding,
    InvalidBalanceDelta,
}

impl core::fmt::Display for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::CrossPhaseDiff => formatter.write_str("cross-phase diffs are not supported"),
            Error::PatchPhaseMismatch => {
                formatter.write_str("patch phase does not match the base value")
            }
            Error::PatchIndexOutOfBounds => formatter.write_str("patch index is out of bounds"),
            Error::PatchListLimitExceeded => formatter.write_str("patch exceeds list limit"),
            Error::PatchLengthMismatch => {
                formatter.write_str("patch result length does not match expected length")
            }
            Error::UnsupportedDiff => {
                formatter.write_str("diff cannot be represented by this patch type")
            }
            Error::InvalidPatchEncoding => formatter.write_str("patch encoding is invalid"),
            Error::InvalidBalanceDelta => formatter.write_str("balance delta is invalid"),
        }
    }
}

impl std::error::Error for Error {}

pub type PatchResult<T> = Result<T, Error>;

pub trait Patch<T>: SszWrite + SszReadDefault + Sized {
    fn diff(base: &T, changed: &T) -> PatchResult<Self>;

    fn apply(self, base: T) -> PatchResult<T>;
}
