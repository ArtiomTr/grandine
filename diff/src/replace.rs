use ssz::{Ssz, SszRead, SszReadDefault, SszWrite};

use crate::{Patch, PatchResult};

#[derive(Clone, Debug, Default, Ssz)]
#[ssz(
    transparent,
    derive_hash = false,
    bound = "T: SszWrite",
    bound_for_read = "T: SszRead<C> + SszWrite"
)]
pub struct ReplacePatch<T>(T);

impl<T: SszReadDefault + SszWrite + Clone> Patch<T> for ReplacePatch<T> {
    fn diff(_base: &T, changed: &T) -> PatchResult<Self> {
        Ok(ReplacePatch(changed.clone()))
    }

    fn apply(self, _base: T) -> PatchResult<T> {
        Ok(self.0)
    }
}
