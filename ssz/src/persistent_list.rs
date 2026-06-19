// TODO(32-bit support): Review all uses of `typenum::Unsigned::USIZE`.

// This implementation is optimized for random access. Some of the lists in `BeaconState` are only
// ever appended to or cleared. An implementation specialized for append-only usage could use less
// memory by taking advantage of the fact that intermediate hashes don't need to be retained for
// subtrees that are completely full.

use core::{
    cmp::Ordering,
    fmt::{Debug, Formatter, Result as FmtResult},
    iter::{Flatten, FusedIterator},
    marker::PhantomData,
    ops::{Bound, RangeBounds},
};

use arithmetic::{NonZeroExt as _, U64Ext as _};
use bit_field::BitField as _;
use derivative::Derivative;
use ethereum_types::H256;
use itertools::Itertools as _;
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, SeqAccess, Visitor},
};
use static_assertions::assert_eq_size;
use std_ext::ArcExt as _;
use triomphe::Arc;
use try_from_iterator::TryFromIterator;
use typenum::{U1, U2, U4, Unsigned};

use crate::{
    bundle_size::BundleSize,
    error::{IndexError, PushError, ReadError, WriteError},
    hc::Hc,
    iter::ExactSize,
    merkle_tree::{self, MerkleTree},
    porcelain::{SszHash, SszRead, SszSize, SszWrite},
    shared,
    size::Size,
    type_level::{FitsInU64, MerkleElements, MinimumBundleSize},
    zero_default::ZeroDefault,
};

/// Minimum number of elements a subtree must span before [`PersistentList::par_warm_hash`] forks it
/// onto another Rayon task. Below this the sequential hash is cheaper than the scheduling overhead.
#[cfg(not(target_os = "zkvm"))]
const PARALLEL_HASH_MIN_ELEMENTS: usize = 1 << 14;

#[derive(Derivative)]
#[derivative(
    Clone(bound = "T: Clone"),
    PartialEq(bound = "T: PartialEq"),
    Eq(bound = "T: Eq"),
    Default(bound = "")
)]
pub struct PersistentList<T, N, B = MinimumBundleSize<T>> {
    root: Option<Arc<Hc<Node<T, B>>>>,
    // TODO(32-bit support): Consider changing the type of `length` to `u64`.
    //
    //                       Persistent lists could have more than `usize::MAX` elements due to
    //                       structural sharing, but changing the type of `PersistentList.length`
    //                       may necessitate intrusive changes to the rest of this crate.
    //
    //                       `VALIDATOR_REGISTRY_LIMIT` is 2 ** 40 in the mainnet preset,
    //                       but the number of validators will likely stay far below the maximum.
    //                       Also, `Validator` containers do not benefit from structural sharing,
    //                       so that many validators would not fit in memory on 32 bit machines.
    length: usize,
    phantom: PhantomData<N>,
}

// This could be a `From` impl if feature `generic_const_exprs` were stable.
// See <https://internals.rust-lang.org/t/const-generics-where-restrictions/12742/6>.
impl<T, N, B, const SIZE: usize> TryFrom<[T; SIZE]> for PersistentList<T, N, B>
where
    N: Unsigned,
    B: BundleSize<T>,
{
    type Error = ReadError;

    fn try_from(array: [T; SIZE]) -> Result<Self, Self::Error> {
        Self::validate_length(SIZE)?;
        Self::try_from_iter(array)
    }
}

#[expect(clippy::into_iter_without_iter)]
impl<'list, T, N, B: BundleSize<T>> IntoIterator for &'list PersistentList<T, N, B> {
    type Item = &'list T;
    type IntoIter = ExactSize<Flatten<Leaves<'list, T, B>>>;

    fn into_iter(self) -> Self::IntoIter {
        let mut stack;

        match self.root.as_ref() {
            Some(node) => {
                stack = Vec::with_capacity(self.depth().max(1).into());
                stack.push(node.as_ref().as_ref());
            }
            None => stack = vec![],
        }

        ExactSize::new(Leaves { stack }.flatten(), self.length)
    }
}

#[expect(clippy::into_iter_without_iter)]
impl<'list, T: Clone, N, B: BundleSize<T>> IntoIterator for &'list mut PersistentList<T, N, B> {
    type Item = &'list mut T;
    type IntoIter = ExactSize<Flatten<LeavesMut<'list, T, B>>>;

    fn into_iter(self) -> Self::IntoIter {
        let depth = self.depth();

        let mut stack;

        match self.root.as_mut() {
            Some(node) => {
                stack = Vec::with_capacity(depth.max(1).into());
                stack.push(node.make_mut().as_mut());
            }
            None => stack = vec![],
        }

        ExactSize::new(LeavesMut { stack }.flatten(), self.length)
    }
}

impl<T: Debug, N, B: BundleSize<T>> Debug for PersistentList<T, N, B> {
    fn fmt(&self, formatter: &mut Formatter) -> FmtResult {
        formatter.debug_list().entries(self).finish()
    }
}

impl<T, N: Unsigned, B: BundleSize<T>> TryFromIterator<T> for PersistentList<T, N, B> {
    type Error = ReadError;

    // Unlike `PersistentVector::try_from_iter`, this does not deduplicate consecutive nodes.
    // Due to the nature of data stored in lists, deduplication is far less effective than it is
    // with vectors. Deserializing lists without deduplication is about 20% faster. The absence of
    // deduplication increases memory consumption by a small amount. Interestingly, state
    // transitions appear to be faster when list nodes are not deduplicated. Is it because more
    // `Arc`s are uniquely owned?
    fn try_from_iter(elements: impl IntoIterator<Item = T>) -> Result<Self, Self::Error> {
        let mut length: usize = 0;

        let mut nodes_with_heights = elements
            .into_iter()
            .inspect(|_| length = length.saturating_add(1))
            .chunks(B::USIZE)
            .into_iter()
            .map(Box::from_iter)
            .map(Node::leaf)
            .map(Hc::arc)
            .map(|node| (node, 0))
            .collect_vec();

        Self::validate_length(length)?;

        if length == 0 {
            return Ok(Self::default());
        }

        for _ in 0..B::depth_of_length(length) {
            nodes_with_heights = nodes_with_heights
                .into_iter()
                .chunks(2)
                .into_iter()
                .map(|mut chunk| match (chunk.next(), chunk.next()) {
                    (Some((left, left_height)), Some((right, right_height))) => (
                        Hc::arc(Node::Internal {
                            left,
                            right,
                            left_height,
                            right_height,
                        }),
                        left_height.saturating_add(1),
                    ),
                    (Some(left_over), None) => left_over,
                    _ => unreachable!("Itertools::chunks never yields empty chunks"),
                })
                .collect();
        }

        let (node, root_height) = nodes_with_heights
            .into_iter()
            .exactly_one()
            .ok()
            .expect("only the root should be left");

        assert_eq!(root_height, B::depth_of_length(length));

        Ok(Self {
            root: Some(node),
            length,
            phantom: PhantomData,
        })
    }
}

impl<'de, T, N, B> Deserialize<'de> for PersistentList<T, N, B>
where
    T: Deserialize<'de>,
    N: Unsigned,
    B: BundleSize<T>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PersistentListVisitor<T, N, B>(PhantomData<(T, N, B)>);

        impl<'de, T, N, B> Visitor<'de> for PersistentListVisitor<T, N, B>
        where
            T: Deserialize<'de>,
            N: Unsigned,
            B: BundleSize<T>,
        {
            type Value = PersistentList<T, N, B>;

            fn expecting(&self, formatter: &mut Formatter) -> FmtResult {
                write!(
                    formatter,
                    "a list of length up to {}",
                    shared::saturating_usize::<N>(),
                )
            }

            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                itertools::process_results(
                    core::iter::from_fn(|| seq.next_element().transpose()),
                    |elements| PersistentList::try_from_iter(elements).map_err(S::Error::custom),
                )?
            }
        }

        deserializer.deserialize_seq(PersistentListVisitor(PhantomData))
    }
}

impl<T: Serialize, N, B: BundleSize<T>> Serialize for PersistentList<T, N, B> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self)
    }
}

impl<T: SszSize, N, B> SszSize for PersistentList<T, N, B> {
    const SIZE: Size = Size::Variable { minimum_size: 0 };
}

impl<C, T: SszRead<C>, N: Unsigned, B: BundleSize<T>> SszRead<C> for PersistentList<T, N, B> {
    fn from_ssz_unchecked(context: &C, bytes: &[u8]) -> Result<Self, ReadError> {
        // TODO(32-bit support): remove saturating_usize, in favor of using u64 for max length checks.
        //
        // this saturating_usize is setting hard limit on 32-bit architectures, where maximum length
        // doesn't fit in 4-byte usize. On 32-bit architectures (for instance zkvms), maximum overflows
        // and becomes 0, failing to deserialize valid structures - BeaconState for example, as it has
        // `validators` field, which upper limit is set to 1099511627776 (2^40).
        shared::read_list(shared::saturating_usize::<N>(), context, bytes)
    }
}

impl<T: SszWrite, N, B: BundleSize<T>> SszWrite for PersistentList<T, N, B> {
    fn write_variable(&self, bytes: &mut Vec<u8>) -> Result<(), WriteError> {
        shared::write_list(bytes, self)
    }
}

impl<T, N, B> SszHash for PersistentList<T, N, B>
where
    T: SszHash + SszWrite,
    N: Unsigned,
    B: BundleSize<T> + MerkleElements<T>,
{
    type PackingFactor = U1;

    fn hash_tree_root(&self) -> H256 {
        let root = match self.root.as_ref() {
            Some(node) => (self.depth()..Self::max_depth())
                .map(B::zero_hash)
                .fold(node.hash_tree_root(), hashing::hash_256_256),
            None => B::zero_hash(Self::max_depth()),
        };

        merkle_tree::mix_in_length(root, self.length)
    }
}

impl<T, N, B> PersistentList<T, N, B> {
    #[must_use]
    pub const fn len_usize(&self) -> usize {
        self.length
    }

    #[must_use]
    pub fn len_u64(&self) -> u64
    where
        N: FitsInU64,
    {
        self.length
            .try_into()
            .expect("the bound on N ensures that self.length fits in u64")
    }

    pub fn get(&self, index: u64) -> Result<&T, IndexError>
    where
        B: BundleSize<T>,
    {
        let index = shared::validate_index(self.length, index)?;

        let mut height = self.depth();

        let mut node = self
            .root
            .as_deref()
            .expect("the length check in validate_index ensures that self.root is Some")
            .as_ref();

        let bundle = loop {
            match node {
                Node::Internal {
                    left,
                    right,
                    left_height,
                    right_height,
                } => {
                    assert_eq!(height, left_height.saturating_add(1));

                    let bit_index = height.saturating_add(B::ilog2()).saturating_sub(1).into();

                    if index.get_bit(bit_index) {
                        height = *right_height;
                        node = right;
                    } else {
                        height = *left_height;
                        node = left;
                    }
                }
                Node::Leaf { bundle, .. } => {
                    assert_eq!(height, 0);
                    break bundle;
                }
            }
        };

        Ok(&bundle[B::index_in_bundle(index)])
    }

    pub fn get_mut(&mut self, index: u64) -> Result<&mut T, IndexError>
    where
        T: Clone,
        B: BundleSize<T>,
    {
        let index = shared::validate_index(self.length, index)?;

        let mut height = self.depth();

        let mut node = self
            .root
            .as_mut()
            .expect("the length check in validate_index ensures that self.root is Some")
            .make_mut()
            .as_mut();

        let bundle = loop {
            match node {
                Node::Internal {
                    left,
                    right,
                    left_height,
                    right_height,
                } => {
                    assert_eq!(height, left_height.saturating_add(1));

                    let bit_index = height.saturating_add(B::ilog2()).saturating_sub(1).into();

                    if index.get_bit(bit_index) {
                        height = *right_height;
                        node = right.make_mut();
                    } else {
                        height = *left_height;
                        node = left.make_mut();
                    }
                }
                Node::Leaf { bundle, .. } => {
                    assert_eq!(height, 0);
                    break bundle;
                }
            }
        };

        Ok(&mut bundle[B::index_in_bundle(index)])
    }

    #[must_use]
    pub fn slice<R>(&self, range: R) -> Self
    where
        T: Clone,
        N: Unsigned,
        B: BundleSize<T>,
        R: RangeBounds<usize>,
    {
        let start = match range.start_bound() {
            Bound::Included(&s) => s,
            Bound::Excluded(&s) => s
                .checked_add(1)
                .expect("slice start bound should not overflow"),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&e) => e
                .checked_add(1)
                .expect("slice end bound should not overflow"),
            Bound::Excluded(&e) => e,
            Bound::Unbounded => self.length,
        };

        assert!(start <= end);
        assert!(end <= self.length);

        if start == 0 && end == self.length {
            return self.clone();
        }

        if start == end {
            return Self::default();
        }

        // Unaligned prefix drop: no leaf and therefore no ancestor of a leaf can be reused from
        // the original (every surviving leaf bundle has different contents than any original
        // leaf bundle once you shift by a fractional bundle). Rebuild from elements directly,
        // skipping the suffix prune since its output would just be thrown away.
        if start > 0 && start % B::USIZE != 0 {
            return Self::try_from_iter(self.into_iter().skip(start).take(end - start).cloned())
                .expect("subrange of a valid list is itself a valid list");
        }

        // Drop suffix via in-place prune on a cloned root. Internal subtrees that fall entirely
        // within the kept prefix are reused as-is via Arc sharing; only nodes on the right spine
        // up to `end` are freshly allocated.
        let pruned = if end < self.length {
            let mut root = self
                .root
                .as_ref()
                .expect("non-empty list should have a root")
                .clone_arc();

            root.make_mut().prune(end);

            Self {
                root: Some(root),
                length: end,
                phantom: PhantomData,
            }
        } else {
            self.clone()
        };

        if start == 0 {
            return pruned;
        }

        // Bundle-aligned prefix drop. The surviving subtrees of the original tree can be reused
        // as Arc-shared segments; we collect them and reassemble in a single segment-merge pass
        // so that only the O(depth) bridging internal nodes on the new right spine are freshly
        // built.
        let root = pruned
            .root
            .as_ref()
            .expect("non-empty list should have a root")
            .clone_arc();
        let depth = pruned.depth();

        let mut survivors = Vec::with_capacity(depth.max(1).into());
        Self::collect_drop_prefix(root, depth, pruned.length, start, &mut survivors);

        let new_length = pruned.length - start;

        // Merge all surviving subtrees into a single segment stack in one pass. Only the last
        // surviving subtree may be partial (the original tail); every earlier one is a full
        // subtree contributing exactly one full segment.
        let mut segments: Vec<Segment<T, B>> = Vec::with_capacity(2 * usize::from(depth.max(1)));
        let survivors_len = survivors.len();
        for (i, (node, height, length)) in survivors.into_iter().enumerate() {
            debug_assert_eq!(height, B::depth_of_length(length));
            if i + 1 < survivors_len {
                Self::append_full_segment(
                    &mut segments,
                    Segment {
                        node,
                        height,
                        full: true,
                    },
                );
            } else {
                let mut piece_segments = Vec::with_capacity(usize::from(height) + 1);
                Self::collect_segments(node, height, length, &mut piece_segments);
                for segment in piece_segments {
                    if segment.full {
                        Self::append_full_segment(&mut segments, segment);
                    } else {
                        segments.push(segment);
                    }
                }
            }
        }

        Self::from_segments(segments, new_length)
    }

    fn collect_drop_prefix(
        node: Arc<Hc<Node<T, B>>>,
        height: Height,
        length: usize,
        drop: usize,
        out: &mut Vec<(Arc<Hc<Node<T, B>>>, Height, usize)>,
    ) where
        T: Clone,
        B: BundleSize<T>,
    {
        if drop == 0 {
            out.push((node, height, length));
            return;
        }
        if drop == length {
            return;
        }
        match node.as_ref().as_ref() {
            Node::Internal {
                left,
                right,
                left_height,
                right_height,
            } => {
                debug_assert_eq!(height, *left_height + 1);
                let left_length = length.min(B::USIZE << left_height);
                let right_length = length - left_length;

                if drop >= left_length {
                    Self::collect_drop_prefix(
                        right.clone_arc(),
                        *right_height,
                        right_length,
                        drop - left_length,
                        out,
                    );
                } else {
                    Self::collect_drop_prefix(
                        left.clone_arc(),
                        *left_height,
                        left_length,
                        drop,
                        out,
                    );
                    if right_length > 0 {
                        out.push((right.clone_arc(), *right_height, right_length));
                    }
                }
            }
            Node::Leaf { .. } => {
                // `drop == 0` and `drop == length` are handled above. Any other in-leaf drop
                // would mean the caller passed an un-bundle-aligned `drop`, which `slice`
                // diverts to the iterator-based rebuild before reaching this function.
                unreachable!("bundle-aligned drop should never split a leaf");
            }
        }
    }

    // This clones the elements being visited and checks them for mutations to avoid rebuilding
    // parts of the tree that have not been modified. An `Iterator` that behaves the same way would
    // be more convenient, but items returned by an iterator cannot borrow from the iterator itself.
    // The `streaming-iterator` crate attempts to solve that but falls short because it does not
    // allow mutable borrows.
    pub fn update(&mut self, mut updater: impl FnMut(&mut T))
    where
        T: Clone + PartialEq,
        B: BundleSize<T>,
    {
        if let Some(node) = self.root.as_mut()
            && let Some(new_node) = node.update(&mut updater)
        {
            *node = new_node;
        }
    }

    /// Applies `updater` to the elements at `indices`, calling it with each
    /// `(index, &mut element)` in ascending index order.
    ///
    /// `indices` must be sorted in ascending order and free of duplicates. The spine is
    /// descended a single time, recursing only into subtrees that actually contain a target
    /// index, so the cost is `O(touched_nodes)` rather than the `O(indices.len() * log(len))`
    /// of calling [`Self::get_mut`] in a loop. Only the nodes on the paths to the touched
    /// elements are cloned (copy-on-write) and have their Merkle caches invalidated, so the
    /// structural sharing with the previous version is preserved for every untouched subtree.
    pub fn update_at_sorted_indices(
        &mut self,
        indices: &[u64],
        mut updater: impl FnMut(u64, &mut T),
    ) -> Result<(), IndexError>
    where
        T: Clone,
        B: BundleSize<T>,
    {
        let Some(&max_index) = indices.last() else {
            return Ok(());
        };

        // The indices are sorted, so bounds checking the maximum bounds checks all of them.
        shared::validate_index(self.length, max_index)?;

        debug_assert!(
            indices.windows(2).all(|window| window[0] < window[1]),
            "update_at_sorted_indices requires strictly ascending indices",
        );

        let root = self
            .root
            .as_mut()
            .expect("the length check above ensures that self.root is Some");

        Node::update_at_sorted_indices(root, 0, indices, &mut updater);

        Ok(())
    }

    /// Populates the per-node Merkle caches in parallel, leaving the list in the same state a call
    /// to [`SszHash::hash_tree_root`] would (which afterwards just recombines the cached roots).
    ///
    /// Hashing a freshly loaded list from a cold cache is single-threaded and dominated by the
    /// largest field. Warming it through a parallel tree descent first turns that into a multi-core
    /// pass. The Merkle root is order-independent, so the cached values are identical to the
    /// sequential ones. Subtrees below [`PARALLEL_HASH_MIN_LEAVES`] are warmed sequentially to keep
    /// task overhead off the small/warm cases.
    #[cfg(not(target_os = "zkvm"))]
    pub fn par_warm_hash(&self)
    where
        T: SszHash + SszWrite + Send + Sync,
        B: BundleSize<T> + MerkleElements<T> + Send + Sync,
    {
        if let Some(root) = self.root.as_ref() {
            Node::par_warm(root);
        }
    }

    pub fn extend(&mut self, elements: impl IntoIterator<Item = T>) -> Result<(), PushError>
    where
        T: Clone,
        N: Unsigned,
        B: BundleSize<T>,
    {
        let mut elements = elements.into_iter().collect_vec();

        if elements.is_empty() {
            return Ok(());
        }

        let new_length = self
            .length
            .checked_add(elements.len())
            .ok_or(PushError::ListFull)?;

        Self::validate_length(new_length).map_err(|_| PushError::ListFull)?;

        let tail_length = B::index_in_bundle(self.length);

        if tail_length != 0 {
            let fill_count = (B::USIZE - tail_length).min(elements.len());

            for element in elements.drain(..fill_count) {
                self.push(element)?;
            }
        }

        if elements.is_empty() {
            return Ok(());
        }

        let suffix = Self::try_from_iter(elements)
            .expect("validated length should allow building a suffix list");

        self.extend_batched_suffix(suffix);

        debug_assert_eq!(self.length, new_length);

        Ok(())
    }

    pub fn push(&mut self, element: T) -> Result<(), PushError>
    where
        T: Clone,
        N: Unsigned,
        B: BundleSize<T>,
    {
        // TODO(32-bit support): Review change.
        let length_u64: u64 = self
            .length
            .try_into()
            .expect("PersistentList length counter should fit to u64");

        match length_u64.cmp(&N::U64) {
            Ordering::Less => {}
            Ordering::Equal => return Err(PushError::ListFull),
            Ordering::Greater => unreachable!("case above prevents list from being overfilled"),
        }

        match self.root.as_mut() {
            Some(node) => node.make_mut().push(element, self.length),
            None => self.root = Some(Node::arc_single(element)),
        }

        self.length = self.length.saturating_add(1);

        Ok(())
    }

    pub fn repeat_zero(length: usize) -> Result<Self, ReadError>
    where
        T: ZeroDefault + SszHash + SszWrite + Clone,
        N: Unsigned,
        B: BundleSize<T> + MerkleElements<T>,
    {
        Self::validate_length(length)?;

        if length == 0 {
            return Ok(Self::default());
        }

        // `From<[T; N]>` for `Box` cannot be used here until `generic_const_exprs` is stable.
        let mut node = Node::leaf(vec![T::default(); B::USIZE]);

        // Construct a perfect binary tree with full structural sharing, then prune it.
        for height in 0..B::depth_of_length(length) {
            // This is the part that relies on `T` implementing `ZeroDefault`.
            let hc = Hc::with_root(node, B::zero_hash(height));
            let arc = Arc::new(hc);

            node = Node::Internal {
                left: arc.clone_arc(),
                right: arc,
                left_height: height,
                right_height: height,
            };
        }

        node.prune(length);

        Ok(Self {
            root: Some(Hc::arc(node)),
            length,
            phantom: PhantomData,
        })
    }

    fn depth(&self) -> u8
    where
        B: BundleSize<T>,
    {
        B::depth_of_length(self.length)
    }

    fn max_depth() -> u8
    where
        N: Unsigned,
        B: BundleSize<T>,
    {
        // TODO(32-bit support): Rethink the new code.
        //                       Try to avoid referring to `Unsigned::U64` or `Unsigned::U128`.
        //                       Try to redesign `BundleSize::depth_of_length` to be usable again.
        N::U64.ilog2_ceil().saturating_sub(B::ilog2())
    }

    const fn validate_length(actual: usize) -> Result<(), ReadError>
    where
        N: Unsigned,
    {
        let maximum = shared::saturating_usize::<N>();

        if actual > maximum {
            return Err(ReadError::ListTooLong { maximum, actual });
        }

        Ok(())
    }

    fn extend_batched_suffix(&mut self, suffix: Self)
    where
        B: BundleSize<T>,
    {
        if suffix.length == 0 {
            return;
        }

        if self.length == 0 {
            *self = suffix;
            return;
        }

        debug_assert_eq!(B::index_in_bundle(self.length), 0);

        let length = self.length + suffix.length;
        let mut segments = self.segments();

        for segment in suffix.segments() {
            if segment.full {
                Self::append_full_segment(&mut segments, segment);
            } else {
                segments.push(segment);
            }
        }

        *self = Self::from_segments(segments, length);
    }

    fn segments(&self) -> Vec<Segment<T, B>>
    where
        B: BundleSize<T>,
    {
        let Some(root) = self.root.as_ref() else {
            return vec![];
        };

        let mut segments = Vec::with_capacity(self.depth().max(1).into());
        Self::collect_segments(root.clone_arc(), self.depth(), self.length, &mut segments);
        segments
    }

    fn collect_segments(
        node: Arc<Hc<Node<T, B>>>,
        height: Height,
        length: usize,
        segments: &mut Vec<Segment<T, B>>,
    ) where
        B: BundleSize<T>,
    {
        if length == 0 {
            return;
        }

        let capacity = B::USIZE << height;

        if length == capacity {
            segments.push(Segment {
                node,
                height,
                full: true,
            });
            return;
        }

        match node.as_ref().as_ref() {
            Node::Internal {
                left,
                right,
                left_height,
                right_height,
            } => {
                debug_assert_eq!(height, *left_height + 1);
                let left_length = length.min(B::USIZE << left_height);
                let right_length = length - left_length;

                Self::collect_segments(left.clone_arc(), *left_height, left_length, segments);
                Self::collect_segments(right.clone_arc(), *right_height, right_length, segments);
            }
            Node::Leaf { bundle, .. } => {
                debug_assert_eq!(height, 0);
                let full = bundle.len() == B::USIZE;

                segments.push(Segment { node, height, full });
            }
        }
    }

    fn append_full_segment(segments: &mut Vec<Segment<T, B>>, segment: Segment<T, B>)
    where
        B: BundleSize<T>,
    {
        let Some(last) = segments.last() else {
            segments.push(segment);
            return;
        };

        debug_assert!(segment.full);
        debug_assert!(last.full);

        match last.height.cmp(&segment.height) {
            Ordering::Greater => segments.push(segment),
            Ordering::Less => {
                let (left, right) = Self::split_full_segment(segment);
                Self::append_full_segment(segments, left);
                Self::append_full_segment(segments, right);
            }
            Ordering::Equal => {
                let left = segments
                    .pop()
                    .expect("checked that the segment stack is non-empty");

                let combined = Segment {
                    node: Hc::arc(Node::Internal {
                        left: left.node,
                        right: segment.node,
                        left_height: left.height,
                        right_height: segment.height,
                    }),
                    height: segment.height + 1,
                    full: true,
                };

                Self::append_full_segment(segments, combined);
            }
        }
    }

    fn split_full_segment(segment: Segment<T, B>) -> (Segment<T, B>, Segment<T, B>)
    where
        B: BundleSize<T>,
    {
        debug_assert!(segment.full);
        debug_assert!(segment.height > 0);

        match segment.node.as_ref().as_ref() {
            Node::Internal {
                left,
                right,
                left_height,
                right_height,
            } => {
                debug_assert_eq!(segment.height, *left_height + 1);
                debug_assert_eq!(left_height, right_height);

                (
                    Segment {
                        node: left.clone_arc(),
                        height: *left_height,
                        full: true,
                    },
                    Segment {
                        node: right.clone_arc(),
                        height: *right_height,
                        full: true,
                    },
                )
            }
            Node::Leaf { .. } => unreachable!("a full leaf segment cannot be split further"),
        }
    }

    fn from_segments(segments: Vec<Segment<T, B>>, length: usize) -> Self
    where
        B: BundleSize<T>,
    {
        if length == 0 {
            return Self::default();
        }

        let (root, height) = Self::assemble_segments(&segments)
            .expect("a non-empty list should be assembled from at least one segment");

        debug_assert_eq!(height, Self::depth_of_length(length));

        Self {
            root: Some(root),
            length,
            phantom: PhantomData,
        }
    }

    fn assemble_segments(segments: &[Segment<T, B>]) -> Option<(Arc<Hc<Node<T, B>>>, Height)> {
        match segments {
            [] => None,
            [segment] => Some((segment.node.clone_arc(), segment.height)),
            [left, rest @ ..] => {
                let (right, right_height) = Self::assemble_segments(rest)?;

                debug_assert!(right_height <= left.height);

                Some((
                    Hc::arc(Node::Internal {
                        left: left.node.clone_arc(),
                        right,
                        left_height: left.height,
                        right_height,
                    }),
                    left.height + 1,
                ))
            }
        }
    }

    fn depth_of_length(length: usize) -> u8
    where
        B: BundleSize<T>,
    {
        B::depth_of_length(length)
    }
}

type Height = u8;

struct Segment<T, B> {
    node: Arc<Hc<Node<T, B>>>,
    height: Height,
    full: bool,
}

#[derive(Derivative)]
#[derivative(
    Clone(bound = "T: Clone"),
    PartialEq(bound = "T: PartialEq"),
    Eq(bound = "T: Eq")
)]
enum Node<T, B> {
    Internal {
        left: Arc<Hc<Self>>,
        right: Arc<Hc<Self>>,
        left_height: Height,
        right_height: Height,
    },
    Leaf {
        // Box the bundle to make `Node` smaller at the cost of a small slowdown.
        // This saves ~450 MB (according to profilers) when processing 1024 mainnet Altair blocks.
        // `Box<GenericArrayVec<T, B>>` is easier to use but makes the allocation bigger.
        // Using `Box<[T]>` saves another 50 MB.
        // `Vec` is too complicated for enum layout optimizations.
        bundle: Box<[T]>,
        phantom: PhantomData<B>,
    },
}

assert_eq_size!(Node<H256, U1>, Node<H256, U2>, Node<H256, U4>, [usize; 3]);

impl<T, B> SszHash for Node<T, B>
where
    T: SszHash + SszWrite,
    B: BundleSize<T> + MerkleElements<T>,
{
    type PackingFactor = U1;

    fn hash_tree_root(&self) -> H256 {
        match self {
            Self::Internal {
                left,
                right,
                left_height,
                right_height,
            } => {
                let right_hash = (*right_height..*left_height)
                    .map(B::zero_hash)
                    .fold(right.hash_tree_root(), hashing::hash_256_256);

                hashing::hash_256_256(left.hash_tree_root(), right_hash)
            }
            Self::Leaf { bundle, .. } => {
                if T::PackingFactor::USIZE == 1 {
                    let chunks = bundle.iter().map(SszHash::hash_tree_root);
                    MerkleTree::<<B as MerkleElements<T>>::UnpackedMerkleTreeDepth>
                        ::merkleize_chunks(chunks)
                } else {
                    MerkleTree::<<B as MerkleElements<T>>::PackedMerkleTreeDepth>::merkleize_packed(
                        bundle,
                    )
                }
            }
        }
    }
}

impl<T, B: BundleSize<T>> Node<T, B> {
    fn arc_single(element: T) -> Arc<Hc<Self>> {
        Hc::arc(Self::leaf([element]))
    }

    fn leaf(bundle: impl Into<Box<[T]>>) -> Self {
        let bundle = bundle.into();
        let phantom = PhantomData;

        assert!(bundle.len() <= B::USIZE);

        Self::Leaf { bundle, phantom }
    }

    fn prune(&mut self, mut length: usize)
    where
        T: Clone,
    {
        assert!(0 < length);

        let mut node = self;

        loop {
            match node {
                Self::Internal {
                    left, left_height, ..
                } if B::depth_of_length(length) <= *left_height => {
                    *node = left.as_ref().as_ref().clone();
                }
                Self::Internal {
                    right,
                    right_height,
                    ..
                } => {
                    let left_length = length.next_power_of_two() / 2;
                    let right_length = length.saturating_sub(left_length);

                    assert!(0 < right_length);

                    if left_length == right_length {
                        return;
                    }

                    *right_height = B::depth_of_length(right_length);

                    node = right.make_mut().as_mut();
                    length = right_length;
                }
                Self::Leaf { bundle, .. } => {
                    assert!(length <= B::USIZE);

                    replace_with::replace_with_or_default(bundle, |bundle| {
                        let mut vec = Vec::from(bundle);
                        vec.truncate(length);
                        vec.into_boxed_slice()
                    });

                    return;
                }
            }
        }
    }

    fn push(&mut self, element: T, current_length_and_new_index: usize)
    where
        T: Clone,
    {
        // Leaves are normally never empty. An empty leaf should only be created if the call to
        // `replace_with` below panics. Using `replace_with::replace_with_or_abort` would make this
        // unnecessary but would leave no stacktrace if the code below panicked due to a bug.
        let make_dummy_leaf = || Self::leaf([]);

        replace_with::replace_with(self, make_dummy_leaf, |node| match node {
            Self::Internal {
                left,
                mut right,
                left_height,
                mut right_height,
            } => {
                if Self::pushing_increases_height(current_length_and_new_index) {
                    assert_eq!(left_height, right_height);

                    Self::Internal {
                        left: Hc::arc(Self::Internal {
                            left,
                            right,
                            left_height,
                            right_height,
                        }),
                        right: Self::arc_single(element),
                        left_height: left_height.saturating_add(1),
                        right_height: 0,
                    }
                } else {
                    let left_length = B::USIZE << left_height;
                    assert!(left_length < current_length_and_new_index);

                    let right_length = current_length_and_new_index.saturating_sub(left_length);
                    assert!(right_length < left_length);

                    right.make_mut().push(element, right_length);
                    if Self::pushing_increases_height(right_length) {
                        right_height = right_height.saturating_add(1);
                    }
                    assert!(right_height <= left_height);

                    Self::Internal {
                        left,
                        right,
                        left_height,
                        right_height,
                    }
                }
            }
            Self::Leaf { bundle, .. } => {
                if bundle.len() == B::USIZE {
                    Self::Internal {
                        left: Hc::arc(Self::leaf(bundle)),
                        right: Self::arc_single(element),
                        left_height: 0,
                        right_height: 0,
                    }
                } else {
                    let mut vec = Vec::from(bundle);
                    vec.reserve_exact(1);
                    vec.push(element);
                    Self::leaf(vec)
                }
            }
        })
    }

    // Mutably borrowing an `FnMut` closure inside a recursive function causes infinite recursion
    // during monomorphization. Borrowing it outside and passing the reference prevents that.
    fn update(&self, updater: &mut impl FnMut(&mut T)) -> Option<Arc<Hc<Self>>>
    where
        T: Clone + PartialEq,
    {
        match self {
            Self::Internal {
                left,
                right,
                left_height,
                right_height,
            } => {
                let (left, right) = match (left.update(updater), right.update(updater)) {
                    (Some(new_left), Some(new_right)) => (new_left, new_right),
                    (Some(new_left), None) => (new_left, right.clone_arc()),
                    (None, Some(new_right)) => (left.clone_arc(), new_right),
                    (None, None) => return None,
                };
                Some(Hc::arc(Self::Internal {
                    left,
                    right,
                    left_height: *left_height,
                    right_height: *right_height,
                }))
            }
            Self::Leaf { bundle, .. } => {
                let mut clone = bundle.clone();
                clone.iter_mut().for_each(updater);
                (bundle != &clone).then(|| Hc::arc(Self::leaf(clone)))
            }
        }
    }

    // Descends in place towards `indices` (sorted ascending, all within this node's range,
    // which starts at element `start`), cloning only the nodes on the paths to them. `left` is
    // always a perfect subtree of `left_height`, so it spans exactly `B::USIZE << left_height`
    // elements; that boundary partitions the sorted `indices` between the two children.
    fn update_at_sorted_indices(
        node: &mut Arc<Hc<Self>>,
        start: u64,
        indices: &[u64],
        updater: &mut impl FnMut(u64, &mut T),
    ) where
        T: Clone,
    {
        match node.make_mut().as_mut() {
            Self::Internal {
                left,
                right,
                left_height,
                ..
            } => {
                let mid = start.saturating_add((B::USIZE << *left_height) as u64);
                let split = indices.partition_point(|&index| index < mid);
                let (left_indices, right_indices) = indices.split_at(split);

                if !left_indices.is_empty() {
                    Self::update_at_sorted_indices(left, start, left_indices, updater);
                }

                if !right_indices.is_empty() {
                    Self::update_at_sorted_indices(right, mid, right_indices, updater);
                }
            }
            Self::Leaf { bundle, .. } => {
                // `start` is bundle-aligned, so `index - start` is the position within the leaf.
                for &index in indices {
                    let offset = usize::try_from(index - start).expect("offset fits in usize");
                    updater(index, &mut bundle[offset]);
                }
            }
        }
    }

    fn pushing_increases_height(current_length_and_new_index: usize) -> bool {
        B::index_of_bundle(current_length_and_new_index).is_power_of_two()
            && B::index_in_bundle(current_length_and_new_index) == 0
    }

    // Warms this node's Merkle cache, descending into large subtrees on parallel Rayon tasks first.
    // Children are warmed bottom-up so that the final `hash_tree_root` here only recombines roots
    // that are already cached. Reads only (caches are filled through interior mutability).
    #[cfg(not(target_os = "zkvm"))]
    fn par_warm(node: &Arc<Hc<Self>>)
    where
        T: SszHash + SszWrite + Send + Sync,
        B: MerkleElements<T> + Send + Sync,
    {
        if let Self::Internal {
            left,
            right,
            left_height,
            ..
        } = &***node
        {
            let left_elements = B::USIZE
                .checked_shl(u32::from(*left_height))
                .unwrap_or(usize::MAX);

            if left_elements >= PARALLEL_HASH_MIN_ELEMENTS {
                rayon::join(|| Self::par_warm(left), || Self::par_warm(right));
            }
        }

        let _ = node.hash_tree_root();
    }
}

pub struct Leaves<'list, T, B> {
    // This cannot be an array because array sizes cannot depend on generic parameters. Making this
    // a `GenericArray` of size `PersistentList::depth()` would require a huge number of trait
    // bounds which might not even be expressible because of the lifetime in the element type.
    stack: Vec<&'list Node<T, B>>,
}

impl<'list, T, B> Iterator for Leaves<'list, T, B> {
    type Item = &'list [T];

    fn next(&mut self) -> Option<Self::Item> {
        self.stack.pop().map(|mut node| {
            loop {
                match node {
                    Node::Internal { left, right, .. } => {
                        self.stack.push(right);
                        node = left;
                    }
                    Node::Leaf { bundle, .. } => break bundle.as_ref(),
                }
            }
        })
    }
}

impl<T, B> FusedIterator for Leaves<'_, T, B> {}

// TODO(Grandine Team): `LeavesMut::next` clones `right` nodes earlier than needed.
//                      Try replacing the `Vec` with a stack of mutable references
//                      from the `recursive_reference` or `generic-cursors` crates.
pub struct LeavesMut<'list, T, B> {
    // This cannot be an array because array sizes cannot depend on generic parameters. Making this
    // a `GenericArray` of size `PersistentList::depth()` would require a huge number of trait
    // bounds which might not even be expressible because of the lifetime in the element type.
    stack: Vec<&'list mut Node<T, B>>,
}

impl<'list, T: Clone, B> Iterator for LeavesMut<'list, T, B> {
    type Item = &'list mut [T];

    fn next(&mut self) -> Option<Self::Item> {
        self.stack.pop().map(|mut node| {
            loop {
                match node {
                    Node::Internal { left, right, .. } => {
                        self.stack.push(right.make_mut());
                        node = left.make_mut();
                    }
                    Node::Leaf { bundle, .. } => break bundle.as_mut(),
                }
            }
        })
    }
}

impl<T: Clone, B> FusedIterator for LeavesMut<'_, T, B> {}

#[cfg(test)]
mod tests {
    use try_from_iterator::TryFromIterator as _;
    use typenum::{U64, U1024, U1048576};

    use super::PersistentList;
    use crate::SszHash as _;

    #[test]
    fn extended_list_hash_matches_canonical_list_hash() {
        let mut extended =
            PersistentList::<u64, U64>::try_from_iter(0..33).expect("list should build");

        extended.extend(33..64).expect("list should extend");

        let canonical =
            PersistentList::<u64, U64>::try_from_iter(0..64).expect("list should build");

        assert_eq!(extended.hash_tree_root(), canonical.hash_tree_root());
    }

    #[test]
    fn update_at_sorted_indices_matches_get_mut_loop() {
        for total in [
            1u64, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 64, 100, 128, 129, 200, 257,
        ] {
            let subsets: [Vec<u64>; 5] = [
                vec![0],
                vec![total - 1],
                (0..total).collect(),
                (0..total).step_by(5).collect(),
                (0..total).step_by(17).collect(),
            ];

            for indices in subsets {
                if indices.is_empty() {
                    continue;
                }

                let mut batched =
                    PersistentList::<u64, U1024>::try_from_iter(0..total).expect("list should build");
                batched
                    .update_at_sorted_indices(&indices, |index, value| {
                        *value = value.wrapping_add(1000 + index);
                    })
                    .expect("indices are in bounds");

                let mut reference =
                    PersistentList::<u64, U1024>::try_from_iter(0..total).expect("list should build");
                for &index in &indices {
                    let value = reference.get_mut(index).expect("index is in bounds");
                    *value = value.wrapping_add(1000 + index);
                }

                assert_eq!(batched, reference, "values differ (total={total})");
                assert_eq!(
                    batched.hash_tree_root(),
                    reference.hash_tree_root(),
                    "roots differ (total={total})",
                );
            }
        }
    }

    #[test]
    fn par_warm_hash_matches_sequential_hash() {
        for total in [0u64, 1, 4, 5, 64, 1000, 16_384, 40_000, 70_000] {
            let warmed = PersistentList::<u64, U1048576>::try_from_iter(0..total)
                .expect("list should build");
            warmed.par_warm_hash();

            let sequential = PersistentList::<u64, U1048576>::try_from_iter(0..total)
                .expect("list should build");

            assert_eq!(
                warmed.hash_tree_root(),
                sequential.hash_tree_root(),
                "roots differ after par_warm_hash (total={total})",
            );
        }
    }

    #[test]
    fn update_at_sorted_indices_empty_is_noop() {
        let mut list = PersistentList::<u64, U1024>::try_from_iter(0..50).expect("list should build");
        let before = list.hash_tree_root();
        list.update_at_sorted_indices(&[], |_, _| unreachable!("no indices"))
            .expect("empty update succeeds");
        assert_eq!(list.hash_tree_root(), before);
    }

    #[test]
    fn update_at_sorted_indices_rejects_out_of_bounds() {
        let mut list = PersistentList::<u64, U1024>::try_from_iter(0..10).expect("list should build");
        list.update_at_sorted_indices(&[5, 10], |_, _| {})
            .expect_err("index 10 is out of bounds");
    }

    #[test]
    fn slice_suffix_keeps_prefix() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..10).expect("list should build");
        let actual = list.slice(..4);
        let expected = PersistentList::<u64, U64>::try_from_iter(0..4).expect("list should build");
        assert_eq!(actual, expected);
        assert_eq!(actual.hash_tree_root(), expected.hash_tree_root());
    }

    #[test]
    fn slice_prefix_drops_head_aligned() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..32).expect("list should build");
        let actual = list.slice(8..);
        let expected = PersistentList::<u64, U64>::try_from_iter(8..32).expect("list should build");
        assert_eq!(actual, expected);
        assert_eq!(actual.hash_tree_root(), expected.hash_tree_root());
    }

    #[test]
    fn slice_prefix_drops_head_unaligned() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..32).expect("list should build");
        let actual = list.slice(3..);
        let expected = PersistentList::<u64, U64>::try_from_iter(3..32).expect("list should build");
        assert_eq!(actual, expected);
        assert_eq!(actual.hash_tree_root(), expected.hash_tree_root());
    }

    #[test]
    fn slice_prefix_drops_partial_tail_aligned() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..5).expect("list should build");
        let actual = list.slice(4..);
        let expected = PersistentList::<u64, U64>::try_from_iter(4..5).expect("list should build");
        assert_eq!(actual, expected);
        assert_eq!(actual.hash_tree_root(), expected.hash_tree_root());
    }

    #[test]
    fn slice_prefix_aligned_various_sizes() {
        for total in [1usize, 2, 3, 4, 7, 8, 13, 16, 17, 31, 32, 33, 100, 128, 129] {
            let list = PersistentList::<u64, U1024>::try_from_iter(0..total as u64)
                .expect("list should build");
            for start in [0usize, 1, 4, 8, 16, 32, 64] {
                if start > total {
                    break;
                }
                let actual = list.slice(start..);
                let expected =
                    PersistentList::<u64, U1024>::try_from_iter(start as u64..total as u64)
                        .expect("list should build");
                assert_eq!(actual, expected, "slice({}..) of 0..{}", start, total);
                assert_eq!(
                    actual.hash_tree_root(),
                    expected.hash_tree_root(),
                    "hash mismatch for slice({}..) of 0..{}",
                    start,
                    total,
                );
            }
        }
    }

    #[test]
    fn slice_full_range_equals_clone() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..10).expect("list should build");
        assert_eq!(list.slice(..), list);
        assert_eq!(list.slice(0..10), list);
    }

    #[test]
    fn slice_subrange_drops_both_ends() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..32).expect("list should build");
        let actual = list.slice(4..16);
        let expected = PersistentList::<u64, U64>::try_from_iter(4..16).expect("list should build");
        assert_eq!(actual, expected);
        assert_eq!(actual.hash_tree_root(), expected.hash_tree_root());
    }

    #[test]
    fn slice_empty_range_yields_empty_list() {
        let list = PersistentList::<u64, U64>::try_from_iter(0..10).expect("list should build");
        assert_eq!(list.slice(5..5).len_usize(), 0);
    }
}
