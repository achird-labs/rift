//! Dense candidate bitsets for the matching index (issue #707).
//!
//! Stub ids are positions in the snapshot's stub vector — dense, ascending, declaration-ordered —
//! so a candidate set is naturally a fixed-width bitset rather than a `Vec<usize>`. The matching
//! index ANDs one bitset per dimension and then walks the surviving bits in ascending order, which
//! is exactly Mountebank's first-match-wins order.
//!
//! Hand-rolled over a `fixedbitset`/`roaring` dependency on purpose: the operations needed are
//! intersect, union, and ascending iteration; the design point is ≤ a few thousand stubs (4096
//! stubs = 512 B), where a dense word vector is both smaller and faster than a compressed
//! representation, and the word-wise AND autovectorizes.

/// Bits per backing word.
const WORD: usize = u64::BITS as usize;

/// A dense set of stub ids in `0..len`.
///
/// Bits at or above `len` are always zero — every constructor and mutator maintains this, so
/// [`Self::iter`] and [`Self::count`] can never report a phantom id past the end of the stub
/// vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateBits {
    words: Vec<u64>,
    len: usize,
}

impl CandidateBits {
    /// An empty set over `0..len`.
    #[must_use]
    pub(crate) fn zeros(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(WORD)],
            len,
        }
    }

    /// The full set `0..len` — the identity for [`Self::intersect_with`], and what a dimension's
    /// `always_bits` degenerates to when it can index nothing.
    #[must_use]
    pub(crate) fn all(len: usize) -> Self {
        let mut bits = Self {
            words: vec![u64::MAX; len.div_ceil(WORD)],
            len,
        };
        bits.mask_tail();
        bits
    }

    /// Clear the bits above `len` in the final word. `u64::MAX` fills whole words, so a `len` that
    /// isn't a word multiple would otherwise leave phantom ids set.
    fn mask_tail(&mut self) {
        let rem = self.len % WORD;
        if rem != 0
            && let Some(last) = self.words.last_mut()
        {
            *last &= (1u64 << rem) - 1;
        }
    }

    /// Add `id` to the set. Ids at or above `len` are not representable and are ignored.
    pub(crate) fn set(&mut self, id: usize) {
        if id < self.len {
            self.words[id / WORD] |= 1u64 << (id % WORD);
        }
    }

    /// Whether `id` is in the set.
    ///
    /// Gated to match its consumers (issue #1000): the only production caller is
    /// `body_field_discriminates`, which is `quamina-matching`-gated, and the bitset's own unit
    /// tests use it unconditionally. Without the `test` arm this is dead in a
    /// `--no-default-features` *lib* build — which rustc now reports, since nothing silences it.
    #[cfg(any(feature = "quamina-matching", test))]
    #[must_use]
    pub(crate) fn contains(&self, id: usize) -> bool {
        id < self.len && self.words[id / WORD] & (1u64 << (id % WORD)) != 0
    }

    /// Overwrite this set with `other`, reusing the existing allocation. Lets the match loop keep
    /// one scratch bitset per request instead of allocating per dimension.
    pub(crate) fn copy_from(&mut self, other: &Self) {
        debug_assert_eq!(self.len, other.len, "bitsets span different id spaces");
        self.words.copy_from_slice(&other.words);
    }

    /// Intersect in place (`self &= other`) — the dimension-combining operation.
    pub(crate) fn intersect_with(&mut self, other: &Self) {
        debug_assert_eq!(self.len, other.len, "bitsets span different id spaces");
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= *b;
        }
    }

    /// Whether no id is set — lets the match loop bail before the remaining dimensions.
    ///
    /// Note this is emptiness of the *set*, not of the id space: `zeros(10).is_empty()` is true.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// How many ids are in the set.
    ///
    /// Gated for the same reason as [`Self::contains`], and on the same terms.
    #[cfg(any(feature = "quamina-matching", test))]
    #[must_use]
    pub(crate) fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// The ids in the set, ascending — i.e. in stub declaration order, which is what makes
    /// first-match-wins fall out of iteration.
    pub(crate) fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(w, word)| {
            let mut rest = *word;
            std::iter::from_fn(move || {
                if rest == 0 {
                    return None;
                }
                let bit = rest.trailing_zeros() as usize;
                rest &= rest - 1;
                Some(w * WORD + bit)
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeros_is_empty_and_all_is_full() {
        let z = CandidateBits::zeros(10);
        assert!(z.is_empty());
        assert_eq!(z.count(), 0);

        let a = CandidateBits::all(10);
        assert!(!a.is_empty());
        assert_eq!(a.count(), 10);
        assert_eq!(a.iter().collect::<Vec<_>>(), (0..10).collect::<Vec<_>>());
    }

    // The tail-masking invariant: `all()` fills whole u64 words, so a non-word-multiple `len` must
    // still not report ids past the end — otherwise the match loop would index out of bounds.
    #[test]
    fn all_masks_bits_past_len() {
        for len in [0usize, 1, 63, 64, 65, 127, 128, 129, 200] {
            let a = CandidateBits::all(len);
            assert_eq!(a.count(), len, "all({len}) must contain exactly len ids");
            assert_eq!(a.iter().max(), len.checked_sub(1), "no phantom id past len");
            assert!(!a.contains(len), "all({len}) must not contain id len");
        }
    }

    #[test]
    fn set_and_contains_across_word_boundaries() {
        let mut b = CandidateBits::zeros(200);
        for id in [0usize, 1, 63, 64, 65, 127, 128, 199] {
            b.set(id);
        }
        assert_eq!(
            b.iter().collect::<Vec<_>>(),
            vec![0, 1, 63, 64, 65, 127, 128, 199]
        );
        assert!(b.contains(64) && !b.contains(66));
        assert_eq!(b.count(), 8);
    }

    // Out-of-range ids are ignored rather than panicking or corrupting a neighbouring word.
    #[test]
    fn set_ignores_out_of_range_ids() {
        let mut b = CandidateBits::zeros(64);
        b.set(64);
        b.set(1000);
        assert!(b.is_empty());
        assert!(!b.contains(64));
    }

    #[test]
    fn intersect_keeps_only_common_ids() {
        let mut a = CandidateBits::zeros(128);
        [1usize, 5, 70].iter().for_each(|i| a.set(*i));
        let mut b = CandidateBits::zeros(128);
        [5usize, 70, 100].iter().for_each(|i| b.set(*i));

        a.intersect_with(&b);
        assert_eq!(a.iter().collect::<Vec<_>>(), vec![5, 70]);
    }

    // `all()` is the identity for intersection — the match loop seeds the accumulator with it.
    #[test]
    fn all_is_the_intersection_identity() {
        let mut b = CandidateBits::zeros(70);
        [3usize, 69].iter().for_each(|i| b.set(*i));
        let mut acc = CandidateBits::all(70);
        acc.intersect_with(&b);
        assert_eq!(acc, b);
    }

    #[test]
    fn copy_from_overwrites_in_place() {
        let mut src = CandidateBits::zeros(70);
        src.set(9);
        let mut dst = CandidateBits::all(70);
        dst.copy_from(&src);
        assert_eq!(dst, src);
        assert_eq!(dst.iter().collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn empty_id_space_is_degenerate_but_sound() {
        let a = CandidateBits::all(0);
        assert!(a.is_empty());
        assert_eq!(a.count(), 0);
        assert_eq!(a.iter().next(), None);
    }
}
