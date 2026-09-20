//! The one report renderer: aligned columns, byte sizes, nothing hand-tuned per report.
//!
//! Four reports used to carry four layouts — four indents, four column widths, four ideas of how a
//! size is spelled — which is how a reader ends up unable to compare two `mirvm` outputs. Widths are
//! computed from the cells here, so adding a row cannot silently push a column out of line.
//!
//! This module is compiled source-for-source into the TSan harness, so it must stay pure `std`.

use std::fmt::Write as _;

/// A cell's alignment inside its column.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Right,
}

/// One cell of a report row.
#[derive(Clone, Debug)]
pub struct Cell {
    text: String,
    align: Align,
}

impl Cell {
    pub fn left(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            align: Align::Left,
        }
    }

    pub fn right(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            align: Align::Right,
        }
    }
}

/// A report body: rows of cells, each column as wide as its widest cell.
pub struct Table {
    indent: usize,
    rows: Vec<Vec<Cell>>,
}

impl Table {
    /// `indent` leading spaces on every line: the nesting a report uses to group rows.
    pub fn new(indent: usize) -> Self {
        Self {
            indent,
            rows: Vec::new(),
        }
    }

    /// Append one row. A row of a single cell is a line of its own: it is neither padded nor measured,
    /// so a heading cannot widen a column of data.
    pub fn row(&mut self, cells: Vec<Cell>) -> &mut Self {
        self.rows.push(cells);
        self
    }

    /// The rendered body, one line per row.
    pub fn render(&self) -> String {
        let widths = self.widths();
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&" ".repeat(self.indent));
            for (index, cell) in row.iter().enumerate() {
                if index > 0 {
                    out.push_str("  ");
                }
                let width = widths[index];
                // A left-aligned last cell is not padded: trailing spaces are invisible in a
                // terminal and would have to be reproduced exactly by anything comparing output. A
                // right-aligned last cell is, so a number still lines up with the column above it.
                if index + 1 == row.len() && cell.align == Align::Left {
                    out.push_str(&cell.text);
                } else if cell.align == Align::Right {
                    let _ = write!(out, "{:>width$}", cell.text);
                } else {
                    let _ = write!(out, "{:<width$}", cell.text);
                }
            }
            out.push('\n');
        }
        out
    }

    /// The width of each column, measured over multi-cell rows only.
    fn widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = Vec::new();
        for row in self.rows.iter().filter(|row| row.len() > 1) {
            for (index, cell) in row.iter().enumerate() {
                let width = cell.text.chars().count();
                match widths.get_mut(index) {
                    Some(slot) => *slot = (*slot).max(width),
                    None => widths.push(width),
                }
            }
        }
        widths
    }
}

/// A byte count in the largest unit that keeps it readable, at one decimal.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_come_from_the_cells_and_headings_do_not_widen_columns() {
        let mut table = Table::new(2);
        table.row(vec![Cell::left(
            "a heading that is much longer than any data row",
        )]);
        table.row(vec![Cell::left("aa"), Cell::right("1")]);
        table.row(vec![Cell::left("b"), Cell::right("100")]);
        assert_eq!(
            table.render(),
            "  a heading that is much longer than any data row\n  aa    1\n  b   100\n"
        );
    }

    #[test]
    fn byte_sizes_share_one_spelling() {
        for (bytes, want) in [
            (0, "0B"),
            (999, "999B"),
            (1024, "1.0K"),
            (1536, "1.5K"),
            (1024 * 1024, "1.0M"),
            (1024 * 1024 * 1024, "1.0G"),
        ] {
            assert_eq!(human_bytes(bytes), want, "bytes={bytes}");
        }
    }
}
