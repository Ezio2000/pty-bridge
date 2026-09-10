#[derive(Default)]
pub(crate) struct TerminalProtocol {
    pending: Vec<u8>,
}

impl TerminalProtocol {
    pub(crate) fn process(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        const QUERIES: [(&[u8], &[u8]); 3] = [
            (b"\x1b[5n", b"\x1b[0n"),
            (b"\x1b[6n", b"\x1b[1;1R"),
            (b"\x1b[?6n", b"\x1b[?1;1R"),
        ];

        self.pending.extend_from_slice(bytes);
        let mut visible = Vec::with_capacity(self.pending.len());
        let mut response = Vec::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let rest = &self.pending[consumed..];
            if let Some((query, answer)) = QUERIES.iter().find(|(query, _)| rest.starts_with(query))
            {
                consumed += query.len();
                response.extend_from_slice(answer);
                continue;
            }
            if QUERIES.iter().any(|(query, _)| query.starts_with(rest)) {
                break;
            }
            visible.push(self.pending[consumed]);
            consumed += 1;
        }
        self.pending.drain(..consumed);
        (visible, response)
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.pending
    }
}
