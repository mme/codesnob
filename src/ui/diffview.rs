//! The flat diff: files, hunks and lines as one scrollable list of rows,
//! with a sticky file header, line numbers, syntax and intra-line highlights.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

use super::Palette;
use super::diagram::Picture;
use crate::diff::{self, FileDiff, Kind, Status};
use crate::git::FileChange;
use crate::highlight::Highlighter;
use crate::marks::Author;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    /// A line of the description block above the files.
    Desc(usize),
    /// A mark's letter and the first line of its text, below its lines.
    Mark(usize),
    /// Line `k` (from 1) of a mark's text.
    MarkLine(usize, usize),
    /// A turn of the thread under a mark's row: the mark's index, the
    /// turn's index.
    Turn(usize, usize),
    /// Line `k` (from 1) of a turn's text.
    TurnLine(usize, usize, usize),
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    Note(&'static str),
    Blank,
    /// The comment or turn being typed, where it will go.
    Compose,
    /// Line `k` (from 1) of what is being typed.
    ComposeLine(usize),
}

impl Row {
    /// Rows the cursor may rest on: not the spacing, nor what is being
    /// typed. The continuation of some text rests it only when the text is
    /// taller than the screen; see [`DiffView::move_cursor`].
    fn rests(self) -> bool {
        !matches!(self, Row::Blank | Row::Compose | Row::ComposeLine(_))
    }

    /// A row of some text after its first.
    fn continues(self) -> bool {
        matches!(self, Row::MarkLine(..) | Row::TurnLine(..))
    }
}

/// A block of text rows placed after a line: a mark with its thread, or a
/// comment being typed where no mark is yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Block {
    Mark(usize),
    Compose,
}

/// The lines of some text: at least one, even when empty.
fn text_lines(text: &str) -> std::str::Split<'_, char> {
    text.split('\n')
}

/// A picture in some text, the same one as long as it is the same
/// drawing.
#[derive(Clone, Debug)]
pub struct Pic(pub Arc<Picture>);

impl PartialEq for Pic {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Pic {}

/// The picture line `k` of some text falls in, if any, and how many lines
/// into it.
fn picture_at(pictures: &[(usize, Pic)], k: usize) -> Option<(&Pic, usize)> {
    pictures
        .iter()
        .find(|(at, p)| (*at..*at + usize::from(p.0.rows)).contains(&k))
        .map(|(at, p)| (p, k - at))
}

/// A mark as drawn: its file, its lines, its letter and text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    pub path: String,
    pub start: usize,
    pub end: usize,
    /// The letter it goes by, fixed when it was made.
    pub label: String,
    /// Its place in the order marks were made; a later mark wins the gutter.
    pub seq: usize,
    pub author: Author,
    /// The words on it.
    pub comment: Option<String>,
    /// Pictures among the words, each at the line it starts on; the lines
    /// it takes are blank in `comment`.
    pub pictures: Vec<(usize, Pic)>,
    /// Its lines no longer exist; drawn near where they were, untinted.
    pub stale: bool,
    /// Its lines are the old side's: deleted lines in a diff.
    pub old_side: bool,
    /// The thread under it, oldest first.
    pub thread: Vec<Turn>,
}

/// A turn in the thread under a mark, drawn on a row of its own below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    /// Its number under the mark.
    pub n: usize,
    pub author: Author,
    pub text: String,
    /// Pictures among the words, as on a mark.
    pub pictures: Vec<(usize, Pic)>,
}

/// Text being typed, shown where it will go once saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composing {
    pub at: ComposeAt,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposeAt {
    /// A new comment on lines `line` to `end` of `path`, with the letter
    /// it will get.
    Comment {
        path: String,
        line: usize,
        end: usize,
        old_side: bool,
        label: String,
    },
    /// New text for the row of the mark `seq` names.
    EditMark(usize),
    /// A new turn at the end of the thread under mark `seq`.
    Turn(usize),
    /// New text for turn `n` under mark `seq`.
    EditTurn(usize, usize),
}

/// Lines the user marked for a comment, in the terms marks are kept in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    pub path: String,
    pub start: usize,
    pub end: usize,
    /// The lines are the old side's: only deleted lines were marked.
    pub old_side: bool,
    /// Deleted lines were among those marked, with others.
    pub with_deletions: bool,
    /// The marked lines as they are now.
    pub text: Vec<String>,
}

/// Where the cursor is, expressed in terms that survive a reload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    path: String,
    /// (side, line number): side 0 is the old file, 1 the new file.
    line: Option<(u8, usize)>,
    /// The cursor was on a hunk header, not on a line.
    at_hunk: bool,
    /// The cursor was on a mark's own row: the mark, by its place in the
    /// order marks were made.
    at_mark: Option<usize>,
    /// The cursor was on a turn under that mark: the turn, by index and
    /// by its number, which stays when turns before it go.
    at_turn: Option<(usize, usize)>,
    /// How many rows into that text the cursor was, for text taller than
    /// the screen.
    at_line: usize,
    /// How many rows below the top of the view the cursor sat.
    offset: usize,
}

impl Anchor {
    /// The same place, wherever on the screen it sits.
    pub fn same_place(&self, other: &Anchor) -> bool {
        self.path == other.path
            && self.line == other.line
            && self.at_hunk == other.at_hunk
            && self.at_mark == other.at_mark
            && self.at_turn.map(|(_, n)| n) == other.at_turn.map(|(_, n)| n)
            && self.at_line == other.at_line
    }
}

#[derive(Default)]
pub struct DiffView {
    pub files: Vec<FileDiff>,
    rows: Vec<Row>,
    file_starts: Vec<usize>,
    hunk_starts: Vec<usize>,
    gutter_w: usize,
    /// The files shown, by index; empty shows every file.
    only: Vec<usize>,
    /// The description block: a commit message or PR body, already wrapped.
    desc: Vec<Line<'static>>,
    /// The first row shown.
    pub scroll: usize,
    pub hscroll: usize,
    /// The row the cursor is on. It never rests on a blank row, and the
    /// view scrolls to keep it in sight.
    cursor: usize,
    /// The height of the body at the last draw; what "in sight" means.
    body_h: usize,
    /// A line to mark with ▶ in a plain file: (path, line number).
    pub target: Option<(String, usize)>,
    /// Scroll to `target` at the next draw, once the screen height is known.
    pub place_target: bool,
    /// The mark to open at then, by its place in the order marks were
    /// made: the first one a show call placed. Else the first shown.
    pub place_seq: Option<usize>,
    /// Marks, drawn over their lines.
    marks: Vec<Mark>,
    /// The row of each mark, by index; `None` when not shown.
    mark_rows: Vec<Option<usize>>,
    /// The row of each mark's first line shown, by index; `None` when none
    /// is, as for a stale mark.
    mark_line_rows: Vec<Option<usize>>,
    /// The mark last jumped to, by index; `<` and `>` go on from here.
    current_mark: Option<usize>,
    /// Rows being marked for a comment, drawn as a would-be mark.
    pub marking: Option<(usize, usize)>,
    /// What is being typed, drawn where it will go.
    compose: Option<Composing>,
    /// The row showing it, when it has one of its own.
    compose_row: Option<usize>,
    /// Pictures encoded for the terminal, by picture, the rows of it shown
    /// and how many, and where it starts on screen.
    encoded: RefCell<HashMap<(usize, usize, u16, u16), Protocol>>,
}

impl DiffView {
    /// Show a diff, also showing the lines `extra` names per path even where
    /// nothing changed.
    pub fn set_files_showing(
        &mut self,
        changes: Vec<FileChange>,
        context: usize,
        extra: &dyn Fn(&str) -> Vec<(usize, usize)>,
    ) {
        self.files = changes
            .into_iter()
            .map(|c| {
                let path = c
                    .new_path
                    .clone()
                    .or(c.old_path.clone())
                    .unwrap_or_default();
                diff::build_with(c, context, &extra(&path))
            })
            .collect();
        self.only.clear();
        self.target = None;
        self.build_rows();
    }

    /// Replace the marks, keeping the line in view. `<` and `>` step
    /// through them by where they sit: in the order the files are shown,
    /// then by line.
    pub fn set_marks(&mut self, mut marks: Vec<Mark>) {
        let at = |path: &str| {
            self.files
                .iter()
                .position(|f| f.path == path)
                .unwrap_or(usize::MAX)
        };
        marks.sort_by_key(|m| (at(&m.path), m.start, m.end, m.seq));
        if self.marks != marks {
            let anchor = self.anchor();
            // The mark last jumped to stays that mark, wherever it now
            // sorts; gone, there is none.
            let current = self.current_mark.map(|i| self.marks[i].seq);
            self.current_mark = current.and_then(|seq| marks.iter().position(|m| m.seq == seq));
            self.marks = marks;
            self.build_rows();
            if let Some(a) = anchor {
                self.restore(a);
            }
        }
    }

    pub fn has_marks(&self) -> bool {
        self.mark_rows.iter().any(Option::is_some)
    }

    /// Show what is being typed where it will go, and keep it in sight.
    /// The rows change only when where it goes does.
    pub fn set_compose(&mut self, compose: Option<Composing>) {
        let shape = |c: Option<&Composing>| c.map(|c| (c.at.clone(), text_lines(&c.text).count()));
        let moved = shape(self.compose.as_ref()) != shape(compose.as_ref());
        if moved {
            // Taken while the rows, and what is typed on them, still stand.
            let anchor = self.anchor();
            self.compose = compose;
            self.build_rows();
            if let Some(a) = anchor {
                self.restore(a);
            }
        } else {
            self.compose = compose;
        }
        let shown = match self.compose.as_ref().map(|c| &c.at) {
            Some(ComposeAt::Comment { .. } | ComposeAt::Turn(_)) => self.compose_row,
            Some(ComposeAt::EditMark(seq)) => self.row_of_mark(*seq),
            Some(ComposeAt::EditTurn(seq, n)) => self.row_of_turn(*seq, *n),
            None => None,
        };
        if let Some(row) = shown {
            // The cursor sits on what is being typed, and the view shows
            // its last line, scrolling just enough.
            self.cursor = row;
            let lines = self
                .compose
                .as_ref()
                .map_or(1, |c| text_lines(&c.text).count());
            let last = row + lines - 1;
            for _ in 0..2 {
                let visible = self.visible_at(self.scroll);
                if last + 1 > self.scroll + visible {
                    self.scroll = (last + 1).saturating_sub(visible);
                }
            }
            if row < self.scroll {
                self.scroll = row;
            }
        }
    }

    fn row_of_mark(&self, seq: usize) -> Option<usize> {
        self.marks
            .iter()
            .position(|m| m.seq == seq)
            .and_then(|i| self.mark_rows.get(i).copied().flatten())
    }

    fn row_of_turn(&self, seq: usize, n: usize) -> Option<usize> {
        let i = self.marks.iter().position(|m| m.seq == seq)?;
        let t = self.marks[i].thread.iter().position(|t| t.n == n)?;
        self.rows.iter().position(|r| *r == Row::Turn(i, t))
    }

    /// Where the terminal's cursor belongs while typing: after the text,
    /// on the last row that shows it. `area` is where the view was drawn.
    pub fn compose_cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let c = self.compose.as_ref()?;
        let (row, prefix) = match &c.at {
            ComposeAt::Comment { label, .. } => (self.compose_row?, 2 + label.len() + 3 + 5),
            ComposeAt::Turn(_) => (self.compose_row?, 6 + 5),
            ComposeAt::EditMark(seq) => {
                let row = self.row_of_mark(*seq)?;
                let m = &self.marks[self.marks.iter().position(|m| m.seq == *seq)?];
                let who = if m.author == Author::User { 5 } else { 0 };
                (row, 2 + m.label.len() + 3 + who)
            }
            ComposeAt::EditTurn(seq, n) => {
                let row = self.row_of_turn(*seq, *n)?;
                let m = &self.marks[self.marks.iter().position(|m| m.seq == *seq)?];
                let t = m.thread.iter().find(|t| t.n == *n)?;
                let who = match t.author {
                    Author::User => "you: ".len(),
                    Author::Agent => "agent: ".len(),
                };
                (row, 6 + who)
            }
        };
        let lines: Vec<&str> = text_lines(&c.text).collect();
        let row = row + lines.len() - 1;
        let last = lines.last().map_or(0, |l| l.chars().count());
        if row < self.scroll {
            return None;
        }
        let sticky =
            self.top_file().is_some() && !matches!(self.rows.get(self.scroll), Some(Row::File(_)));
        let y = area.y + (row - self.scroll) as u16 + u16::from(sticky);
        if y >= area.bottom() {
            return None;
        }
        let x = area.x + (prefix + last) as u16;
        Some((x.min(area.right().saturating_sub(1)), y))
    }

    /// Jump to mark `i` (by index): its first line comes to the top, with
    /// its text below the lines, and the cursor lands on that first line.
    /// A mark with no line shown, as a stale one, is opened at its text.
    fn go_to_mark(&mut self, i: usize) -> bool {
        let row = self.mark_rows.get(i).copied().flatten();
        let line = self.mark_line_rows.get(i).copied().flatten();
        match line.or(row) {
            Some(row) => {
                self.jump_to(row);
                self.current_mark = Some(i);
                true
            }
            None => false,
        }
    }

    /// The row the mark `seq` names starts at: its first line shown, else
    /// its text; `None` when it is not shown.
    pub fn start_row(&self, seq: usize) -> Option<usize> {
        let i = self.marks.iter().position(|m| m.seq == seq)?;
        self.mark_line_rows[i].or(self.mark_rows[i])
    }

    /// Put the cursor on the row of the mark `seq` names, scrolling only as
    /// far as needed.
    pub fn rest_on_mark(&mut self, seq: usize) {
        if let Some(row) = self.row_of_mark(seq) {
            self.cursor = row;
            self.follow();
        }
    }

    /// Open at the first mark shown.
    pub fn first_mark(&mut self) -> bool {
        (0..self.mark_rows.len()).any(|i| self.go_to_mark(i))
    }

    /// Jump to turn `n` under the mark `seq` names, if it is shown: the
    /// mark's row at the top, the cursor on the turn.
    pub fn go_to_turn(&mut self, seq: usize, n: usize) -> bool {
        let Some(i) = self.marks.iter().position(|m| m.seq == seq) else {
            return false;
        };
        let top = self.mark_line_rows.get(i).copied().flatten().or(self
            .mark_rows
            .get(i)
            .copied()
            .flatten());
        let (Some(top), Some(turn_row)) = (top, self.row_of_turn(seq, n)) else {
            return false;
        };
        self.jump_to(top);
        self.cursor = turn_row;
        self.follow();
        self.current_mark = Some(i);
        true
    }

    /// Open at the mark `seq` names, if it is shown.
    pub fn go_to_seq(&mut self, seq: usize) -> bool {
        self.marks
            .iter()
            .position(|m| m.seq == seq)
            .is_some_and(|i| self.go_to_mark(i))
    }

    /// Jump to the mark covering `line` of `path`, if one is shown.
    pub fn mark_at(&mut self, path: &str, line: usize) -> bool {
        self.marks
            .iter()
            .position(|m| m.path == path && (m.start..=m.end).contains(&line))
            .is_some_and(|i| self.go_to_mark(i))
    }

    /// The mark jumped to last, if any, as (its letter, its place among
    /// the marks), and how many marks there are.
    pub fn mark_position(&self) -> Option<(Option<(String, usize)>, usize)> {
        if !self.has_marks() {
            return None;
        }
        let current = self
            .current_mark
            .map(|c| (self.marks[c].label.clone(), c + 1));
        Some((current, self.marks.len()))
    }

    /// The letters of the marks covering the cursor's line, newest last.
    pub fn labels_at_cursor(&self) -> Vec<String> {
        let Some(Row::Line(fi, hi, li)) = self.rows.get(self.cursor) else {
            return Vec::new();
        };
        let f = &self.files[*fi];
        let line = &f.hunks[*hi].lines[*li];
        let mut covering: Vec<&Mark> = self
            .marks
            .iter()
            .filter(|m| !m.stale && m.path == f.path && m.covers(f, line))
            .collect();
        covering.sort_by_key(|m| m.seq);
        covering.into_iter().map(|m| m.label.clone()).collect()
    }

    /// The marks shown, as (row, index), top to bottom.
    /// The marks shown, as (the row each starts at, its index), top to
    /// bottom. A mark starts at its first line, or at its text when no
    /// line of its is shown.
    fn shown_marks(&self) -> Vec<(usize, usize)> {
        let mut rows: Vec<(usize, usize)> = (0..self.marks.len())
            .filter_map(|i| self.mark_line_rows[i].or(self.mark_rows[i]).map(|r| (r, i)))
            .collect();
        rows.sort_unstable();
        rows
    }

    /// Where the cursor is among the marks shown: the mark it was last
    /// taken to, when it is still on that mark's first row.
    fn at_mark(&self, marks: &[(usize, usize)]) -> Option<usize> {
        let current = self.current_mark?;
        marks
            .iter()
            .position(|(r, i)| *i == current && *r == self.cursor)
    }

    /// The next mark below the cursor; from a mark, the one after it,
    /// though it start on the same line.
    pub fn next_mark(&mut self) {
        let marks = self.shown_marks();
        let next = match self.at_mark(&marks) {
            Some(pos) => marks.get(pos + 1).copied(),
            None => marks.iter().find(|(r, _)| *r > self.cursor).copied(),
        };
        if let Some((_, i)) = next {
            self.go_to_mark(i);
        }
    }

    /// The mark above the cursor; the mark whose lines, row or replies the
    /// cursor is on is where we are, not where we go.
    pub fn prev_mark(&mut self) {
        let marks = self.shown_marks();
        if let Some(pos) = self.at_mark(&marks) {
            if let Some(&(_, i)) = pos.checked_sub(1).and_then(|p| marks.get(p)) {
                self.go_to_mark(i);
            }
            return;
        }
        // The innermost of the marks the cursor is in: the one starting last.
        let here = marks
            .iter()
            .filter(|(r, i)| (*r..=self.block_end(*i)).contains(&self.cursor))
            .map(|(r, _)| *r)
            .max()
            .unwrap_or(self.cursor);
        if let Some((_, i)) = marks.into_iter().rev().find(|(r, _)| *r < here) {
            self.go_to_mark(i);
        }
    }

    /// The last row of mark `i` (by index): the end of its thread, or of
    /// what is being typed under it.
    fn block_end(&self, i: usize) -> usize {
        let Some(mut r) = self.mark_rows.get(i).copied().flatten() else {
            return 0;
        };
        while matches!(
            self.rows.get(r + 1),
            Some(
                Row::MarkLine(m, _) | Row::Turn(m, _) | Row::TurnLine(m, _, _)
            ) if *m == i
        ) || matches!(
            self.rows.get(r + 1),
            Some(Row::Compose | Row::ComposeLine(_))
        ) {
            r += 1;
        }
        r
    }

    /// The mark covering a line of file `fi`, if any: the newest when
    /// several do, and never a stale one.
    fn mark_on(&self, fi: usize, line: &crate::diff::DiffLine) -> Option<(&Mark, usize)> {
        let f = &self.files[fi];
        self.marks
            .iter()
            .filter(|m| !m.stale && m.path == f.path && m.covers(f, line))
            .max_by_key(|m| m.seq)
            .and_then(|m| mark_no(f, line, m.old_side).map(|no| (m, no)))
    }

    /// The row the cursor is on.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The turn whose row the cursor is on: its mark, by its place in the
    /// order marks were made, and the turn.
    pub fn turn_at_cursor(&self) -> Option<(usize, &Turn)> {
        let (Row::Turn(m, t) | Row::TurnLine(m, t, _)) = *self.rows.get(self.cursor)? else {
            return None;
        };
        let mark = &self.marks[m];
        Some((mark.seq, &mark.thread[t]))
    }

    /// The mark under the cursor, anyone's, by its place in the order marks
    /// were made: the newest covering the cursor's line, or the one whose
    /// row the cursor is on.
    pub fn any_mark_at_cursor(&self) -> Option<usize> {
        match self.rows.get(self.cursor)? {
            Row::Mark(m) | Row::MarkLine(m, _) => Some(self.marks[*m].seq),
            Row::Line(fi, hi, li) => {
                let f = &self.files[*fi];
                let line = &f.hunks[*hi].lines[*li];
                self.marks
                    .iter()
                    .filter(|m| !m.stale && m.path == f.path && m.covers(f, line))
                    .max_by_key(|m| m.seq)
                    .map(|m| m.seq)
            }
            _ => None,
        }
    }

    /// The user's comment under the cursor, by its place in the order marks
    /// were made: the newest covering the cursor's line, or the one whose
    /// row the cursor is on.
    pub fn comment_at_cursor(&self) -> Option<usize> {
        match self.rows.get(self.cursor)? {
            Row::Mark(m) | Row::MarkLine(m, _) => {
                let mark = &self.marks[*m];
                (mark.author == Author::User).then_some(mark.seq)
            }
            Row::Line(fi, hi, li) => {
                let f = &self.files[*fi];
                let line = &f.hunks[*hi].lines[*li];
                self.marks
                    .iter()
                    .filter(|m| {
                        m.author == Author::User
                            && !m.stale
                            && m.path == f.path
                            && m.covers(f, line)
                    })
                    .max_by_key(|m| m.seq)
                    .map(|m| m.seq)
            }
            _ => None,
        }
    }

    /// The lines between rows `a` and `b` (either order) as a region to
    /// comment on: the lines of the file `a` is in, up to its end. `None`
    /// when `a` is not on a line.
    pub fn region(&self, a: usize, b: usize) -> Option<Region> {
        let Row::Line(fi, ..) = *self.rows.get(a)? else {
            return None;
        };
        let f = &self.files[fi];
        let (lo, hi) = (a.min(b), a.max(b));
        let mut old = Vec::new();
        let mut new = Vec::new();
        let mut deletions = false;
        for row in &self.rows[lo..=hi] {
            match *row {
                Row::Line(f2, hi, li) if f2 == fi => {
                    let l = &f.hunks[hi].lines[li];
                    match l.kind {
                        Kind::Del => {
                            deletions = true;
                            old.extend(l.old_no);
                        }
                        _ => new.extend(l.new_no),
                    }
                }
                Row::Line(..) | Row::File(_) => break,
                _ => {}
            }
        }
        let (start, end, old_side, lines) = if new.is_empty() {
            let (s, e) = (*old.iter().min()?, *old.iter().max()?);
            (s, e, true, &f.old_lines)
        } else {
            let (s, e) = (*new.iter().min()?, *new.iter().max()?);
            (s, e, false, &f.new_lines)
        };
        let text = lines
            .get(start - 1..end)
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        Some(Region {
            path: f.path.clone(),
            start,
            end,
            old_side,
            with_deletions: deletions && !old_side,
            text,
        })
    }

    /// How wide the gutter's mark column is: the longest letter.
    fn label_w(&self) -> usize {
        self.marks
            .iter()
            .map(|m| m.label.chars().count())
            .max()
            .unwrap_or(0)
    }

    /// Show files built elsewhere, such as plain files.
    pub fn set_built(&mut self, files: Vec<FileDiff>) {
        self.files = files;
        self.only.clear();
        self.target = None;
        self.build_rows();
    }

    /// The row showing `line` of `path`, if it is on screen.
    pub fn row_of_line(&self, path: &str, line: usize) -> Option<usize> {
        self.rows.iter().position(|r| match *r {
            Row::Line(fi, hi, li) => {
                self.files[fi].path == path
                    && self.files[fi].hunks[hi].lines[li].new_no == Some(line)
            }
            _ => false,
        })
    }

    /// Narrow to one file, or show all with `None`. Returns false, and shows
    /// all, when the path is not part of this diff.
    pub fn narrow_to_path(&mut self, path: Option<&str>) -> bool {
        let fi = match path {
            Some(p) => match self.files.iter().position(|f| f.path == p) {
                Some(fi) => Some(fi),
                None => {
                    self.narrow_to(None);
                    return false;
                }
            },
            None => None,
        };
        self.narrow_to(fi);
        true
    }

    pub fn narrow_to(&mut self, fi: Option<usize>) {
        let only: Vec<usize> = fi.into_iter().collect();
        if self.only != only {
            self.only = only;
            self.build_rows();
        }
    }

    /// Show only the files at `paths`, in diff order. Returns how many of
    /// them are part of this diff; with none, every file shows.
    pub fn narrow_to_paths(&mut self, paths: &[String]) -> usize {
        let only: Vec<usize> = (0..self.files.len())
            .filter(|&fi| paths.iter().any(|p| *p == self.files[fi].path))
            .collect();
        let found = only.len();
        if self.only != only {
            self.only = only;
            self.build_rows();
        }
        found
    }

    /// The one file shown, when narrowed to a single file.
    pub fn narrowed(&self) -> Option<usize> {
        match self.only.as_slice() {
            [fi] => Some(*fi),
            _ => None,
        }
    }

    /// Replace the description block, keeping the diff line in view.
    pub fn set_description(&mut self, lines: Vec<Line<'static>>) {
        // At the very top, stay there so a new description is what you see.
        let anchor = if self.scroll == 0 {
            None
        } else {
            self.anchor()
        };
        self.desc = lines;
        self.build_rows();
        match anchor {
            Some(a) => self.restore(a),
            None => self.scroll = 0,
        }
    }

    /// Move to the header of file `fi`.
    pub fn scroll_to_file(&mut self, fi: usize) {
        if let Some(r) = self.file_row(fi) {
            self.jump_to(r);
        }
    }

    fn file_row(&self, fi: usize) -> Option<usize> {
        self.file_starts
            .iter()
            .copied()
            .find(|&r| self.rows[r] == Row::File(fi))
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn has_files(&self) -> bool {
        !self.files.is_empty()
    }

    /// Lines added and removed across the files shown.
    pub fn totals(&self) -> (usize, usize) {
        self.shown()
            .iter()
            .map(|&fi| &self.files[fi])
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    /// The indices of the files shown, in diff order.
    pub fn shown(&self) -> Vec<usize> {
        if self.only.is_empty() {
            (0..self.files.len()).collect()
        } else {
            self.only.clone()
        }
    }

    fn build_rows(&mut self) {
        self.rows.clear();
        self.file_starts.clear();
        self.hunk_starts.clear();
        self.compose_row = None;
        self.mark_rows = vec![None; self.marks.len()];
        self.mark_line_rows = vec![None; self.marks.len()];
        let mut placed = vec![false; self.marks.len()];
        let compose_at = self.compose.as_ref().map(|c| c.at.clone());
        let mut max_no = 1;
        for i in 0..self.desc.len() {
            self.rows.push(Row::Desc(i));
        }
        for fi in 0..self.files.len() {
            if !self.only.is_empty() && !self.only.contains(&fi) {
                continue;
            }
            // What the file is, taken first: the rows are pushed through
            // methods that need the whole view.
            let f = &self.files[fi];
            let path = f.path.clone();
            let note = if f.binary {
                Some("binary file")
            } else if f.hunks.is_empty() {
                Some("no textual changes")
            } else {
                None
            };
            max_no = max_no.max(f.old_lines.len()).max(f.new_lines.len());
            // A plain file shown whole needs no hunk header.
            let whole = f.plain
                && f.hunks.len() == 1
                && f.hunks[0].new_start == 1
                && f.hunks[0].new_count == f.new_lines.len();
            let hunk_lens: Vec<usize> = f.hunks.iter().map(|h| h.lines.len()).collect();
            // Every line of the file shown, in order, with the numbers a
            // mark names it by: the new side's (or the file's) and the old.
            let nos: Vec<(Option<usize>, Option<usize>)> = f
                .hunks
                .iter()
                .flat_map(|h| h.lines.iter())
                .map(|l| (mark_no(f, l, false), mark_no(f, l, true)))
                .collect();
            let no_of = |idx: usize, old_side: bool| if old_side { nos[idx].1 } else { nos[idx].0 };
            let lines = nos.len();
            if !self.rows.is_empty() {
                self.rows.push(Row::Blank);
            }
            self.file_starts.push(self.rows.len());
            self.rows.push(Row::File(fi));
            if let Some(note) = note {
                self.rows.push(Row::Note(note));
            }
            // Where each block of text goes: after the line at `after`, or
            // before the first line when `None` and placed. And which line
            // each mark starts at, as (line, mark).
            let mut after: Vec<(Option<usize>, Block)> = Vec::new();
            let mut firsts: Vec<(usize, usize)> = Vec::new();
            for (m, mark) in self.marks.iter().enumerate() {
                if placed[m] || mark.path != path {
                    continue;
                }
                let (first, last) = if mark.stale {
                    // Near where its lines were: after the last line before.
                    let before = (0..lines)
                        .rev()
                        .find(|&i| no_of(i, mark.old_side).is_some_and(|no| no < mark.start));
                    (None, before)
                } else {
                    let mut hit = (0..lines).filter(|&i| {
                        no_of(i, mark.old_side)
                            .is_some_and(|no| (mark.start..=mark.end).contains(&no))
                    });
                    let Some(first) = hit.next() else {
                        continue;
                    };
                    (Some(first), Some(hit.next_back().unwrap_or(first)))
                };
                placed[m] = true;
                if let Some(first) = first {
                    firsts.push((first, m));
                }
                after.push((last, Block::Mark(m)));
            }
            if let Some(ComposeAt::Comment {
                path: at,
                line,
                end,
                old_side,
                ..
            }) = &compose_at
                && self.compose_row.is_none()
                && *at == path
                && let Some(last) = (0..lines)
                    .rev()
                    .find(|&i| no_of(i, *old_side).is_some_and(|no| (*line..=*end).contains(&no)))
            {
                after.push((Some(last), Block::Compose));
            }
            // Blocks with no line before them open the file.
            for (_, b) in after.iter().filter(|(a, _)| a.is_none()) {
                self.push_block(*b, &compose_at);
            }
            let mut idx = 0;
            for (hi, &len) in hunk_lens.iter().enumerate() {
                self.hunk_starts.push(self.rows.len());
                if !whole {
                    self.rows.push(Row::Hunk(fi, hi));
                }
                for li in 0..len {
                    for &(_, m) in firsts.iter().filter(|(i, _)| *i == idx) {
                        self.mark_line_rows[m] = Some(self.rows.len());
                    }
                    self.rows.push(Row::Line(fi, hi, li));
                    for (_, b) in after.iter().filter(|(a, _)| *a == Some(idx)) {
                        self.push_block(*b, &compose_at);
                    }
                    idx += 1;
                }
            }
        }
        if self.files.is_empty() && !self.desc.is_empty() {
            self.rows.push(Row::Blank);
            self.rows.push(Row::Note("no files changed"));
        }
        // With marks, room for their letters so they never cover a digit.
        self.gutter_w = max_no.to_string().len().max(3)
            + self.label_w().max(usize::from(self.marking.is_some()));
        self.fit();
    }

    /// The rows of a mark's text, its thread, and what is being typed
    /// under it, or of a comment being typed on lines with no mark yet.
    fn push_block(&mut self, block: Block, compose_at: &Option<ComposeAt>) {
        match block {
            Block::Mark(m) => {
                self.mark_rows[m] = Some(self.rows.len());
                self.rows.push(Row::Mark(m));
                let n = text_lines(&self.mark_text(m)).count();
                self.rows.extend((1..n).map(|k| Row::MarkLine(m, k)));
                for t in 0..self.marks[m].thread.len() {
                    self.rows.push(Row::Turn(m, t));
                    let n = text_lines(&self.turn_text(m, t)).count();
                    self.rows.extend((1..n).map(|k| Row::TurnLine(m, t, k)));
                }
                if *compose_at == Some(ComposeAt::Turn(self.marks[m].seq)) {
                    self.push_block(Block::Compose, compose_at);
                }
            }
            Block::Compose => {
                self.compose_row = Some(self.rows.len());
                self.rows.push(Row::Compose);
                let n = self
                    .compose
                    .as_ref()
                    .map_or(1, |c| text_lines(&c.text).count());
                self.rows.extend((1..n).map(Row::ComposeLine));
            }
        }
    }

    /// A mark's text as shown: what is being typed for it, else its own.
    fn mark_text(&self, m: usize) -> String {
        let mark = &self.marks[m];
        self.compose
            .as_ref()
            .filter(|c| c.at == ComposeAt::EditMark(mark.seq))
            .map(|c| c.text.clone())
            .or_else(|| mark.comment.clone())
            .unwrap_or_default()
    }

    /// A turn's text as shown: what is being typed for it, else its own.
    fn turn_text(&self, m: usize, t: usize) -> String {
        let mark = &self.marks[m];
        let turn = &mark.thread[t];
        self.compose
            .as_ref()
            .filter(|c| c.at == ComposeAt::EditTurn(mark.seq, turn.n))
            .map_or_else(|| turn.text.clone(), |c| c.text.clone())
    }

    // ---- position --------------------------------------------------------

    pub fn anchor(&self) -> Option<Anchor> {
        self.anchor_of(self.cursor)
    }

    /// Where `row` is, in terms that survive a reload.
    pub fn anchor_of(&self, row: usize) -> Option<Anchor> {
        let fi = self.file_at(row)?;
        let path = self.files[fi].path.clone();
        let mut at_mark = None;
        let mut at_turn = None;
        let mut at_line = 0;
        let (line, at_hunk) = match self.rows.get(row)? {
            Row::Line(fi, hi, li) => {
                let l = &self.files[*fi].hunks[*hi].lines[*li];
                (l.new_no.map(|n| (1, n)).or(l.old_no.map(|n| (0, n))), false)
            }
            Row::Hunk(fi, hi) => {
                let h = &self.files[*fi].hunks[*hi];
                let at = if h.new_count > 0 {
                    (1, h.new_start)
                } else {
                    (0, h.old_start)
                };
                (Some(at), true)
            }
            // A mark's row or a turn's: the mark's first line, and the row
            // itself if it stays.
            Row::Mark(m) | Row::MarkLine(m, _) | Row::Turn(m, _) | Row::TurnLine(m, _, _) => {
                let mark = &self.marks[*m];
                at_mark = Some(mark.seq);
                match self.rows[row] {
                    Row::Turn(_, t) => at_turn = Some((t, mark.thread[t].n)),
                    Row::TurnLine(_, t, k) => {
                        at_turn = Some((t, mark.thread[t].n));
                        at_line = k;
                    }
                    Row::MarkLine(_, k) => at_line = k,
                    _ => {}
                }
                let old = mark.old_side || self.files[fi].status == Status::Deleted;
                (Some((if old { 0 } else { 1 }, mark.start)), false)
            }
            // What is being typed: once saved, the cursor belongs on it.
            Row::Compose | Row::ComposeLine(_) => match self.compose.as_ref().map(|c| &c.at) {
                Some(ComposeAt::Comment { line, old_side, .. }) => {
                    let old = *old_side || self.files[fi].status == Status::Deleted;
                    (Some((if old { 0 } else { 1 }, *line)), false)
                }
                Some(ComposeAt::Turn(seq)) => {
                    at_mark = Some(*seq);
                    at_turn = Some((usize::MAX, usize::MAX));
                    (None, false)
                }
                _ => (None, false),
            },
            _ => (None, false),
        };
        Some(Anchor {
            path,
            line,
            at_hunk,
            at_mark,
            at_turn,
            at_line,
            offset: row.saturating_sub(self.scroll),
        })
    }

    pub fn restore(&mut self, a: Anchor) {
        let Some(row) = self.locate(&a) else {
            self.fit();
            return;
        };
        self.cursor = row;
        self.scroll = self.cursor.saturating_sub(a.offset);
        self.fit();
    }

    /// The row `a` names now, if its file is still shown: the same line,
    /// or the nearest; the same mark's row or turn when it was on one.
    pub fn locate(&self, a: &Anchor) -> Option<usize> {
        let fi = self.files.iter().position(|f| f.path == a.path)?;
        let start = self.file_row(fi)?;
        let end = self
            .file_starts
            .iter()
            .copied()
            .find(|&r| r > start)
            .unwrap_or(self.rows.len());
        let mut best: Option<(usize, usize)> = None; // (distance, row)
        if let Some((side, target)) = a.line {
            for (idx, row) in self.rows[start..end].iter().enumerate() {
                let Row::Line(f, h, l) = row else { continue };
                let line = &self.files[*f].hunks[*h].lines[*l];
                let no = if side == 1 { line.new_no } else { line.old_no };
                let Some(no) = no else { continue };
                let dist = no.abs_diff(target);
                if best.is_none_or(|(d, _)| dist < d) {
                    best = Some((dist, start + idx));
                }
            }
        }
        let mut at = match best {
            // Back onto the hunk header when we were on one.
            Some((_, r)) if a.at_hunk => self
                .hunk_starts
                .iter()
                .rev()
                .find(|&&s| s <= r)
                .copied()
                .unwrap_or(r),
            Some((_, r)) => r,
            None => start,
        };
        // Back onto the mark's row, or the turn's under it, when we were on
        // one and it is still there.
        if let Some(seq) = a.at_mark
            && let Some(m) = self.marks.iter().position(|m| m.seq == seq)
            && let Some(row) = self.mark_rows[m]
        {
            // The same turn by its number; a turn that went leaves the
            // cursor where it was in the thread, or at the thread's end.
            let thread = &self.marks[m].thread;
            at = match a.at_turn {
                Some((i, n)) if !thread.is_empty() => {
                    let t = thread
                        .iter()
                        .position(|t| t.n == n)
                        .unwrap_or(i.min(thread.len() - 1));
                    self.rows
                        .iter()
                        .position(|r| *r == Row::Turn(m, t))
                        .unwrap_or(row)
                }
                _ => row,
            };
            // As far into the text as before, while it has the rows.
            let (_, end) = self.text_span(at);
            at = (at + a.at_line).min(end);
        }
        Some(at)
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    /// Rows of the body that show content below the sticky header, were
    /// the view to start at `scroll`.
    fn visible_at(&self, scroll: usize) -> usize {
        let sticky =
            self.file_at(scroll).is_some() && !matches!(self.rows.get(scroll), Some(Row::File(_)));
        self.body_h.saturating_sub(usize::from(sticky)).max(1)
    }

    /// Bring the cursor onto a row it may rest on, stepping in `dir`
    /// (then the other way) past blank rows.
    fn settle_cursor(&mut self, dir: isize) {
        self.cursor = self.cursor.min(self.max_scroll());
        let rests = |v: &Self, r: usize| v.rows.get(r).is_some_and(|row| row.rests());
        if rests(self, self.cursor) {
            return;
        }
        let step = |r: usize, d: isize| r.checked_add_signed(d).filter(|&r| r < self.rows.len());
        let mut r = self.cursor;
        while let Some(n) = step(r, dir) {
            r = n;
            if rests(self, r) {
                self.cursor = r;
                return;
            }
        }
        let mut r = self.cursor;
        while let Some(n) = step(r, -dir) {
            r = n;
            if rests(self, r) {
                self.cursor = r;
                return;
            }
        }
    }

    /// Scroll just enough to keep the cursor in sight: the view moves only
    /// once the cursor would leave it at an edge.
    fn follow(&mut self) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        // Scrolling can bring in the sticky header, which takes a row, so
        // the check runs again at the new position.
        for _ in 0..2 {
            let visible = self.visible_at(self.scroll);
            if self.cursor + 1 > self.scroll + visible {
                self.scroll = (self.cursor + 1).saturating_sub(visible);
            }
        }
        // On the first row of a block of text, the whole block when it fits.
        let end = self.block_end_at(self.cursor);
        let visible = self.visible_at(self.scroll);
        if end > self.cursor && end - self.cursor < visible && end + 1 > self.scroll + visible {
            self.scroll = end + 1 - visible;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// The last row of the block of text starting at `row`: the thread
    /// under a mark, what is typed under it, or `row` itself.
    fn block_end_at(&self, row: usize) -> usize {
        match self.rows.get(row) {
            Some(Row::Mark(m) | Row::Turn(m, _)) => self.block_end(*m),
            Some(Row::Compose) => {
                let mut r = row;
                while matches!(self.rows.get(r + 1), Some(Row::ComposeLine(_))) {
                    r += 1;
                }
                r
            }
            _ => row,
        }
    }

    /// Keep scroll and cursor within the rows, and the cursor in sight.
    pub fn fit(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
        self.settle_cursor(1);
        self.follow();
    }

    /// Tell the view how tall the body is, which decides what is in sight.
    pub fn resize(&mut self, body_h: usize) {
        self.body_h = body_h.max(1);
        self.fit();
    }

    /// First row index at which the last row lands on the bottom line,
    /// accounting for the sticky header that is drawn above the body.
    pub fn end_scroll(&self, body_h: usize) -> usize {
        let mut s = self.rows.len().saturating_sub(body_h);
        if s > 0 && !matches!(self.rows.get(s), Some(Row::File(_))) {
            s += 1;
        }
        s.min(self.max_scroll())
    }

    /// The index into `files` of the file row `row` belongs to.
    fn file_at(&self, row: usize) -> Option<usize> {
        if self.file_starts.is_empty() || row < self.file_starts[0] {
            // In the description block, above every file.
            return None;
        }
        let i = match self.file_starts.binary_search(&row) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        match self.rows[self.file_starts[i]] {
            Row::File(fi) => Some(fi),
            _ => None,
        }
    }

    /// The index into `files` of the file under the cursor.
    pub fn current_file(&self) -> Option<usize> {
        self.file_at(self.cursor)
    }

    /// The file at the top of the view, whose header sticks.
    fn top_file(&self) -> Option<usize> {
        self.file_at(self.scroll)
    }

    /// Move the cursor by `delta` rows; the view scrolls to keep up.
    /// Move the cursor `delta` rows, one stop at a time. A text's rows
    /// after its first are one stop with it while the text fits on the
    /// screen; a taller text is walked row by row, so all of it can be seen.
    pub fn move_cursor(&mut self, delta: isize) {
        let dir = delta.signum();
        for _ in 0..delta.unsigned_abs() {
            let mut r = self.cursor;
            while let Some(next) = r.checked_add_signed(dir).filter(|&n| n < self.rows.len()) {
                r = next;
                let row = self.rows[r];
                if !row.rests() || row.continues() && self.text_fits(r) {
                    continue;
                }
                self.cursor = r;
                break;
            }
        }
        self.follow();
    }

    /// Whether the text the row `r` belongs to fits on the screen.
    fn text_fits(&self, r: usize) -> bool {
        let (start, end) = self.text_span(r);
        end - start < self.visible_at(self.scroll)
    }

    /// The first and last row of the text `r` is a row of: a mark's words
    /// or a turn's.
    fn text_span(&self, r: usize) -> (usize, usize) {
        let same = |a: Row, b: Row| match (a, b) {
            (Row::Mark(m) | Row::MarkLine(m, _), Row::Mark(n) | Row::MarkLine(n, _)) => m == n,
            (
                Row::Turn(m, t) | Row::TurnLine(m, t, _),
                Row::Turn(n, u) | Row::TurnLine(n, u, _),
            ) => m == n && t == u,
            _ => false,
        };
        let row = self.rows[r];
        let mut start = r;
        while start > 0 && same(row, self.rows[start - 1]) {
            start -= 1;
        }
        let mut end = r;
        while end + 1 < self.rows.len() && same(row, self.rows[end + 1]) {
            end += 1;
        }
        (start, end)
    }

    /// Move the view by `delta` rows, the cursor keeping its place on the
    /// screen where it can.
    pub fn scroll_by(&mut self, delta: isize) {
        let end = self.max_scroll() as isize;
        let before = self.scroll;
        self.scroll = (self.scroll as isize + delta).clamp(0, end) as usize;
        let moved = self.scroll as isize - before as isize;
        let max = self.max_scroll() as isize;
        self.cursor = (self.cursor as isize + moved).clamp(0, max) as usize;
        self.pull_cursor(delta.signum());
    }

    /// Bring the cursor into the rows in sight without scrolling.
    fn pull_cursor(&mut self, dir: isize) {
        let visible = self.visible_at(self.scroll);
        let last = (self.scroll + visible).saturating_sub(1);
        let lo = self.scroll.min(self.max_scroll());
        let hi = last.min(self.max_scroll()).max(lo);
        self.cursor = self.cursor.clamp(lo, hi);
        self.settle_cursor(dir);
        self.follow();
    }

    /// Put the cursor on `row`, at the top of the view.
    pub fn jump_to(&mut self, row: usize) {
        let row = row.min(self.max_scroll());
        self.scroll = row;
        self.cursor = row;
        self.settle_cursor(1);
        self.follow();
    }

    /// Put the cursor on `row`, about a third of the way down the view.
    pub fn center_on(&mut self, row: usize) {
        let row = row.min(self.max_scroll());
        self.scroll = row.saturating_sub(self.body_h / 3);
        self.cursor = row;
        self.settle_cursor(1);
        self.follow();
    }

    /// The cursor on a row the mouse points at, `y` rows below the body's
    /// top, when that row shows content.
    pub fn click(&mut self, y: usize) {
        let sticky =
            self.top_file().is_some() && !matches!(self.rows.get(self.scroll), Some(Row::File(_)));
        let Some(y) = y.checked_sub(usize::from(sticky)) else {
            return;
        };
        let row = self.scroll + y;
        if self.rows.get(row).is_some_and(|r| r.rests()) {
            // A click into text that fits on the screen is a click on it.
            self.cursor = if self.rows[row].continues() && self.text_fits(row) {
                self.text_span(row).0
            } else {
                row
            };
            self.follow();
        }
    }

    pub fn go_top(&mut self) {
        self.scroll = 0;
        self.cursor = 0;
        self.fit();
    }

    pub fn go_bottom(&mut self) {
        self.scroll = self.end_scroll(self.body_h);
        self.cursor = self.max_scroll();
        self.settle_cursor(-1);
        self.follow();
    }

    pub fn next_hunk(&mut self) {
        if let Some(&s) = self.hunk_starts.iter().find(|&&s| s > self.cursor) {
            self.jump_to(s);
        }
    }

    pub fn prev_hunk(&mut self) {
        if let Some(&s) = self.hunk_starts.iter().rev().find(|&&s| s < self.cursor) {
            self.jump_to(s);
        }
    }

    pub fn next_file(&mut self) {
        if let Some(&s) = self.file_starts.iter().find(|&&s| s > self.cursor) {
            self.jump_to(s);
        }
    }

    pub fn prev_file(&mut self) {
        if let Some(&s) = self.file_starts.iter().rev().find(|&&s| s < self.cursor) {
            self.jump_to(s);
        }
    }

    // ---- rendering --------------------------------------------------------

    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        p: &Palette,
        hl: &mut Highlighter,
        picker: Option<&Picker>,
    ) {
        let mut y = area.y;
        let mut idx = self.scroll;
        // Sticky header unless the real header is the first visible row.
        if let Some(fi) = self.top_file()
            && !matches!(self.rows.get(self.scroll), Some(Row::File(_)))
        {
            self.render_file_header(fi, Rect::new(area.x, y, area.width, 1), buf, p);
            y += 1;
        }
        while y < area.bottom() {
            let Some(&row) = self.rows.get(idx) else {
                break;
            };
            let line = Rect::new(area.x, y, area.width, 1);
            self.render_row(idx, row, line, buf, p, hl, picker);
            if idx == self.cursor {
                self.render_cursor(row, line, buf, p);
            }
            y += 1;
            idx += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        idx: usize,
        row: Row,
        area: Rect,
        buf: &mut Buffer,
        p: &Palette,
        hl: &mut Highlighter,
        picker: Option<&Picker>,
    ) {
        match row {
            Row::Mark(m) | Row::MarkLine(m, _) => {
                let k = if let Row::MarkLine(_, k) = row { k } else { 0 };
                let mark = &self.marks[m];
                let (bg, fg) = p.mark_colors(mark.author);
                let tint = Style::default().bg(bg);
                buf.set_style(area, tint);
                let editing = self
                    .compose
                    .as_ref()
                    .is_some_and(|c| c.at == ComposeAt::EditMark(mark.seq));
                let mut lead = vec![Span::styled(
                    format!("  [{}] ", mark.label),
                    tint.fg(fg).add_modifier(Modifier::BOLD),
                )];
                if mark.stale && !editing {
                    lead.push(Span::styled("stale · ", tint.fg(p.note_fg)));
                }
                if mark.author == Author::User {
                    lead.push(Span::styled(
                        "you: ",
                        tint.fg(fg).add_modifier(Modifier::BOLD),
                    ));
                }
                // While its source is edited, the words are what is shown. A
                // picture on the first row still has the lead before it.
                if !editing && let Some((pic, into)) = picture_at(&mark.pictures, k) {
                    let lead_w: usize = lead.iter().map(|s| s.content.chars().count()).sum();
                    if k == 0 {
                        buf.set_line(area.x, area.y, &Line::from(lead), area.width);
                    }
                    self.render_picture(pic, into, idx, lead_w, area, buf, picker);
                    return;
                }
                let text = self.mark_text(m);
                let line = text_lines(&text).nth(k).unwrap_or_default().to_string();
                self.render_text_row(area, buf, lead, line, k, tint.fg(fg));
            }
            Row::Turn(m, t) | Row::TurnLine(m, t, _) => {
                let k = if let Row::TurnLine(_, _, k) = row {
                    k
                } else {
                    0
                };
                let turn = &self.marks[m].thread[t];
                let (bg, fg) = p.mark_colors(turn.author);
                let tint = Style::default().bg(bg);
                buf.set_style(area, tint);
                let editing = self
                    .compose
                    .as_ref()
                    .is_some_and(|c| c.at == ComposeAt::EditTurn(self.marks[m].seq, turn.n));
                let who = match turn.author {
                    Author::User => "you",
                    Author::Agent => "agent",
                };
                let lead = vec![Span::styled(
                    format!("      {who}: "),
                    tint.fg(fg).add_modifier(Modifier::BOLD),
                )];
                if !editing && let Some((pic, into)) = picture_at(&turn.pictures, k) {
                    let lead_w: usize = lead.iter().map(|s| s.content.chars().count()).sum();
                    if k == 0 {
                        buf.set_line(area.x, area.y, &Line::from(lead), area.width);
                    }
                    self.render_picture(pic, into, idx, lead_w, area, buf, picker);
                    return;
                }
                let text = self.turn_text(m, t);
                let line = text_lines(&text).nth(k).unwrap_or_default().to_string();
                self.render_text_row(area, buf, lead, line, k, tint.fg(fg));
            }
            Row::Compose | Row::ComposeLine(_) => {
                let k = if let Row::ComposeLine(k) = row { k } else { 0 };
                let Some(c) = &self.compose else { return };
                let (bg, fg) = p.mark_colors(Author::User);
                let tint = Style::default().bg(bg);
                buf.set_style(area, tint);
                let lead = match &c.at {
                    ComposeAt::Comment { label, .. } => format!("  [{label}] you: "),
                    _ => "      you: ".to_string(),
                };
                let lead = vec![Span::styled(lead, tint.fg(fg).add_modifier(Modifier::BOLD))];
                let line = text_lines(&c.text).nth(k).unwrap_or_default().to_string();
                self.render_text_row(area, buf, lead, line, k, tint.fg(fg));
            }
            Row::Desc(i) => {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(self.desc[i].spans.iter().cloned());
                buf.set_line(area.x, area.y, &Line::from(spans), area.width);
            }
            Row::Blank => {}
            Row::File(fi) => self.render_file_header(fi, area, buf, p),
            Row::Note(text) => {
                let line = Line::from(Span::styled(
                    format!("  {text}"),
                    Style::default().fg(p.note_fg),
                ));
                buf.set_line(area.x, area.y, &line, area.width);
            }
            Row::Hunk(fi, hi) => {
                let h = &self.files[fi].hunks[hi];
                let mut text = if self.files[fi].plain {
                    // An excerpt of a plain file says which lines it shows.
                    let end = h.new_start + h.new_count.saturating_sub(1);
                    format!("  ⋯ lines {}–{end}", h.new_start)
                } else {
                    format!(
                        "@@ -{},{} +{},{} @@",
                        h.old_start, h.old_count, h.new_start, h.new_count
                    )
                };
                if let Some(f) = &h.func {
                    text.push(' ');
                    text.push_str(f);
                }
                let line = Line::from(Span::styled(text, Style::default().fg(p.hunk_fg)));
                buf.set_line(area.x, area.y, &line, area.width);
            }
            Row::Line(fi, hi, li) => self.render_line(idx, fi, hi, li, area, buf, p, hl),
        }
    }

    /// Restyle the cursor's row once drawn: its background shifted, its
    /// line numbers bold.
    fn render_cursor(&self, row: Row, area: Rect, buf: &mut Buffer, p: &Palette) {
        let gutter = match row {
            Row::Line(fi, ..) if self.files[fi].plain => self.gutter_w + 1,
            Row::Line(..) => self.gutter_w * 2 + 2,
            _ => 0,
        } as u16;
        for x in area.x..area.right() {
            let Some(cell) = buf.cell_mut((x, area.y)) else {
                continue;
            };
            let bg = p.cursor_over(cell.bg);
            cell.set_bg(bg);
            if x < area.x + gutter {
                cell.set_fg(p.cursor_fg);
                cell.modifier.insert(Modifier::BOLD);
            }
        }
    }

    /// A picture's rows: drawn once, from the first of its rows on screen
    /// down as far as the screen goes, `lead` cells in from the left. Rows
    /// before that one, and terminals that show no pictures, get the tint
    /// alone.
    #[allow(clippy::too_many_arguments)]
    fn render_picture(
        &self,
        pic: &Pic,
        into: usize,
        idx: usize,
        lead: usize,
        area: Rect,
        buf: &mut Buffer,
        picker: Option<&Picker>,
    ) {
        let Some(picker) = picker else { return };
        // The first row of it on screen draws it: its own first row, or
        // the top of the screen when it starts above.
        if into != 0 && idx != self.scroll {
            return;
        }
        let picture = &pic.0;
        let left = usize::from(picture.rows) - into;
        let shown = (self.scroll + self.visible_at(self.scroll))
            .saturating_sub(idx)
            .min(left)
            .min(usize::from(u16::MAX)) as u16;
        if shown == 0 || area.width as usize <= lead {
            return;
        }
        let cols = picture.cols.min(area.width - lead as u16);
        let key = (Arc::as_ptr(picture) as usize, into, shown, cols);
        let mut encoded = self.encoded.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(slot) = encoded.entry(key) {
            let cell_h = picture.image.height() / u32::from(picture.rows.max(1));
            let cell_w = picture.image.width() / u32::from(picture.cols.max(1));
            let slice = image::imageops::crop_imm(
                &picture.image,
                0,
                into as u32 * cell_h,
                u32::from(cols) * cell_w,
                u32::from(shown) * cell_h,
            )
            .to_image();
            let size = Size::new(cols, shown);
            let Ok(protocol) = picker.new_protocol(
                image::DynamicImage::ImageRgba8(slice),
                size,
                Resize::Fit(None),
            ) else {
                return;
            };
            slot.insert(protocol);
        }
        let at = Rect::new(area.x + lead as u16, area.y, cols, shown);
        Image::new(&encoded[&key])
            .allow_clipping(true)
            .render(at, buf);
    }

    /// One row of a block of text: the lead (letter, who) on the first
    /// line, the same width of blanks on the lines after it.
    fn render_text_row(
        &self,
        area: Rect,
        buf: &mut Buffer,
        lead: Vec<Span<'_>>,
        line: String,
        k: usize,
        style: Style,
    ) {
        let mut spans = if k == 0 {
            lead
        } else {
            let w: usize = lead.iter().map(|s| s.content.chars().count()).sum();
            vec![Span::styled(" ".repeat(w), style)]
        };
        spans.push(Span::styled(line, style));
        buf.set_line(area.x, area.y, &Line::from(spans), area.width);
    }

    fn render_file_header(&self, fi: usize, area: Rect, buf: &mut Buffer, p: &Palette) {
        let f = &self.files[fi];
        let (tag, color) = match f.status {
            Status::Added => ("A", Color::Green),
            Status::Deleted => ("D", Color::Red),
            Status::Modified => ("M", Color::Yellow),
            Status::Renamed => ("R", Color::Magenta),
        };
        let base = Style::default().bg(p.header_bg);
        let mut spans = if f.plain {
            // A file as it is: no change to label.
            vec![Span::styled(
                format!(" {}", f.display_path()),
                base.add_modifier(Modifier::BOLD),
            )]
        } else {
            vec![
                Span::styled(
                    format!(" {tag} "),
                    base.fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(f.display_path(), base.add_modifier(Modifier::BOLD)),
            ]
        };
        if f.binary {
            spans.push(Span::styled("  binary", base.fg(p.note_fg)));
        } else if f.plain {
            let n = f.new_lines.len();
            spans.push(Span::styled(
                format!("  {n} line{}", if n == 1 { "" } else { "s" }),
                base.fg(p.note_fg),
            ));
        } else {
            spans.push(Span::styled(
                format!("  +{}", f.added),
                base.fg(Color::Green),
            ));
            spans.push(Span::styled(
                format!(" −{}", f.removed),
                base.fg(Color::Red),
            ));
        }
        let pos = Span::styled(
            format!(" {}/{} ", fi + 1, self.files.len()),
            base.fg(p.note_fg),
        );
        buf.set_style(area, base);
        let line = Line::from(spans);
        let used = buf.set_line(area.x, area.y, &line, area.width).0;
        let pw = pos.width() as u16;
        if area.right() >= used + pw {
            buf.set_span(area.right() - pw, area.y, &pos, pw);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_line(
        &self,
        idx: usize,
        fi: usize,
        hi: usize,
        li: usize,
        area: Rect,
        buf: &mut Buffer,
        p: &Palette,
        hl: &mut Highlighter,
    ) {
        let l = &self.files[fi].hunks[hi].lines[li];
        let (line_bg, emph_bg, marker) = match l.kind {
            Kind::Add => (Some(p.add_bg), p.add_emph, "+"),
            Kind::Del => (Some(p.del_bg), p.del_emph, "-"),
            Kind::Context => (None, Color::Reset, " "),
        };
        // A marked line, or one being marked, takes the mark's tint over
        // the diff's: the + or - still says what the line is.
        let marked = self.mark_on(fi, l);
        let marking = self
            .marking
            .is_some_and(|(lo, hi)| (lo..=hi).contains(&idx));
        let (line_bg, emph_bg) = match (marked, marking) {
            (_, true) => (Some(p.user_bg), p.user_bg),
            (Some((m, _)), _) => {
                let bg = p.mark_colors(m.author).0;
                (Some(bg), bg)
            }
            _ => (line_bg, emph_bg),
        };
        let row_style = match line_bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        buf.set_style(area, row_style);

        let w = self.gutter_w;
        let plain = self.files[fi].plain;
        let gutter = if plain {
            // A file as it is has one numbering.
            format!(
                "{:>w$} ",
                l.new_no.map(|n| n.to_string()).unwrap_or_default()
            )
        } else {
            format!(
                "{:>w$} {:>w$} ",
                l.old_no.map(|n| n.to_string()).unwrap_or_default(),
                l.new_no.map(|n| n.to_string()).unwrap_or_default(),
            )
        };
        let is_target = plain
            && self.target.as_ref().is_some_and(|(path, line)| {
                *path == self.files[fi].path && l.new_no == Some(*line)
            });
        let (marker, marker_style) = match l.kind {
            _ if is_target => ("▶", row_style.fg(p.hunk_fg)),
            Kind::Add => (marker, row_style.fg(Color::Green)),
            Kind::Del => (marker, row_style.fg(Color::Red)),
            Kind::Context => (marker, row_style),
        };
        let mut spans = Vec::new();
        // The mark column: its letter on the mark's first line. The tint
        // says how far it runs.
        let gutter = match marked {
            Some((m, no)) if !marking && no == m.start => {
                let w = self.label_w();
                let fg = p.mark_colors(m.author).1;
                spans.push(Span::styled(
                    format!("{:<w$}", m.label),
                    row_style.fg(fg).add_modifier(Modifier::BOLD),
                ));
                gutter.chars().skip(w).collect()
            }
            _ => gutter,
        };
        spans.push(Span::styled(gutter, row_style.fg(p.gutter_fg)));
        spans.push(Span::styled(
            format!("{marker} "),
            marker_style.add_modifier(Modifier::BOLD),
        ));

        // Syntax spans for the line's source file side.
        let f = &self.files[fi];
        let (side, src_idx, src) = match l.kind {
            Kind::Del => (0u8, l.old_no.unwrap_or(1) - 1, &f.old_lines),
            _ => (1u8, l.new_no.unwrap_or(1) - 1, &f.new_lines),
        };
        let syntax: Vec<(Style, usize, usize)> = hl
            .lines((fi, side), &f.path, src)
            .get(src_idx)
            .cloned()
            .unwrap_or_default();
        let text = &l.text;

        // Merge syntax ranges with emphasis ranges into flat segments.
        let mut cuts: Vec<usize> = vec![0, text.len()];
        for &(_, s, e) in &syntax {
            cuts.push(s);
            cuts.push(e);
        }
        for &(s, e) in &l.emph {
            cuts.push(s);
            cuts.push(e);
        }
        cuts.sort_unstable();
        cuts.dedup();
        let mut skip = self.hscroll;
        for pair in cuts.windows(2) {
            let (s, e) = (pair[0], pair[1]);
            if e <= s || !text.is_char_boundary(s) || !text.is_char_boundary(e) {
                continue;
            }
            let mut seg = &text[s..e];
            if skip > 0 {
                let n = seg.chars().count();
                if n <= skip {
                    skip -= n;
                    continue;
                }
                let off = seg
                    .char_indices()
                    .nth(skip)
                    .map(|(i, _)| i)
                    .unwrap_or(seg.len());
                seg = &seg[off..];
                skip = 0;
            }
            let mut style = syntax
                .iter()
                .find(|&&(_, ss, se)| ss <= s && e <= se)
                .map(|&(st, _, _)| st)
                .unwrap_or_default();
            if let Some(bg) = line_bg {
                style = style.bg(bg);
            }
            if l.emph.iter().any(|&(es, ee)| es <= s && e <= ee) {
                style = style.bg(emph_bg);
            }
            spans.push(Span::styled(seg.to_string(), style));
        }
        let line = Line::from(spans);
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

impl Mark {
    /// Whether `line` of `f` is among this mark's lines.
    fn covers(&self, f: &FileDiff, line: &crate::diff::DiffLine) -> bool {
        mark_no(f, line, self.old_side).is_some_and(|no| (self.start..=self.end).contains(&no))
    }
}

/// The number a mark is addressed by: the new side's, or the old side's
/// in a deleted file.
fn line_no(f: &FileDiff, line: &crate::diff::DiffLine) -> Option<usize> {
    match f.status {
        Status::Deleted => line.old_no,
        _ => line.new_no,
    }
}

/// The number a mark addresses `line` by: the old side's for a mark on
/// deleted lines, else as `line_no`.
fn mark_no(f: &FileDiff, line: &crate::diff::DiffLine, old_side: bool) -> Option<usize> {
    if old_side {
        line.old_no
    } else {
        line_no(f, line)
    }
}
