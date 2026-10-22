//! Marks on a location's code, shared between the daemon and every viewer.
//! A mark is a lettered line range with an author, the words on it if any,
//! and the thread of turns under it; the user's and an agent's are the same
//! thing, and every mark stays until someone removes it. Everything that
//! changes a mark is an [`Op`], applied the same way wherever it runs, so a
//! viewer can apply what it did at once and the daemon can keep the one
//! copy that counts.

use serde::{Deserialize, Serialize};

/// Who placed a mark or wrote a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    /// An agent placed the mark, or wrote the turn.
    Agent,
    /// The user did.
    User,
}

impl Author {
    pub fn word(self) -> &'static str {
        match self {
            Author::Agent => "agent",
            Author::User => "user",
        }
    }
}

/// A turn in the thread under a mark.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// Its number under the mark, fixed when it was made; never reused.
    pub n: usize,
    pub author: Author,
    pub text: String,
    /// An agent has read it since it was made or last edited. The agent's
    /// own start read.
    pub seen: bool,
}

/// A mark on a location's code, with the text it was placed on so it can
/// follow that text through edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// Its place in the order marks were made on this location.
    pub seq: usize,
    /// Its letter, fixed when it was made: A to Z, then AA on.
    pub label: String,
    pub author: Author,
    pub path: String,
    pub start: usize,
    pub end: usize,
    /// The words on it: the user's always have some, an agent's may not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// The marked lines as first seen; `None` until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Vec<String>>,
    /// Its lines are nowhere to be found since the last load.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// Its lines are the old side's: a comment on deleted lines.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub old_side: bool,
    /// Deleted lines were among those commented on, with others.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_deletions: bool,
    /// An agent has read it since it was made or last edited. An agent's
    /// own start read.
    pub seen: bool,
    /// The thread under it, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread: Vec<Turn>,
    /// How many turns were ever made under it: numbers are not reused.
    #[serde(default)]
    pub turns_made: usize,
}

impl Mark {
    /// The user's: they made it, or wrote in its thread. Only they, or an
    /// agent they explicitly asked, may remove it, and then whole.
    pub fn is_users(&self) -> bool {
        self.author == Author::User || self.thread.iter().any(|t| t.author == Author::User)
    }

    /// Words of the user's here that no agent has read.
    pub fn unread(&self) -> usize {
        let own = usize::from(self.author == Author::User && !self.seen);
        own + self
            .thread
            .iter()
            .filter(|t| t.author == Author::User && !t.seen)
            .count()
    }

    /// The user's words here: the mark if they made it, and their turns.
    pub fn users_words(&self) -> usize {
        let own = usize::from(self.author == Author::User);
        own + self
            .thread
            .iter()
            .filter(|t| t.author == Author::User)
            .count()
    }

    fn add_turn(&mut self, author: Author, text: String) -> usize {
        self.turns_made += 1;
        self.thread.push(Turn {
            n: self.turns_made,
            author,
            text,
            seen: author == Author::Agent,
        });
        self.turns_made
    }

    /// As reported to an agent.
    pub fn info(&self) -> MarkInfo {
        let user = self.author == Author::User;
        MarkInfo {
            label: self.label.clone(),
            author: self.author,
            path: self.path.clone(),
            start: self.start,
            end: self.end,
            old_side: self.old_side,
            with_deletions: self.with_deletions,
            stale: self.stale,
            new: user && !self.seen,
            theirs: self.is_users(),
            text: self.comment.clone(),
            lines: if user {
                self.text.clone().unwrap_or_default()
            } else {
                Vec::new()
            },
            thread: self
                .thread
                .iter()
                .map(|t| TurnInfo {
                    author: t.author,
                    text: t.text.clone(),
                    new: t.author == Author::User && !t.seen,
                })
                .collect(),
        }
    }
}

/// A mark a show call places, 1-based and inclusive, with optional words.
/// In a diff the lines are the new side's, or the old side's for a deleted
/// file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkSpec {
    /// Relative to the repository root.
    pub path: String,
    pub start: usize,
    pub end: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The most marks one show call can place.
pub const MAX_MARKS: usize = 20;

/// Put new marks in the order the viewer shows them, so their letters run
/// down the screen: file by file as the view lists files, then by line. A
/// comparison lists its files by path; files as they are come in the order
/// asked for (`files`), or, when none was asked for, as the marks first
/// name them.
pub fn in_view_order(adds: &mut [MarkSpec], files: &[String], diff: bool) {
    let mut order: Vec<String> = files.to_vec();
    if order.is_empty() {
        for a in adds.iter() {
            if !order.contains(&a.path) {
                order.push(a.path.clone());
            }
        }
    }
    let rank = |p: &str| order.iter().position(|o| o == p).unwrap_or(usize::MAX);
    adds.sort_by(|a, b| {
        let by_file = if diff {
            a.path.cmp(&b.path)
        } else {
            rank(&a.path)
                .cmp(&rank(&b.path))
                .then_with(|| a.path.cmp(&b.path))
        };
        by_file.then(a.start.cmp(&b.start)).then(a.end.cmp(&b.end))
    });
}

/// One thing a show does to a location's marks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum MarkEntry {
    /// A new mark, lettered next.
    Add(MarkSpec),
    /// The mark `letter` names goes to other lines, keeping its letter,
    /// words and thread.
    Move {
        letter: String,
        path: String,
        start: usize,
        end: usize,
    },
    /// The mark `letter` names goes, thread and all.
    Delete { letter: String },
}

/// What a show's entries came to, once applied: the letters deleted, the
/// marks moved, and the new marks still to place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Edited {
    pub deleted: Vec<String>,
    pub moved: Vec<MarkInfo>,
    pub adds: Vec<MarkSpec>,
}

/// Where a mark's lines were found at the last load.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchored {
    pub seq: usize,
    pub start: usize,
    pub end: usize,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Vec<String>>,
}

/// Something a viewer does to the marks, applied where it happens and
/// again by the daemon, which then tells every viewer what the marks are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// A show call's marks, in the order they are lettered: as the daemon
    /// sorted them, file by file down the screen. Nothing else changes:
    /// every mark stays until removed.
    Marks { marks: Vec<MarkSpec> },
    /// A mark with words on lines: the user's from the viewer, with the
    /// lines' text, or an agent's, whose text the viewer takes at its next
    /// load.
    Mark {
        author: Author,
        path: String,
        start: usize,
        end: usize,
        old_side: bool,
        with_deletions: bool,
        text: String,
        lines: Vec<String>,
    },
    /// New text for the user's mark `seq` names.
    EditMark { seq: usize, text: String },
    /// A turn under the mark `seq` names.
    Turn {
        seq: usize,
        author: Author,
        text: String,
    },
    /// New text for the user's turn `n` under `seq`.
    EditTurn { seq: usize, n: usize, text: String },
    /// The user removes a mark, anyone's, with its whole thread, or one turn
    /// of theirs.
    Delete {
        seq: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        n: Option<usize>,
    },
    /// The user removes every mark here.
    DeleteAll,
    /// Marks followed their text, or lost it, on a load.
    Anchored { marks: Vec<Anchored> },
}

/// The marks on one location.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marks {
    pub marks: Vec<Mark>,
    /// How many marks were ever made here; the next one's letter.
    pub made: usize,
}

/// The letter of the `n`th mark made on a location, counting from 0:
/// A to Z, then AA, AB and on, as spreadsheet columns go.
pub fn letter(n: usize) -> String {
    let mut n = n;
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (n % 26) as u8);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    out.reverse();
    String::from_utf8(out).expect("ASCII letters")
}

/// A mark's letter as an agent or the user wrote it: upper-cased, without
/// brackets. `None` for anything else, such as the old reply labels (C2).
pub fn parse_letter(s: &str) -> Option<String> {
    let s = s
        .trim()
        .trim_matches(['[', ']'])
        .trim()
        .to_ascii_uppercase();
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_uppercase())).then_some(s)
}

impl Marks {
    pub fn get(&self, seq: usize) -> Option<&Mark> {
        self.marks.iter().find(|m| m.seq == seq)
    }

    fn get_mut(&mut self, seq: usize) -> Option<&mut Mark> {
        self.marks.iter_mut().find(|m| m.seq == seq)
    }

    /// The mark a letter names, by its place in the order marks were made.
    pub fn find_label(&self, label: &str) -> Option<usize> {
        let label = parse_letter(label)?;
        self.marks.iter().find(|m| m.label == label).map(|m| m.seq)
    }

    /// Words of the user's that no agent has read.
    pub fn unread(&self) -> usize {
        self.marks.iter().map(Mark::unread).sum()
    }

    /// The user's words here: the marks they made, and their turns.
    pub fn users_words(&self) -> usize {
        self.marks.iter().map(Mark::users_words).sum()
    }

    fn add(
        &mut self,
        author: Author,
        path: String,
        start: usize,
        end: usize,
        comment: Option<String>,
    ) -> usize {
        let seq = self.made;
        self.made += 1;
        self.marks.push(Mark {
            seq,
            label: letter(seq),
            author,
            path,
            start,
            end,
            comment,
            text: None,
            stale: false,
            old_side: false,
            with_deletions: false,
            seen: author == Author::Agent,
            thread: Vec::new(),
            turns_made: 0,
        });
        seq
    }

    /// Apply an op; the marks it touched, by their place in the order marks
    /// were made, so the caller can name them.
    pub fn apply(&mut self, op: &Op) -> Vec<usize> {
        match op {
            Op::Marks { marks } => marks
                .iter()
                .take(MAX_MARKS)
                .map(|h| {
                    self.add(
                        Author::Agent,
                        h.path.clone(),
                        h.start,
                        h.end,
                        h.text.clone(),
                    )
                })
                .collect(),
            Op::Mark {
                author,
                path,
                start,
                end,
                old_side,
                with_deletions,
                text,
                lines,
            } => {
                let seq = self.add(*author, path.clone(), *start, *end, Some(text.clone()));
                let m = self.get_mut(seq).expect("just added");
                m.old_side = *old_side;
                m.with_deletions = *with_deletions;
                m.text = (!lines.is_empty()).then(|| lines.clone());
                vec![seq]
            }
            Op::EditMark { seq, text } => match self.get_mut(*seq) {
                Some(m) if m.author == Author::User => {
                    m.comment = Some(text.clone());
                    m.seen = false;
                    vec![*seq]
                }
                _ => Vec::new(),
            },
            Op::Turn { seq, author, text } => match self.get_mut(*seq) {
                Some(m) => {
                    m.add_turn(*author, text.clone());
                    vec![*seq]
                }
                None => Vec::new(),
            },
            Op::EditTurn { seq, n, text } => {
                let Some(m) = self.get_mut(*seq) else {
                    return Vec::new();
                };
                match m.thread.iter_mut().find(|t| t.n == *n) {
                    Some(t) if t.author == Author::User => {
                        t.text = text.clone();
                        t.seen = false;
                        vec![*seq]
                    }
                    _ => Vec::new(),
                }
            }
            Op::Delete { seq, n: None } => {
                let before = self.marks.len();
                self.marks.retain(|m| m.seq != *seq);
                if self.marks.len() < before {
                    vec![*seq]
                } else {
                    Vec::new()
                }
            }
            Op::Delete { seq, n: Some(n) } => {
                let Some(m) = self.get_mut(*seq) else {
                    return Vec::new();
                };
                let before = m.thread.len();
                m.thread.retain(|t| t.n != *n || t.author != Author::User);
                if m.thread.len() < before {
                    vec![*seq]
                } else {
                    Vec::new()
                }
            }
            Op::DeleteAll => {
                let gone: Vec<usize> = self.marks.iter().map(|m| m.seq).collect();
                self.marks.clear();
                gone
            }
            Op::Anchored { marks } => {
                let mut touched = Vec::new();
                for a in marks {
                    if let Some(m) = self.get_mut(a.seq) {
                        m.start = a.start;
                        m.end = a.end;
                        m.stale = a.stale;
                        if a.text.is_some() {
                            m.text = a.text.clone();
                        }
                        touched.push(a.seq);
                    }
                }
                touched
            }
        }
    }

    // ---- what agents do ---------------------------------------------------

    /// Every mark for an agent, by where it sits, or only the ones
    /// `letters` names; the user's words on them count as read afterwards.
    pub fn read(&mut self, letters: &[String]) -> Result<Vec<MarkInfo>, String> {
        let only: Vec<usize> = letters
            .iter()
            .map(|l| self.find_label(l).ok_or_else(|| no_mark(l)))
            .collect::<Result<_, _>>()?;
        let mut marks: Vec<&mut Mark> = self
            .marks
            .iter_mut()
            .filter(|m| only.is_empty() || only.contains(&m.seq))
            .collect();
        marks.sort_by_key(|m| (m.path.clone(), m.old_side, m.start, m.end, m.seq));
        Ok(marks
            .into_iter()
            .map(|m| {
                let info = m.info();
                m.seen = true;
                for t in &mut m.thread {
                    t.seen = true;
                }
                info
            })
            .collect())
    }

    /// The agent's own marks here: the ones the user has not written under.
    pub fn mine(&self) -> Vec<MarkInfo> {
        self.marks
            .iter()
            .filter(|m| !m.is_users())
            .map(Mark::info)
            .collect()
    }

    /// An agent's answer under the mark `letter` names: one the user made,
    /// or wrote in the thread of.
    pub fn reply(&mut self, letter: &str, text: &str) -> Result<String, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a reply needs text".to_string());
        }
        let seq = self.find_label(letter).ok_or_else(|| no_mark(letter))?;
        let m = self.get_mut(seq).expect("found");
        if !m.is_users() {
            return Err(format!(
                "[{}] is your own and the user has not written under it; replies go under marks the user wrote on",
                m.label
            ));
        }
        m.add_turn(Author::Agent, text.to_string());
        Ok(m.label.clone())
    }

    /// Move the mark `letter` names to other lines, of `path` or of its
    /// own file. It keeps its letter, its text and its thread.
    pub fn move_to(
        &mut self,
        letter: &str,
        path: Option<String>,
        start: usize,
        end: usize,
    ) -> Result<&Mark, String> {
        check_lines(start, end)?;
        let seq = self.find_label(letter).ok_or_else(|| no_mark(letter))?;
        // Where it already is, on the new side: nothing changes, least of
        // all its text.
        let same = self.get(seq).is_some_and(|m| {
            !m.old_side
                && path.as_ref().is_none_or(|p| *p == m.path)
                && start == m.start
                && end == m.end
        });
        if same {
            return Ok(self.get(seq).expect("found"));
        }
        let m = self.get_mut(seq).expect("found");
        if let Some(path) = path {
            m.path = path;
        }
        m.start = start;
        m.end = end;
        // Taken afresh from its new lines at the next load.
        m.text = None;
        m.stale = false;
        m.old_side = false;
        m.with_deletions = false;
        Ok(m)
    }

    /// Remove the marks `letters` names, for an agent. A mark the user made
    /// or wrote in goes whole, and only when `user_asked` says the user
    /// explicitly asked for that. Nothing goes unless every one may. The
    /// letters removed.
    pub fn delete(&mut self, letters: &[String], user_asked: bool) -> Result<Vec<String>, String> {
        let seqs: Vec<usize> = letters
            .iter()
            .map(|l| self.find_label(l).ok_or_else(|| no_mark(l)))
            .collect::<Result<_, _>>()?;
        let theirs: Vec<String> = self
            .marks
            .iter()
            .filter(|m| seqs.contains(&m.seq) && m.is_users())
            .map(|m| format!("[{}]", m.label))
            .collect();
        if !user_asked && !theirs.is_empty() {
            let (are, they, go) = if theirs.len() == 1 {
                ("is", "it", "goes")
            } else {
                ("are", "they", "go")
            };
            return Err(format!(
                "{} {are} the user's: {they} {go} only when the user explicitly asks for \
                 that (user_explicitly_asked_to_delete). To say a mark is dealt with, \
                 reply under it",
                theirs.join(" ")
            ));
        }
        let cleared = self
            .marks
            .iter()
            .filter(|m| seqs.contains(&m.seq))
            .map(|m| m.label.clone())
            .collect();
        self.marks.retain(|m| !seqs.contains(&m.seq));
        Ok(cleared)
    }

    /// A show's doings, all or nothing: with `replace`, the agent's own
    /// marks go first; then the deletions, which need `user_asked` for a
    /// mark the user made or wrote under; then the moves. The additions
    /// are checked and handed back for the viewer to place.
    pub fn edit(
        &mut self,
        entries: &[MarkEntry],
        replace: bool,
        user_asked: bool,
    ) -> Result<Edited, String> {
        let mut deletes = Vec::new();
        let mut moves = Vec::new();
        let mut adds = Vec::new();
        for e in entries {
            match e {
                MarkEntry::Add(spec) => {
                    check_lines(spec.start, spec.end)?;
                    adds.push(spec.clone());
                }
                MarkEntry::Move {
                    letter,
                    path,
                    start,
                    end,
                } => {
                    check_lines(*start, *end)?;
                    let seq = self.find_label(letter).ok_or_else(|| no_mark(letter))?;
                    moves.push((seq, path.clone(), *start, *end));
                }
                MarkEntry::Delete { letter } => {
                    deletes.push(parse_letter(letter).ok_or_else(|| no_mark(letter))?);
                }
            }
        }
        if adds.len() > MAX_MARKS {
            return Err(format!(
                "at most {MAX_MARKS} new marks per call (got {}); keep the ones that matter most",
                adds.len()
            ));
        }
        // Checked before anything changes.
        let theirs: Vec<String> = deletes
            .iter()
            .filter_map(|l| self.find_label(l).and_then(|seq| self.get(seq)))
            .filter(|m| m.is_users())
            .map(|m| format!("[{}]", m.label))
            .collect();
        if !user_asked && !theirs.is_empty() {
            let (are, they, go) = if theirs.len() == 1 {
                ("is", "it", "goes")
            } else {
                ("are", "they", "go")
            };
            return Err(format!(
                "{} {are} the user's: {they} {go} only when the user explicitly asks for \
                 that (user_explicitly_asked_to_delete). To say a mark is dealt with, \
                 reply under it",
                theirs.join(" ")
            ));
        }
        for l in &deletes {
            self.find_label(l).ok_or_else(|| no_mark(l))?;
        }
        let mut deleted = if replace {
            self.replace_mine()
        } else {
            Vec::new()
        };
        deleted.extend(self.delete(&deletes, user_asked)?);
        let mut moved = Vec::new();
        for (seq, path, start, end) in moves {
            let Some(label) = self.get(seq).map(|m| m.label.clone()) else {
                continue;
            };
            let m = self.move_to(&label, Some(path), start, end)?;
            moved.push(m.info());
        }
        Ok(Edited {
            deleted,
            moved,
            adds,
        })
    }

    /// Remove the agent's own marks: the ones the user has not written
    /// under. The letters removed.
    pub fn replace_mine(&mut self) -> Vec<String> {
        let gone: Vec<String> = self
            .marks
            .iter()
            .filter(|m| !m.is_users())
            .map(|m| m.label.clone())
            .collect();
        self.marks.retain(Mark::is_users);
        gone
    }
}

fn check_lines(start: usize, end: usize) -> Result<(), String> {
    if start == 0 || end < start {
        return Err(format!(
            "lines {start}-{end}: lines start at 1 and run forward"
        ));
    }
    Ok(())
}

fn no_mark(letter: &str) -> String {
    let shown = letter.trim().trim_matches(['[', ']']).trim().to_uppercase();
    if shown.chars().any(|c| c.is_ascii_digit()) {
        format!(
            "no mark [{shown}]; replies have no letters of their own, name the mark they are under"
        )
    } else {
        format!("no mark [{shown}]")
    }
}

// ---- as reported to agents --------------------------------------------------

/// A mark as reported to an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkInfo {
    pub label: String,
    pub author: Author,
    pub path: String,
    pub start: usize,
    pub end: usize,
    /// The lines are the old side's: deleted lines.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub old_side: bool,
    /// Deleted lines were among those marked, with others.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub with_deletions: bool,
    /// Its lines are nowhere to be found since the last load.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// The user's, and the agent has not read it yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
    /// The user's: they made it or wrote in its thread, so it goes only
    /// when they ask, and whole.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub theirs: bool,
    /// The words on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The marked code, as it was when the mark was made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    /// The thread under it, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thread: Vec<TurnInfo>,
}

impl Default for MarkInfo {
    fn default() -> Self {
        Self {
            label: String::new(),
            author: Author::Agent,
            path: String::new(),
            start: 0,
            end: 0,
            old_side: false,
            with_deletions: false,
            stale: false,
            new: false,
            theirs: false,
            text: None,
            lines: Vec::new(),
            thread: Vec::new(),
        }
    }
}

/// A turn in a thread, as reported to an agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInfo {
    pub author: Author,
    pub text: String,
    /// The user's, and the agent has not read it yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub new: bool,
}

/// The most code lines a mark is reported with.
pub const MAX_MARK_LINES: usize = 12;

impl MarkInfo {
    pub fn lines_word(&self) -> String {
        let side = if self.old_side { "old " } else { "" };
        if self.start == self.end {
            format!("{side}{}", self.start)
        } else {
            format!("{side}{}-{}", self.start, self.end)
        }
    }

    /// One line, plus the code for the user's marks and the thread.
    pub fn render(&self) -> String {
        let mut flags = Vec::new();
        if self.new {
            flags.push("new");
        }
        if self.stale {
            flags.push("stale");
        }
        if self.with_deletions {
            flags.push("includes deleted lines");
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" ({})", flags.join(", "))
        };
        let mut out = format!(
            "[{}] {} mark {} {}{flags}",
            self.label,
            self.author.word(),
            self.path,
            self.lines_word()
        );
        match &self.text {
            Some(t) if !t.is_empty() => {
                out.push_str(": ");
                out.push_str(&t.replace('\n', "\n    "));
            }
            _ => {}
        }
        if self.author == Author::User {
            let w = self.end.to_string().len();
            for (i, line) in self.lines.iter().take(MAX_MARK_LINES).enumerate() {
                out.push_str(&format!("\n    {:>w$} | {line}", self.start + i));
            }
            if self.lines.len() > MAX_MARK_LINES {
                out.push_str(&format!(
                    "\n    … {} more lines",
                    self.lines.len() - MAX_MARK_LINES
                ));
            }
        }
        for t in &self.thread {
            let new = if t.new { " (new)" } else { "" };
            out.push_str(&format!(
                "\n    {}{new}: {}",
                t.author.word(),
                t.text.replace('\n', "\n      ")
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hl(path: &str, start: usize, end: usize, text: Option<&str>) -> MarkSpec {
        MarkSpec {
            path: path.to_string(),
            start,
            end,
            text: text.map(str::to_owned),
        }
    }

    fn comment(path: &str, start: usize, end: usize, text: &str) -> Op {
        Op::Mark {
            author: Author::User,
            path: path.to_string(),
            start,
            end,
            old_side: false,
            with_deletions: false,
            text: text.to_string(),
            lines: (start..=end).map(|i| format!("line {i}")).collect(),
        }
    }

    fn labels(m: &Marks) -> Vec<String> {
        m.marks.iter().map(|m| m.label.clone()).collect()
    }

    #[test]
    fn new_marks_are_ordered_as_the_viewer_shows_them() {
        let paths = |adds: &[MarkSpec]| -> Vec<(String, usize)> {
            adds.iter().map(|a| (a.path.clone(), a.start)).collect()
        };
        // A comparison lists files by path.
        let mut adds = vec![
            hl("src/z.rs", 3, 3, None),
            hl("a.rs", 9, 9, None),
            hl("src/z.rs", 1, 2, None),
        ];
        in_view_order(&mut adds, &["src/z.rs".to_string()], true);
        assert_eq!(
            paths(&adds),
            [
                ("a.rs".to_string(), 9),
                ("src/z.rs".to_string(), 1),
                ("src/z.rs".to_string(), 3)
            ]
        );
        // Files as they are come as asked for, then by line.
        let mut adds = vec![
            hl("a.rs", 9, 9, None),
            hl("src/z.rs", 3, 3, None),
            hl("src/z.rs", 1, 2, None),
        ];
        in_view_order(
            &mut adds,
            &["src/z.rs".to_string(), "a.rs".to_string()],
            false,
        );
        assert_eq!(
            paths(&adds),
            [
                ("src/z.rs".to_string(), 1),
                ("src/z.rs".to_string(), 3),
                ("a.rs".to_string(), 9)
            ]
        );
        // Without files, as the marks first name them.
        let mut adds = vec![
            hl("src/z.rs", 3, 3, None),
            hl("a.rs", 9, 9, None),
            hl("src/z.rs", 1, 2, None),
        ];
        in_view_order(&mut adds, &[], false);
        assert_eq!(
            paths(&adds),
            [
                ("src/z.rs".to_string(), 1),
                ("src/z.rs".to_string(), 3),
                ("a.rs".to_string(), 9)
            ]
        );
    }

    #[test]
    fn letters_go_on_like_spreadsheet_columns() {
        let got: Vec<String> = [0, 1, 25, 26, 27, 51, 52, 701, 702]
            .into_iter()
            .map(letter)
            .collect();
        assert_eq!(got, ["A", "B", "Z", "AA", "AB", "AZ", "BA", "ZZ", "AAA"]);
        assert_eq!(parse_letter(" [c] "), Some("C".to_string()));
        assert_eq!(parse_letter("C2"), None, "replies have no letters");
        assert_eq!(parse_letter(""), None);
    }

    #[test]
    fn marks_accumulate_whoever_made_them_and_letters_go_on() {
        let mut m = Marks::default();
        m.apply(&Op::Marks {
            marks: vec![hl("a.rs", 1, 2, Some("one")), hl("a.rs", 5, 5, None)],
        });
        m.apply(&comment("a.rs", 3, 3, "mine"));
        assert_eq!(labels(&m), ["A", "B", "C"]);
        m.apply(&Op::Marks {
            marks: vec![hl("a.rs", 9, 9, None)],
        });
        assert_eq!(labels(&m), ["A", "B", "C", "D"], "a show removes nothing");
        assert!(!m.marks[2].seen, "the user's mark starts unread");
        assert!(m.marks[0].seen, "the agent's is its own");
        assert_eq!(m.mine().len(), 3);
        assert_eq!(m.replace_mine(), ["A", "B", "D"]);
        assert_eq!(labels(&m), ["C"]);
        m.apply(&Op::DeleteAll);
        assert!(m.marks.is_empty());
    }

    #[test]
    fn an_agent_places_marks_in_the_order_given() {
        let mut m = Marks::default();
        m.apply(&Op::Marks {
            marks: vec![hl("a.rs", 1, 2, Some("look"))],
        });
        let add = |specs: &[MarkSpec]| -> Vec<MarkEntry> {
            specs.iter().cloned().map(MarkEntry::Add).collect()
        };
        let err = m
            .edit(&add(&[hl("a.rs", 5, 4, Some("x"))]), false, false)
            .unwrap_err();
        assert!(err.contains("run forward"), "{err}");
        let err = m
            .edit(
                &add(&[hl("a.rs", 5, 5, Some("ok")), hl("a.rs", 7, 6, None)]),
                false,
                false,
            )
            .unwrap_err();
        assert!(err.contains("run forward"), "{err}");
        assert_eq!(m.marks.len(), 1, "nothing placed when one is unsound");
        assert_eq!(m.edit(&[], false, false).unwrap(), Edited::default());
        let edited = m
            .edit(
                &add(&[
                    hl("b.rs", 1, 1, Some("and this")),
                    hl("a.rs", 5, 6, Some(" unused ")),
                    hl("a.rs", 9, 9, Some("  ")),
                ]),
                false,
                false,
            )
            .unwrap();
        assert_eq!(edited.adds.len(), 3, "new marks are the viewer's to place");
        let seqs = m.apply(&Op::Marks { marks: edited.adds });
        let placed: Vec<(&str, &str)> = seqs
            .iter()
            .map(|s| m.get(*s).unwrap())
            .map(|m| (m.label.as_str(), m.path.as_str()))
            .collect();
        assert_eq!(
            placed,
            [("B", "b.rs"), ("C", "a.rs"), ("D", "a.rs")],
            "lettered in the order given"
        );
        let c = m.get(2).unwrap();
        assert_eq!(
            (c.label.as_str(), c.author, c.seen, c.text.is_none()),
            ("C", Author::Agent, true, true),
            "it is lettered, read, and takes its lines at the next load"
        );
        assert_eq!(c.comment.as_deref(), Some(" unused "));
        assert_eq!(m.get(3).unwrap().comment.as_deref(), Some("  "));
        assert!(!c.is_users());
        assert_eq!(m.users_words(), 0);
        assert_eq!(
            m.read(&[]).unwrap()[1].render(),
            "[C] agent mark a.rs 5-6:  unused "
        );
        let only = m.read(&["b".to_string()]).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].label, "B");
        assert!(m.read(&["Q".to_string()]).is_err());
        m.apply(&Op::Delete { seq: 3, n: None });
        m.apply(&Op::Delete { seq: 1, n: None });
        let moved = m.move_to("C", None, 7, 8).unwrap();
        assert_eq!(
            (moved.start, moved.end),
            (7, 8),
            "an agent moves its own mark"
        );
        m.apply(&Op::Anchored {
            marks: vec![Anchored {
                seq: 2,
                start: 7,
                end: 8,
                stale: false,
                text: Some(vec!["x".to_string(), "y".to_string()]),
            }],
        });
        let same = m.move_to("C", Some("a.rs".to_string()), 7, 8).unwrap();
        assert!(same.text.is_some(), "moved nowhere, it keeps its text");
        let moved = m.move_to("A", None, 9, 9).unwrap();
        assert_eq!((moved.start, moved.end), (9, 9), "shown ones too");
        let err = m.reply("C", "and?").unwrap_err();
        assert!(err.starts_with("[C] is your own"), "{err}");
        m.apply(&Op::Turn {
            seq: 2,
            author: Author::User,
            text: "why?".to_string(),
        });
        assert!(m.marks[1].is_users(), "the user wrote under it");
        assert_eq!(m.replace_mine(), ["A"]);
        assert!(m.delete(&["C".to_string()], false).is_err());
    }

    #[test]
    fn a_thread_is_the_users_once_they_wrote_in_it() {
        let mut m = Marks::default();
        m.apply(&Op::Marks {
            marks: vec![hl("a.rs", 1, 1, Some("look"))],
        });
        assert!(!m.marks[0].is_users());
        let err = m.reply("A", "hello?").unwrap_err();
        assert!(err.contains("your own"), "{err}");
        m.apply(&Op::Turn {
            seq: 0,
            author: Author::User,
            text: "not a loop".to_string(),
        });
        assert!(m.marks[0].is_users());
        assert_eq!((m.unread(), m.users_words()), (1, 1));
        assert_eq!(m.reply("a", " agreed ").unwrap(), "A");
        let thread: Vec<(Author, &str, bool)> = m.marks[0]
            .thread
            .iter()
            .map(|t| (t.author, t.text.as_str(), t.seen))
            .collect();
        assert_eq!(
            thread,
            [
                (Author::User, "not a loop", false),
                (Author::Agent, "agreed", true)
            ]
        );
        // Nothing is the agent's alone any more: replacing its own touches
        // nothing, and the thread goes whole only when the user asked.
        assert_eq!(m.replace_mine(), Vec::<String>::new());
        assert!(m.delete(&["A".to_string()], false).is_err());
        assert_eq!(m.delete(&["A".to_string()], true).unwrap(), ["A"]);
        assert!(m.marks.is_empty());
    }

    #[test]
    fn deleting_is_whole_marks_and_the_users_need_the_flag() {
        let mut m = Marks::default();
        m.apply(&Op::Marks {
            marks: vec![hl("a.rs", 1, 1, Some("a")), hl("a.rs", 9, 9, Some("b"))],
        });
        m.apply(&comment("a.rs", 3, 3, "one"));
        m.apply(&comment("a.rs", 5, 5, "two"));
        m.reply("C", "noted").unwrap();
        m.apply(&Op::Turn {
            seq: 1,
            author: Author::User,
            text: "why b?".to_string(),
        });
        // [B] is the user's now: they wrote under it.
        let err = m
            .delete(&["A".to_string(), "b".to_string()], false)
            .unwrap_err();
        assert!(
            err.starts_with("[B] is the user's: it goes only when the user explicitly asks")
                && err.ends_with("reply under it"),
            "{err}"
        );
        assert_eq!(labels(&m), ["A", "B", "C", "D"], "nothing went");
        let all: Vec<String> = ["B", "C", "D"].iter().map(|l| l.to_string()).collect();
        let err = m.delete(&all, false).unwrap_err();
        assert!(err.starts_with("[B] [C] [D] are the user's"), "{err}");
        assert_eq!(m.replace_mine(), ["A"]);
        assert_eq!(
            m.delete(&["C2".to_string()], true).unwrap_err(),
            "no mark [C2]; replies have no letters of their own, name the mark they are under"
        );
        assert_eq!(m.delete(&["C".to_string()], true).unwrap(), ["C"]);
        assert_eq!(
            m.delete(&["B".to_string(), "D".to_string()], true).unwrap(),
            ["B", "D"]
        );
        assert!(m.marks.is_empty());
        assert_eq!(m.delete(&[], false).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn the_user_edits_and_deletes_their_own_words_only() {
        let mut m = Marks::default();
        m.apply(&comment("a.rs", 3, 3, "one"));
        m.reply("A", "fixed").unwrap();
        m.apply(&Op::Turn {
            seq: 0,
            author: Author::User,
            text: "thanks".to_string(),
        });
        m.read(&[]).unwrap();
        assert_eq!(m.unread(), 0);
        assert_eq!(
            m.apply(&Op::EditTurn {
                seq: 0,
                n: 1,
                text: "no".to_string()
            }),
            Vec::<usize>::new(),
            "the agent's turn is not the user's to edit"
        );
        m.apply(&Op::EditTurn {
            seq: 0,
            n: 2,
            text: "thanks!".to_string(),
        });
        assert_eq!(m.marks[0].thread[1].text, "thanks!");
        assert_eq!(m.unread(), 1, "edited, it is new again");
        m.apply(&Op::Delete { seq: 0, n: Some(1) });
        assert_eq!(m.marks[0].thread.len(), 2, "the agent's turn stays");
        m.apply(&Op::Delete { seq: 0, n: Some(2) });
        assert_eq!(m.marks[0].thread.len(), 1);
        m.apply(&Op::Turn {
            seq: 0,
            author: Author::User,
            text: "again".to_string(),
        });
        assert_eq!(m.marks[0].thread[1].n, 3, "numbers are not reused");
        m.apply(&Op::Delete { seq: 0, n: None });
        assert!(m.marks.is_empty());
    }

    #[test]
    fn a_review_sorts_by_place_and_reads_everything() {
        let mut m = Marks::default();
        m.apply(&comment("b.rs", 1, 1, "late"));
        m.apply(&comment("a.rs", 9, 9, "second"));
        m.apply(&comment("a.rs", 2, 3, "first"));
        m.reply("A", "ok").unwrap();
        m.apply(&Op::Turn {
            seq: 0,
            author: Author::User,
            text: "sure".to_string(),
        });
        assert_eq!(m.unread(), 4);
        let infos = m.read(&[]).unwrap();
        let order: Vec<&str> = infos.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(order, ["C", "B", "A"]);
        assert!(infos.iter().all(|i| i.new && i.theirs));
        assert_eq!(m.unread(), 0);
        let text = infos[2].render();
        assert_eq!(
            text,
            "[A] user mark b.rs 1 (new): late\n    1 | line 1\n    agent: ok\n    user (new): sure"
        );
        let text = m.read(&[]).unwrap()[2].render();
        assert_eq!(
            text,
            "[A] user mark b.rs 1: late\n    1 | line 1\n    agent: ok\n    user: sure"
        );
    }

    #[test]
    fn moving_takes_the_users_comment_to_new_lines() {
        let mut m = Marks::default();
        m.apply(&Op::Marks {
            marks: vec![hl("a.rs", 1, 1, None)],
        });
        m.apply(&comment("a.rs", 3, 3, "here"));
        let err = m.move_to("B", None, 5, 4).unwrap_err();
        assert!(err.contains("run forward"), "{err}");
        let moved = m.move_to("b", Some("c.rs".to_string()), 40, 41).unwrap();
        assert_eq!(
            (
                moved.path.as_str(),
                moved.start,
                moved.end,
                moved.text.is_none()
            ),
            ("c.rs", 40, 41, true)
        );
        m.apply(&Op::Anchored {
            marks: vec![Anchored {
                seq: 1,
                start: 41,
                end: 42,
                stale: false,
                text: Some(vec!["x".to_string(), "y".to_string()]),
            }],
        });
        assert_eq!((m.marks[1].start, m.marks[1].end), (41, 42));
        assert_eq!(m.marks[1].text.as_deref().map(|t| t.len()), Some(2));
    }

    #[test]
    fn long_comments_show_twelve_lines_and_say_how_many_more() {
        let info = MarkInfo {
            label: "B".to_string(),
            author: Author::User,
            path: "a.txt".to_string(),
            start: 1,
            end: 20,
            lines: (1..=20).map(|i| format!("l{i}")).collect(),
            ..Default::default()
        };
        let text = info.render();
        assert!(
            text.contains("12 | l12") && !text.contains("13 | l13"),
            "{text}"
        );
        assert!(text.ends_with("… 8 more lines"), "{text}");
    }
}
