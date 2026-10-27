//! prvb: pick a GitHub PR, get its changes as unstaged edits in your local clone, review in your editor.
//!
//! Query: `prvb <github search query>`, or $PRVB_QUERY. Clones are looked up in the direct children
//! of $PRVB_ROOTS (colon separated, default: parent of the current directory) by origin url.

mod gh;
mod git;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use gh::{Check, Pr};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const DEFAULT_QUERY: &str =
    "is:pr is:open draft:false archived:false review-requested:@me sort:updated-desc";

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const CYAN: &str = "\x1b[36m";
const REV: &str = "\x1b[7m";

enum Msg {
    Key(KeyEvent),
    Redraw,
    Prs(String, anyhow::Result<String>),
    Clones(HashMap<String, PathBuf>),
    Fetched(String, anyhow::Result<()>),
    /// Result of a git worker command: what is now checked out.
    Applied(Result<Option<Current>, String>),
    Commits(String, u64, anyhow::Result<Vec<git::Commit>>),
}

struct Current {
    repo: String,
    number: u64,
    dir: PathBuf,
    label: String,
}

/// Commit picker for one PR: everything checked by default.
struct Picker {
    repo: String,
    number: u64,
    commits: Vec<git::Commit>,
    on: Vec<bool>,
    sel: usize,
    top: usize,
}

enum Cmd {
    Recover(Vec<PathBuf>),
    Apply(git::Target),
    Restore(Option<Sender<()>>),
}

/// Owns all checkout state; serializes git operations off the UI thread.
fn git_worker(rx: Receiver<Cmd>, tx: Sender<Msg>) {
    let mut sessions: HashMap<PathBuf, git::Session> = HashMap::new();
    let restore_all = |sessions: &mut HashMap<PathBuf, git::Session>, keep: Option<&PathBuf>| {
        let dirs: Vec<_> = sessions
            .keys()
            .filter(|d| Some(*d) != keep)
            .cloned()
            .collect();
        for d in dirs {
            sessions.get_mut(&d).unwrap().restore()?;
            sessions.remove(&d);
        }
        anyhow::Ok(())
    };
    for cmd in rx {
        match cmd {
            Cmd::Recover(dirs) => {
                for s in dirs.iter().filter_map(|d| git::Session::load(d)) {
                    if let Some(a) = &s.applied {
                        let cur = Current {
                            repo: a.repo.clone(),
                            number: a.number,
                            dir: s.dir.clone(),
                            label: a.label.clone(),
                        };
                        let _ = tx.send(Msg::Applied(Ok(Some(cur))));
                    }
                    sessions.insert(s.dir.clone(), s);
                }
            }
            Cmd::Apply(t) => {
                let res = restore_all(&mut sessions, Some(&t.dir)).and_then(|_| {
                    if !sessions.contains_key(&t.dir) {
                        sessions.insert(t.dir.clone(), git::Session::open(&t.dir)?);
                    }
                    let s = sessions.get_mut(&t.dir).unwrap();
                    s.apply(&t)?;
                    Ok(s.applied.as_ref().unwrap().label.clone())
                });
                let msg = res
                    .map(|label| {
                        Some(Current {
                            repo: t.repo,
                            number: t.number,
                            dir: t.dir,
                            label,
                        })
                    })
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Msg::Applied(msg));
            }
            Cmd::Restore(ack) => {
                let res = restore_all(&mut sessions, None).map(|_| None);
                let _ = tx.send(Msg::Applied(res.map_err(|e| format!("{e:#}"))));
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
        }
    }
}

struct App {
    query: String,
    prs: Vec<Pr>,
    sel: usize,
    top: usize,
    clones: HashMap<String, PathBuf>,
    current: Option<Current>,
    picker: Option<Picker>,
    busy: bool,
    loading: bool,
    fresh: bool,
    fetching: usize,
    refreshed: Option<Instant>,
    status: Option<(String, bool)>,
    input: Option<String>,
    tx: Sender<Msg>,
    cmd: Sender<Cmd>,
}

impl App {
    fn refresh(&mut self) {
        self.loading = true;
        let (q, tx) = (self.query.clone(), self.tx.clone());
        thread::spawn(move || {
            let res = gh::fetch(&q);
            let _ = tx.send(Msg::Prs(q, res));
        });
    }

    fn set_prs(&mut self, prs: Vec<Pr>) {
        let selected = self.prs.get(self.sel).map(|p| (p.repo.clone(), p.number));
        self.prs = prs;
        self.sel = selected
            .and_then(|(r, n)| self.prs.iter().position(|p| p.repo == r && p.number == n))
            .unwrap_or(0);
    }

    fn clone_dir(&self, pr: &Pr) -> Option<&PathBuf> {
        self.clones.get(&pr.repo.to_lowercase())
    }

    /// Fetch all listed PR heads up front so switching never waits on the network.
    fn prefetch(&mut self) {
        if !self.fresh || self.clones.is_empty() {
            return;
        }
        let mut by_dir: HashMap<PathBuf, (String, Vec<(String, u64)>)> = HashMap::new();
        for pr in &self.prs {
            if let Some(dir) = self.clone_dir(pr) {
                let e = by_dir
                    .entry(dir.clone())
                    .or_insert_with(|| (pr.repo.clone(), vec![]));
                e.1.push((pr.base.clone(), pr.number));
            }
        }
        self.fetching += by_dir.len();
        for (dir, (repo, prs)) in by_dir {
            let tx = self.tx.clone();
            thread::spawn(move || {
                let res = git::fetch(&dir, &prs);
                let _ = tx.send(Msg::Fetched(repo, res));
            });
        }
    }

    /// The selected PR and its local clone, or an error in the status line.
    fn selected(&mut self) -> Option<(&Pr, PathBuf)> {
        let pr = self.prs.get(self.sel)?;
        match self.clone_dir(pr) {
            Some(dir) => Some((pr, dir.clone())),
            None => {
                self.status = Some((
                    format!("no local clone of {} under $PRVB_ROOTS", pr.repo),
                    true,
                ));
                None
            }
        }
    }

    fn checkout(&mut self, pick: git::Pick) {
        if self.busy {
            return;
        }
        let Some((pr, dir)) = self.selected() else {
            return;
        };
        let target = git::Target {
            dir,
            repo: pr.repo.clone(),
            number: pr.number,
            sha: pr.sha.clone(),
            base: pr.base.clone(),
            pick,
        };
        self.busy = true;
        self.status = Some((
            format!("checking out {}#{}…", target.repo, target.number),
            false,
        ));
        let _ = self.cmd.send(Cmd::Apply(target));
    }

    fn checkout_since_review(&mut self) {
        let Some(pr) = self.prs.get(self.sel) else {
            return;
        };
        match &pr.my_review {
            Some((oid, _)) => self.checkout(git::Pick::Since(oid.clone())),
            None => self.status = Some(("you have not reviewed this PR yet".into(), true)),
        }
    }

    fn open_picker(&mut self) {
        let Some((pr, dir)) = self.selected() else {
            return;
        };
        let (repo, number, sha, base) =
            (pr.repo.clone(), pr.number, pr.sha.clone(), pr.base.clone());
        self.status = Some((format!("listing commits of {repo}#{number}…"), false));
        let tx = self.tx.clone();
        thread::spawn(move || {
            let res = git::commits(&dir, &sha, &base, number);
            let _ = tx.send(Msg::Commits(repo, number, res));
        });
    }

    fn picker_key(&mut self, k: KeyEvent, ctrl_c: bool) {
        let p = self.picker.as_mut().unwrap();
        let last = p.commits.len().saturating_sub(1);
        match k.code {
            _ if ctrl_c => self.picker = None,
            KeyCode::Esc | KeyCode::Char('q') => self.picker = None,
            KeyCode::Char('j') | KeyCode::Down => p.sel = (p.sel + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => p.sel = 0,
            KeyCode::Char('G') | KeyCode::End => p.sel = last,
            KeyCode::Char(' ') => {
                p.on[p.sel] = !p.on[p.sel];
                p.sel = (p.sel + 1).min(last);
            }
            KeyCode::Char('a') => {
                let all = p.on.iter().all(|o| *o);
                p.on.iter_mut().for_each(|o| *o = !all);
            }
            KeyCode::Enter if !self.busy => {
                let p = self.picker.take().unwrap();
                let picked: Vec<String> = p
                    .commits
                    .into_iter()
                    .zip(p.on)
                    .filter(|(_, on)| *on)
                    .map(|(c, _)| c.sha)
                    .collect();
                if picked.is_empty() {
                    self.status = Some(("no commits picked".into(), true));
                    return;
                }
                // The picker belongs to the PR it was opened on, even if the list has moved.
                if let Some(i) = self
                    .prs
                    .iter()
                    .position(|pr| pr.repo == p.repo && pr.number == p.number)
                {
                    self.sel = i;
                    self.checkout(git::Pick::Commits(picked));
                }
            }
            _ => {}
        }
    }

    fn handle(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Key(k) => return self.key(k),
            Msg::Redraw => {}
            Msg::Prs(q, res) if q == self.query => {
                self.loading = false;
                match res.and_then(|raw| gh::parse(&raw).map(|prs| (raw, prs))) {
                    Ok((raw, prs)) => {
                        gh::save_cache(&q, &raw);
                        self.set_prs(prs);
                        self.fresh = true;
                        self.refreshed = Some(Instant::now());
                        self.prefetch();
                    }
                    Err(e) => self.status = Some((format!("{e:#}"), true)),
                }
            }
            Msg::Prs(..) => {}
            Msg::Clones(c) => {
                self.clones = c;
                let _ = self
                    .cmd
                    .send(Cmd::Recover(self.clones.values().cloned().collect()));
                self.prefetch();
            }
            Msg::Fetched(repo, res) => {
                self.fetching -= 1;
                if let Err(e) = res {
                    self.status = Some((format!("prefetch {repo}: {e:#}"), true));
                }
            }
            Msg::Commits(repo, number, res) => match res {
                Ok(commits) if commits.is_empty() => {
                    self.status = Some((format!("{repo}#{number} has no commits"), true));
                }
                Ok(commits) => {
                    self.status = None;
                    self.picker = Some(Picker {
                        repo,
                        number,
                        on: vec![true; commits.len()],
                        commits,
                        sel: 0,
                        top: 0,
                    });
                }
                Err(e) => self.status = Some((format!("{e:#}"), true)),
            },
            Msg::Applied(res) => {
                self.busy = false;
                match res {
                    Ok(cur) => {
                        self.status = None;
                        self.current = cur;
                    }
                    Err(e) => {
                        self.status = Some((e, true));
                        self.current = None;
                    }
                }
            }
        }
        true
    }

    fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        if let Some(input) = &mut self.input {
            match k.code {
                _ if ctrl_c => self.input = None,
                KeyCode::Esc => self.input = None,
                KeyCode::Enter => {
                    self.query = self.input.take().unwrap();
                    self.prs.clear();
                    self.fresh = false;
                    if let Some(raw) = gh::load_cache(&self.query) {
                        self.set_prs(gh::parse(&raw).unwrap_or_default());
                    }
                    self.refresh();
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                _ => {}
            }
            return true;
        }
        if self.picker.is_some() {
            self.picker_key(k, ctrl_c);
            return true;
        }
        let last = self.prs.len().saturating_sub(1);
        match k.code {
            _ if ctrl_c => return false,
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('j') | KeyCode::Down => self.sel = (self.sel + 1).min(last),
            KeyCode::Char('k') | KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.sel = last,
            KeyCode::Enter | KeyCode::Char(' ') => self.checkout(git::Pick::All),
            KeyCode::Char('s') => self.checkout_since_review(),
            KeyCode::Char('c') => self.open_picker(),
            KeyCode::Char('x') if !self.busy => {
                self.busy = true;
                self.status = Some(("restoring…".into(), false));
                let _ = self.cmd.send(Cmd::Restore(None));
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('/') => self.input = Some(self.query.clone()),
            KeyCode::Char('o') => {
                if let Some(pr) = self.prs.get(self.sel) {
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else {
                        "xdg-open"
                    };
                    let _ = std::process::Command::new(opener)
                        .arg(&pr.url)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                }
            }
            _ => {}
        }
        true
    }
}

/// A screen line clipped to the terminal width; styles never count towards the width.
struct Line {
    out: String,
    left: usize,
}

impl Line {
    fn new(width: usize) -> Self {
        Line {
            out: String::new(),
            left: width,
        }
    }

    fn push(&mut self, text: &str, style: &str) -> &mut Self {
        let t: String = text.chars().take(self.left).collect();
        self.left -= t.chars().count();
        if style.is_empty() {
            self.out += &t;
        } else {
            self.out += style;
            self.out += &t;
            self.out += RESET;
        }
        self
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// "2026-10-20T12:02:33Z" -> unix seconds.
fn parse_ts(s: &str) -> Option<i64> {
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + hh * 3600 + mm * 60 + ss)
}

fn age(ts: &str) -> String {
    let Some(t) = parse_ts(ts) else {
        return "?".into();
    };
    let s = (now() - t).max(0);
    match s {
        0..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        86400..1209600 => format!("{}d", s / 86400),
        _ => format!("{}w", s / 604800),
    }
}

fn ci_icon(pr: &Pr) -> (&'static str, &'static str) {
    match pr.ci.as_str() {
        "SUCCESS" => ("✓", GREEN),
        "FAILURE" | "ERROR" => ("✗", RED),
        "PENDING" | "EXPECTED" => ("●", YELLOW),
        _ => ("·", DIM),
    }
}

fn review_style(state: &str) -> (&'static str, &'static str) {
    match state {
        "APPROVED" => ("approved", GREEN),
        "CHANGES_REQUESTED" => ("changes requested", RED),
        "REVIEW_REQUIRED" => ("review required", YELLOW),
        "COMMENTED" => ("commented", DIM),
        "DISMISSED" => ("dismissed", DIM),
        _ => ("", ""),
    }
}

fn draw(app: &mut App, out: &mut impl Write) -> std::io::Result<()> {
    let (w, h) = terminal::size()?;
    let (w, h) = (w as usize, h as usize);
    let mut lines: Vec<String> = Vec::with_capacity(h);

    let mut l = Line::new(w);
    l.push(" prvb ", &format!("{REV}{BOLD}"));
    l.push(&format!(" {} PRs  ", app.prs.len()), "");
    if app.loading {
        l.push("refreshing… ", YELLOW);
    } else if let Some(t) = app.refreshed {
        l.push(&format!("updated {}s ago ", t.elapsed().as_secs()), DIM);
    } else {
        l.push("cached ", DIM);
    }
    if app.fetching > 0 {
        l.push(&format!("prefetching {} repos… ", app.fetching), YELLOW);
    }
    l.push(&app.query, DIM);
    lines.push(l.out);

    let mut l = Line::new(w);
    match &app.current {
        Some(c) => {
            l.push(" reviewing ", GREEN)
                .push(&format!("{}#{}", c.repo, c.number), BOLD);
            if !c.label.is_empty() {
                l.push(&format!(" ({})", c.label), CYAN);
            }
            l.push(&format!(" in {}", c.dir.display()), "");
        }
        None => {
            l.push(" nothing checked out", DIM);
        }
    }
    lines.push(l.out);

    // List gets up to half the screen; details take the rest.
    let list_h = app.prs.len().min((h.saturating_sub(4)) / 2).max(1);
    app.sel = app.sel.min(app.prs.len().saturating_sub(1));
    if app.sel < app.top {
        app.top = app.sel;
    } else if app.sel >= app.top + list_h {
        app.top = app.sel + 1 - list_h;
    }
    let repo_w = app
        .prs
        .iter()
        .map(|p| p.repo.rsplit('/').next().unwrap_or("").len())
        .max()
        .unwrap_or(0);
    for i in app.top..(app.top + list_h) {
        let mut l = Line::new(w);
        if let Some(pr) = app.prs.get(i) {
            let selected = i == app.sel;
            let is_current = app
                .current
                .as_ref()
                .is_some_and(|c| c.repo == pr.repo && c.number == pr.number);
            l.push(if selected { "›" } else { " " }, CYAN);
            l.push(if is_current { "▶" } else { " " }, GREEN);
            let (icon, color) = ci_icon(pr);
            l.push(icon, color);
            let (dec, color) = match pr.decision.as_str() {
                "APPROVED" => ("✔", GREEN),
                "CHANGES_REQUESTED" => ("±", RED),
                _ => (" ", ""),
            };
            l.push(dec, color);
            let short = pr.repo.rsplit('/').next().unwrap_or("");
            let no_clone =
                !app.clones.contains_key(&pr.repo.to_lowercase()) && !app.clones.is_empty();
            l.push(
                &format!(" {short:<repo_w$} #{:<6}", pr.number),
                if no_clone { DIM } else { "" },
            );
            l.push(&format!("{:<4} ", age(&pr.updated)), DIM);
            l.push(&pr.title, if selected { BOLD } else { "" });
            l.push(&format!("  {}", pr.author), DIM);
        } else if i == 0 {
            l.push(
                if app.loading {
                    "  loading…"
                } else {
                    "  no PRs"
                },
                DIM,
            );
        }
        lines.push(l.out);
    }
    lines.push(format!("{DIM}{}{RESET}", "─".repeat(w)));

    if let Some(p) = &mut app.picker {
        let mut l = Line::new(w);
        let n = p.on.iter().filter(|o| **o).count();
        l.push(&format!("{}#{}  ", p.repo, p.number), CYAN)
            .push(&format!("{n} of {} commits picked", p.commits.len()), BOLD);
        lines.push(l.out);
        let list_h = h.saturating_sub(lines.len() + 1).max(1);
        if p.sel < p.top {
            p.top = p.sel;
        } else if p.sel >= p.top + list_h {
            p.top = p.sel + 1 - list_h;
        }
        for (i, c) in p.commits.iter().enumerate().skip(p.top).take(list_h) {
            let mut l = Line::new(w);
            let selected = i == p.sel;
            l.push(if selected { "›" } else { " " }, CYAN);
            l.push(
                if p.on[i] { "[x] " } else { "[ ] " },
                if p.on[i] { GREEN } else { DIM },
            );
            l.push(&format!("{} ", &c.sha[..c.sha.len().min(7)]), YELLOW);
            l.push(
                &c.subject,
                if !p.on[i] {
                    DIM
                } else if selected {
                    BOLD
                } else {
                    ""
                },
            );
            l.push(&format!("  {}", c.author), DIM);
            lines.push(l.out);
        }
    } else if let Some(pr) = app.prs.get(app.sel) {
        let mut l = Line::new(w);
        l.push(&pr.title, BOLD);
        lines.push(l.out);
        let mut l = Line::new(w);
        l.push(&format!("{}#{}  ", pr.repo, pr.number), CYAN)
            .push(&pr.url, DIM);
        lines.push(l.out);
        let mut l = Line::new(w);
        l.push(&pr.author, BOLD)
            .push(&format!("  {} → {}  ", pr.head, pr.base), "");
        l.push(&format!("+{}", pr.additions), GREEN)
            .push(&format!(" -{}", pr.deletions), RED);
        l.push(&format!(" in {} files  ", pr.files), "");
        l.push(
            &format!(
                "opened {} ago, updated {} ago",
                age(&pr.created),
                age(&pr.updated)
            ),
            DIM,
        );
        lines.push(l.out);

        let mut l = Line::new(w);
        let (dec, color) = review_style(&pr.decision);
        l.push("review: ", DIM)
            .push(if dec.is_empty() { "-" } else { dec }, color);
        if pr.mergeable == "CONFLICTING" {
            l.push("  conflicts", RED);
        }
        lines.push(l.out);
        if !pr.reviews.is_empty() {
            let mut l = Line::new(w);
            l.push("reviews: ", DIM);
            for (i, (who, state)) in pr.reviews.iter().enumerate() {
                let (text, color) = review_style(state);
                l.push(if i > 0 { ", " } else { "" }, "")
                    .push(who, "")
                    .push(&format!(" {text}"), color);
            }
            lines.push(l.out);
        }
        if let Some((oid, at)) = &pr.my_review {
            let mut l = Line::new(w);
            l.push("your last review: ", DIM)
                .push(&oid[..oid.len().min(7)], YELLOW)
                .push(&format!(" {} ago", age(at)), DIM);
            if *oid == pr.sha {
                l.push("  (up to date)", GREEN);
            } else {
                l.push("  s: show changes since", "");
            }
            lines.push(l.out);
        }
        if !pr.requested.is_empty() {
            let mut l = Line::new(w);
            l.push("requested: ", DIM)
                .push(&pr.requested.join(", "), "");
            lines.push(l.out);
        }

        let count = |c: Check| pr.checks.iter().filter(|(_, s)| *s == c).count();
        let mut l = Line::new(w);
        let (icon, color) = ci_icon(pr);
        l.push("ci: ", DIM)
            .push(&format!("{icon} {}", pr.ci.to_lowercase()), color);
        l.push(&format!("  {} passed", count(Check::Pass)), GREEN);
        for (c, label, color) in [
            (Check::Fail, "failed", RED),
            (Check::Pending, "pending", YELLOW),
            (Check::Skip, "skipped", DIM),
        ] {
            if count(c) > 0 {
                l.push(&format!(", {} {label}", count(c)), color);
            }
        }
        lines.push(l.out);
        for (name, state) in &pr.checks {
            let (icon, color) = match state {
                Check::Fail => ("  ✗ ", RED),
                Check::Pending => ("  ● ", YELLOW),
                _ => continue,
            };
            let mut l = Line::new(w);
            l.push(icon, color).push(name, "");
            lines.push(l.out);
        }

        let mut l = Line::new(w);
        match app.clone_dir(pr) {
            Some(dir) => l.push("local: ", DIM).push(&dir.display().to_string(), ""),
            None if app.clones.is_empty() => l.push("local: looking for clones…", DIM),
            None => l.push("local: no clone found under $PRVB_ROOTS", RED),
        };
        lines.push(l.out);
    }

    lines.truncate(h.saturating_sub(1));
    lines.resize(h.saturating_sub(1), String::new());
    let mut l = Line::new(w);
    if let Some(input) = &app.input {
        l.push("/ ", CYAN).push(input, "").push("█", "");
    } else if let Some((msg, err)) = &app.status {
        l.push(msg, if *err { RED } else { YELLOW });
    } else if app.picker.is_some() {
        l.push(
            "space toggle  a all/none  j/k move  enter checkout picked  esc cancel",
            DIM,
        );
    } else {
        l.push(
            "enter checkout  s since review  c commits  x restore  j/k  r refresh  / query  o browser  q quit",
            DIM,
        );
    }
    lines.push(l.out);

    // Single write per frame, wrapped in a synchronized update to avoid flicker.
    let mut buf = String::from("\x1b[?2026h\x1b[H");
    for (i, line) in lines.iter().enumerate() {
        buf += line;
        buf += "\x1b[K";
        if i + 1 < lines.len() {
            buf += "\r\n";
        }
    }
    buf += "\x1b[?2026l";
    out.write_all(buf.as_bytes())?;
    out.flush()
}

fn restore_terminal() {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        cursor::Show,
        terminal::LeaveAlternateScreen
    );
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: prvb [github search query]   (default: $PRVB_QUERY or `{DEFAULT_QUERY}`)");
        println!("clones are found in the children of $PRVB_ROOTS (colon separated, default: ..)");
        return Ok(());
    }
    let query = if !args.is_empty() {
        args.join(" ")
    } else {
        std::env::var("PRVB_QUERY").unwrap_or_else(|_| DEFAULT_QUERY.into())
    };
    let roots: Vec<PathBuf> = match std::env::var("PRVB_ROOTS") {
        Ok(r) => std::env::split_paths(&r).collect(),
        Err(_) => vec![std::env::current_dir()?
            .parent()
            .map(PathBuf::from)
            .unwrap_or_default()],
    };

    let (tx, rx) = channel();
    let (cmd, cmd_rx) = channel();
    {
        let tx = tx.clone();
        thread::spawn(move || git_worker(cmd_rx, tx));
    }
    {
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Msg::Clones(git::discover(&roots)));
        });
    }
    {
        let tx = tx.clone();
        thread::spawn(move || loop {
            let msg = match event::read() {
                Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => Msg::Key(k),
                Ok(Event::Resize(..)) => Msg::Redraw,
                Ok(_) => continue,
                Err(_) => return,
            };
            if tx.send(msg).is_err() {
                return;
            }
        });
    }

    let mut app = App {
        query,
        prs: vec![],
        sel: 0,
        top: 0,
        clones: HashMap::new(),
        current: None,
        picker: None,
        busy: false,
        loading: false,
        fresh: false,
        fetching: 0,
        refreshed: None,
        status: None,
        input: None,
        tx,
        cmd,
    };
    if let Some(raw) = gh::load_cache(&app.query) {
        app.set_prs(gh::parse(&raw).unwrap_or_default());
    }
    app.refresh();

    terminal::enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        terminal::EnterAlternateScreen,
        cursor::Hide
    )?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));

    let mut out = std::io::stdout().lock();
    loop {
        draw(&mut app, &mut out)?;
        let msg = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(m) => m,
            Err(_) => Msg::Redraw,
        };
        if !app.handle(msg) {
            break;
        }
        // Drain whatever else arrived so bursts (key repeat, many results) cost one frame.
        let mut quit = false;
        while let Ok(m) = rx.try_recv() {
            if !app.handle(m) {
                quit = true;
                break;
            }
        }
        if quit {
            break;
        }
    }

    app.status = Some(("restoring original branches…".into(), false));
    draw(&mut app, &mut out)?;
    let (ack, done) = channel();
    let _ = app.cmd.send(Cmd::Restore(Some(ack)));
    let _ = done.recv_timeout(Duration::from_secs(60));
    let err = rx.try_iter().find_map(|m| match m {
        Msg::Applied(Err(e)) => Some(e),
        _ => None,
    });
    drop(out);
    restore_terminal();
    if let Some(e) = err {
        eprintln!("prvb: restore failed: {e}");
        std::process::exit(1);
    }
    Ok(())
}
