use std::ops::Range;

use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::LineAnnotation;
use helix_core::Position;

use crate::{Document, ViewId};

/// A diff hunk that has been expanded to show its removed text inline.
#[derive(Debug, Clone)]
pub struct ExpandedHunk {
    /// Char index of the line ending after which the removed text is shown.
    /// This is the end of the line above the hunk (or the end of the first
    /// line for hunks at the very start of the document).
    pub anchor: usize,
    /// Line range of the hunk in the diff base ("old" text).
    pub before: Range<u32>,
    /// Line range of the hunk in the current document text.
    pub after: Range<u32>,
}

/// Collects the expanded hunks of `view_id` resolved against the current
/// diff. Stale entries which no longer match a hunk are skipped.
pub fn expanded_hunks(doc: &Document, view_id: ViewId) -> Vec<ExpandedHunk> {
    let Some(expanded) = doc.expanded_diff_hunks(view_id) else {
        return Vec::new();
    };
    let Some(diff_handle) = doc.diff_handle() else {
        return Vec::new();
    };
    let text = doc.text().slice(..);
    let diff = diff_handle.load();
    (0..diff.len())
        .filter_map(|i| {
            let hunk = diff.nth_hunk(i);
            if !expanded.contains(&hunk.after.start) {
                return None;
            }
            let anchor_line =
                (hunk.after.start.saturating_sub(1) as usize).min(text.len_lines() - 1);
            Some(ExpandedHunk {
                anchor: line_end_char_index(&text, anchor_line),
                before: hunk.before.clone(),
                after: hunk.after.clone(),
            })
        })
        .collect()
}

/// Reserves virtual lines for the removed text of expanded diff hunks.
/// The reserved lines are filled by the matching decoration in the
/// terminal frontend.
pub struct InlineDiffAnnotation {
    /// `(anchor char idx, number of removed lines)` sorted by anchor
    hunks: Vec<(usize, usize)>,
    idx: usize,
    rows: usize,
}

impl InlineDiffAnnotation {
    pub fn new(doc: &Document, view_id: ViewId) -> Self {
        let hunks = expanded_hunks(doc, view_id)
            .into_iter()
            .filter(|hunk| !hunk.before.is_empty())
            .map(|hunk| (hunk.anchor, hunk.before.len()))
            .collect();
        InlineDiffAnnotation {
            hunks,
            idx: 0,
            rows: 0,
        }
    }

    fn next_anchor(&self) -> usize {
        self.hunks
            .get(self.idx)
            .map_or(usize::MAX, |&(anchor, _)| anchor)
    }
}

impl LineAnnotation for InlineDiffAnnotation {
    fn reset_pos(&mut self, char_idx: usize) -> usize {
        self.idx = self
            .hunks
            .partition_point(|&(anchor, _)| anchor < char_idx);
        self.rows = 0;
        self.next_anchor()
    }

    fn skip_concealed_anchors(&mut self, conceal_end_char_idx: usize) -> usize {
        self.idx += self.hunks[self.idx..]
            .partition_point(|&(anchor, _)| anchor < conceal_end_char_idx);
        self.next_anchor()
    }

    fn process_anchor(&mut self, _grapheme: &FormattedGrapheme) -> usize {
        self.rows += self.hunks[self.idx].1;
        self.idx += 1;
        self.next_anchor()
    }

    fn insert_virtual_lines(
        &mut self,
        _line_end_char_idx: usize,
        _line_end_visual_pos: Position,
        _doc_line: usize,
    ) -> Position {
        Position::new(std::mem::take(&mut self.rows), 0)
    }
}
