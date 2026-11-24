//! A read of a file that is handed to core in parts as the client takes them (see `azemu_core::stream`).
//!
//! A file can be declared as large as 4 TiB and hold nothing, so an answer is never put together in one buffer: it is a list of
//! [`Piece`]s (handles to the stored pages, and the lengths of the holes between them) and each part is made when the
//! connection can take it. Memory in use is one part plus the list, and the list is as long as the number of stored pages in the
//! range, never as long as the range. A client that goes away drops the stream, and no further part is made.
//!
//! The pieces are what the file held when the request was served, so a file that changes while it is sent is read as it was.

use std::collections::VecDeque;
use std::task::{Context, Poll};

use azemu_core::bytes::Bytes;

use crate::state::Piece;

/// The size of a part. Core drops a client that takes no part for the body idle time (60 s), so a part is small enough for a
/// slow link.
pub const PART: usize = 512 * 1024;

/// A body of at least this many bytes is sent in parts; a smaller one is put together at once.
pub const STREAM_MIN: u64 = 1024 * 1024;

/// Zeros for the holes: parts of a hole are slices of this, not new buffers.
static ZEROS: [u8; PART] = [0; PART];

pub struct Download {
    pieces: VecDeque<Piece>,
}

impl Download {
    pub fn new(pieces: Vec<Piece>) -> Download {
        Download { pieces: pieces.into() }
    }
}

impl azemu_core::stream::BodyStream for Download {
    fn poll_chunk(&mut self, _cx: &mut Context<'_>) -> Poll<Option<std::io::Result<Bytes>>> {
        let Some(first) = self.pieces.pop_front() else { return Poll::Ready(None) };
        // a part of a hole is a slice of the static zeros; a part of stored data is made of whole pages, joined
        let part = match first {
            Piece::Zeros(n) => {
                let now = n.min(PART as u64);
                if n > now {
                    self.pieces.push_front(Piece::Zeros(n - now));
                }
                Bytes::from_static(&ZEROS[..now as usize])
            }
            Piece::Data(d) => {
                let mut buf = Vec::with_capacity(PART.min(d.len().max(1) * 8));
                self.take_data(d, &mut buf);
                Bytes::from(buf)
            }
        };
        Poll::Ready(Some(Ok(part)))
    }
}

impl Download {
    /// Fill `buf` with `first` and the stored data that follows it, up to a part.
    fn take_data(&mut self, first: Bytes, buf: &mut Vec<u8>) {
        let mut next = Some(first);
        while let Some(d) = next.take() {
            let room = PART - buf.len();
            if d.len() > room {
                buf.extend_from_slice(&d[..room]);
                self.pieces.push_front(Piece::Data(d.slice(room..)));
                return;
            }
            buf.extend_from_slice(&d);
            if buf.len() == PART {
                return;
            }
            match self.pieces.pop_front() {
                Some(Piece::Data(more)) => next = Some(more),
                Some(zeros) => {
                    // a hole ends the part: it is sent as it is, in the next one
                    self.pieces.push_front(zeros);
                }
                None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use azemu_core::stream::BodyStream;

    fn all(pieces: Vec<Piece>) -> Vec<Vec<u8>> {
        let mut d = Download::new(pieces);
        let mut parts = Vec::new();
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        while let Poll::Ready(Some(Ok(b))) = d.poll_chunk(&mut cx) {
            parts.push(b.to_vec());
        }
        parts
    }

    #[test]
    fn holes_and_data_come_out_in_order_in_parts_of_at_most_a_part() {
        let page = |b: u8| Piece::Data(Bytes::from(vec![b; 512]));
        let parts = all(vec![page(1), page(2), Piece::Zeros(PART as u64 + 10), page(3)]);
        assert!(parts.iter().all(|p| p.len() <= PART));
        let joined: Vec<u8> = parts.concat();
        assert_eq!(joined.len(), 512 * 3 + PART + 10);
        assert_eq!(&joined[..512], &[1u8; 512][..]);
        assert_eq!(&joined[512..1024], &[2u8; 512][..]);
        assert!(joined[1024..1024 + PART + 10].iter().all(|b| *b == 0));
        assert_eq!(&joined[1024 + PART + 10..], &[3u8; 512][..]);
        // many pages are joined into parts, not sent one by one
        let many: Vec<Piece> = (0..4096).map(|i| page((i % 251) as u8)).collect();
        let parts = all(many);
        assert_eq!(parts.iter().map(Vec::len).sum::<usize>(), 4096 * 512);
        assert!(parts.len() <= 5, "{}", parts.len());
    }

    #[test]
    fn a_hole_of_terabytes_is_a_list_of_one() {
        let mut d = Download::new(vec![Piece::Zeros(4 << 40)]);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let Poll::Ready(Some(Ok(first))) = d.poll_chunk(&mut cx) else { panic!("no part") };
        assert_eq!(first.len(), PART);
        assert_eq!(d.pieces.len(), 1);
    }
}
