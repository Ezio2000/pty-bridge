//! Terminal queries answered locally, in stream order with the emulated screen.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Query {
    Status,
    CursorPosition,
    ExtendedCursorPosition,
    PrimaryAttributes,
}

impl Query {
    /// Answers from the screen after every byte preceding the query was applied.
    pub(crate) fn answer(self, screen: &vt100::Screen) -> Vec<u8> {
        let (row, col) = screen.cursor_position();
        match self {
            Self::Status => b"\x1b[0n".to_vec(),
            Self::CursorPosition => format!("\x1b[{};{}R", row + 1, col + 1).into_bytes(),
            Self::ExtendedCursorPosition => format!("\x1b[?{};{}R", row + 1, col + 1).into_bytes(),
            // VT100 with advanced video.
            Self::PrimaryAttributes => b"\x1b[?1;2c".to_vec(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Chunk {
    Visible(Vec<u8>),
    Query(Query),
}

#[derive(Default)]
pub(crate) struct TerminalProtocol {
    pending: Vec<u8>,
}

impl TerminalProtocol {
    /// Splits output into visible runs and queries, preserving their order.
    pub(crate) fn process(&mut self, bytes: &[u8]) -> Vec<Chunk> {
        const QUERIES: [(&[u8], Query); 5] = [
            (b"\x1b[5n", Query::Status),
            (b"\x1b[6n", Query::CursorPosition),
            (b"\x1b[?6n", Query::ExtendedCursorPosition),
            (b"\x1b[c", Query::PrimaryAttributes),
            (b"\x1b[0c", Query::PrimaryAttributes),
        ];

        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut visible = Vec::with_capacity(self.pending.len());
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let rest = &self.pending[consumed..];
            if let Some((sequence, query)) = QUERIES.iter().find(|(q, _)| rest.starts_with(q)) {
                consumed += sequence.len();
                if !visible.is_empty() {
                    chunks.push(Chunk::Visible(std::mem::take(&mut visible)));
                }
                chunks.push(Chunk::Query(*query));
                continue;
            }
            if QUERIES.iter().any(|(q, _)| q.starts_with(rest)) {
                break;
            }
            visible.push(self.pending[consumed]);
            consumed += 1;
        }
        if !visible.is_empty() {
            chunks.push(Chunk::Visible(visible));
        }
        self.pending.drain(..consumed);
        chunks
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_split_visible_output_in_order_across_reads() {
        let mut protocol = TerminalProtocol::default();
        assert_eq!(
            protocol.process(b"ab\x1b[6ncd\x1b["),
            vec![
                Chunk::Visible(b"ab".to_vec()),
                Chunk::Query(Query::CursorPosition),
                Chunk::Visible(b"cd".to_vec()),
            ]
        );
        assert_eq!(
            protocol.process(b"c\x1b[1m"),
            vec![
                Chunk::Query(Query::PrimaryAttributes),
                Chunk::Visible(b"\x1b[1m".to_vec()),
            ]
        );
        assert!(protocol.finish().is_empty());
    }

    #[test]
    fn cursor_answers_reflect_the_screen() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\r\n\r\nabc");
        assert_eq!(Query::CursorPosition.answer(parser.screen()), b"\x1b[3;4R");
        assert_eq!(
            Query::ExtendedCursorPosition.answer(parser.screen()),
            b"\x1b[?3;4R"
        );
    }
}
