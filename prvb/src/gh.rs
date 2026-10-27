use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Clone, Copy, PartialEq)]
pub enum Check {
    Pass,
    Fail,
    Pending,
    Skip,
}

pub struct Pr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub created: String,
    pub updated: String,
    pub additions: u64,
    pub deletions: u64,
    pub files: u64,
    pub base: String,
    pub head: String,
    pub sha: String,
    pub mergeable: String,
    pub decision: String,
    pub requested: Vec<String>,
    pub reviews: Vec<(String, String)>,
    /// Commit oid and time of the viewer's latest review, for "changes since last review".
    pub my_review: Option<(String, String)>,
    pub ci: String,
    pub checks: Vec<(String, Check)>,
}

// One request for everything shown in the UI; `gh pr list` + `gh pr view` per PR is far slower.
const GQL: &str = r#"query($q: String!) { viewer { login } search(query: $q, type: ISSUE_ADVANCED, first: 100) { nodes { ... on PullRequest {
  number title url createdAt updatedAt additions deletions changedFiles
  repository { nameWithOwner } author { login } baseRefName headRefName headRefOid mergeable reviewDecision
  reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } ... on Team { name } } } }
  latestReviews(first: 20) { nodes { author { login } state submittedAt commit { oid } } }
  commits(last: 1) { nodes { commit { statusCheckRollup { state contexts(first: 100) { nodes {
    __typename ... on CheckRun { name conclusion status } ... on StatusContext { context state } } } } } } }
} } } }"#;

pub fn fetch(query: &str) -> Result<String> {
    let out = Command::new("gh")
        .args(["api", "graphql", "-f"])
        .arg(format!("q={query}"))
        .arg("-f")
        .arg(format!("query={GQL}"))
        .stdin(Stdio::null())
        .output()
        .context("failed to run gh")?;
    if !out.status.success() {
        bail!("gh: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn s(v: &Value, ptr: &str) -> String {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn n(v: &Value, ptr: &str) -> u64 {
    v.pointer(ptr).and_then(Value::as_u64).unwrap_or(0)
}

fn nodes<'a>(v: &'a Value, ptr: &str) -> impl Iterator<Item = &'a Value> {
    v.pointer(ptr)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

pub fn parse(raw: &str) -> Result<Vec<Pr>> {
    let v: Value = serde_json::from_str(raw)?;
    if let Some(err) = v.pointer("/errors/0/message").and_then(Value::as_str) {
        bail!("github: {err}");
    }
    let me = s(&v, "/data/viewer/login");
    let prs = nodes(&v, "/data/search/nodes")
        .filter(|p| p.get("number").is_some())
        .map(|p| {
            let rollup = "/commits/nodes/0/commit/statusCheckRollup";
            let checks = nodes(p, &format!("{rollup}/contexts/nodes"))
                .map(|c| {
                    if s(c, "/__typename") == "CheckRun" {
                        let state = match (s(c, "/status").as_str(), s(c, "/conclusion").as_str()) {
                            (st, _) if st != "COMPLETED" => Check::Pending,
                            (_, "SUCCESS") => Check::Pass,
                            (_, "NEUTRAL" | "SKIPPED") => Check::Skip,
                            _ => Check::Fail,
                        };
                        (s(c, "/name"), state)
                    } else {
                        let state = match s(c, "/state").as_str() {
                            "SUCCESS" => Check::Pass,
                            "PENDING" | "EXPECTED" => Check::Pending,
                            _ => Check::Fail,
                        };
                        (s(c, "/context"), state)
                    }
                })
                .collect();
            Pr {
                repo: s(p, "/repository/nameWithOwner"),
                number: n(p, "/number"),
                title: s(p, "/title"),
                url: s(p, "/url"),
                author: s(p, "/author/login"),
                created: s(p, "/createdAt"),
                updated: s(p, "/updatedAt"),
                additions: n(p, "/additions"),
                deletions: n(p, "/deletions"),
                files: n(p, "/changedFiles"),
                base: s(p, "/baseRefName"),
                head: s(p, "/headRefName"),
                sha: s(p, "/headRefOid"),
                mergeable: s(p, "/mergeable"),
                decision: s(p, "/reviewDecision"),
                requested: nodes(p, "/reviewRequests/nodes")
                    .map(|r| {
                        let login = s(r, "/requestedReviewer/login");
                        if login.is_empty() {
                            s(r, "/requestedReviewer/name")
                        } else {
                            login
                        }
                    })
                    .collect(),
                reviews: nodes(p, "/latestReviews/nodes")
                    .map(|r| (s(r, "/author/login"), s(r, "/state")))
                    .collect(),
                my_review: nodes(p, "/latestReviews/nodes")
                    .find(|r| !me.is_empty() && s(r, "/author/login") == me)
                    .map(|r| (s(r, "/commit/oid"), s(r, "/submittedAt")))
                    .filter(|(oid, _)| !oid.is_empty()),
                ci: s(p, &format!("{rollup}/state")),
                checks,
            }
        })
        .collect();
    Ok(prs)
}

fn cache_path(query: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut h);
    Some(dir.join("prvb").join(format!("{:016x}.json", h.finish())))
}

pub fn load_cache(query: &str) -> Option<String> {
    std::fs::read_to_string(cache_path(query)?).ok()
}

pub fn save_cache(query: &str, raw: &str) {
    if let Some(p) = cache_path(query) {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, raw);
    }
}
