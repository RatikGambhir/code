//! A renderer-independent copy of the visible terminal screen.

/// A terminal color as the program asked for it. The UI maps indexed colors
/// onto its theme's palette.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum TermColor {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for TermColor {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => TermColor::Default,
            vt100::Color::Idx(i) => TermColor::Indexed(i),
            vt100::Color::Rgb(r, g, b) => TermColor::Rgb(r, g, b),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CellStyle {
    pub fg: TermColor,
    pub bg: TermColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

impl Default for CellStyle {
    fn default() -> Self {
        Self {
            fg: TermColor::Default,
            bg: TermColor::Default,
            bold: false,
            dim: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

/// Consecutive cells sharing one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub text: String,
    pub style: CellStyle,
    /// Number of terminal columns the run occupies (wide characters take two).
    pub columns: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub runs: Vec<Run>,
}

impl Line {
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub rows: u16,
    pub cols: u16,
    pub lines: Vec<Line>,
    /// Cursor (row, col), or `None` when hidden or scrolled into history.
    pub cursor: Option<(u16, u16)>,
    /// How many lines the view is scrolled up into history.
    pub scrollback: usize,
    pub alternate_screen: bool,
    pub exited: Option<Option<u32>>,
}

impl Snapshot {
    pub(crate) fn capture<CB: vt100::Callbacks>(
        parser: &vt100::Parser<CB>,
        exited: Option<Option<u32>>,
    ) -> Self {
        let screen = parser.screen();
        let (rows, cols) = screen.size();
        let mut lines = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut runs: Vec<Run> = Vec::new();
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let style = CellStyle {
                    fg: cell.fgcolor().into(),
                    bg: cell.bgcolor().into(),
                    bold: cell.bold(),
                    dim: cell.dim(),
                    italic: cell.italic(),
                    underline: cell.underline(),
                    inverse: cell.inverse(),
                };
                let text = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                let width = if cell.is_wide() { 2 } else { 1 };
                match runs.last_mut() {
                    Some(last) if last.style == style => {
                        last.text.push_str(text);
                        last.columns += width;
                    }
                    _ => runs.push(Run {
                        text: text.to_string(),
                        style,
                        columns: width,
                    }),
                }
            }
            // Trailing default-styled blanks carry no information.
            if let Some(last) = runs.last_mut() {
                if last.style == CellStyle::default() {
                    let trimmed = last.text.trim_end_matches(' ');
                    let removed = (last.text.len() - trimmed.len()) as u16;
                    last.text.truncate(trimmed.len());
                    last.columns -= removed;
                    if last.text.is_empty() {
                        runs.pop();
                    }
                }
            }
            lines.push(Line { runs });
        }

        let scrollback = screen.scrollback();
        let cursor = (!screen.hide_cursor() && scrollback == 0).then(|| screen.cursor_position());

        Self {
            rows,
            cols,
            lines,
            cursor,
            scrollback,
            alternate_screen: screen.alternate_screen(),
            exited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(bytes: &[u8]) -> Snapshot {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(bytes);
        Snapshot::capture(&parser, None)
    }

    #[test]
    fn styled_runs_are_merged() {
        let snapshot = capture(b"ab\x1b[32mcd\x1b[0mef");
        let runs = &snapshot.lines[0].runs;
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].text, "ab");
        assert_eq!(runs[1].text, "cd");
        assert_eq!(runs[1].style.fg, TermColor::Indexed(2));
        assert_eq!(runs[2].text, "ef");
        assert_eq!(snapshot.lines[0].text(), "abcdef");
    }

    #[test]
    fn wide_characters_take_two_columns() {
        let snapshot = capture("日本".as_bytes());
        assert_eq!(snapshot.lines[0].runs[0].text, "日本");
        assert_eq!(snapshot.lines[0].runs[0].columns, 4);
    }

    #[test]
    fn cursor_position_and_hidden_cursor() {
        let snapshot = capture(b"abc\r\nd");
        assert_eq!(snapshot.cursor, Some((1, 1)));
        let hidden = capture(b"\x1b[?25l");
        assert_eq!(hidden.cursor, None);
    }

    #[test]
    fn background_colored_blanks_are_kept() {
        let snapshot = capture(b"\x1b[41m   \x1b[0m");
        assert_eq!(snapshot.lines[0].runs.len(), 1);
        assert_eq!(snapshot.lines[0].runs[0].style.bg, TermColor::Indexed(1));
    }
}
