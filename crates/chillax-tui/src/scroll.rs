//! Scroll offsets for lists drawn from `&self`: the window keeps its place while the
//! selection is visible and moves only far enough to bring it back.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The first row shown in a list window, remembered between draws.
#[derive(Debug, Default)]
pub(crate) struct Sticky(AtomicUsize);

impl Clone for Sticky {
    fn clone(&self) -> Self {
        Self(AtomicUsize::new(self.0.load(Ordering::Relaxed)))
    }
}

/// View state is not part of a value's identity.
impl PartialEq for Sticky {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Sticky {}

impl Sticky {
    /// First row to draw so row `selected` of `len` shows in a window of `height` rows.
    pub(crate) fn reveal(&self, selected: usize, len: usize, height: usize) -> usize {
        let top = reveal_from(self.0.load(Ordering::Relaxed), selected, len, height);
        self.0.store(top, Ordering::Relaxed);
        top
    }
}

/// The new top for a window that was at `top`.
pub(crate) fn reveal_from(top: usize, selected: usize, len: usize, height: usize) -> usize {
    let height = height.max(1);
    let mut top = top.min(len.saturating_sub(height));
    if selected < top {
        top = selected;
    } else if selected >= top + height {
        top = selected + 1 - height;
    }
    top
}
