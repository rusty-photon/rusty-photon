//! Reading one of the child's output streams to its end, keeping what its
//! [`Capture`] asks for.
//!
//! The stream is read for as long as the child writes, whatever is kept: a
//! pipe nobody reads fills, and a child blocked writing to it never finishes.

use std::io::{self, Read};

use crate::{Capture, Error, Stream};

/// A callback handed each line of stdout as it arrives.
pub type LineSink = Box<dyn FnMut(&str) + Send>;

/// The longest a line is held back waiting for its newline before it is
/// handed over as it stands — a child that never writes one must not make
/// the reader buffer without bound.
pub const MAX_LINE: usize = 65_536;

const CHUNK: usize = 8192;

/// Read `source` to end-of-file. Returns what `capture` keeps, or why the
/// stream could not be read to its end.
pub fn drain(
    mut source: impl Read,
    stream: Stream,
    capture: Capture,
    sink: Option<LineSink>,
) -> Result<Vec<u8>, Error> {
    let mut kept = Kept::new(capture);
    let mut lines = sink.map(Lines::new);
    let mut chunk = [0u8; CHUNK];
    loop {
        let read = match source.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => return Err(Error::Read { stream, source }),
        };
        let Some(bytes) = chunk.get(..read) else {
            return Err(Error::Read {
                stream,
                source: io::Error::other("a read reported more bytes than its buffer holds"),
            });
        };
        if let Some(lines) = lines.as_mut() {
            lines.feed(bytes);
        }
        kept.push(bytes)
            .map_err(|limit| Error::OutputLimit { stream, limit })?;
    }
    if let Some(lines) = lines.as_mut() {
        lines.finish();
    }
    Ok(kept.into_bytes())
}

/// What a [`Capture`] keeps of a stream, as the stream is read.
#[derive(Debug)]
pub struct Kept {
    capture: Capture,
    bytes: Vec<u8>,
}

impl Kept {
    pub const fn new(capture: Capture) -> Self {
        Self {
            capture,
            bytes: Vec::new(),
        }
    }

    /// Take in the next bytes read. `Err` carries a [`Capture::Full`] limit
    /// these bytes would exceed; nothing of them is kept then.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), usize> {
        match self.capture {
            // An inherited stream is read only for a line callback.
            Capture::Discard | Capture::Inherit => Ok(()),
            Capture::Full(limit) => {
                if bytes.len() > limit.saturating_sub(self.bytes.len()) {
                    return Err(limit);
                }
                self.bytes.extend_from_slice(bytes);
                Ok(())
            }
            Capture::Head(len) => {
                let room = len.saturating_sub(self.bytes.len());
                let kept = bytes.get(..room.min(bytes.len())).unwrap_or_default();
                self.bytes.extend_from_slice(kept);
                Ok(())
            }
            Capture::Tail(len) => {
                self.bytes.extend_from_slice(bytes);
                // Trimmed only once the buffer reaches twice the tail, so the
                // shift that drops the front is paid once per `len` bytes
                // read rather than once per read.
                if self.bytes.len() > len.saturating_mul(2) {
                    self.keep_last(len);
                }
                Ok(())
            }
        }
    }

    pub fn into_bytes(mut self) -> Vec<u8> {
        if let Capture::Tail(len) = self.capture {
            self.keep_last(len);
        }
        self.bytes
    }

    fn keep_last(&mut self, len: usize) {
        let excess = self.bytes.len().saturating_sub(len);
        self.bytes.drain(..excess);
    }
}

/// Splits a stream into lines for a [`LineSink`]: on `\n`, without a
/// trailing `\r`, invalid UTF-8 replaced, an unterminated last line handed
/// over at end-of-file, and a line longer than [`MAX_LINE`] handed over in
/// pieces.
pub struct Lines {
    sink: LineSink,
    partial: Vec<u8>,
    /// The line in `partial` continues one already handed over in part, so
    /// its newline ends that line rather than an empty one of its own.
    continued: bool,
}

impl Lines {
    pub fn new(sink: LineSink) -> Self {
        Self {
            sink,
            partial: Vec::new(),
            continued: false,
        }
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        while let Some(newline) = bytes.iter().position(|b| *b == b'\n') {
            let Some((line, rest)) = bytes.split_at_checked(newline) else {
                break;
            };
            self.partial.extend_from_slice(line);
            if self.continued && matches!(self.partial.as_slice(), [] | [b'\r']) {
                // The newline, or the CRLF, ends a line already handed over
                // in full.
                self.partial.clear();
                self.continued = false;
            } else {
                self.emit_line();
            }
            // `rest` starts with the newline just found.
            bytes = rest.get(1..).unwrap_or_default();
        }
        self.partial.extend_from_slice(bytes);
        if self.partial.len() >= MAX_LINE {
            self.emit_piece();
        }
    }

    pub fn finish(&mut self) {
        if !self.partial.is_empty() {
            self.emit_line();
        }
    }

    /// Hand over a whole line, or a long line's last piece.
    fn emit_line(&mut self) {
        if self.partial.last() == Some(&b'\r') {
            self.partial.pop();
        }
        self.emit();
        self.continued = false;
    }

    /// Hand over a piece of a line too long to hold back. A `\r` it ends on
    /// is the line's, not a line ending.
    fn emit_piece(&mut self) {
        self.emit();
        self.continued = true;
    }

    fn emit(&mut self) {
        let line = String::from_utf8_lossy(&self.partial);
        (self.sink)(&line);
        self.partial.clear();
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    fn kept(capture: Capture, pushes: &[&[u8]]) -> Result<Vec<u8>, usize> {
        let mut kept = Kept::new(capture);
        for bytes in pushes {
            kept.push(bytes)?;
        }
        Ok(kept.into_bytes())
    }

    #[test]
    fn test_discard_keeps_nothing() {
        assert_eq!(kept(Capture::Discard, &[b"abc", b"def"]).unwrap(), b"");
    }

    #[test]
    fn test_full_keeps_everything_up_to_its_limit() {
        assert_eq!(
            kept(Capture::Full(6), &[b"abc", b"def"]).unwrap(),
            b"abcdef"
        );
    }

    #[test]
    fn test_full_refuses_a_byte_past_its_limit() {
        assert_eq!(kept(Capture::Full(5), &[b"abc", b"def"]).unwrap_err(), 5);
    }

    #[test]
    fn test_tail_keeps_the_last_bytes_across_pushes() {
        assert_eq!(
            kept(Capture::Tail(4), &[b"abc", b"defgh", b"ij"]).unwrap(),
            b"ghij"
        );
    }

    /// The trim inside `push` and the one in `into_bytes` must agree: a tail
    /// that crossed the doubling threshold mid-stream ends at `len` all the
    /// same.
    #[test]
    fn test_tail_trims_mid_stream_and_at_the_end_alike() {
        let long = vec![b'x'; 100];
        assert_eq!(kept(Capture::Tail(4), &[&long, b"tail"]).unwrap(), b"tail");
    }

    #[test]
    fn test_head_keeps_the_first_bytes_across_pushes() {
        assert_eq!(
            kept(Capture::Head(4), &[b"ab", b"cdef", b"gh"]).unwrap(),
            b"abcd"
        );
    }

    #[test]
    fn test_head_never_fails_on_more_output() {
        assert_eq!(kept(Capture::Head(2), &[&[b'x'; 10_000]]).unwrap(), b"xx");
    }

    #[test]
    fn test_inherit_keeps_nothing_of_a_stream_read_for_lines() {
        assert_eq!(kept(Capture::Inherit, &[b"abc"]).unwrap(), b"");
    }

    #[test]
    fn test_tail_of_zero_keeps_nothing() {
        assert_eq!(kept(Capture::Tail(0), &[b"abc"]).unwrap(), b"");
    }

    fn collected(feeds: &[&[u8]]) -> Vec<String> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let mut lines = Lines::new(Box::new(move |line| {
            sink.lock().unwrap().push(line.to_string());
        }));
        for bytes in feeds {
            lines.feed(bytes);
        }
        lines.finish();
        drop(lines);
        Arc::try_unwrap(seen).unwrap().into_inner().unwrap()
    }

    #[test]
    fn test_lines_split_on_newlines_and_drop_carriage_returns() {
        assert_eq!(collected(&[b"one\r\ntwo\nthree"]), ["one", "two", "three"]);
    }

    #[test]
    fn test_a_line_split_across_reads_arrives_whole() {
        assert_eq!(
            collected(&[b"hel", b"lo\nwor", b"ld\n"]),
            ["hello", "world"]
        );
    }

    #[test]
    fn test_an_empty_line_is_still_a_line() {
        assert_eq!(collected(&[b"a\n\nb\n"]), ["a", "", "b"]);
    }

    #[test]
    fn test_invalid_utf8_is_replaced_not_dropped() {
        assert_eq!(collected(&[b"bad \xff byte\n"]), ["bad \u{fffd} byte"]);
    }

    #[test]
    fn test_a_line_past_the_limit_is_handed_over_in_pieces() {
        let long = vec![b'x'; MAX_LINE];
        let lines = collected(&[&long, b"rest\n"]);
        assert_eq!(
            lines.len(),
            2,
            "{:?}",
            lines.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(lines[0].len(), MAX_LINE);
        assert_eq!(lines[1], "rest");
    }

    /// A piece handed over at the limit, with the line's newline arriving in
    /// the next read: the newline ends that line, and is not a line of its
    /// own.
    #[test]
    fn test_a_newline_after_a_piece_ends_the_line_it_continues() {
        let long = vec![b'x'; MAX_LINE];
        let lines = collected(&[&long, b"\nnext\n"]);
        assert_eq!(
            lines.len(),
            2,
            "{:?}",
            lines.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(lines[1], "next");
    }

    /// The same with a CRLF ending, whose `\r` arrives with the newline.
    #[test]
    fn test_a_crlf_after_a_piece_ends_the_line_it_continues() {
        let long = vec![b'x'; MAX_LINE];
        let lines = collected(&[&long, b"\r\nnext\r\n"]);
        assert_eq!(
            lines.len(),
            2,
            "{:?}",
            lines.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(lines[1], "next");
    }

    /// A `\r` a piece happens to end on belongs to the line.
    #[test]
    fn test_a_piece_keeps_a_carriage_return_it_ends_on() {
        let mut long = vec![b'x'; MAX_LINE - 1];
        long.push(b'\r');
        let lines = collected(&[&long, b"rest\n"]);
        assert_eq!(lines[0].len(), MAX_LINE);
        assert!(lines[0].ends_with('\r'));
        assert_eq!(lines[1], "rest");
    }

    #[test]
    fn test_drain_reads_to_the_end_and_feeds_the_sink() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let bytes = drain(
            &b"first\nsecond\n"[..],
            Stream::Stdout,
            Capture::Full(1024),
            Some(Box::new(move |line| {
                sink.lock().unwrap().push(line.to_string());
            })),
        )
        .unwrap();
        assert_eq!(bytes, b"first\nsecond\n");
        assert_eq!(*seen.lock().unwrap(), ["first", "second"]);
    }

    #[test]
    fn test_drain_names_the_stream_that_overflowed() {
        let error = drain(&b"too long"[..], Stream::Stderr, Capture::Full(3), None).unwrap_err();
        assert!(
            matches!(
                error,
                Error::OutputLimit {
                    stream: Stream::Stderr,
                    limit: 3
                }
            ),
            "{error:?}"
        );
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("pipe broke"))
        }
    }

    #[test]
    fn test_drain_reports_a_read_error_with_its_stream() {
        let error = drain(FailingReader, Stream::Stdout, Capture::Discard, None).unwrap_err();
        assert!(
            matches!(
                error,
                Error::Read {
                    stream: Stream::Stdout,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("pipe broke"), "{error}");
    }
}
