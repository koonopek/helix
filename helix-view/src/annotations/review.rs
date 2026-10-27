use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::line_ending::line_end_char_index;
use helix_core::text_annotations::LineAnnotation;
use helix_core::Position;

use crate::Document;

/// `(anchor char idx, comment idx)` of the review comments of `doc` sorted by
/// anchor. A comment is shown below the last line it is attached to, so its
/// anchor is the line ending of that line.
pub fn review_comment_anchors(doc: &Document) -> Vec<(usize, usize)> {
    let text = doc.text().slice(..);
    let mut anchors: Vec<_> = doc
        .review_comments()
        .iter()
        .enumerate()
        .map(|(i, comment)| {
            (
                line_end_char_index(&text, text.char_to_line(comment.end)),
                i,
            )
        })
        .collect();
    anchors.sort_unstable();
    anchors
}

/// Number of virtual lines a review comment occupies.
pub fn review_comment_height(comment: &str) -> usize {
    comment.lines().count().max(1)
}

/// Reserves virtual lines below commented lines. The reserved lines are
/// filled by the matching decoration in the terminal frontend.
pub struct ReviewCommentAnnotation {
    /// `(anchor char idx, number of lines)` sorted by anchor
    comments: Vec<(usize, usize)>,
    idx: usize,
    rows: usize,
}

impl ReviewCommentAnnotation {
    pub fn new(doc: &Document) -> Self {
        let comments = review_comment_anchors(doc)
            .into_iter()
            .map(|(anchor, i)| {
                let height = review_comment_height(&doc.review_comments()[i].text);
                (anchor, height)
            })
            .collect();
        ReviewCommentAnnotation {
            comments,
            idx: 0,
            rows: 0,
        }
    }

    fn next_anchor(&self) -> usize {
        self.comments
            .get(self.idx)
            .map_or(usize::MAX, |&(anchor, _)| anchor)
    }
}

impl LineAnnotation for ReviewCommentAnnotation {
    fn reset_pos(&mut self, char_idx: usize) -> usize {
        self.idx = self
            .comments
            .partition_point(|&(anchor, _)| anchor < char_idx);
        self.rows = 0;
        self.next_anchor()
    }

    fn skip_concealed_anchors(&mut self, conceal_end_char_idx: usize) -> usize {
        self.idx +=
            self.comments[self.idx..].partition_point(|&(anchor, _)| anchor < conceal_end_char_idx);
        self.next_anchor()
    }

    fn process_anchor(&mut self, _grapheme: &FormattedGrapheme) -> usize {
        self.rows += self.comments[self.idx].1;
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
