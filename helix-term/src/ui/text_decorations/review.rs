use helix_core::doc_formatter::FormattedGrapheme;
use helix_core::Position;
use helix_view::annotations::review::{review_comment_anchors, review_comment_height};
use helix_view::graphics::Rect;
use helix_view::theme::Style;
use helix_view::{Document, Theme};

use crate::ui::document::{LinePos, TextRenderer};
use crate::ui::text_decorations::Decoration;

const PREFIX: &str = "  ┃ ";

/// Renders review comments into the virtual lines reserved by
/// [`helix_view::annotations::review::ReviewCommentAnnotation`].
pub struct ReviewComments<'a> {
    doc: &'a Document,
    /// `(anchor char idx, comment idx)` sorted by anchor
    anchors: Vec<(usize, usize)>,
    idx: usize,
    /// Comments to render at the end of the current visual line
    armed: Vec<usize>,
    style: Style,
}

impl<'a> ReviewComments<'a> {
    pub fn new(doc: &'a Document, theme: &Theme) -> Option<Self> {
        if doc.review_comments().is_empty() {
            return None;
        }
        Some(ReviewComments {
            doc,
            anchors: review_comment_anchors(doc),
            idx: 0,
            armed: Vec::new(),
            style: theme
                .try_get("ui.virtual.review-comment")
                .unwrap_or_else(|| theme.get("info")),
        })
    }

    fn next_anchor(&self) -> usize {
        self.anchors
            .get(self.idx)
            .map_or(usize::MAX, |&(anchor, _)| anchor)
    }
}

impl Decoration for ReviewComments<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.idx = self.anchors.partition_point(|&(anchor, _)| anchor < pos);
        self.armed.clear();
        self.next_anchor()
    }

    fn skip_concealed_anchor(&mut self, conceal_end_char_idx: usize) -> usize {
        self.idx +=
            self.anchors[self.idx..].partition_point(|&(anchor, _)| anchor < conceal_end_char_idx);
        self.next_anchor()
    }

    fn decorate_grapheme(
        &mut self,
        _renderer: &mut TextRenderer,
        _grapheme: &FormattedGrapheme,
    ) -> usize {
        self.armed.push(self.anchors[self.idx].1);
        self.idx += 1;
        self.next_anchor()
    }

    fn render_virt_lines(
        &mut self,
        renderer: &mut TextRenderer,
        pos: LinePos,
        virt_off: Position,
    ) -> Position {
        if self.armed.is_empty() {
            return Position::new(0, 0);
        }
        let comments = self.doc.review_comments();
        let width = renderer.viewport.width;
        let start_row = pos.visual_line as usize + virt_off.row;
        let mut row = start_row;
        for comment_idx in std::mem::take(&mut self.armed) {
            let text = &comments[comment_idx].text;
            let height = review_comment_height(text);
            for (i, line) in text.lines().enumerate() {
                let line_row = row + i;
                if line_row >= renderer.viewport.height as usize {
                    break;
                }
                if let Some(bg) = self.style.bg {
                    renderer.set_style(
                        Rect::new(renderer.viewport.x, line_row as u16, width, 1),
                        Style::default().bg(bg),
                    );
                }
                let prefix = if i == 0 { PREFIX } else { "    " };
                let (col, _) = renderer.set_string_truncated(
                    renderer.viewport.x,
                    line_row as u16,
                    prefix,
                    width as usize,
                    |_| self.style,
                    false,
                    false,
                );
                let col = col - renderer.viewport.x;
                renderer.set_string_truncated(
                    renderer.viewport.x + col,
                    line_row as u16,
                    line,
                    width.saturating_sub(col) as usize,
                    |_| self.style,
                    true,
                    false,
                );
            }
            row += height;
        }
        Position::new(row - start_row, 0)
    }
}
