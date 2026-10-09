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
    /// How many rows below the top of the view the cursor sat.
    offset: usize,
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
                if matches!(self.rows.get(row + 1), Some(Row::Line(..))) {
                    self.cursor = row + 1;
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

    /// The mark above the cursor; the mark whose row or first line the
    /// cursor is on is where we are, not where we go.
    pub fn prev_mark(&mut self) {
        let marks = self.shown_marks();
        let here = marks
            .iter()
            .find(|(r, _)| *r == self.cursor || *r + 1 == self.cursor)
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
        self.mark_rows = vec![None; self.marks.len()];
        let mut placed = vec![false; self.marks.len()];
        let mut max_no = 1;
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
                        }
                    }
                    self.rows.push(Row::Line(fi, hi, li));
                }
            }
            // A stale mark past every line shown ends the file.
            for (m, mark) in self.marks.iter().enumerate() {
                if !placed[m] && mark.stale && mark.path == f.path {
                    placed[m] = true;
                    self.mark_rows[m] = Some(self.rows.len());
                    self.rows.push(Row::Mark(m));
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

    // ---- position --------------------------------------------------------

    pub fn anchor(&self) -> Option<Anchor> {
        let fi = self.current_file()?;
        let path = self.files[fi].path.clone();
        let mut at_mark = None;
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
            // A mark's row: its first line, and the row itself if it stays.
            Row::Mark(m) => {
                let mark = &self.marks[*m];
                at_mark = Some(mark.seq);
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
            offset: self.cursor - self.scroll,
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
        // Back onto the mark's row when we were on one and it is still
        // there, among the rows of the marks above the line.
        if let Some(seq) = a.at_mark {
            let mut r = self.cursor;
            while r > 0
                && let Some(Row::Mark(m)) = self.rows.get(r - 1)
            {
                r -= 1;
                if self.marks[*m].seq == seq {
                    self.cursor = r;
                    break;
                }
            }
        }
        self.scroll = self.cursor.saturating_sub(a.offset);
        self.fit();
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
        self.scroll = self.scroll.min(self.max_scroll());
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
    pub fn move_cursor(&mut self, delta: isize) {
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
        if row < self.rows.len() && !matches!(self.rows[row], Row::Blank) {
            self.cursor = row;
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

    pub fn render(&self, area: Rect, buf: &mut Buffer, p: &Palette, hl: &mut Highlighter) {
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
            self.render_row(idx, row, line, buf, p, hl);
            if idx == self.cursor {
                self.render_cursor(row, line, buf, p);
            }
            y += 1;
            idx += 1;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_row(
        &self,
        idx: usize,
        row: Row,
        area: Rect,
        buf: &mut Buffer,
        p: &Palette,
        hl: &mut Highlighter,
    ) {
        match row {
            Row::Mark(m) => {
                let mark = &self.marks[m];
                let (bg, fg) = p.mark_colors(mark.author);
                let tint = Style::default().bg(bg);
                buf.set_style(area, tint);
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
                    spans.push(Span::styled(note.clone(), tint.fg(fg)));
                }
                buf.set_line(area.x, area.y, &Line::from(spans), area.width);
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
