//! [`Slot<T>`] — explicit-tagged optional storage cell.
//!
//! Semantically equivalent to `Option<T>` but with a fixed `#[repr(C)]`
//! layout that makes the all-zero byte pattern decode to the empty state
//! (`occupied: false`). Rust's stock `Option<T>` is free to use
//! niche-encoded discriminants where the all-zero pattern often decodes
//! as `Some(<garbage>)` — which corrupts any storage placed in
//! NOLOAD / runtime-zero-filled memory (PSRAM, DMA regions, shared
//! memory across cores).
//!
//! Use `Slot<T>` for any `[Option<T>; N]`-shaped storage that may live
//! in such memory. Hot-path local variables can keep `Option<T>` —
//! niche encoding doesn't bite when types are constructed via
//! `Option::None` / `Option::Some`.

use core::mem::MaybeUninit;

#[repr(C)]
pub struct Slot<T> {
    occupied: bool,
    state: MaybeUninit<T>,
}

impl<T> Slot<T> {
    pub const EMPTY: Self = Self {
        occupied: false,
        state: MaybeUninit::uninit(),
    };

    pub const fn new() -> Self {
        Self::EMPTY
    }

    pub fn is_some(&self) -> bool {
        self.occupied
    }

    pub fn is_none(&self) -> bool {
        !self.occupied
    }

    pub fn as_ref(&self) -> Option<&T> {
        if self.occupied {
            // SAFETY: `occupied == true` is the invariant maintained by
            // every mutating method below — the only writer of the state
            // field that flips occupied to `true` also runs `MaybeUninit::write`.
            Some(unsafe { self.state.assume_init_ref() })
        } else {
            None
        }
    }

    pub fn as_mut(&mut self) -> Option<&mut T> {
        if self.occupied {
            // SAFETY: see `as_ref`.
            Some(unsafe { self.state.assume_init_mut() })
        } else {
            None
        }
    }

    /// Replace contents, dropping any previous value. Returns the old.
    pub fn replace(&mut self, value: T) -> Option<T> {
        let old = if self.occupied {
            // SAFETY: previous occupancy implies state was initialised.
            // We're moving it out; mark non-occupied before write to keep
            // the invariant valid even if `T::drop` (called when `old` is
            // dropped) panics — though we re-flip below, the intermediate
            // state is consistent.
            self.occupied = false;
            Some(unsafe { self.state.assume_init_read() })
        } else {
            None
        };
        self.state.write(value);
        self.occupied = true;
        old
    }

    /// Take the value out, leaving the slot empty.
    pub fn take(&mut self) -> Option<T> {
        if self.occupied {
            self.occupied = false;
            // SAFETY: previous occupancy implies state was initialised.
            Some(unsafe { self.state.assume_init_read() })
        } else {
            None
        }
    }
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<T> Drop for Slot<T> {
    fn drop(&mut self) {
        if self.occupied {
            // SAFETY: `occupied == true` ⇒ state was initialised.
            unsafe { core::ptr::drop_in_place(self.state.as_mut_ptr()) };
        }
    }
}

impl<T: Clone> Clone for Slot<T> {
    fn clone(&self) -> Self {
        let mut s = Self::EMPTY;
        if let Some(v) = self.as_ref() {
            s.replace(v.clone());
        }
        s
    }
}

impl<T: PartialEq> PartialEq for Slot<T> {
    fn eq(&self, other: &Self) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl<T: Eq> Eq for Slot<T> {}

impl<T: core::fmt::Debug> core::fmt::Debug for Slot<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.as_ref() {
            Some(v) => f.debug_tuple("Slot").field(v).finish(),
            None => f.write_str("Slot::EMPTY"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Slot;

    #[test]
    fn empty_is_none() {
        let s: Slot<u32> = Slot::EMPTY;
        assert!(s.is_none());
        assert_eq!(s.as_ref(), None);
    }

    #[test]
    fn replace_returns_none_first_time() {
        let mut s: Slot<u32> = Slot::EMPTY;
        assert_eq!(s.replace(42), None);
        assert_eq!(s.as_ref(), Some(&42));
    }

    #[test]
    fn replace_returns_old_value() {
        let mut s: Slot<u32> = Slot::EMPTY;
        s.replace(1);
        assert_eq!(s.replace(2), Some(1));
        assert_eq!(s.as_ref(), Some(&2));
    }

    #[test]
    fn take_returns_value_and_empties_slot() {
        let mut s: Slot<u32> = Slot::EMPTY;
        s.replace(42);
        assert_eq!(s.take(), Some(42));
        assert!(s.is_none());
    }

    #[test]
    fn zeroed_slot_is_empty() {
        // The whole point: an all-zero byte pattern must decode to None,
        // not Some(<garbage>) — that's what `Option<T>` gets wrong when
        // it picks niche encoding.
        let s: Slot<u32> = unsafe { core::mem::zeroed() };
        assert!(s.is_none());
        // Don't drop — drop wouldn't panic for u32, but for types with
        // Drop on garbage state it could be UB. The Slot's Drop impl
        // checks `occupied` first, so it's safe even from zeroed.
    }

    #[test]
    fn zeroed_slot_with_drop_type_is_empty() {
        // Drop runs but observes `occupied == false`, so does not touch
        // the (uninitialised) inner T.
        let _s: Slot<std::vec::Vec<u32>> = unsafe { core::mem::zeroed() };
    }

    #[test]
    fn drop_calls_inner_drop_when_occupied() {
        use std::rc::Rc;
        let counter = Rc::new(());
        {
            let mut s: Slot<Rc<()>> = Slot::EMPTY;
            s.replace(counter.clone());
            assert_eq!(Rc::strong_count(&counter), 2);
            // s drops here
        }
        assert_eq!(Rc::strong_count(&counter), 1);
    }
}
