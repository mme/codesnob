//! The flat diff: files, hunks and lines as one scrollable list of rows,
//! with a sticky file header, line numbers, syntax and intra-line highlights.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::Palette;
use crate::diff::{self, FileDiff, Kind, Status};
use crate::git::FileChange;
use crate::highlight::Highlighter;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Row {
    /// A line of the description block above the files.
    Desc(usize),
    /// A highlight's number and note, above its lines.
    Mark(usize),
    /// A reply under a mark's row: the mark's index, the reply's index.
    Reply(usize, usize),
    File(usize),
    Hunk(usize, usize),
    Line(usize, usize, usize),
    Note(&'static str),
    Blank,
}

/// Who placed a mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    /// A highlight an agent placed.
    Agent,
    /// A comment the user wrote.
    User,
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
    pub note: Option<String>,
    /// Its lines no longer exist; drawn near where they were, untinted.
    pub stale: bool,
    /// Its lines are the old side's: deleted lines in a diff.
    pub old_side: bool,
    /// The letter of the mark this one answers.
    pub re: Option<String>,
    /// The answers under it, oldest first.
    pub replies: Vec<Reply>,
}

/// An answer under a comment, drawn on a row of its own below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The comment's letter and its number under it: A1, A2…
    pub label: String,
    pub author: Author,
    pub text: String,
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
    /// The letter of the mark this comment answers, when made on its row.
    pub re: Option<String>,
}

/// Where the cursor is, expressed in terms that survive a reload.
pub struct Anchor {
    path: String,
    /// (side, line number): side 0 is the old file, 1 the new file.
    line: Option<(u8, usize)>,
    /// The cursor was on a hunk header, not on a line.
    at_hunk: bool,
    /// The cursor was on a mark's own row: the mark, by its place in the
    /// order marks were made.
    at_mark: Option<usize>,
    /// The cursor was on a reply under that mark: the reply, by index.
    at_reply: Option<usize>,
    /// How many rows below the top of the view the cursor sat; above it
    /// when negative, scrolled away.
    offset: isize,
    /// How much of the top row was scrolled out of sight.
    hidden: usize,
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
    /// Highlights, drawn over their lines.
    marks: Vec<Mark>,
    /// The row of each highlight's note, by number; `None` when not shown.
    mark_rows: Vec<Option<usize>>,
    /// The highlight last jumped to, by index; `<` and `>` go on from here.
    current_mark: Option<usize>,
    /// Rows being marked for a comment, drawn as a would-be mark.
    pub marking: Option<(usize, usize)>,
    /// How tall rows are drawn, where they differ; without it every row
    /// is one unit, a terminal's line.
    pub measure: Option<std::rc::Rc<dyn Measure>>,
    /// How much of the top row is scrolled out of sight, in the units of
    /// `measure`; always 0 without it.
    top_hidden: usize,
    /// The longest line shown, in characters.
    widest: usize,
    /// The view was scrolled away from the cursor: it stays where it is, in
    /// sight or not, until it moves, and the view stays put meanwhile.
    away: bool,
}

/// How tall a front end draws rows, when they differ in height. Heights
/// and the body's height (`resize`) are in the same units.
pub trait Measure {
    /// The height of a line of code: what pages are counted in.
    fn line(&self) -> usize;
    /// The height of row `idx` of `view`.
    fn height(&self, view: &DiffView, idx: usize) -> usize;
}

impl DiffView {
    /// An empty view whose rows are as tall as `measure` says.
    pub fn new(measure: Option<std::rc::Rc<dyn Measure>>) -> Self {
        Self {
            measure,
            ..Self::default()
        }
    }

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
            // The same marks after a reload keep their place in order.
            if self.marks.len() != marks.len() {
                self.current_mark = None;
            }
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

    /// Jump to highlight `i` (by index): its note comes into view and the
    /// cursor lands on its first line.
    fn go_to_mark(&mut self, i: usize) -> bool {
        match self.mark_rows.get(i).copied().flatten() {
            Some(row) => {
                self.jump_to(row);
                let below = self.below_mark(row);
                if matches!(self.rows.get(below), Some(Row::Line(..))) {
                    self.cursor = below;
                }
                self.current_mark = Some(i);
                true
            }
            None => false,
        }
    }

    /// Open at the first mark shown.
    pub fn first_mark(&mut self) -> bool {
        (0..self.mark_rows.len()).any(|i| self.go_to_mark(i))
    }

    /// Jump to the highlight covering `line` of `path`, if one is shown.
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
    fn shown_marks(&self) -> Vec<(usize, usize)> {
        let mut rows: Vec<(usize, usize)> = self
            .mark_rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.map(|r| (r, i)))
            .collect();
        rows.sort_unstable();
        rows
    }

    /// The next mark below the cursor.
    pub fn next_mark(&mut self) {
        let next = self
            .shown_marks()
            .into_iter()
            .find(|(r, _)| *r > self.cursor);
        if let Some((_, i)) = next {
            self.go_to_mark(i);
        }
    }

    /// The mark above the cursor; the mark whose row, replies or first
    /// line the cursor is on is where we are, not where we go.
    pub fn prev_mark(&mut self) {
        let marks = self.shown_marks();
        let here = marks
            .iter()
            .find(|(r, _)| (*r..=self.below_mark(*r)).contains(&self.cursor))
            .map_or(self.cursor, |(r, _)| *r);
        if let Some((_, i)) = marks.into_iter().rev().find(|(r, _)| *r < here) {
            self.go_to_mark(i);
        }
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

    /// The first row below mark row `row` and the replies under it.
    fn below_mark(&self, row: usize) -> usize {
        let mut r = row + 1;
        while matches!(self.rows.get(r), Some(Row::Reply(..))) {
            r += 1;
        }
        r
    }

    /// The reply whose row the cursor is on: its comment, by its place in
    /// the order marks were made, and the reply, by index.
    pub fn reply_at_cursor(&self) -> Option<(usize, usize, &Reply)> {
        let Row::Reply(m, r) = *self.rows.get(self.cursor)? else {
            return None;
        };
        let mark = &self.marks[m];
        Some((mark.seq, r, &mark.replies[r]))
    }

    /// The user's comment under the cursor, by its place in the order marks
    /// were made: the newest covering the cursor's line, or the one whose
    /// row the cursor is on.
    pub fn comment_at_cursor(&self) -> Option<usize> {
        match self.rows.get(self.cursor)? {
            Row::Mark(m) => {
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

    /// The agent's highlight whose row the cursor is on, as a region to
    /// comment on: the same lines, answering that letter.
    pub fn highlight_at_cursor(&self) -> Option<Region> {
        let Row::Mark(m) = *self.rows.get(self.cursor)? else {
            return None;
        };
        let mark = &self.marks[m];
        if mark.author != Author::Agent {
            return None;
        }
        let f = self.files.iter().find(|f| f.path == mark.path)?;
        let old = mark.old_side || f.status == Status::Deleted;
        let lines = if old { &f.old_lines } else { &f.new_lines };
        Some(Region {
            path: mark.path.clone(),
            start: mark.start,
            end: mark.end,
            old_side: mark.old_side,
            with_deletions: false,
            text: lines
                .get(mark.start - 1..mark.end)
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
            re: Some(mark.label.clone()),
        })
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
            re: None,
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
            None => {
                self.set_scroll(0);
                self.top_hidden = 0;
            }
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
        self.mark_rows = vec![None; self.marks.len()];
        let mut placed = vec![false; self.marks.len()];
        let mut max_no = 1;
        let mut widest = 0;
        for i in 0..self.desc.len() {
            self.rows.push(Row::Desc(i));
        }
        for (fi, f) in self.files.iter().enumerate() {
            if !self.only.is_empty() && !self.only.contains(&fi) {
                continue;
            }
            if !self.rows.is_empty() {
                self.rows.push(Row::Blank);
            }
            self.file_starts.push(self.rows.len());
            self.rows.push(Row::File(fi));
            if f.binary {
                self.rows.push(Row::Note("binary file"));
            } else if f.hunks.is_empty() {
                self.rows.push(Row::Note("no textual changes"));
            }
            max_no = max_no.max(f.old_lines.len()).max(f.new_lines.len());
            // A plain file shown whole needs no hunk header.
            let whole = f.plain
                && f.hunks.len() == 1
                && f.hunks[0].new_start == 1
                && f.hunks[0].new_count == f.new_lines.len();
            for (hi, h) in f.hunks.iter().enumerate() {
                self.hunk_starts.push(self.rows.len());
                if !whole {
                    self.rows.push(Row::Hunk(fi, hi));
                }
                for li in 0..h.lines.len() {
                    // A mark's row goes above its first line shown; a stale
                    // mark's above the first line at or past where it was.
                    for (m, mark) in self.marks.iter().enumerate() {
                        let Some(no) = mark_no(f, &h.lines[li], mark.old_side) else {
                            continue;
                        };
                        let here = if mark.stale {
                            no >= mark.start
                        } else {
                            (mark.start..=mark.end).contains(&no)
                        };
                        if !placed[m] && mark.path == f.path && here {
                            placed[m] = true;
                            self.mark_rows[m] = Some(self.rows.len());
                            self.rows.push(Row::Mark(m));
                            self.rows
                                .extend((0..mark.replies.len()).map(|r| Row::Reply(m, r)));
                        }
                    }
                    self.rows.push(Row::Line(fi, hi, li));
                    widest = widest.max(h.lines[li].text.chars().count());
                }
            }
            // A stale mark past every line shown ends the file.
            for (m, mark) in self.marks.iter().enumerate() {
                if !placed[m] && mark.stale && mark.path == f.path {
                    placed[m] = true;
                    self.mark_rows[m] = Some(self.rows.len());
                    self.rows.push(Row::Mark(m));
                    self.rows
                        .extend((0..mark.replies.len()).map(|r| Row::Reply(m, r)));
                }
            }
        }
        if self.files.is_empty() && !self.desc.is_empty() {
            self.rows.push(Row::Blank);
            self.rows.push(Row::Note("no files changed"));
        }
        self.widest = widest;
        // With marks, room for their letters so they never cover a digit.
        self.gutter_w = max_no.to_string().len().max(3)
            + self.label_w().max(usize::from(self.marking.is_some()));
        self.fit();
    }

    // ---- position --------------------------------------------------------

    pub fn anchor(&self) -> Option<Anchor> {
        let fi = self.current_file()?;
        let path = self.files[fi].path.clone();
        let mut at_mark = None;
        let mut at_reply = None;
        let (line, at_hunk) = match self.rows.get(self.cursor)? {
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
            // A mark's row or a reply's: the mark's first line, and the row
            // itself if it stays.
            Row::Mark(m) | Row::Reply(m, _) => {
                let mark = &self.marks[*m];
                at_mark = Some(mark.seq);
                if let Row::Reply(_, r) = self.rows[self.cursor] {
                    at_reply = Some(r);
                }
                let old = mark.old_side || self.files[fi].status == Status::Deleted;
                (Some((if old { 0 } else { 1 }, mark.start)), false)
            }
            _ => (None, false),
        };
        Some(Anchor {
            path,
            line,
            at_hunk,
            at_mark,
            at_reply,
            offset: self.cursor as isize - self.scroll as isize,
            hidden: self.top_hidden,
        })
    }

    pub fn restore(&mut self, a: Anchor) {
        let Some(fi) = self.files.iter().position(|f| f.path == a.path) else {
            self.fit();
            return;
        };
        let Some(start) = self.file_row(fi) else {
            self.fit();
            return;
        };
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
        self.cursor = match best {
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
        // Back onto the mark's row, or the reply's under it, when we were on
        // one and it is still there, among the rows of the marks above the
        // line.
        if let Some(seq) = a.at_mark {
            let mut r = self.cursor;
            while r > 0
                && let Some(Row::Mark(m) | Row::Reply(m, _)) = self.rows.get(r - 1)
            {
                r -= 1;
                if matches!(self.rows[r], Row::Mark(_)) && self.marks[*m].seq == seq {
                    // A reply that went leaves the cursor on the one above.
                    let replies = self.marks[*m].replies.len();
                    self.cursor = match a.at_reply {
                        Some(i) if replies > 0 => r + 1 + i.min(replies - 1),
                        _ => r,
                    };
                    break;
                }
            }
        }
        self.set_scroll((self.cursor as isize - a.offset).max(0) as usize);
        self.top_hidden = if self.measure.is_some() { a.hidden } else { 0 };
        self.fit();
    }

    fn max_scroll(&self) -> usize {
        self.rows.len().saturating_sub(1)
    }

    /// How tall a line of code is drawn: the unit pages are counted in.
    fn line(&self) -> usize {
        self.measure.as_ref().map_or(1, |m| m.line().max(1))
    }

    /// How tall row `r` is drawn; past the last row, a line's height.
    fn height(&self, r: usize) -> usize {
        match &self.measure {
            Some(m) if r < self.rows.len() => m.height(self, r).max(1),
            _ => self.line(),
        }
    }

    /// How tall the sticky header is when the view starts at `scroll`:
    /// nothing when no file's header sticks there.
    fn sticky_at(&self, scroll: usize) -> usize {
        match self.file_at(scroll) {
            Some(fi) if !matches!(self.rows.get(scroll), Some(Row::File(_))) => {
                self.file_row(fi).map_or(self.line(), |r| self.height(r))
            }
            _ => 0,
        }
    }

    /// The room for rows below the sticky header, were the view to start
    /// at `scroll`; the part of the top row scrolled out of sight counts
    /// as room.
    fn room_at(&self, scroll: usize) -> usize {
        let hidden = if scroll == self.scroll {
            self.top_hidden
        } else {
            0
        };
        (self.body_h + hidden).saturating_sub(self.sticky_at(scroll))
    }

    /// Rows of the body that show content below the sticky header, were
    /// the view to start at `scroll`.
    fn visible_at(&self, scroll: usize) -> usize {
        self.rows_in(scroll, true, self.room_at(scroll)).max(1)
    }

    /// How many rows from `from` fit in `room`, going down or (from the
    /// row above `from`) up. Past either end rows count a line each.
    fn rows_in(&self, from: usize, down: bool, room: usize) -> usize {
        let mut used = 0;
        let mut n = 0;
        loop {
            let h = if down {
                self.height(from + n)
            } else {
                match from.checked_sub(n + 1) {
                    Some(r) => self.height(r),
                    None => self.line(),
                }
            };
            if used + h > room {
                return n;
            }
            used += h;
            n += 1;
        }
    }

    /// The first row of a view that ends with row `last` in `room`.
    fn top_for(&self, last: usize, room: usize) -> usize {
        last - self
            .rows_in(last + 1, false, room)
            .saturating_sub(1)
            .min(last)
    }

    fn set_scroll(&mut self, scroll: usize) {
        if scroll != self.scroll {
            self.top_hidden = 0;
        }
        self.scroll = scroll;
    }

    // ---- with a measure: rows by the pixel, the header floating -------------
    //
    // A windowed front end scrolls by the pixel, the top row partly out of
    // sight, and the file header that sticks floats over the rows at the
    // top instead of taking a line; the view stops where the last row ends
    // at the bottom.

    /// How much the floating header covers of the rows at the top, as the
    /// view stands.
    pub fn cover(&self) -> usize {
        match self.file_at(self.scroll) {
            Some(fi)
                if self.top_hidden > 0
                    || !matches!(self.rows.get(self.scroll), Some(Row::File(_))) =>
            {
                self.file_row(fi).map_or(self.line(), |r| self.height(r))
            }
            _ => 0,
        }
    }

    /// How much a floating header would cover with `row` near the top.
    fn cover_for(&self, row: usize) -> usize {
        match self.file_at(row) {
            Some(fi) if !matches!(self.rows.get(row), Some(Row::File(_))) => {
                self.file_row(fi).map_or(self.line(), |r| self.height(r))
            }
            _ => 0,
        }
    }

    /// Where row `row` starts, from the body's top; negative above it.
    /// Rows far below stop being counted past twice the body.
    pub fn y_of(&self, row: usize) -> isize {
        let hidden = self.top_hidden as isize;
        if row < self.scroll {
            let above: usize = (row..self.scroll).map(|r| self.height(r)).sum();
            return -(above as isize) - hidden;
        }
        let mut y = -hidden;
        for r in self.scroll..row {
            y += self.height(r) as isize;
            if y > 2 * self.body_h as isize {
                break;
            }
        }
        y
    }

    /// Start the view so that `row` begins `at` below the body's top.
    fn place_top(&mut self, row: usize, at: usize) {
        let mut s = row;
        let mut above = 0;
        while above < at && s > 0 {
            s -= 1;
            above += self.height(s);
        }
        self.scroll = s;
        self.top_hidden = above.saturating_sub(at);
    }

    /// Start the view so that `row` ends at the body's bottom.
    fn place_bottom(&mut self, row: usize) {
        let mut s = row;
        let mut used = self.height(row);
        while used < self.body_h && s > 0 {
            s -= 1;
            used += self.height(s);
        }
        self.scroll = s;
        self.top_hidden = used.saturating_sub(self.body_h);
    }

    /// Where the view starts when its last row ends at the bottom.
    fn end(&self) -> (usize, usize) {
        let Some(last) = self.rows.len().checked_sub(1) else {
            return (0, 0);
        };
        let mut s = last;
        let mut used = self.height(last);
        while used < self.body_h && s > 0 {
            s -= 1;
            used += self.height(s);
        }
        (s, used.saturating_sub(self.body_h))
    }

    /// Not past the end.
    fn clamp_end(&mut self) {
        if self.measure.is_none() {
            return;
        }
        let end = self.end();
        if (self.scroll, self.top_hidden) > end {
            (self.scroll, self.top_hidden) = end;
        }
    }

    /// The rows wholly in sight below the floating header, first and
    /// last; the first alone when none is.
    fn in_sight(&self) -> (usize, usize) {
        let cover = self.cover() as isize;
        let max = self.max_scroll();
        let mut y = -(self.top_hidden as isize);
        let mut r = self.scroll;
        while r < max && y < cover {
            y += self.height(r) as isize;
            r += 1;
        }
        let lo = r.min(max);
        let mut hi = lo;
        let mut bottom = y + self.height(lo) as isize;
        while hi < max && bottom + self.height(hi + 1) as isize <= self.body_h as isize {
            hi += 1;
            bottom += self.height(hi) as isize;
        }
        (lo, hi)
    }

    /// Keep the cursor's row in sight: below the floating header and above
    /// the bottom; a row taller than the room shows its top.
    fn follow_overlay(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
        if self.away {
            self.away = !self.cursor_shown();
            self.clamp_end();
            return;
        }
        let c = self.cursor;
        let y = self.y_of(c);
        let h = self.height(c) as isize;
        if y < self.cover() as isize {
            self.place_top(c, self.cover_for(c));
        } else if y + h > self.body_h as isize {
            if self.height(c) + self.cover_for(c) > self.body_h {
                self.place_top(c, self.cover_for(c));
            } else {
                self.place_bottom(c);
            }
        }
        self.clamp_end();
    }

    /// Bring the cursor onto a row it may rest on, stepping in `dir`
    /// (then the other way) past blank rows.
    fn settle_cursor(&mut self, dir: isize) {
        let dir = if dir == 0 { 1 } else { dir };
        self.cursor = self.cursor.min(self.max_scroll());
        let rests = |v: &Self, r: usize| !matches!(v.rows.get(r), Some(Row::Blank));
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
        if self.measure.is_some() {
            self.follow_overlay();
            return;
        }
        if self.cursor < self.scroll {
            self.set_scroll(self.cursor);
        }
        // Scrolling can bring in the sticky header, which takes room, so
        // the check runs again at the new position.
        for _ in 0..2 {
            let visible = self.visible_at(self.scroll);
            if self.cursor + 1 > self.scroll + visible {
                let top = self.top_for(self.cursor, self.room_at(self.scroll));
                self.set_scroll(top);
            }
        }
        self.set_scroll(self.scroll.min(self.max_scroll()));
    }

    /// Keep scroll and cursor within the rows, and the cursor in sight.
    pub fn fit(&mut self) {
        self.set_scroll(self.scroll.min(self.max_scroll()));
        self.settle_cursor(1);
        self.follow();
    }

    /// Tell the view how tall the body is, which decides what is in sight.
    pub fn resize(&mut self, body_h: usize) {
        let body_h = body_h.max(1);
        // Scrolled by the pixel, the view is where the wheel put it, cursor
        // in sight or not, until the size changes.
        if self.measure.is_some() && body_h == self.body_h {
            return;
        }
        self.body_h = body_h;
        self.fit();
    }

    /// How many lines of code the body holds.
    pub fn lines(&self) -> usize {
        self.body_h / self.line()
    }

    /// First row index at which the last row lands on the bottom line,
    /// accounting for the sticky header that is drawn above the body.
    pub fn end_scroll(&self, body_h: usize) -> usize {
        let n = self.rows.len();
        let mut s = n - self.rows_in(n, false, body_h).min(n);
        if s > 0 && !matches!(self.rows.get(s), Some(Row::File(_))) {
            // The sticky header takes room from the top rows.
            let sticky = self
                .file_at(s)
                .and_then(|fi| self.file_row(fi))
                .map_or(self.line(), |r| self.height(r));
            let mut used: usize = (s..n).map(|r| self.height(r)).sum();
            loop {
                used -= self.height(s);
                s += 1;
                if s >= n || used + sticky <= body_h {
                    break;
                }
            }
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
    pub fn move_cursor(&mut self, delta: isize) {
        // Scrolled away from, the cursor's first move brings it back to the
        // edge it left by.
        if self.away && !self.cursor_shown() {
            self.cursor_back();
            return;
        }
        self.away = false;
        let max = self.max_scroll() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        self.settle_cursor(delta.signum());
        self.follow();
    }

    /// Move the view by `delta` rows, the cursor keeping its place on the
    /// screen where it can.
    pub fn scroll_by(&mut self, delta: isize) {
        let end = self.max_scroll() as isize;
        let before = self.scroll;
        self.set_scroll((self.scroll as isize + delta).clamp(0, end) as usize);
        let moved = self.scroll as isize - before as isize;
        let max = self.max_scroll() as isize;
        self.cursor = (self.cursor as isize + moved).clamp(0, max) as usize;
        self.pull_cursor(delta.signum());
    }

    /// A page down or up: the rows the body holds, less two lines; or, with
    /// `half`, the rows in half of it.
    pub fn page(&mut self, down: bool, half: bool) {
        let room = if half {
            self.body_h / 2
        } else {
            self.body_h.saturating_sub(2 * self.line())
        };
        if self.measure.is_some() {
            let room = room.max(self.line()) as isize;
            if self.away {
                self.cursor_back();
            }
            let at = self.y_of(self.cursor);
            self.glide(if down { room } else { -room });
            // The cursor keeps its place on the screen.
            self.cursor = self.row_at(at);
            self.settle_cursor(if down { 1 } else { -1 });
            self.follow();
            return;
        }
        let rows = self.rows_in(self.scroll, down, room).max(1) as isize;
        self.scroll_by(if down { rows } else { -rows });
    }

    /// Move the view by `delta` units, smoothly: the top row may end up
    /// partly scrolled out of sight. The cursor stays on its row, in sight
    /// or not; `cursor_back` brings it back once the scrolling stops.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn glide(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let mut hidden = self.top_hidden as isize + delta;
        while hidden < 0 {
            if self.scroll == 0 {
                hidden = 0;
                break;
            }
            self.scroll -= 1;
            hidden += self.height(self.scroll) as isize;
        }
        while hidden > 0 && hidden >= self.height(self.scroll) as isize {
            if self.scroll >= self.max_scroll() {
                hidden = 0;
                break;
            }
            hidden -= self.height(self.scroll) as isize;
            self.scroll += 1;
        }
        self.top_hidden = hidden as usize;
        self.clamp_end();
        self.away = !self.cursor_shown();
    }

    /// Whether the cursor's row is wholly in sight, below the floating
    /// header and above the bottom.
    fn cursor_shown(&self) -> bool {
        if self.cursor < self.scroll {
            return false;
        }
        let y = self.y_of(self.cursor);
        y >= self.cover() as isize && y + self.height(self.cursor) as isize <= self.body_h as isize
    }

    /// A cursor scrolled out of sight comes back by the edge it left by,
    /// onto the last row in sight when it went off the bottom and onto the
    /// first below the floating header when off the top. The view stays
    /// where it is.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn cursor_back(&mut self) {
        self.away = false;
        let above = self.cursor < self.scroll || self.y_of(self.cursor) < self.cover() as isize;
        let below = !above
            && self.y_of(self.cursor) + self.height(self.cursor) as isize > self.body_h as isize;
        if !above && !below {
            return;
        }
        let (lo, hi) = self.in_sight();
        let rests = |r: &usize| !matches!(self.rows.get(*r), Some(Row::Blank));
        self.cursor = if above {
            (lo..=hi).find(rests).unwrap_or(lo)
        } else {
            (lo..=hi).rev().find(rests).unwrap_or(hi)
        };
    }

    /// The row drawn at `y` below the body's top, kept within the rows in
    /// sight.
    fn row_at(&self, y: isize) -> usize {
        let (lo, hi) = self.in_sight();
        let mut top = self.y_of(lo);
        for r in lo..=hi {
            let h = self.height(r) as isize;
            if y < top + h {
                return r;
            }
            top += h;
        }
        hi
    }

    /// How much of the top row is scrolled out of sight.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn top_hidden(&self) -> usize {
        self.top_hidden
    }

    /// Bring the cursor into the rows in sight without scrolling.
    fn pull_cursor(&mut self, dir: isize) {
        if self.measure.is_some() {
            // Onto a row it may rest on among those in sight.
            let (lo, hi) = self.in_sight();
            let at = self.cursor.clamp(lo, hi);
            let rests = |r: &usize| !matches!(self.rows.get(*r), Some(Row::Blank));
            let down = (at..=hi).find(rests);
            let up = (lo..=at).rev().find(rests);
            self.cursor = match (dir < 0, down, up) {
                (false, Some(r), _) | (true, _, Some(r)) => r,
                (_, Some(r), None) | (_, None, Some(r)) => r,
                _ => at,
            };
            // With none in sight it rests just outside; the view stays
            // where it was scrolled to, as a text view's does.
            self.settle_cursor(dir);
            return;
        }
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
        self.away = false;
        let row = row.min(self.max_scroll());
        if self.measure.is_some() {
            self.cursor = row;
            self.settle_cursor(1);
            self.place_top(self.cursor, self.cover_for(self.cursor));
            self.follow();
            return;
        }
        self.set_scroll(row);
        self.top_hidden = 0;
        self.cursor = row;
        self.settle_cursor(1);
        self.follow();
    }

    /// Put the cursor on `row`, about a third of the way down the view.
    pub fn center_on(&mut self, row: usize) {
        self.away = false;
        let row = row.min(self.max_scroll());
        if self.measure.is_some() {
            self.cursor = row;
            self.settle_cursor(1);
            let at = (self.body_h / 3).max(self.cover_for(self.cursor));
            self.place_top(self.cursor, at);
            self.follow();
            return;
        }
        let above = self.rows_in(row, false, self.body_h / 3).min(row);
        self.set_scroll(row - above);
        self.top_hidden = 0;
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
        self.click_row(self.scroll + y);
    }

    /// The cursor on `row`, when it shows content.
    pub fn click_row(&mut self, row: usize) {
        if row < self.rows.len() && !matches!(self.rows[row], Row::Blank) {
            self.away = false;
            self.cursor = row;
            self.follow();
        }
    }

    pub fn go_top(&mut self) {
        self.away = false;
        self.set_scroll(0);
        self.top_hidden = 0;
        self.cursor = 0;
        self.fit();
    }

    pub fn go_bottom(&mut self) {
        self.away = false;
        if self.measure.is_some() {
            (self.scroll, self.top_hidden) = self.end();
            self.cursor = self.max_scroll();
            self.settle_cursor(-1);
            self.follow();
            return;
        }
        self.set_scroll(self.end_scroll(self.body_h));
        self.top_hidden = 0;
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

    pub fn render(&self, area: Rect, buf: &mut Buffer, p: &Palette, hl: &mut Highlighter) {
        let mut y = area.y;
        let mut idx = self.scroll;
        if let Some(header) = self.sticky(p) {
            header.paint(Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }
        while y < area.bottom() {
            let Some(view) = self.row_view(idx, p, hl) else {
                break;
            };
            let line = Rect::new(area.x, y, area.width, 1);
            view.paint(line, buf);
            if idx == self.cursor {
                render_cursor(&view, line, buf, p);
            }
            y += 1;
            idx += 1;
        }
    }

    /// The header of the file at the top of the view, held there while its
    /// rows scroll under it; `None` when the real header is the top row.
    pub fn sticky(&self, p: &Palette) -> Option<RowView> {
        let fi = self.top_file()?;
        if matches!(self.rows.get(self.scroll), Some(Row::File(_))) {
            return None;
        }
        Some(self.file_header(fi, p))
    }

    /// The text of the code line on row `idx`, whole, however far the view
    /// is scrolled sideways; `None` on rows that are not lines of code.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn line_text(&self, idx: usize) -> Option<&str> {
        let Row::Line(fi, hi, li) = *self.rows.get(idx)? else {
            return None;
        };
        Some(&self.files[fi].hunks[hi].lines[li].text)
    }

    /// The longest line shown, in characters.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn widest(&self) -> usize {
        self.widest
    }

    /// How many rows there are.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The file whose header sticks at the top of the view, when its own
    /// header has scrolled away.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn sticky_file(&self) -> Option<usize> {
        let fi = self.top_file()?;
        (self.top_hidden > 0 || !matches!(self.rows.get(self.scroll), Some(Row::File(_))))
            .then_some(fi)
    }

    /// What row `idx` is.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn row(&self, idx: usize) -> Option<Row> {
        self.rows.get(idx).copied()
    }

    /// The row of the mark made `seq`th, when it is shown.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn mark_row(&self, seq: usize) -> Option<usize> {
        let i = self.marks.iter().position(|m| m.seq == seq)?;
        self.mark_rows.get(i).copied().flatten()
    }

    /// The row showing line `line` of `path`, counted on the old side when
    /// `old_side`, as marks count them.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn line_row(&self, path: &str, line: usize, old_side: bool) -> Option<usize> {
        self.rows.iter().position(|r| match *r {
            Row::Line(fi, hi, li) => {
                let f = &self.files[fi];
                f.path == path && mark_no(f, &f.hunks[hi].lines[li], old_side) == Some(line)
            }
            _ => false,
        })
    }

    /// The marks, in the order `<` and `>` step through them.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn marks(&self) -> &[Mark] {
        &self.marks
    }

    /// The description block's lines.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn desc(&self) -> &[Line<'static>] {
        &self.desc
    }

    /// How many digits the longest line number has, at least three.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn number_width(&self) -> usize {
        self.files
            .iter()
            .map(|f| f.old_lines.len().max(f.new_lines.len()))
            .max()
            .unwrap_or(1)
            .to_string()
            .len()
            .max(3)
    }

    /// The mark drawn over line `li` of hunk `hi` of file `fi`, if any,
    /// and whether this is its first line.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub fn mark_over(&self, fi: usize, hi: usize, li: usize) -> Option<(&Mark, bool)> {
        let line = &self.files[fi].hunks[hi].lines[li];
        self.mark_on(fi, line).map(|(m, no)| (m, no == m.start))
    }

    /// Row `idx` as drawn; `None` past the last row.
    pub fn row_view(&self, idx: usize, p: &Palette, hl: &mut Highlighter) -> Option<RowView> {
        let row = *self.rows.get(idx)?;
        let plain = |spans: Vec<Span<'static>>| RowView {
            spans,
            ..RowView::default()
        };
        Some(match row {
            Row::Mark(m) => {
                let mark = &self.marks[m];
                let (bg, fg) = p.mark_colors(mark.author);
                let tint = Style::default().bg(bg);
                let mut spans = vec![Span::styled(
                    format!("  [{}] ", mark.label),
                    tint.fg(fg).add_modifier(Modifier::BOLD),
                )];
                if mark.stale {
                    spans.push(Span::styled("stale · ", tint.fg(p.note_fg)));
                }
                if mark.author == Author::User {
                    let who = match &mark.re {
                        Some(re) => format!("you re [{re}]: "),
                        None => "you: ".to_string(),
                    };
                    spans.push(Span::styled(who, tint.fg(fg).add_modifier(Modifier::BOLD)));
                }
                if let Some(note) = &mark.note {
                    spans.push(Span::styled(one_line(note), tint.fg(fg)));
                }
                RowView {
                    fill: tint,
                    ..plain(spans)
                }
            }
            Row::Reply(m, r) => {
                let reply = &self.marks[m].replies[r];
                let (bg, fg) = p.mark_colors(reply.author);
                let tint = Style::default().bg(bg);
                let who = match reply.author {
                    Author::User => "you: ",
                    Author::Agent => "",
                };
                let spans = vec![
                    Span::styled(
                        format!("      [{}] {who}", reply.label),
                        tint.fg(fg).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(one_line(&reply.text), tint.fg(fg)),
                ];
                RowView {
                    fill: tint,
                    ..plain(spans)
                }
            }
            Row::Desc(i) => {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(self.desc[i].spans.iter().cloned());
                plain(spans)
            }
            Row::Blank => RowView::default(),
            Row::File(fi) => self.file_header(fi, p),
            Row::Note(text) => plain(vec![Span::styled(
                format!("  {text}"),
                Style::default().fg(p.note_fg),
            )]),
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
                plain(vec![Span::styled(text, Style::default().fg(p.hunk_fg))])
            }
            Row::Line(fi, hi, li) => self.line_view(idx, fi, hi, li, p, hl),
        })
    }

    /// The code of line `li` of hunk `hi` of file `fi` in pieces: the text,
    /// its syntax style, and whether it is emphasized as what changed; the
    /// first `skip` characters left out.
    pub fn code_pieces(
        &self,
        fi: usize,
        hi: usize,
        li: usize,
        hl: &mut Highlighter,
        skip: usize,
    ) -> Vec<(String, Style, bool)> {
        let f = &self.files[fi];
        let l = &f.hunks[hi].lines[li];
        // Syntax spans for the line's source file side.
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
        let mut skip = skip;
        let mut out = Vec::new();
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
            let style = syntax
                .iter()
                .find(|&&(_, ss, se)| ss <= s && e <= se)
                .map(|&(st, _, _)| st)
                .unwrap_or_default();
            let emph = l.emph.iter().any(|&(es, ee)| es <= s && e <= ee);
            out.push((seg.to_string(), style, emph));
        }
        out
    }

    fn file_header(&self, fi: usize, p: &Palette) -> RowView {
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
        RowView {
            fill: base,
            spans,
            right: Some(pos),
            ..RowView::default()
        }
    }

    fn line_view(
        &self,
        idx: usize,
        fi: usize,
        hi: usize,
        li: usize,
        p: &Palette,
        hl: &mut Highlighter,
    ) -> RowView {
        let l = &self.files[fi].hunks[hi].lines[li];
        let (line_bg, emph_bg, marker) = match l.kind {
            Kind::Add => (Some(p.add_bg), p.add_emph, "+"),
            Kind::Del => (Some(p.del_bg), p.del_emph, "-"),
            Kind::Context => (None, Color::Reset, " "),
        };
        // Highlighted context lines take the highlight's tint; changed lines
        // keep their color and get the bar in the gutter only.
        let marked = self.mark_on(fi, l);
        let marking = self
            .marking
            .is_some_and(|(lo, hi)| (lo..=hi).contains(&idx));
        let line_bg = match (line_bg, marked, marking) {
            (None, _, true) => Some(p.comment_bg),
            (None, Some((m, _)), _) => Some(p.mark_colors(m.author).0),
            (bg, ..) => bg,
        };
        let row_style = match line_bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };

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
        let gutter_cells = gutter.chars().count();
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
        // The mark column: its letter on the mark's first line, a bar below;
        // a bar too on lines being marked.
        let gutter = match marked {
            _ if marking => {
                let w = self.label_w().max(1);
                spans.push(Span::styled(
                    format!("{:<w$}", "▌"),
                    row_style.fg(p.comment_fg).add_modifier(Modifier::BOLD),
                ));
                gutter.chars().skip(w).collect()
            }
            Some((m, no)) => {
                let w = self.label_w();
                let fg = p.mark_colors(m.author).1;
                let column = if no == m.start {
                    format!("{:<w$}", m.label)
                } else {
                    format!("{:<w$}", "▌")
                };
                spans.push(Span::styled(
                    column,
                    row_style.fg(fg).add_modifier(Modifier::BOLD),
                ));
                gutter.chars().skip(w).collect()
            }
            None => gutter,
        };
        spans.push(Span::styled(gutter, row_style.fg(p.gutter_fg)));
        spans.push(Span::styled(
            format!("{marker} "),
            marker_style.add_modifier(Modifier::BOLD),
        ));

        for (seg, style, emph) in self.code_pieces(fi, hi, li, hl, self.hscroll) {
            let mut style = style;
            if let Some(bg) = line_bg {
                style = style.bg(bg);
            }
            if emph {
                style = style.bg(emph_bg);
            }
            spans.push(Span::styled(seg, style));
        }
        RowView {
            fill: row_style,
            spans,
            right: None,
            gutter: gutter_cells,
        }
    }
}

/// A row as drawn, for either front end to paint: what fills the row, the
/// spans from its left edge, and a span flush right where it fits.
#[derive(Default)]
pub struct RowView {
    pub fill: Style,
    pub spans: Vec<Span<'static>>,
    pub right: Option<Span<'static>>,
    /// Cells at the left holding line numbers, which the cursor's row
    /// shows in bold.
    pub gutter: usize,
}

impl RowView {
    /// Paint into one line of a terminal buffer.
    pub fn paint(&self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, self.fill);
        let used = buf
            .set_line(area.x, area.y, &Line::from(self.spans.clone()), area.width)
            .0;
        if let Some(right) = &self.right {
            let rw = right.width() as u16;
            if area.right() >= used + rw {
                buf.set_span(area.right() - rw, area.y, right, rw);
            }
        }
    }
}

/// Restyle the cursor's row once drawn: its background shifted, its line
/// numbers bold.
fn render_cursor(view: &RowView, area: Rect, buf: &mut Buffer, p: &Palette) {
    let gutter = view.gutter as u16;
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

impl Mark {
    /// Whether `line` of `f` is among this mark's lines.
    fn covers(&self, f: &FileDiff, line: &crate::diff::DiffLine) -> bool {
        mark_no(f, line, self.old_side).is_some_and(|no| (self.start..=self.end).contains(&no))
    }
}

/// Text with line breaks, on one line: each break shown as ↵.
fn one_line(text: &str) -> String {
    text.split('\n').collect::<Vec<_>>().join(" ↵ ")
}

/// The number a highlight is addressed by: the new side's, or the old side's
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

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;

    /// Lines 10 tall, a file's header 30, and the rows named 50.
    struct Tall(Vec<usize>);

    impl Measure for Tall {
        fn line(&self) -> usize {
            10
        }

        fn height(&self, view: &DiffView, idx: usize) -> usize {
            match view.row(idx) {
                Some(Row::File(_)) => 30,
                _ if self.0.contains(&idx) => 50,
                _ => 10,
            }
        }
    }

    /// A file of `lines` lines shown whole: its header is row 0, line n
    /// row n.
    fn view(lines: usize, tall: Vec<usize>, body: usize) -> DiffView {
        let text: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
        let mut v = DiffView::new(Some(Rc::new(Tall(tall))));
        v.set_built(vec![crate::diff::plain(
            "a.txt".into(),
            text.as_bytes(),
            &[],
        )]);
        v.resize(body);
        v
    }

    /// Two files of `lines` lines each, shown whole.
    fn two(lines: usize, body: usize) -> DiffView {
        let text: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
        let mut v = DiffView::new(Some(Rc::new(Tall(Vec::new()))));
        v.set_built(vec![
            crate::diff::plain("a.txt".into(), text.as_bytes(), &[]),
            crate::diff::plain("b.txt".into(), text.as_bytes(), &[]),
        ]);
        v.resize(body);
        v
    }

    /// Whether the cursor's row is wholly in sight, below the header
    /// floating over the top.
    fn cursor_in_sight(v: &DiffView) -> bool {
        let y = v.y_of(v.cursor());
        y >= v.cover() as isize && y + v.height(v.cursor()) as isize <= v.body_h as isize
    }

    #[test]
    fn the_cursor_stays_in_sight_past_tall_rows() {
        let mut v = view(20, vec![5], 100);
        v.move_cursor(6);
        assert_eq!(v.cursor(), 6);
        // Row 6 ends at the bottom: rows 1 to 6 (100) fill the body, the
        // header floating over the first of them.
        assert_eq!((v.scroll, v.top_hidden()), (1, 0));
        assert!(cursor_in_sight(&v));
        v.move_cursor(-5);
        assert!(cursor_in_sight(&v));
    }

    #[test]
    fn gliding_scrolls_part_of_a_row() {
        let mut v = view(20, vec![], 100);
        v.glide(15);
        assert_eq!((v.scroll, v.top_hidden()), (0, 15));
        v.glide(20);
        assert_eq!((v.scroll, v.top_hidden()), (1, 5));
        v.glide(-10);
        assert_eq!((v.scroll, v.top_hidden()), (0, 25));
        v.glide(0);
        assert_eq!((v.scroll, v.top_hidden()), (0, 25));
        // The cursor stayed where it was.
        assert_eq!(v.cursor(), 0);
        v.move_cursor(15);
        assert!(cursor_in_sight(&v));
    }

    #[test]
    fn a_cursor_scrolled_away_from_stays_until_it_moves() {
        let mut v = view(50, vec![], 100);
        v.move_cursor(3);
        v.glide(200);
        // Left behind, and the view stays where it was scrolled, through a
        // resize and a reload too.
        assert_eq!(v.cursor(), 3);
        let at = (v.scroll, v.top_hidden());
        v.resize(120);
        v.resize(100);
        assert_eq!((v.scroll, v.top_hidden()), at);
        assert_eq!(v.cursor(), 3);
        let a = v.anchor().unwrap();
        v.restore(a);
        assert_eq!((v.scroll, v.top_hidden()), at);
        // The first move, down or up, brings it back to the top edge.
        v.move_cursor(1);
        assert!(cursor_in_sight(&v));
        assert_eq!(v.y_of(v.cursor()), 30);
        let back = v.cursor();
        v.move_cursor(1);
        assert_eq!(v.cursor(), back + 1);
        // Off the bottom: back at the bottom.
        v.glide(-300);
        v.move_cursor(-1);
        assert!(cursor_in_sight(&v));
        assert_eq!(v.y_of(v.cursor()) + 10, 100);
    }

    #[test]
    fn the_cursor_comes_back_by_the_edge_it_left_by() {
        let mut v = view(50, vec![], 100);
        v.move_cursor(3);
        // Scrolled off the top: back at the top, below the header.
        v.glide(200);
        assert_eq!(v.cursor(), 3);
        v.cursor_back();
        assert!(cursor_in_sight(&v));
        assert_eq!(v.y_of(v.cursor()), 30);
        let (scroll, hidden) = (v.scroll, v.top_hidden());
        // Off the bottom: back at the bottom; the view never moves.
        v.glide(-150);
        let (scroll2, hidden2) = (v.scroll, v.top_hidden());
        v.cursor_back();
        assert!(cursor_in_sight(&v));
        assert_eq!(v.y_of(v.cursor()) + 10, 100);
        assert_eq!((v.scroll, v.top_hidden()), (scroll2, hidden2));
        assert_ne!((scroll, hidden), (scroll2, hidden2));
        // In sight already: it stays.
        let at = v.cursor();
        v.cursor_back();
        assert_eq!(v.cursor(), at);
    }

    #[test]
    fn a_page_keeps_the_cursor_on_its_place_on_the_screen() {
        let mut v = view(80, vec![], 100);
        v.move_cursor(5);
        let y = v.y_of(v.cursor());
        v.page(true, false);
        assert_eq!(v.y_of(v.cursor()), y);
    }

    #[test]
    fn rows_move_with_the_view_across_a_file_header() {
        let mut v = two(8, 60);
        // The second file's first line.
        let row = v.line_row("b.txt", 1, false).unwrap();
        let mut y = v.y_of(row);
        for _ in 0..80 {
            v.glide(1);
            let now = v.y_of(row);
            if now == y {
                break;
            }
            assert_eq!(
                now,
                y - 1,
                "at scroll {} hidden {}",
                v.scroll,
                v.top_hidden()
            );
            y = now;
        }
        for _ in 0..80 {
            v.glide(-1);
            let now = v.y_of(row);
            assert_eq!(
                now,
                y + 1,
                "at scroll {} hidden {}",
                v.scroll,
                v.top_hidden()
            );
            y = now;
            if (v.scroll, v.top_hidden()) == (0, 0) {
                break;
            }
        }
    }

    #[test]
    fn the_view_stops_where_the_last_row_ends() {
        let mut v = view(20, vec![], 100);
        v.glide(10_000);
        // The header (30) and twenty lines (200): 130 past the top.
        assert_eq!((v.scroll, v.top_hidden()), (11, 0));
        assert_eq!(v.y_of(20) + 10, 100);
        v.cursor_back();
        assert!(cursor_in_sight(&v));
    }

    #[test]
    fn a_page_is_the_body_less_two_lines() {
        let mut v = view(50, vec![], 100);
        v.page(true, false);
        // 80 down: the header (30) and five lines.
        assert_eq!((v.scroll, v.top_hidden()), (6, 0));
        v.page(false, false);
        assert_eq!((v.scroll, v.top_hidden()), (0, 0));
    }

    #[test]
    fn the_bottom_ends_with_the_last_row() {
        let mut v = view(20, vec![], 100);
        v.go_bottom();
        assert_eq!(v.cursor(), 20);
        assert_eq!((v.scroll, v.top_hidden()), (11, 0));
        assert!(cursor_in_sight(&v));
    }

    #[test]
    fn centering_counts_heights_above() {
        let mut v = view(50, vec![], 90);
        v.center_on(30);
        assert_eq!((v.scroll, v.cursor()), (27, 30));
    }

    #[test]
    fn jumping_puts_the_row_below_the_header() {
        let mut v = view(50, vec![], 100);
        v.jump_to(20);
        assert_eq!(v.cursor(), 20);
        assert_eq!(v.y_of(20), 30);
        assert!(cursor_in_sight(&v));
    }
}
