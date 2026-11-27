use std::io::{self, Write};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{display, display_draft};
use crate::{completion::Matches, transcript::window_size};

pub(super) struct Screen<'a> {
    prompt: &'a str,
    columns: usize,
    rows: usize,
    old_row: usize,
}

impl<'a> Screen<'a> {
    pub(super) fn new(prompt: &'a str, before: &str) -> Self {
        let size = window_size().ok();
        let mut screen = Self {
            // The header of a multiline prompt is already on screen.
            prompt: prompt.rsplit('\n').next().unwrap_or_default(),
            columns: size
                .map(|s| usize::from(s.ws_col))
                .filter(|&n| n > 0)
                .unwrap_or(80),
            rows: size
                .map(|s| usize::from(s.ws_row))
                .filter(|&n| n > 0)
                .unwrap_or(24),
            old_row: 0,
        };
        screen.old_row = screen.position(before, "").row;
        screen
    }

    pub(super) fn at_start(mut self) -> Self {
        self.old_row = 0;
        self
    }

    pub(super) fn clear(&self, echo: &mut impl Write) -> io::Result<()> {
        if self.old_row > 0 {
            write!(echo, "\x1b[{}A", self.old_row.min(self.rows - 1))?;
        }
        echo.write_all(b"\r\x1b[J")
    }

    pub(super) fn draw(
        &self,
        echo: &mut impl Write,
        line: &[u8],
        cursor: usize,
        menu: Option<(&Matches, Option<usize>)>,
    ) -> io::Result<Option<usize>> {
        let before = display_draft(&line[..cursor]);
        let after = display_draft(&line[cursor..]);
        let caret = self.position(&before, "");
        let end = self.position(&before, &after);
        self.clear(echo)?;
        write!(echo, "{}{before}{after}", self.prompt)?;
        let height = self.rows.saturating_sub(end.row + 1).min(12);
        let drawn = if let Some((matches, selected)) = menu.filter(|_| height > 0) {
            let labels: Vec<_> = matches
                .labels()
                .map(|label| fit(&display(label.as_bytes()), self.columns))
                .collect();
            draw_grid(echo, &labels, self.columns, height, selected)?
        } else {
            0
        };
        // Relative movement also works if displaying the grid scrolled the tty.
        let up = (end.row - caret.row + drawn).min(self.rows - 1);
        if up > 0 {
            write!(echo, "\x1b[{up}A")?;
        }
        if up > 0 || !after.is_empty() {
            write!(echo, "\r")?;
            let column = caret.column.min(self.columns - 1);
            if column > 0 {
                write!(echo, "\x1b[{column}C")?;
            }
        }
        Ok((drawn > 0).then_some(end.row - caret.row + 1))
    }

    fn position(&self, before: &str, after: &str) -> Position {
        let mut pos = Position::default();
        // PROMPT uses SGR for styling; those sequences take no screen space.
        let mut parts = self.prompt.split("\x1b[");
        pos.advance(parts.next().unwrap_or_default(), self.columns);
        for part in parts {
            let style_len = part
                .bytes()
                .take_while(|b| b.is_ascii_digit() || *b == b';')
                .count();
            let text = part
                .get(style_len..)
                .and_then(|tail| tail.strip_prefix('m'))
                .unwrap_or(part);
            pos.advance(text, self.columns);
        }
        pos.advance(before, self.columns);
        pos.advance(after, self.columns);
        pos
    }
}

#[derive(Default)]
struct Position {
    row: usize,
    column: usize,
}

impl Position {
    fn advance(&mut self, text: &str, columns: usize) {
        for ch in text.chars() {
            if ch == '\n' {
                self.row += 1;
                self.column = 0;
                continue;
            }
            let width = ch.width().unwrap_or(0).min(columns);
            if width > 0 && self.column + width > columns {
                self.row += 1;
                self.column = 0;
            }
            self.column += width;
        }
    }
}

pub(super) fn dismiss(echo: &mut impl Write, down: usize) -> io::Result<()> {
    // Save only when clearing: this operation never scrolls.
    write!(echo, "\x1b7\x1b[{down}B\r\x1b[J\x1b8")?;
    echo.flush()
}

const GAP: usize = 2;

fn draw_grid(
    echo: &mut impl Write,
    labels: &[String],
    columns: usize,
    height: usize,
    selected: Option<usize>,
) -> io::Result<usize> {
    let widths: Vec<_> = labels.iter().map(|label| label.width()).collect();
    let (rows, widths) = (1..=labels.len().min(columns.div_ceil(GAP + 1)))
        .rev()
        .find_map(|count| {
            let rows = labels.len().div_ceil(count);
            let widths: Vec<_> = widths
                .chunks(rows)
                .map(|col| *col.iter().max().unwrap())
                .collect();
            let space = widths.iter().sum::<usize>() + GAP * (widths.len() - 1);
            (space <= columns).then_some((rows, widths))
        })
        .expect("every label fits on its own row");
    let height = height.min(rows);
    let top = selected
        .map_or(0, |index| index % rows / height * height)
        .min(rows - height);
    for row in top..top + height {
        // Move to column zero and advance one row, even with ONLCR disabled.
        echo.write_all(b"\r\x1bD")?;
        let mut padding = 0;
        for (column, width) in widths.iter().enumerate() {
            let index = column * rows + row;
            let Some(label) = labels.get(index) else {
                break;
            };
            write!(echo, "{:padding$}", "")?;
            if selected == Some(index) {
                write!(echo, "\x1b[7m{label}\x1b[27m")?;
            } else {
                echo.write_all(label.as_bytes())?;
            }
            padding = width - label.width() + GAP;
        }
    }
    Ok(height)
}

fn fit(label: &str, columns: usize) -> String {
    if label.width() <= columns {
        return label.into();
    }
    let mut width = 0;
    let clipped: String = label
        .chars()
        .take_while(|ch| {
            width += ch.width().unwrap_or(0);
            width < columns
        })
        .collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_uses_column_major_order_and_terminal_cell_widths() {
        let labels = ["a", "界", "files", "note"].map(str::to_owned);
        let mut output = Vec::new();
        let rows = draw_grid(&mut output, &labels, 9, 4, Some(1)).unwrap();
        assert_eq!(rows, 2);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\r\x1bDa   files\r\x1bD\x1b[7m界\x1b[27m  note"
        );
    }

    #[test]
    fn narrow_grid_keeps_the_selected_row_visible() {
        let labels = ["first", "second", "第三者長"].map(|label| fit(label, 6));
        let mut output = Vec::new();
        let rows = draw_grid(&mut output, &labels, 6, 2, Some(2)).unwrap();
        assert_eq!(rows, 2);
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\r\x1bDsecond\r\x1bD\x1b[7m第三…\x1b[27m"
        );
    }
}
