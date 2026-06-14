//! W2.6: the pane resize contract (#2048), named.
//!
//! A pane's PTY/vterm is sized at TWO chokepoints — kept deliberately separate
//! by #2048:
//!
//! 1. **Layout pre-computes** (`layout::collect_resize_needs` →
//!    [`crate::layout::Pane::resize_pty`]): derives `(cols, rows)` from the split
//!    geometry and sizes the PTY before the first frame. An ESTIMATE — enough to
//!    spawn the child with a sane size, but not necessarily the final on-screen
//!    rect (chrome / rounding / a mid-frame layout change can shift it).
//!
//! 2. **Render is AUTHORITATIVE** (`core_render::render_pane` and `scratch`):
//!    recomputes the pane's content rect from the actually-rendered `area` and
//!    corrects the vterm + PTY to it. The final rendered inner rect WINS — this
//!    is what keeps the PTY rows aligned with what is actually drawn (#2046/#2048,
//!    `render_resizes_vterm_to_pane_content_rows_2046`).
//!
//! These two helpers NAME that contract: [`PaneContentRect`] is the content
//! region of a bordered pane; [`ResizeDecision`] is the render-time "does the
//! vterm need correcting to the content rect?" decision.
//!
//! Do NOT remove the render-time resize: the layout estimate alone leaves the
//! PTY misaligned on the frames between a layout change and the next layout pass,
//! and the render path is the only one with the truly-final rect.

use ratatui::layout::Rect;

/// The content region of a bordered pane: its outer `area` inset by the 1-cell
/// border on every side. This rect is the authority for the pane's PTY / vterm
/// dimensions — the agent's terminal content occupies exactly these cells.
///
/// Equivalent to `Block::default().borders(Borders::ALL).inner(area)` — ratatui
/// insets each border with `saturating_sub`, and a title rides on the top border
/// without consuming an extra row. It is written explicitly here so the edge
/// behaviour is local and the concept has a name; `core_render::render_pane`
/// (which does not build a `Block` for the inner calc) uses it, while the
/// `scratch` path derives the same rect via `block.inner(oa)` because it already
/// constructs the `Block` to draw the border. The equivalence is pinned by
/// `pane_content_rect_matches_block_inner_borders_all`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneContentRect(pub Rect);

impl PaneContentRect {
    /// Inset `area` by the pane border (1 cell on each side).
    pub fn from_bordered_area(area: Rect) -> Self {
        PaneContentRect(Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        ))
    }

    /// The inner content rect.
    pub fn rect(self) -> Rect {
        self.0
    }

    /// True when the content area has zero cells (the border consumed the whole
    /// pane). Render must skip vterm/PTY work in that case.
    pub fn is_empty(self) -> bool {
        self.0.width == 0 || self.0.height == 0
    }
}

/// The render-time resize decision — the AUTHORITATIVE half of the contract.
///
/// [`ResizeDecision::needed`] returns `Some` with the target dimensions when the
/// content rect differs from the vterm's current size (render must correct the
/// vterm + PTY) and `None` when they already match (the steady-state frame, no
/// resize work). The decision is identical at both render chokepoints; only the
/// way each computes its content rect differs (see [`PaneContentRect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResizeDecision {
    pub cols: u16,
    pub rows: u16,
}

impl ResizeDecision {
    /// Decide whether `content` requires resizing a vterm currently sized
    /// `vterm_cols` × `vterm_rows`.
    pub fn needed(content: Rect, vterm_cols: u16, vterm_rows: u16) -> Option<Self> {
        if content.width != vterm_cols || content.height != vterm_rows {
            Some(ResizeDecision {
                cols: content.width,
                rows: content.height,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_content_rect_insets_border() {
        let c = PaneContentRect::from_bordered_area(Rect::new(0, 0, 40, 20));
        assert_eq!(c.rect(), Rect::new(1, 1, 38, 18));
        assert!(!c.is_empty());
    }

    #[test]
    fn pane_content_rect_saturates_tiny_area() {
        // 1×1 area → border consumes everything → empty content (no underflow).
        let c = PaneContentRect::from_bordered_area(Rect::new(5, 5, 1, 1));
        assert_eq!(c.rect(), Rect::new(6, 6, 0, 0));
        assert!(c.is_empty());
    }

    #[test]
    fn pane_content_rect_matches_block_inner_borders_all() {
        // The contract relies on this equivalence: the `scratch` render path
        // computes the same content rect via `block.inner(oa)`.
        use ratatui::widgets::{Block, Borders};
        let area = Rect::new(2, 3, 30, 12);
        let block_inner = Block::default().borders(Borders::ALL).inner(area);
        assert_eq!(
            PaneContentRect::from_bordered_area(area).rect(),
            block_inner
        );
    }

    #[test]
    fn resize_decision_some_when_differs_none_when_equal() {
        let content = Rect::new(1, 1, 38, 18);
        assert_eq!(ResizeDecision::needed(content, 38, 18), None);
        assert_eq!(
            ResizeDecision::needed(content, 10, 18),
            Some(ResizeDecision { cols: 38, rows: 18 })
        );
        assert_eq!(
            ResizeDecision::needed(content, 38, 5),
            Some(ResizeDecision { cols: 38, rows: 18 })
        );
    }
}
