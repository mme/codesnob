//! Text selected with the mouse. A point is a row and a character in that
//! row's text, which for a line of code is the code alone, however far the
//! view is scrolled sideways, so copying never picks up line numbers.

/// A place in the text: a row of the view, a character in its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: usize,
    pub col: usize,
}

/// From where the mouse went down to where it is now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
}

impl Selection {
    pub fn at(p: Point) -> Self {
        Self { anchor: p, head: p }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The two ends, first one first.
    fn ends(&self) -> (Point, Point) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The characters selected on `row`, whose text is `len` characters
    /// long, as a start and an end; `None` when it has none selected.
    pub fn cols(&self, row: usize, len: usize) -> Option<(usize, usize)> {
        let (a, b) = self.ends();
        if self.is_empty() || row < a.row || row > b.row {
            return None;
        }
        let start = if row == a.row { a.col.min(len) } else { 0 };
        let end = if row == b.row { b.col.min(len) } else { len };
        (start < end).then_some((start, end))
    }

    /// The selected text, rows joined by newlines, given each row's text.
    pub fn text(&self, mut row_text: impl FnMut(usize) -> String) -> String {
        let (a, b) = self.ends();
        let mut out = Vec::new();
        for row in a.row..=b.row {
            let text = row_text(row);
            let len = text.chars().count();
            let (start, end) = self.cols(row, len).unwrap_or((len, len));
            out.push(
                text.chars()
                    .skip(start)
                    .take(end - start)
                    .collect::<String>(),
            );
        }
        out.join("\n")
    }
}

/// The word around character `col` of `text`, as a start and an end: a run
/// of letters, digits and underscores, or else the one character there.
pub fn word(text: &str, col: usize) -> (usize, usize) {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() {
        return (chars.len(), chars.len());
    }
    let wordy = |c: char| c.is_alphanumeric() || c == '_';
    if !wordy(chars[col]) {
        return (col, col + 1);
    }
    let start = chars[..col]
        .iter()
        .rposition(|&c| !wordy(c))
        .map_or(0, |i| i + 1);
    let end = chars[col..]
        .iter()
        .position(|&c| !wordy(c))
        .map_or(chars.len(), |i| col + i);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(a: (usize, usize), b: (usize, usize)) -> Selection {
        Selection {
            anchor: Point { row: a.0, col: a.1 },
            head: Point { row: b.0, col: b.1 },
        }
    }

    fn rows(row: usize) -> String {
        ["fn main() {", "    let x = 1;", "", "}"]
            .get(row)
            .unwrap_or(&"")
            .to_string()
    }

    #[test]
    fn within_one_row() {
        let s = sel((1, 8), (1, 13));
        assert_eq!(s.text(rows), "x = 1");
        assert_eq!(s.cols(1, 14), Some((8, 13)));
        assert_eq!(s.cols(0, 11), None);
    }

    #[test]
    fn across_rows_either_way() {
        let down = sel((0, 3), (1, 7));
        let up = sel((1, 7), (0, 3));
        assert_eq!(down.text(rows), "main() {\n    let");
        assert_eq!(up.text(rows), down.text(rows));
        // Whole rows between, empty ones too.
        assert_eq!(sel((0, 10), (3, 1)).text(rows), "{\n    let x = 1;\n\n}");
    }

    #[test]
    fn past_the_end_of_a_row() {
        assert_eq!(sel((1, 12), (1, 99)).text(rows), "1;");
        assert!(sel((2, 0), (2, 0)).is_empty());
    }

    #[test]
    fn words() {
        assert_eq!(word("let foo_bar = 1;", 6), (4, 11));
        assert_eq!(word("let foo_bar = 1;", 12), (12, 13));
        assert_eq!(word("abc", 0), (0, 3));
        assert_eq!(word("abc", 7), (3, 3));
    }
}
