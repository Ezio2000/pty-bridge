//! Plain-text views of terminal output produced by a VT100 emulator.
use serde::Serialize;

/// Logical rows kept while rendering one text range; older rows are reported as dropped.
pub const TEXT_SCROLLBACK: usize = 4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CursorPosition {
    pub row: u16,
    pub col: u16,
}

/// A run of reverse-video cells, which TUIs commonly use for selection and focus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Highlight {
    pub row: u16,
    pub col: u16,
    pub len: u16,
}

/// The emulated screen after applying every output byte before `end_cursor`.
#[derive(Debug, Clone, Serialize)]
pub struct ScreenSnapshot {
    pub rows: u16,
    pub cols: u16,
    /// Physical rows without trailing blanks; trailing empty rows are omitted.
    pub lines: Vec<String>,
    pub cursor: CursorPosition,
    pub cursor_visible: bool,
    pub alternate_screen: bool,
    pub application_cursor: bool,
    pub highlights: Vec<Highlight>,
    pub end_cursor: u64,
}

/// Rendered text of the byte range `start_cursor..next_cursor`.
#[derive(Debug, Clone, Serialize)]
pub struct TextRead {
    pub text: String,
    pub start_cursor: u64,
    pub next_cursor: u64,
    pub dropped_bytes: u64,
    /// Early rows of this range exceeded `TEXT_SCROLLBACK` and are missing from `text`.
    pub rows_dropped: bool,
}

pub(crate) fn snapshot(screen: &vt100::Screen, end_cursor: u64) -> ScreenSnapshot {
    let (rows, cols) = screen.size();
    let (row, col) = screen.cursor_position();
    // Spaces the program wrote before the cursor, such as after a prompt, are kept.
    let mut lines: Vec<String> = screen
        .rows(0, cols)
        .enumerate()
        .map(|(index, line)| {
            if index == usize::from(row) {
                line
            } else {
                line.trim_end().to_string()
            }
        })
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut highlights = Vec::new();
    for row in 0..rows {
        let mut run: Option<Highlight> = None;
        for col in 0..cols {
            let inverse = screen.cell(row, col).is_some_and(vt100::Cell::inverse);
            match (&mut run, inverse) {
                (Some(current), true) => current.len += 1,
                (None, true) => run = Some(Highlight { row, col, len: 1 }),
                (Some(_), false) => highlights.extend(run.take()),
                (None, false) => {}
            }
        }
        highlights.extend(run);
    }
    ScreenSnapshot {
        rows,
        cols,
        lines,
        cursor: CursorPosition { row, col },
        cursor_visible: !screen.hide_cursor(),
        alternate_screen: screen.alternate_screen(),
        application_cursor: screen.application_cursor(),
        highlights,
        end_cursor,
    }
}

/// Renders a byte range on a fresh emulator of the session width, joining wrapped rows.
/// Returns the text and whether early rows exceeded `TEXT_SCROLLBACK`.
pub fn render_text(bytes: &[u8], rows: u16, cols: u16) -> (String, bool) {
    let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), TEXT_SCROLLBACK);
    parser.process(bytes);
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let total = screen.scrollback();
    let mut physical = Vec::new();
    // Each scrollback offset exposes a window whose first rows are the oldest unseen ones.
    let mut offset = total;
    while offset > 0 {
        screen.set_scrollback(offset);
        let take = offset.min(usize::from(rows));
        collect(screen, take, &mut physical);
        offset -= take;
    }
    screen.set_scrollback(0);
    let cursor_row = physical.len() + usize::from(screen.cursor_position().0);
    collect(screen, usize::from(rows), &mut physical);

    // Trailing spaces are kept only on the cursor's line, where prompts end.
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut keep = false;
    for (index, (text, wrapped)) in physical.into_iter().enumerate() {
        current.push_str(&text);
        keep |= index == cursor_row;
        if !wrapped {
            lines.push(finish(&mut current, keep));
            keep = false;
        }
    }
    if !current.is_empty() {
        lines.push(finish(&mut current, keep));
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    (
        collapse_blank_runs(lines).join("\n"),
        total >= TEXT_SCROLLBACK,
    )
}

/// Runs of three or more empty lines, typical of cleared or full-screen drawing, become one.
fn collapse_blank_runs(lines: Vec<String>) -> Vec<String> {
    let mut compact = Vec::with_capacity(lines.len());
    let mut blanks = 0;
    for line in lines {
        if line.is_empty() {
            blanks += 1;
            continue;
        }
        compact.extend(std::iter::repeat_n(
            String::new(),
            if blanks >= 3 { 1 } else { blanks },
        ));
        blanks = 0;
        compact.push(line);
    }
    compact
}

fn finish(line: &mut String, keep: bool) -> String {
    let text = if keep {
        line.clone()
    } else {
        line.trim_end().to_string()
    };
    line.clear();
    text
}

fn collect(screen: &vt100::Screen, take: usize, out: &mut Vec<(String, bool)>) {
    let (_, cols) = screen.size();
    for (index, text) in screen.rows(0, cols).take(take).enumerate() {
        out.push((text, screen.row_wrapped(index as u16)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_editor_redraws_collapse_to_final_text() {
        // Shape of the Python REPL echo: redraw the prompt and input after every key.
        let bytes = b"\x1b[1;35m>>> \x1b[0m\x1b[4D\x1b[4C\x1b[4D\x1b[1;35m>>> \x1b[0mp\x1b[5D\
\x1b[1;35m>>> \x1b[0mpr\x1b[6D\x1b[1;35m>>> \x1b[0mprint(1)\x1b[12D\r\n\x1b[?2004lhi\r\n2\r\n>>> ";
        let (text, dropped) = render_text(bytes, 24, 80);
        assert_eq!(text, ">>> print(1)\nhi\n2\n>>> ");
        assert!(!dropped);
    }

    #[test]
    fn scrollback_is_kept_in_order_and_wrapped_rows_are_joined() {
        let mut bytes = Vec::new();
        for i in 0..50 {
            bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        bytes.extend_from_slice(&[b'x'; 25]);
        let (text, dropped) = render_text(&bytes, 5, 10);
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 51);
        assert_eq!(lines[0], "line 0");
        assert_eq!(lines[49], "line 49");
        assert_eq!(lines[50], "x".repeat(25));
        assert!(!dropped);
    }

    #[test]
    fn long_blank_runs_collapse_but_paragraph_breaks_stay() {
        let (text, _) = render_text(b"a\r\n\r\nb\r\n\r\n\r\n\r\n\r\nc\r\n", 24, 80);
        assert_eq!(text, "a\n\nb\n\nc");
    }

    #[test]
    fn reports_rows_beyond_scrollback() {
        let bytes = "y\r\n".repeat(TEXT_SCROLLBACK + 100);
        let (text, dropped) = render_text(bytes.as_bytes(), 24, 80);
        assert!(dropped);
        assert!(text.lines().count() <= TEXT_SCROLLBACK + 24);
    }

    #[test]
    fn screen_reports_cursor_alternate_screen_and_inverse_runs() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"\x1b[?1049h\x1b[?1h\x1b[2;3H\x1b[7m[OK]\x1b[0m done\x1b[3;1H");
        let screen = snapshot(parser.screen(), 42);
        assert!(screen.alternate_screen);
        assert!(screen.application_cursor);
        assert_eq!(screen.lines, vec!["", "  [OK] done"]);
        assert_eq!(screen.cursor, CursorPosition { row: 2, col: 0 });
        assert_eq!(
            screen.highlights,
            vec![Highlight {
                row: 1,
                col: 2,
                len: 4
            }]
        );
        assert_eq!(screen.end_cursor, 42);
    }
}
