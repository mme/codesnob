//! Turn a pair of file contents into hunks with line numbers and
//! intra-line change ranges.

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};

use crate::git::FileChange;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Context,
    Add,
    Del,
}

pub struct DiffLine {
    pub kind: Kind,
    pub old_no: Option<usize>,
    pub new_no: Option<usize>,
    /// Index into the file's `old_lines` or `new_lines` (new for Add/Context).
    pub text: String,
    /// Byte ranges within `text` that differ from the paired line.
    pub emph: Vec<(usize, usize)>,
}

pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub func: Option<String>,
    pub lines: Vec<DiffLine>,
}

pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub status: Status,
    pub binary: bool,
    pub added: usize,
    pub removed: usize,
    pub hunks: Vec<Hunk>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// A file shown as it is, not a change: every line is context.
    pub plain: bool,
}

impl FileDiff {
    /// The path used for syntax detection and display.
    pub fn display_path(&self) -> String {
        match (&self.old_path, self.status) {
            (Some(old), Status::Renamed) => format!("{old} → {}", self.path),
            _ => self.path.clone(),
        }
    }
}

pub fn build(change: FileChange, context: usize) -> FileDiff {
    let status = match (&change.old_path, &change.new_path) {
        (Some(a), Some(b)) if a != b => Status::Renamed,
        (Some(_), Some(_)) => Status::Modified,
        (None, Some(_)) => Status::Added,
        _ => Status::Deleted,
    };
    let path = change
        .new_path
        .clone()
        .or(change.old_path.clone())
        .unwrap_or_default();
    let old_path = if status == Status::Renamed {
        change.old_path.clone()
    } else {
        None
    };

    let binary = is_binary(change.old.as_deref()) || is_binary(change.new.as_deref());
    if binary {
        return FileDiff {
            path,
            old_path,
            status,
            binary,
            added: 0,
            removed: 0,
            hunks: Vec::new(),
            old_lines: Vec::new(),
            new_lines: Vec::new(),
            plain: false,
        };
    }

    let old_text = expand_tabs(&String::from_utf8_lossy(
        change.old.as_deref().unwrap_or(b""),
    ));
    let new_text = expand_tabs(&String::from_utf8_lossy(
        change.new.as_deref().unwrap_or(b""),
    ));
    let old_lines: Vec<String> = split_lines(&old_text)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let new_lines: Vec<String> = split_lines(&new_text)
        .into_iter()
        .map(str::to_owned)
        .collect();

    let input = InternedInput::new(old_text.as_str(), new_text.as_str());
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    // Group raw change blocks into hunks with shared context.
    let raw: Vec<(usize, usize, usize, usize)> = diff
        .hunks()
        .map(|h| {
            (
                h.before.start as usize,
                h.before.end as usize,
                h.after.start as usize,
                h.after.end as usize,
            )
        })
        .collect();

    let mut groups: Vec<Vec<(usize, usize, usize, usize)>> = Vec::new();
    for h in raw {
        match groups.last_mut() {
            Some(g) if h.0 <= g.last().unwrap().1 + 2 * context => g.push(h),
            _ => groups.push(vec![h]),
        }
    }

    let mut hunks = Vec::new();
    let (mut added, mut removed) = (0, 0);
    for g in groups {
        let first = g[0];
        let last = *g.last().unwrap();
        let old_from = first.0.saturating_sub(context);
        let old_to = (last.1 + context).min(old_lines.len());
        let new_from = first.2.saturating_sub(context);
        let new_to = (last.3 + context).min(new_lines.len());

        let mut lines = Vec::new();
        let (mut o, mut n) = (old_from, new_from);
        for (os, oe, ns, ne) in g {
            while o < os {
                lines.push(DiffLine {
                    kind: Kind::Context,
                    old_no: Some(o + 1),
                    new_no: Some(n + 1),
                    text: new_lines[n].clone(),
                    emph: Vec::new(),
                });
                o += 1;
                n += 1;
            }
            let del_start = lines.len();
            for (i, text) in old_lines.iter().enumerate().take(oe).skip(os) {
                lines.push(DiffLine {
                    kind: Kind::Del,
                    old_no: Some(i + 1),
                    new_no: None,
                    text: text.clone(),
                    emph: Vec::new(),
                });
            }
            let add_start = lines.len();
            for (i, text) in new_lines.iter().enumerate().take(ne).skip(ns) {
                lines.push(DiffLine {
                    kind: Kind::Add,
                    old_no: None,
                    new_no: Some(i + 1),
                    text: text.clone(),
                    emph: Vec::new(),
                });
            }
            removed += oe - os;
            added += ne - ns;
            let end = lines.len();
            emphasize(&mut lines, del_start, add_start, end);
            o = oe;
            n = ne;
        }
        while o < old_to && n < new_to {
            lines.push(DiffLine {
                kind: Kind::Context,
                old_no: Some(o + 1),
                new_no: Some(n + 1),
                text: new_lines[n].clone(),
                emph: Vec::new(),
            });
            o += 1;
            n += 1;
        }

        let func = match status {
            Status::Added => find_func(&new_lines, new_from),
            _ => find_func(&old_lines, old_from),
        };
        let (old_count, new_count) = (old_to - old_from, new_to - new_from);
        hunks.push(Hunk {
            old_start: if old_count == 0 { 0 } else { old_from + 1 },
            old_count,
            new_start: if new_count == 0 { 0 } else { new_from + 1 },
            new_count,
            func,
            lines,
        });
    }

    FileDiff {
        path,
        old_path,
        status,
        binary,
        added,
        removed,
        hunks,
        old_lines,
        new_lines,
        plain: false,
    }
}

/// A file shown as it is. `windows` are 1-based inclusive line ranges to
/// show as excerpts; empty shows the whole file.
pub fn plain(path: String, content: &[u8], windows: &[(usize, usize)]) -> FileDiff {
    let binary = is_binary(Some(content));
    let text = if binary {
        String::new()
    } else {
        expand_tabs(&String::from_utf8_lossy(content))
    };
    let lines: Vec<String> = split_lines(&text).into_iter().map(str::to_owned).collect();
    let n = lines.len();
    let mut spans: Vec<(usize, usize)> = if windows.is_empty() {
        if n == 0 { Vec::new() } else { vec![(1, n)] }
    } else {
        windows
            .iter()
            .map(|&(a, b)| (a.clamp(1, n.max(1)), b.clamp(1, n.max(1))))
            .filter(|_| n > 0)
            .collect()
    };
    spans.sort();
    // Overlapping or touching excerpts become one.
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (a, b) in spans {
        match merged.last_mut() {
            Some(last) if a <= last.1 + 1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    let hunks = merged
        .into_iter()
        .map(|(a, b)| Hunk {
            old_start: a,
            old_count: b + 1 - a,
            new_start: a,
            new_count: b + 1 - a,
            func: find_func(&lines, a - 1),
            lines: (a..=b)
                .map(|no| DiffLine {
                    kind: Kind::Context,
                    old_no: Some(no),
                    new_no: Some(no),
                    text: lines[no - 1].clone(),
                    emph: Vec::new(),
                })
                .collect(),
        })
        .collect();
    FileDiff {
        path,
        old_path: None,
        status: Status::Modified,
        binary,
        added: 0,
        removed: 0,
        hunks,
        old_lines: Vec::new(),
        new_lines: lines,
        plain: true,
    }
}

/// Git's default funcname heuristic: nearest preceding line that starts
/// with a letter, underscore or dollar sign.
fn find_func(lines: &[String], before: usize) -> Option<String> {
    lines[..before].iter().rev().find_map(|l| {
        let c = l.chars().next()?;
        if c.is_alphabetic() || c == '_' || c == '$' {
            let t: String = l.trim_end().chars().take(80).collect();
            Some(t)
        } else {
            None
        }
    })
}

fn is_binary(data: Option<&[u8]>) -> bool {
    data.is_some_and(|d| d.iter().take(8000).any(|&b| b == 0))
}

fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 16);
    let mut col = 0;
    for c in s.chars() {
        match c {
            '\t' => {
                let n = 4 - (col % 4);
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

/// Same tokenization as imara-diff's line source, with line endings stripped.
fn split_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let (line, tail) = match rest.find('\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        out.push(line.strip_suffix('\r').unwrap_or(line));
        rest = tail;
    }
    out
}

// ---- intra-line emphasis -------------------------------------------------

struct Words<'a>(&'a str);

struct WordIter<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Iterator for WordIter<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        let rest = &self.s[self.pos..];
        let mut chars = rest.char_indices();
        let (_, first) = chars.next()?;
        let class = char_class(first);
        let mut end = first.len_utf8();
        if class != 2 {
            for (i, c) in chars {
                if char_class(c) != class {
                    end = i;
                    break;
                }
                end = i + c.len_utf8();
            }
        }
        let tok = &rest[..end];
        self.pos += end;
        Some(tok)
    }
}

fn char_class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

impl<'a> TokenSource for Words<'a> {
    type Token = &'a str;
    type Tokenizer = WordIter<'a>;
    fn tokenize(&self) -> WordIter<'a> {
        WordIter { s: self.0, pos: 0 }
    }
    fn estimate_tokens(&self) -> u32 {
        (self.0.len() / 4) as u32 + 1
    }
}

/// Pair removed and added lines one to one and mark the differing tokens.
fn emphasize(lines: &mut [DiffLine], del_start: usize, add_start: usize, end: usize) {
    let dels = add_start - del_start;
    let adds = end - add_start;
    if dels == 0 || adds == 0 {
        return;
    }
    for k in 0..dels.min(adds) {
        let (d, a) = (del_start + k, add_start + k);
        let (old_emph, new_emph) = word_diff(&lines[d].text, &lines[a].text);
        lines[d].emph = old_emph;
        lines[a].emph = new_emph;
    }
}

/// Byte ranges within a line.
type Ranges = Vec<(usize, usize)>;

fn word_diff(old: &str, new: &str) -> (Ranges, Ranges) {
    let input = InternedInput::new(Words(old), Words(new));
    let diff = Diff::compute(Algorithm::Myers, &input);
    let old_tokens: Vec<&str> = Words(old).tokenize().collect();
    let new_tokens: Vec<&str> = Words(new).tokenize().collect();
    let changed = diff.count_removals() as usize + diff.count_additions() as usize;
    let total = old_tokens.len() + new_tokens.len();
    // A nearly complete rewrite is clearer without emphasis noise.
    if total == 0 || changed * 10 > total * 7 {
        return (Vec::new(), Vec::new());
    }
    let ranges = |tokens: &[&str], flag: &dyn Fn(u32) -> bool| {
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut pos = 0;
        for (i, t) in tokens.iter().enumerate() {
            let next = pos + t.len();
            if flag(i as u32) {
                match out.last_mut() {
                    Some(last) if last.1 == pos => last.1 = next,
                    _ => out.push((pos, next)),
                }
            }
            pos = next;
        }
        out
    };
    (
        ranges(&old_tokens, &|i| diff.is_removed(i)),
        ranges(&new_tokens, &|i| diff.is_added(i)),
    )
}
