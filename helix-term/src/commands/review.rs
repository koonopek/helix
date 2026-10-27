//! Code review comments (see [`helix_view::review`]): adding comments to
//! lines, copying them as markdown for an LLM and submitting them as a
//! GitHub pull request review.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::process::Stdio;

use anyhow::{anyhow, bail, ensure, Context as _};
use helix_core::Selection;
use helix_view::review::{self, FileComments, LineRange};
use helix_view::Editor;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{push_jump, Context};
use crate::compositor;
use crate::job::Callback;
use crate::ui::{self, Prompt, PromptEvent};

pub fn review_comment(cx: &mut Context) {
    let (view, doc) = current_ref!(cx.editor);
    if doc.path().is_none() {
        cx.editor
            .set_error("Review comments can only be added to files on disk");
        return;
    }
    let doc_id = doc.id();
    let text = doc.text().slice(..);
    let (start, end) = doc.selection(view.id).primary().line_range(text);
    let mut lines = LineRange::new(start, end);
    let exact = doc
        .review_comments()
        .iter()
        .find(|comment| comment.lines(text) == lines);
    // with a single line selected edit the comment covering that line
    let covering = || {
        doc.review_comments()
            .iter()
            .find(|comment| start == end && comment.lines(text).contains(start))
    };
    let prefill = match exact.or_else(covering) {
        Some(comment) => {
            lines = comment.lines(text);
            comment.text.clone()
        }
        None => String::new(),
    };

    let label = if lines.start == lines.end {
        format!("comment L{}:", lines.start + 1)
    } else {
        format!("comment L{}-{}:", lines.start + 1, lines.end + 1)
    };
    let prompt = Prompt::new(
        label.into(),
        None,
        ui::completers::none,
        move |cx, input: &str, event| {
            if event != PromptEvent::Validate {
                return;
            }
            let Some(doc) = cx.editor.documents.get_mut(&doc_id) else {
                return;
            };
            doc.set_review_comment(lines, input.to_string());
            cx.editor.persist_review_comments();
            let count = review_comment_count(cx.editor);
            cx.editor.set_status(format!(
                "{count} review comment{}",
                if count == 1 { "" } else { "s" }
            ));
        },
    )
    .with_line(prefill, cx.editor);
    cx.push_layer(Box::new(prompt));
}

fn goto_review_comment_impl(cx: &mut Context, forward: bool) {
    let motion = move |editor: &mut Editor| {
        let (view, doc) = current!(editor);
        let text = doc.text().slice(..);
        let cursor_line = doc.selection(view.id).primary().cursor_line(text);
        let comments = doc.review_comments();
        let comment = if forward {
            comments
                .iter()
                .find(|comment| text.char_to_line(comment.start) > cursor_line)
        } else {
            comments
                .iter()
                .rev()
                .find(|comment| text.char_to_line(comment.start) < cursor_line)
        };
        let Some(comment) = comment else {
            return;
        };
        let selection = Selection::point(comment.start);
        push_jump(view, doc);
        doc.set_selection(view.id, selection);
    };
    cx.editor.apply_motion(motion);
}

pub fn goto_next_review_comment(cx: &mut Context) {
    goto_review_comment_impl(cx, true)
}

pub fn goto_prev_review_comment(cx: &mut Context) {
    goto_review_comment_impl(cx, false)
}

pub fn review_comment_count(editor: &Editor) -> usize {
    editor
        .documents()
        .map(|doc| doc.review_comments().len())
        .chain(editor.review_stash.values().map(|file| file.comments.len()))
        .sum()
}

/// The review as markdown, ready to be pasted into an LLM.
pub fn review_markdown(editor: &Editor) -> anyhow::Result<String> {
    let files = editor.review_comments();
    ensure!(!files.is_empty(), "No review comments");
    Ok(format!(
        "{}\n\n{}",
        review::EXPORT_INTRO,
        review::to_markdown(&files)
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub fn parse(arg: Option<&str>) -> anyhow::Result<Self> {
        match arg {
            None | Some("comment") => Ok(Self::Comment),
            Some("approve") => Ok(Self::Approve),
            Some("request-changes") => Ok(Self::RequestChanges),
            Some(other) => bail!(
                "Unknown review event '{other}', expected comment, approve or request-changes"
            ),
        }
    }

    fn github_event(self) -> &'static str {
        match self {
            Self::Comment => "COMMENT",
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// Submits all review comments as a review of the GitHub pull request of the
/// current branch, using the `gh` CLI. Comments on lines which are part of the
/// pull request diff become inline comments, all others are listed in the
/// review body.
pub fn submit_review(cx: &mut compositor::Context, event: ReviewEvent) -> anyhow::Result<()> {
    let editor = &mut *cx.editor;
    let files = editor.review_comments();
    let workspace = match files.first() {
        Some(file) => file.workspace.clone(),
        None if event == ReviewEvent::Comment => bail!("No review comments"),
        None => doc!(editor).workspace_root().to_path_buf(),
    };
    ensure!(
        files.iter().all(|file| file.workspace == workspace),
        "Review comments span multiple repositories"
    );
    editor.set_status("Submitting review...");

    let future = async move {
        let result = submit_review_impl(&workspace, files, event).await;
        let call = Callback::Editor(Box::new(move |editor: &mut Editor| match result {
            Ok(status) => editor.set_status(status),
            Err(err) => editor.set_error(format!("Failed to submit review: {err:#}")),
        }));
        Ok(call)
    };
    cx.jobs.callback(future);
    Ok(())
}

async fn submit_review_impl(
    workspace: &Path,
    files: Vec<FileComments>,
    event: ReviewEvent,
) -> anyhow::Result<String> {
    let pr: Value = serde_json::from_str(
        &run(
            workspace,
            "gh",
            &["pr", "view", "--json", "number,headRefOid,url"],
            None,
        )
        .await
        .context("no pull request found for the current branch")?,
    )?;
    let number = pr["number"]
        .as_u64()
        .ok_or_else(|| anyhow!("unexpected `gh pr view` output"))?
        .to_string();
    let head = pr["headRefOid"].as_str().unwrap_or_default().to_string();
    let url = pr["url"].as_str().unwrap_or_default().to_string();

    let diff = run(workspace, "gh", &["pr", "diff", &number], None).await?;
    let hunks = parse_diff_hunks(&diff);

    let mut inline = Vec::new();
    let mut outside = Vec::new();
    for file in files {
        let path = file.relative_path().to_string_lossy().replace('\\', "/");
        let file_hunks = hunks.get(&path).map(Vec::as_slice).unwrap_or_default();
        let mut rest = Vec::new();
        for comment in file.comments {
            let (start, end) = (comment.lines.start as u32 + 1, comment.lines.end as u32 + 1);
            let Some(hunk) = file_hunks.iter().find(|hunk| hunk.contains(&end)) else {
                rest.push(comment);
                continue;
            };
            let mut inline_comment = json!({
                "path": path,
                "line": end,
                "side": "RIGHT",
                "body": comment.text,
            });
            if start != end {
                if hunk.contains(&start) {
                    inline_comment["start_line"] = start.into();
                    inline_comment["start_side"] = "RIGHT".into();
                } else {
                    // GitHub requires both ends of a range in the same hunk
                    inline_comment["body"] =
                        format!("(lines {start}-{end}) {}", comment.text).into();
                }
            }
            inline.push(inline_comment);
        }
        if !rest.is_empty() {
            outside.push(FileComments {
                comments: rest,
                ..file
            });
        }
    }

    let body = if outside.is_empty() {
        String::new()
    } else {
        format!(
            "Comments outside of the pull request diff:\n\n{}",
            review::to_markdown(&outside)
        )
    };
    let payload = json!({
        "commit_id": head,
        "event": event.github_event(),
        "body": body,
        "comments": inline,
    });
    let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{number}/reviews");
    run(
        workspace,
        "gh",
        &["api", &endpoint, "--method", "POST", "--input", "-"],
        Some(payload.to_string()),
    )
    .await?;

    let local_head = run(workspace, "git", &["rev-parse", "HEAD"], None)
        .await
        .unwrap_or_default();
    let mut status = format!(
        "Submitted review to {url}: {} inline, {} in body",
        inline.len(),
        outside
            .iter()
            .map(|file| file.comments.len())
            .sum::<usize>()
    );
    if local_head.trim() != head {
        status.push_str(" (warning: local HEAD differs from the PR head, lines may be off)");
    }
    Ok(status)
}

async fn run(
    cwd: &Path,
    program: &str,
    args: &[&str],
    stdin: Option<String>,
) -> anyhow::Result<String> {
    let mut process = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to run `{program}`"))?;
    if let (Some(input), Some(mut pipe)) = (stdin, process.stdin.take()) {
        pipe.write_all(input.as_bytes()).await?;
    }
    let output = process.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!("`{program} {}` failed: {}", args.join(" "), message.trim());
    }
    Ok(String::from_utf8(output.stdout)?)
}

/// Parses a unified diff into the (1-based, exclusive end) line ranges of
/// the new side of each hunk by file path. These are the lines GitHub
/// accepts inline comments on.
fn parse_diff_hunks(diff: &str) -> HashMap<String, Vec<Range<u32>>> {
    let mut hunks: HashMap<String, Vec<Range<u32>>> = HashMap::new();
    let mut path = None;
    let mut prev = "";
    for line in diff.lines() {
        let is_header = prev.starts_with("--- ");
        prev = line;
        if let Some(new_path) = line.strip_prefix("+++ ").filter(|_| is_header) {
            path = new_path.strip_prefix("b/").map(str::to_string);
        } else if let (Some(header), Some(path)) = (line.strip_prefix("@@ "), &path) {
            let Some(new) = header.split(' ').find_map(|part| part.strip_prefix('+')) else {
                continue;
            };
            let (start, len) = match new.split_once(',') {
                Some((start, len)) => (start.parse(), len.parse()),
                None => (new.parse(), Ok(1)),
            };
            if let (Ok(start), Ok(len)) = (start, len) {
                hunks
                    .entry(path.clone())
                    .or_default()
                    .push(start..start + len);
            }
        }
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hunks() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n\
                    --- a/src/a.rs\n\
                    +++ b/src/a.rs\n\
                    @@ -1,3 +1,4 @@ fn main() {\n\
                    +x\n\
                    @@ -10 +11 @@\n\
                    diff --git a/old.rs b/old.rs\n\
                    --- a/old.rs\n\
                    +++ /dev/null\n\
                    @@ -1,2 +0,0 @@\n";
        let hunks = parse_diff_hunks(diff);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks["src/a.rs"], vec![1..5, 11..12]);
    }
}
