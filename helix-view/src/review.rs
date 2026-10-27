//! Code review comments attached to lines of a document, modeled after
//! [tuicr](https://github.com/agavra/tuicr).
//!
//! Comments live on the [`Document`](crate::Document) as char positions so
//! they follow edits for free (they are mapped through every transaction just
//! like diagnostics). When a document is closed its comments are stashed on
//! the [`Editor`](crate::Editor) by path and restored when it is reopened.
//!
//! Comments are persisted per workspace (see [`storage_path`]) so they
//! survive restarting the editor.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::io;
use std::path::{Path, PathBuf};

use helix_core::RopeSlice;
use serde::{Deserialize, Serialize};

/// A comment on a range of lines of a document.
#[derive(Debug, Clone)]
pub struct ReviewComment {
    /// Char index of the start of the first commented line.
    pub start: usize,
    /// Char index of the start of the last commented line.
    pub end: usize,
    pub text: String,
}

impl ReviewComment {
    pub fn new(text: RopeSlice, lines: LineRange, comment: String) -> Self {
        Self {
            start: text.line_to_char(lines.start),
            end: text.line_to_char(lines.end),
            text: comment,
        }
    }

    /// The (0-based, inclusive) lines this comment is attached to.
    pub fn lines(&self, text: RopeSlice) -> LineRange {
        LineRange::new(text.char_to_line(self.start), text.char_to_line(self.end))
    }
}

/// Inclusive, 0-based range of lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start: start.min(end),
            end: start.max(end),
        }
    }

    pub fn contains(&self, line: usize) -> bool {
        (self.start..=self.end).contains(&line)
    }
}

/// A comment resolved to lines, used for stashing comments of closed
/// documents and for exporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineComment {
    pub lines: LineRange,
    pub text: String,
    /// Trimmed content of the first commented line, used to find the line
    /// again if the file changed in the meantime.
    pub anchor: String,
}

impl LineComment {
    /// Moves the comment to the line closest to its original position whose
    /// content matches [`LineComment::anchor`]. Comments are left in place if
    /// the line still matches or no matching line exists.
    pub fn relocate(&mut self, text: RopeSlice) {
        let line_matches = |line: usize| {
            line < text.len_lines()
                && std::borrow::Cow::<str>::from(text.line(line)).trim() == self.anchor
        };
        if self.anchor.is_empty() || line_matches(self.lines.start) {
            return;
        }
        let start = self.lines.start;
        let max_distance = start.max(text.len_lines());
        let found = (1..=max_distance).find_map(|distance| {
            [start.checked_sub(distance), Some(start + distance)]
                .into_iter()
                .flatten()
                .find(|&line| line_matches(line))
        });
        if let Some(line) = found {
            let len = self.lines.end - self.lines.start;
            self.lines = LineRange::new(line, line + len);
        }
    }
}

/// All comments of a single file, resolved to lines.
#[derive(Debug, Clone)]
pub struct FileComments {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Root of the workspace (repository) the file belongs to.
    pub workspace: PathBuf,
    /// Sorted by line.
    pub comments: Vec<LineComment>,
}

impl FileComments {
    /// The path relative to the workspace root, which is what both LLMs and
    /// forges expect. Falls back to the absolute path.
    pub fn relative_path(&self) -> &Path {
        self.path
            .strip_prefix(&self.workspace)
            .unwrap_or(&self.path)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredComment {
    /// 1-based, inclusive
    start: usize,
    end: usize,
    text: String,
    #[serde(default)]
    anchor: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredReview {
    /// Comments by path relative to the workspace root.
    files: BTreeMap<String, Vec<StoredComment>>,
}

/// Where the review comments of `workspace` are stored: inside the git
/// directory (`.git/helix-review.json`, so they are never committed), also
/// for worktrees where `.git` is a file pointing to the git directory.
/// Outside of git repositories they are stored in the cache directory.
pub fn storage_path(workspace: &Path) -> PathBuf {
    const FILE_NAME: &str = "helix-review.json";
    let dot_git = workspace.join(".git");
    if dot_git.is_dir() {
        return dot_git.join(FILE_NAME);
    }
    if let Some(git_dir) = std::fs::read_to_string(&dot_git)
        .ok()
        .and_then(|content| Some(content.strip_prefix("gitdir:")?.trim().to_string()))
    {
        return workspace.join(git_dir).join(FILE_NAME);
    }
    let name: String = workspace
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '%' })
        .collect();
    helix_loader::cache_dir()
        .join("reviews")
        .join(format!("{name}.json"))
}

/// Loads the stored review comments of `workspace`.
pub fn load(workspace: &Path) -> Vec<FileComments> {
    let path = storage_path(workspace);
    let review: StoredReview = match std::fs::read(&path) {
        Ok(content) => match serde_json::from_slice(&content) {
            Ok(review) => review,
            Err(err) => {
                log::error!("Failed to parse review comments {}: {err}", path.display());
                return Vec::new();
            }
        },
        Err(_) => return Vec::new(),
    };
    review
        .files
        .into_iter()
        .map(|(relative_path, comments)| FileComments {
            path: workspace.join(relative_path),
            workspace: workspace.to_path_buf(),
            comments: comments
                .into_iter()
                .map(|comment| LineComment {
                    lines: LineRange::new(
                        comment.start.saturating_sub(1),
                        comment.end.saturating_sub(1),
                    ),
                    text: comment.text,
                    anchor: comment.anchor,
                })
                .collect(),
        })
        .collect()
}

/// Stores the review comments of `workspace`, replacing previously stored
/// comments. The file is removed if there are no comments.
pub fn store<'a>(
    workspace: &Path,
    files: impl IntoIterator<Item = &'a FileComments>,
) -> io::Result<()> {
    let path = storage_path(workspace);
    let review = StoredReview {
        files: files
            .into_iter()
            .filter(|file| !file.comments.is_empty())
            .map(|file| {
                let comments = file
                    .comments
                    .iter()
                    .map(|comment| StoredComment {
                        start: comment.lines.start + 1,
                        end: comment.lines.end + 1,
                        text: comment.text.clone(),
                        anchor: comment.anchor.clone(),
                    })
                    .collect();
                (
                    file.relative_path().to_string_lossy().into_owned(),
                    comments,
                )
            })
            .collect(),
    };
    if review.files.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&review)?)?;
    std::fs::rename(tmp, path)
}

pub const EXPORT_INTRO: &str =
    "I reviewed your code and have the following comments. Please address them.";

/// Renders the comments as a numbered markdown list (the tuicr clipboard
/// format) so it can be pasted into an LLM or a review body:
///
/// ```text
/// 1. `src/main.rs:42` - comment
/// 2. `src/lib.rs:10-12` - multi line
///    comment
/// ```
///
/// Line numbers are 1-based.
pub fn to_markdown<'a>(files: impl IntoIterator<Item = &'a FileComments>) -> String {
    let mut md = String::new();
    let mut number = 0;
    for file in files {
        let path = file.relative_path().display();
        for comment in &file.comments {
            number += 1;
            let marker = format!("{number}.");
            let location = if comment.lines.start == comment.lines.end {
                format!("`{path}:{}`", comment.lines.start + 1)
            } else {
                format!(
                    "`{path}:{}-{}`",
                    comment.lines.start + 1,
                    comment.lines.end + 1
                )
            };
            let mut lines = comment.text.lines();
            let first = lines.next().unwrap_or_default();
            let _ = writeln!(md, "{marker} {location} - {first}");
            let indent = " ".repeat(marker.len() + 1);
            for line in lines {
                let _ = writeln!(md, "{indent}{line}");
            }
        }
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_export() {
        let files = [FileComments {
            path: PathBuf::from("/repo/src/main.rs"),
            workspace: PathBuf::from("/repo"),
            comments: vec![
                LineComment {
                    lines: LineRange::new(41, 41),
                    text: "rename this".into(),
                    anchor: String::new(),
                },
                LineComment {
                    lines: LineRange::new(9, 11),
                    text: "first\nsecond".into(),
                    anchor: String::new(),
                },
            ],
        }];
        assert_eq!(
            to_markdown(&files),
            "1. `src/main.rs:42` - rename this\n\
             2. `src/main.rs:10-12` - first\n   second\n"
        );
    }

    #[test]
    fn relocate() {
        let text = helix_core::Rope::from("a\nnew\nnew\nfoo()\nb\n");
        let mut comment = LineComment {
            lines: LineRange::new(1, 2),
            text: "x".into(),
            anchor: "foo()".into(),
        };
        comment.relocate(text.slice(..));
        assert_eq!(comment.lines, LineRange::new(3, 4));
        // no match: stays in place
        comment.anchor = "gone".into();
        comment.relocate(text.slice(..));
        assert_eq!(comment.lines, LineRange::new(3, 4));
    }
}
