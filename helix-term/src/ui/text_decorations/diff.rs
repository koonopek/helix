use helix_core::doc_formatter::{DocumentFormatter, FormattedGrapheme, TextFormat};
use helix_core::syntax::{self, Syntax};
use helix_core::text_annotations::TextAnnotations;
use helix_core::Position;
use helix_vcs::Diff;
use helix_view::annotations::diff::{expanded_hunks, ExpandedHunk};
use helix_view::graphics::{Color, Rect};
use helix_view::theme::Style;
use helix_view::{Document, Theme, ViewId};

use crate::ui::document::{LinePos, SyntaxHighlighter, TextRenderer};
use crate::ui::text_decorations::Decoration;

/// Maximum size of the diff base that is syntax-highlighted when rendering
/// the removed text of expanded hunks. The diff base is re-parsed on every
/// frame so this is capped to keep rendering fast; above the limit the
/// removed text is rendered without syntax highlighting.
const MAX_HIGHLIGHTED_BYTES: usize = 1024 * 1024;

/// Synthesizes a background style for inline diff lines by mixing the
/// theme's diff color into the editor background. Used as a fallback for
/// themes which don't define the `ui.virtual.inline-diff.*` scopes.
/// Requires both colors to be true color, otherwise `None` is returned.
fn synthesized_diff_bg(theme: &Theme, key: &str) -> Option<Style> {
    // percentage of the diff color mixed into the background
    const MIX: u16 = 25;
    let (Some(Color::Rgb(fr, fg, fb)), Some(Color::Rgb(br, bg, bb))) =
        (theme.get(key).fg, theme.get("ui.background").bg)
    else {
        return None;
    };
    let mix = |f: u8, b: u8| ((f as u16 * MIX + b as u16 * (100 - MIX)) / 100) as u8;
    Some(Style::default().bg(Color::Rgb(mix(fr, br), mix(fg, bg), mix(fb, bb))))
}

/// Renders the removed text of expanded diff hunks into the virtual lines
/// reserved by [`helix_view::annotations::diff::InlineDiffAnnotation`] and
/// highlights the added lines of those hunks.
pub struct InlineDiff<'a> {
    diff: Diff<'a>,
    hunks: Vec<ExpandedHunk>,
    /// `(anchor char idx, index into hunks)` for hunks with removed text,
    /// sorted by anchor
    anchors: Vec<(usize, usize)>,
    idx: usize,
    /// Hunks to render at the end of the current visual line
    armed: Vec<usize>,
    syntax: Option<Syntax>,
    loader: &'a syntax::Loader,
    theme: &'a Theme,
    removed_style: Style,
    added_style: Option<Style>,
    tab_width: u16,
}

impl<'a> InlineDiff<'a> {
    pub fn new(
        doc: &'a Document,
        view_id: ViewId,
        theme: &'a Theme,
        loader: &'a syntax::Loader,
    ) -> Option<Self> {
        let hunks = expanded_hunks(doc, view_id);
        if hunks.is_empty() {
            return None;
        }
        let diff = doc.diff_handle()?.load();
        let base = diff.diff_base();
        let syntax = if base.len_bytes() <= MAX_HIGHLIGHTED_BYTES {
            doc.language_config()
                .and_then(|config| Syntax::new(base.slice(..), config.language(), loader).ok())
        } else {
            None
        };
        let anchors = hunks
            .iter()
            .enumerate()
            .filter(|(_, hunk)| !hunk.before.is_empty())
            .map(|(i, hunk)| (hunk.anchor, i))
            .collect();
        Some(InlineDiff {
            diff,
            hunks,
            anchors,
            idx: 0,
            armed: Vec::new(),
            syntax,
            loader,
            theme,
            removed_style: theme
                .try_get("ui.virtual.inline-diff.removed")
                .or_else(|| synthesized_diff_bg(theme, "diff.minus"))
                .unwrap_or_else(|| theme.get("diff.minus")),
            added_style: theme
                .try_get("ui.virtual.inline-diff.added")
                .or_else(|| synthesized_diff_bg(theme, "diff.plus")),
            tab_width: doc.tab_width() as u16,
        })
    }

    fn next_anchor(&self) -> usize {
        self.anchors
            .get(self.idx)
            .map_or(usize::MAX, |&(anchor, _)| anchor)
    }
}

impl Decoration for InlineDiff<'_> {
    fn reset_pos(&mut self, pos: usize) -> usize {
        self.idx = self.anchors.partition_point(|&(anchor, _)| anchor < pos);
        self.armed.clear();
        self.next_anchor()
    }

    fn skip_concealed_anchor(&mut self, conceal_end_char_idx: usize) -> usize {
        self.idx += self.anchors[self.idx..]
            .partition_point(|&(anchor, _)| anchor < conceal_end_char_idx);
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

    fn decorate_line(&mut self, renderer: &mut TextRenderer, pos: LinePos) {
        let Some(added_style) = self.added_style else {
            return;
        };
        let in_hunk = self.hunks.iter().any(|hunk| {
            (hunk.after.start as usize..hunk.after.end as usize).contains(&pos.doc_line)
        });
        if in_hunk {
            renderer.set_style(
                Rect::new(
                    renderer.viewport.x,
                    pos.visual_line,
                    renderer.viewport.width,
                    1,
                ),
                added_style,
            );
        }
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
        let base = self.diff.diff_base();
        let text_style = renderer.text_style.patch(self.removed_style);
        let text_fmt = TextFormat {
            tab_width: self.tab_width,
            ..TextFormat::default()
        };
        let viewport_height = renderer.viewport.height as usize;
        let start_row = pos.visual_line as usize + virt_off.row;
        let mut row = start_row;

        for hunk_idx in std::mem::take(&mut self.armed) {
            let before = self.hunks[hunk_idx].before.clone();

            if let Some(bg) = self.removed_style.bg {
                for i in 0..before.len() {
                    let fill_row = row + i;
                    if fill_row >= viewport_height {
                        break;
                    }
                    renderer.set_style(
                        Rect::new(
                            renderer.viewport.x,
                            fill_row as u16,
                            renderer.viewport.width,
                            1,
                        ),
                        Style::default().bg(bg),
                    );
                }
            }

            let start_char = base.line_to_char(before.start as usize);
            let end_char = base.line_to_char(before.end as usize);
            let highlighter = self.syntax.as_ref().map(|syntax| {
                let range =
                    base.char_to_byte(start_char) as u32..base.char_to_byte(end_char) as u32;
                syntax.highlighter(base.slice(..), self.loader, range)
            });
            let mut highlighter =
                SyntaxHighlighter::new(highlighter, base.slice(..), self.theme, text_style);

            let annotations = TextAnnotations::default();
            let formatter = DocumentFormatter::new_at_prev_checkpoint(
                base.slice(start_char..end_char),
                &text_fmt,
                &annotations,
                0,
            );
            for grapheme in formatter {
                let grapheme_row = row + grapheme.visual_pos.row;
                if grapheme_row >= viewport_height {
                    break;
                }
                let char_idx = start_char + grapheme.char_idx;
                while char_idx >= highlighter.pos {
                    highlighter.advance();
                }
                let Some(col) = grapheme.visual_pos.col.checked_sub(renderer.offset.col) else {
                    continue;
                };
                renderer.draw_decoration_grapheme(
                    grapheme.raw,
                    highlighter.style,
                    grapheme_row as u16,
                    col as u16,
                );
            }
            row += before.len();
        }
        Position::new(row - start_row, 0)
    }
}
