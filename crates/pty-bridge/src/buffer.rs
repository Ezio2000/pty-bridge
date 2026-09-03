use std::collections::VecDeque;

#[derive(Debug)]
pub struct OutputBuffer {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
    capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferRead {
    pub bytes: Vec<u8>,
    pub next_cursor: u64,
    pub dropped_bytes: u64,
}

impl OutputBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            bytes: VecDeque::with_capacity(capacity),
            start: 0,
            end: 0,
            capacity,
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        for byte in data {
            self.bytes.push_back(*byte);
            self.end += 1;
            if self.bytes.len() > self.capacity {
                self.bytes.pop_front();
                self.start += 1;
            }
        }
    }

    pub fn read(&self, cursor: u64, max_bytes: usize) -> BufferRead {
        let effective = cursor.max(self.start).min(self.end);
        let dropped_bytes = self.start.saturating_sub(cursor);
        let offset = (effective - self.start) as usize;
        let available = self.bytes.len().saturating_sub(offset);
        let len = available.min(max_bytes);
        let bytes = self.bytes.iter().skip(offset).take(len).copied().collect();
        BufferRead {
            bytes,
            next_cursor: effective + len as u64,
            dropped_bytes,
        }
    }

    pub fn tail(&self, max_bytes: usize) -> Vec<u8> {
        self.bytes
            .iter()
            .skip(self.bytes.len().saturating_sub(max_bytes))
            .copied()
            .collect()
    }

    pub fn range(&self) -> (u64, u64) {
        (self.start, self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_dropped_bytes_and_monotonic_cursor() {
        let mut buffer = OutputBuffer::new(4);
        buffer.append(b"abcdef");
        let read = buffer.read(0, 10);
        assert_eq!(read.bytes, b"cdef");
        assert_eq!(read.dropped_bytes, 2);
        assert_eq!(read.next_cursor, 6);
    }

    #[test]
    fn supports_independent_cursors() {
        let mut buffer = OutputBuffer::new(16);
        buffer.append(b"hello");
        assert_eq!(buffer.read(0, 2).bytes, b"he");
        assert_eq!(buffer.read(0, 5).bytes, b"hello");
    }
}
