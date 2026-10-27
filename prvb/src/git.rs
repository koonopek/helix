use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// `owner/name` (lowercase) from a GitHub remote url.
fn slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let mut parts = url.rsplit(['/', ':']);
    let name = parts.next()?;
    let owner = parts.next()?;
    Some(format!("{owner}/{name}").to_lowercase())
}

fn origin_url(config: &str) -> Option<&str> {
    let mut in_origin = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            in_origin = line == r#"[remote "origin"]"#;
        } else if in_origin {
            if let Some(url) = line.strip_prefix("url").map(|l| l.trim_start()) {
                return url.strip_prefix('=').map(str::trim);
            }
        }
    }
    None
}

/// Maps `owner/name` to a local clone. Scans direct children of `roots` (reading `.git/config`
/// directly, no process spawns); a dir named like the repo wins, the current repo wins over all.
pub fn discover(roots: &[PathBuf]) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let Ok(config) = std::fs::read_to_string(dir.join(".git/config")) else {
                continue;
            };
            let Some(slug) = origin_url(&config).and_then(slug) else {
                continue;
            };
            let name_match = dir.file_name().is_some_and(|f| {
                slug.ends_with(&format!("/{}", f.to_string_lossy().to_lowercase()))
            });
            if name_match || !map.contains_key(&slug) {
                map.insert(slug, dir);
            }
        }
    }
    let cwd = Path::new(".");
    if let (Ok(top), Ok(url)) = (
        git(cwd, &["rev-parse", "--show-toplevel"]),
        git(cwd, &["remote", "get-url", "origin"]),
    ) {
        if let Some(slug) = slug(&url) {
            map.insert(slug, PathBuf::from(top));
        }
    }
    map
}

/// Fetches PR heads and their base branches in a single `git fetch`.
pub fn fetch(dir: &Path, prs: &[(String, u64)]) -> Result<()> {
    let mut specs: Vec<String> = prs
        .iter()
        .map(|(_, n)| format!("refs/pull/{n}/head"))
        .collect();
    let mut bases: Vec<&str> = prs.iter().map(|(b, _)| b.as_str()).collect();
    bases.sort();
    bases.dedup();
    specs.extend(
        bases
            .iter()
            .map(|b| format!("+refs/heads/{b}:refs/remotes/origin/{b}")),
    );
    let mut args = vec!["fetch", "-q", "--no-tags", "origin"];
    args.extend(specs.iter().map(String::as_str));
    git(dir, &args).map(drop)
}

/// Which part of the PR to show as unstaged changes.
pub enum Pick {
    All,
    /// Changes since this commit (the viewer's last review).
    Since(String),
    /// These commits only (oldest first), as listed by [`commits`].
    Commits(Vec<String>),
}

pub struct Target {
    pub dir: PathBuf,
    pub repo: String,
    pub number: u64,
    pub sha: String,
    pub base: String,
    pub pick: Pick,
}

pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub author: String,
}

/// Merge base of the PR head with its base branch, fetching whatever is missing.
fn merge_base(dir: &Path, sha: &str, base: &str, number: u64) -> Result<String> {
    let base_ref = format!("refs/remotes/origin/{base}");
    let have_sha = git(dir, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_ok();
    if !have_sha || git(dir, &["rev-parse", "-q", "--verify", &base_ref]).is_err() {
        fetch(dir, &[(base.to_string(), number)])?;
    }
    git(dir, &["merge-base", sha, &base_ref])
}

/// The PR's commits, oldest first. First parent only, so merges of the base branch into the PR
/// show up as a single commit and each commit's parent is the one listed before it.
pub fn commits(dir: &Path, sha: &str, base: &str, number: u64) -> Result<Vec<Commit>> {
    let mb = merge_base(dir, sha, base, number)?;
    let out = git(
        dir,
        &[
            "log",
            "--first-parent",
            "--reverse",
            "--format=%H%x1f%an%x1f%s",
            &format!("{mb}..{sha}"),
        ],
    )?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\x1f');
            Some(Commit {
                sha: f.next()?.to_string(),
                author: f.next()?.to_string(),
                subject: f.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", a, b]).is_ok()
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

pub struct Applied {
    pub repo: String,
    pub number: u64,
    /// What part of the PR is shown, for display.
    pub label: String,
    /// What the clone is reset to on unapply.
    merge_base: String,
    added: Vec<String>,
}

/// A clone we are reviewing in. State is persisted in `.git/prvb-state`, so a crashed or killed
/// session can be resumed and restored on the next run.
pub struct Session {
    pub dir: PathBuf,
    state_file: PathBuf,
    original: String,
    pub applied: Option<Applied>,
}

impl Session {
    fn state_file(dir: &Path) -> Result<PathBuf> {
        Ok(PathBuf::from(git(dir, &["rev-parse", "--absolute-git-dir"])?).join("prvb-state"))
    }

    /// Resumes a session left behind by a previous run, if any.
    pub fn load(dir: &Path) -> Option<Session> {
        let state_file = Self::state_file(dir).ok()?;
        let text = std::fs::read_to_string(&state_file).ok()?;
        let mut kv: HashMap<&str, &str> = HashMap::new();
        let mut added = Vec::new();
        for line in text.lines() {
            let (k, v) = line.split_once(' ')?;
            if k == "added" {
                added.push(v.to_string());
            } else {
                kv.insert(k, v);
            }
        }
        let applied = kv.get("merge_base").map(|mb| Applied {
            repo: kv.get("repo").unwrap_or(&"").to_string(),
            number: kv.get("number").and_then(|n| n.parse().ok()).unwrap_or(0),
            label: kv.get("label").unwrap_or(&"").to_string(),
            merge_base: mb.to_string(),
            added,
        });
        Some(Session {
            dir: dir.to_path_buf(),
            state_file,
            original: kv.get("original")?.to_string(),
            applied,
        })
    }

    pub fn open(dir: &Path) -> Result<Session> {
        if let Some(s) = Self::load(dir) {
            return Ok(s);
        }
        if !git(dir, &["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
            bail!(
                "{} has uncommitted changes, stash or commit them first",
                dir.display()
            );
        }
        let original = git(dir, &["symbolic-ref", "-q", "--short", "HEAD"])
            .or_else(|_| git(dir, &["rev-parse", "HEAD"]))?;
        let s = Session {
            dir: dir.to_path_buf(),
            state_file: Self::state_file(dir)?,
            original,
            applied: None,
        };
        s.save()?;
        Ok(s)
    }

    fn save(&self) -> Result<()> {
        let mut out = format!("original {}\n", self.original);
        if let Some(a) = &self.applied {
            out += &format!(
                "repo {}\nnumber {}\nlabel {}\nmerge_base {}\n",
                a.repo, a.number, a.label, a.merge_base
            );
            for p in &a.added {
                out += &format!("added {p}\n");
            }
        }
        std::fs::write(&self.state_file, out).context("writing prvb-state")
    }

    /// Leaves the picked part of the PR as unstaged changes on top of a detached HEAD.
    pub fn apply(&mut self, t: &Target) -> Result<()> {
        self.unapply()?;
        let dir = &self.dir;
        let mb = merge_base(dir, &t.sha, &t.base, t.number)?;
        // A contiguous range is a checkout of its last commit reset to the one before its first;
        // anything else is cherry-picked (uncommitted) onto the parent of the first picked commit.
        let (from, to, label) = match &t.pick {
            Pick::All => (mb, Some(t.sha.clone()), "all changes".to_string()),
            Pick::Since(c) => {
                if !is_ancestor(dir, c, &t.sha) {
                    bail!(
                        "your last reviewed commit {} is not in the PR anymore (force-pushed?)",
                        short(c)
                    );
                }
                // If the base branch was merged into the PR since the review, diffing from the
                // reviewed commit would show everything that merge brought in. Compare against
                // the reviewed commit merged with the current base instead, so only the PR's own
                // changes remain.
                let from = if is_ancestor(dir, &mb, c) {
                    c.clone()
                } else {
                    let tree =
                        git(dir, &["merge-tree", "--write-tree", c, &mb]).with_context(|| {
                            format!("{} conflicts with the current base branch", short(c))
                        })?;
                    git(
                        dir,
                        &[
                            "-c",
                            "user.name=prvb",
                            "-c",
                            "user.email=prvb@localhost",
                            "commit-tree",
                            &tree,
                            "-p",
                            c,
                            "-p",
                            &mb,
                            "-m",
                            "prvb: last review on current base",
                        ],
                    )?
                };
                let n = git(
                    dir,
                    &[
                        "rev-list",
                        "--count",
                        "--first-parent",
                        &format!("{from}..{}", t.sha),
                    ],
                )?;
                if n == "0" {
                    bail!("no new commits since your last review");
                }
                let label = format!("{n} commits since your review of {}", short(c));
                (from, Some(t.sha.clone()), label)
            }
            Pick::Commits(picked) => {
                let all: Vec<String> = commits(dir, &t.sha, &t.base, t.number)?
                    .into_iter()
                    .map(|c| c.sha)
                    .collect();
                let pos: Vec<usize> = picked
                    .iter()
                    .filter_map(|p| all.iter().position(|c| c == p))
                    .collect();
                let (Some(&first), Some(&last)) = (pos.first(), pos.last()) else {
                    bail!("no commits picked");
                };
                let from = git(dir, &["rev-parse", &format!("{}^", all[first])])?;
                let label = format!("{} of {} commits", pos.len(), all.len());
                if last - first + 1 == pos.len() {
                    (from, Some(all[last].clone()), label)
                } else {
                    git(dir, &["checkout", "-q", "--detach", &from])?;
                    for &i in &pos {
                        let c = &all[i];
                        let merge = git(dir, &["rev-parse", "-q", "--verify", &format!("{c}^2")]);
                        let mut args = vec!["cherry-pick", "-n"];
                        if merge.is_ok() {
                            args.extend(["-m", "1"]);
                        }
                        args.push(c);
                        if let Err(e) = git(dir, &args) {
                            let _ = git(dir, &["reset", "-q", "--hard"]);
                            bail!(
                                "{} does not apply without the commits before it: {e}",
                                short(c)
                            );
                        }
                    }
                    (from, None, label)
                }
            }
        };
        // Files the PR adds become untracked after the reset; remember them so they can be
        // removed again (`reset --hard` leaves untracked files behind). --no-renames so a
        // rename's new path is reported as added.
        let mut diff = vec![
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--diff-filter=A",
        ];
        match &to {
            Some(to) => diff.extend([from.as_str(), to.as_str()]),
            None => diff.push("--cached"),
        }
        let added = git(dir, &diff)?
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
        if let Some(to) = &to {
            git(dir, &["checkout", "-q", "--detach", to])?;
        }
        git(dir, &["reset", "-q", &from])?;
        self.applied = Some(Applied {
            repo: t.repo.clone(),
            number: t.number,
            label,
            merge_base: from,
            added,
        });
        self.save()
    }

    /// Drops the PR's changes (including edits made while reviewing), leaving a clean merge base.
    pub fn unapply(&mut self) -> Result<()> {
        let Some(a) = &self.applied else {
            return Ok(());
        };
        git(&self.dir, &["reset", "-q", "--hard", &a.merge_base])?;
        for p in &a.added {
            let path = self.dir.join(p);
            let _ = std::fs::remove_file(&path);
            // Remove directories the PR introduced, stopping at the first non-empty one.
            let mut parent = path.parent();
            while let Some(d) = parent.filter(|d| *d != self.dir) {
                if std::fs::remove_dir(d).is_err() {
                    break;
                }
                parent = d.parent();
            }
        }
        self.applied = None;
        self.save()
    }

    /// Back to the branch the clone was on before prvb touched it.
    pub fn restore(&mut self) -> Result<()> {
        self.unapply()?;
        git(&self.dir, &["checkout", "-q", &self.original])?;
        let _ = std::fs::remove_file(&self.state_file);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(
            slug("git@github.com:Org/Repo.git").as_deref(),
            Some("org/repo")
        );
        assert_eq!(
            slug("https://github.com/org/repo").as_deref(),
            Some("org/repo")
        );
        assert_eq!(
            origin_url(
                "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:a/b.git\n"
            ),
            Some("git@github.com:a/b.git")
        );
    }
}

#[cfg(test)]
mod flow {
    use super::*;

    fn sh(dir: &Path, script: &str) {
        let ok = Command::new("sh")
            .arg("-ec")
            .arg(script)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(ok.success(), "{script}");
    }

    #[test]
    fn apply_unapply_restore() {
        let root = std::env::temp_dir().join(format!("prvb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = "git -c user.name=t -c user.email=t@t";
        sh(&root, &format!(
            "git init -q -b main origin && cd origin && echo a > a && echo old > moved && {g} add . && {g} commit -qm base \
             && git checkout -qb feat && echo b >> a && mkdir -p new/dir && echo n > new/dir/f && git mv moved renamed \
             && {g} add . && {g} commit -qm feat && git update-ref refs/pull/1/head feat && git checkout -q main \
             && cd .. && git clone -q origin clone && cd clone && echo keep > untracked"
        ));
        let dir = root.join("clone");
        let sha = git(&root.join("origin"), &["rev-parse", "feat"]).unwrap();
        let mut s = Session::open(&dir).unwrap();
        s.apply(&Target {
            dir: dir.clone(),
            repo: "o/r".into(),
            number: 1,
            sha,
            base: "main".into(),
            pick: Pick::All,
        })
        .unwrap();
        let st = git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(
            st.contains(" M a")
                && st.contains(" D moved")
                && st.contains("?? new/")
                && st.contains("?? renamed"),
            "{st}"
        );

        // A fresh process resumes the session from .git/prvb-state.
        let mut s = Session::load(&dir).unwrap();
        s.restore().unwrap();
        assert_eq!(
            git(&dir, &["status", "--porcelain"]).unwrap(),
            "?? untracked"
        );
        assert_eq!(git(&dir, &["branch", "--show-current"]).unwrap(), "main");
        assert!(!dir.join("new").exists() && !dir.join("renamed").exists());
        assert!(Session::load(&dir).is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn picks() {
        let root = std::env::temp_dir().join(format!("prvb-picks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = "git -c user.name=t -c user.email=t@t";
        sh(&root, &format!(
            "git init -q -b main origin && cd origin && echo a > a && {g} add . && {g} commit -qm base \
             && git checkout -qb feat && echo 1 > one && {g} add . && {g} commit -qm one \
             && echo 2 >> a && {g} commit -qam two && echo 3 > three && {g} add . && {g} commit -qm three \
             && git update-ref refs/pull/1/head feat && git checkout -q main \
             && cd .. && git clone -q origin clone"
        ));
        let dir = root.join("clone");
        let sha = git(&root.join("origin"), &["rev-parse", "feat"]).unwrap();
        let cs = commits(&dir, &sha, "main", 1).unwrap();
        let subjects: Vec<_> = cs.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["one", "two", "three"]);
        let target = |pick| Target {
            dir: dir.clone(),
            repo: "o/r".into(),
            number: 1,
            sha: sha.clone(),
            base: "main".into(),
            pick,
        };
        let status = || git(&dir, &["status", "--porcelain"]).unwrap();
        let mut s = Session::open(&dir).unwrap();

        s.apply(&target(Pick::Since(cs[0].sha.clone()))).unwrap();
        assert_eq!(status(), " M a\n?? three");
        assert!(dir.join("one").exists());

        s.apply(&target(Pick::Commits(vec![
            cs[0].sha.clone(),
            cs[2].sha.clone(),
        ])))
        .unwrap();
        assert_eq!(status(), "?? one\n?? three");
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "a\n");

        s.apply(&target(Pick::Commits(vec![cs[1].sha.clone()])))
            .unwrap();
        assert_eq!(status(), " M a");

        s.apply(&target(Pick::Since(sha.clone()))).unwrap_err();
        s.restore().unwrap();
        assert_eq!(status(), "");
        assert!(!dir.join("one").exists() && !dir.join("three").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn since_review_ignores_merged_base() {
        let root = std::env::temp_dir().join(format!("prvb-since-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let g = "git -c user.name=t -c user.email=t@t";
        sh(&root, &format!(
            "git init -q -b main origin && cd origin && echo a > a && {g} add . && {g} commit -qm base \
             && git checkout -qb feat && echo 1 > one && {g} add . && {g} commit -qm one \
             && git checkout -q main && echo m > m && {g} add . && {g} commit -qm main-moved \
             && git checkout -q feat && {g} merge -q --no-edit main && echo 2 > two && {g} add . && {g} commit -qm two \
             && git update-ref refs/pull/1/head feat && git checkout -q main \
             && cd .. && git clone -q origin clone"
        ));
        let dir = root.join("clone");
        let origin = root.join("origin");
        let sha = git(&origin, &["rev-parse", "feat"]).unwrap();
        let reviewed = git(&origin, &["rev-parse", "feat~2"]).unwrap();
        let mut s = Session::open(&dir).unwrap();
        s.apply(&Target {
            dir: dir.clone(),
            repo: "o/r".into(),
            number: 1,
            sha,
            base: "main".into(),
            pick: Pick::Since(reviewed),
        })
        .unwrap();
        assert_eq!(git(&dir, &["status", "--porcelain"]).unwrap(), "?? two");
        s.restore().unwrap();
        assert_eq!(git(&dir, &["status", "--porcelain"]).unwrap(), "");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
