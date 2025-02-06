use super::BodyState;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use http_body::Body;
use std::{collections::VecDeque, io::IoSlice};

/// Data returned by `ReplayBody`'s `http_body::Body` implementation is either
/// `Bytes` returned by the initial body, or a list of all `Bytes` chunks
/// returned by the initial body (when replaying it).
#[derive(Debug)]
pub enum Data {
    Initial(Bytes),
    Replay(Replay),
}

/// A replayable [`Buf`] of body data.
///
/// This storage is backed by cheaply cloneable [`Bytes`].
#[derive(Clone, Debug, Default)]
pub struct Replay {
    bufs: VecDeque<Bytes>,
}

// === impl BodyState ===

impl<B: Body> BodyState<B> {
    /// Records a chunk of data yielded by the inner `B`-typed [`Body`].
    ///
    /// This returns the next chunk of data as a chunk of [`Bytes`].
    ///
    /// This records the chunk in the replay buffer, unless the maximum capacity has been exceeded.
    /// If the buffer's capacity has been exceeded, the buffer will be emptied. The initial body
    /// will be permitted to continue, but cloned replays will fail with a
    /// [`Capped`][super::Capped] error when polled.
    pub(super) fn record_bytes(&mut self, mut data: B::Data) -> Data {
        let length = data.remaining();
        self.max_bytes = self.max_bytes.saturating_sub(length);

        let bytes = if self.is_capped() {
            // If there's data in the buffer, discard it now, since we won't
            // allow any clones to have a complete body.
            if self.replay.has_remaining() {
                tracing::debug!(
                    buf.size = self.replay.remaining(),
                    "Buffered maximum capacity, discarding buffer"
                );
                self.replay = Default::default();
            }
            data.copy_to_bytes(length)
        } else {
            // Buffer a clone of the bytes read on this poll.
            let bytes = data.copy_to_bytes(length);
            self.replay.bufs.push_back(bytes.clone());
            bytes
        };

        Data::Initial(bytes)
    }
}

// === impl Data ===

impl Buf for Data {
    #[inline]
    fn remaining(&self) -> usize {
        match self {
            Data::Initial(buf) => buf.remaining(),
            Data::Replay(replay) => replay.remaining(),
        }
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        match self {
            Data::Initial(buf) => buf.chunk(),
            Data::Replay(replay) => replay.chunk(),
        }
    }

    #[inline]
    fn chunks_vectored<'iovs>(&'iovs self, iovs: &mut [IoSlice<'iovs>]) -> usize {
        match self {
            Data::Initial(buf) => buf.chunks_vectored(iovs),
            Data::Replay(replay) => replay.chunks_vectored(iovs),
        }
    }

    #[inline]
    fn advance(&mut self, amt: usize) {
        match self {
            Data::Initial(buf) => buf.advance(amt),
            Data::Replay(replay) => replay.advance(amt),
        }
    }

    #[inline]
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        match self {
            Data::Initial(buf) => buf.copy_to_bytes(len),
            Data::Replay(replay) => replay.copy_to_bytes(len),
        }
    }
}

// === impl Replay ===

impl Buf for Replay {
    fn remaining(&self) -> usize {
        self.bufs.iter().map(Buf::remaining).sum()
    }

    fn chunk(&self) -> &[u8] {
        self.bufs.front().map(Buf::chunk).unwrap_or(&[])
    }

    fn chunks_vectored<'iovs>(&'iovs self, iovs: &mut [IoSlice<'iovs>]) -> usize {
        // Are there more than zero iovecs to write to?
        if iovs.is_empty() {
            return 0;
        }

        // Loop over the buffers in the replay buffer list, and try to fill as
        // many iovecs as we can from each buffer.
        let mut filled = 0;
        for buf in &self.bufs {
            filled += buf.chunks_vectored(&mut iovs[filled..]);
            if filled == iovs.len() {
                return filled;
            }
        }

        filled
    }

    fn advance(&mut self, mut amt: usize) {
        while amt > 0 {
            let rem = self.bufs[0].remaining();
            // If the amount to advance by is less than the first buffer in
            // the buffer list, advance that buffer's cursor by `amt`,
            // and we're done.
            if rem > amt {
                self.bufs[0].advance(amt);
                return;
            }

            // Otherwise, advance the first buffer to its end, and
            // continue.
            self.bufs[0].advance(rem);
            amt -= rem;

            self.bufs.pop_front();
        }
    }

    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        // If the length of the requested `Bytes` is <= the length of the front
        // buffer, we can just use its `copy_to_bytes` implementation (which is
        // just a reference count bump).
        match self.bufs.front_mut() {
            Some(first) if len <= first.remaining() => {
                let buf = first.copy_to_bytes(len);
                // If we consumed the first buffer, also advance our "cursor" by
                // popping it.
                if first.remaining() == 0 {
                    self.bufs.pop_front();
                }

                buf
            }
            _ => {
                assert!(len <= self.remaining(), "`len` greater than remaining");
                let mut buf = BytesMut::with_capacity(len);
                buf.put(self.take(len));
                buf.freeze()
            }
        }
    }
}
