use tokio::io::{AsyncRead, AsyncWrite};

use super::Source;

/// Wraps serverside transports with additional functionality.
pub trait Accept<T: AsyncRead + AsyncWrite> {
    type Io: AsyncRead + AsyncWrite;

    fn accept(&self, source: &Source, inner: T) -> Self::Io;
}

/// Create a new builder with only an "identity" layer.
pub fn builder() -> Builder<()> {
    Builder { accept: () }
}

/// Chains multiple `Accept`s together in onion-y layers.
#[derive(Clone, Debug)]
pub struct Builder<L> {
    accept: L,
}

impl<L> Builder<L> {
    /// Add a layer to this Accept builder.
    ///
    /// This layer will be applied to connections *before* the
    /// previously added layers in this builder.
    pub fn and_then<T>(self, layer: T) -> Builder<Stack<T, L>> {
        Builder {
            accept: Stack {
                inner: layer,
                outer: self.accept,
            },
        }
    }
}

impl<L, T> Accept<T> for Builder<L>
where
    L: Accept<T>,
    T: AsyncRead + AsyncWrite,
{
    type Io = L::Io;

    fn accept(&self, source: &Source, io: T) -> Self::Io {
        self.accept.accept(source, io)
    }
}

/// The identity `Accept`.
impl<T> Accept<T> for ()
where
    T: AsyncRead + AsyncWrite,
{
    type Io = T;

    fn accept(&self, _: &Source, inner: T) -> T {
        inner
    }
}

/// An `Accept` combinator.
#[derive(Clone, Debug)]
pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer, T> Accept<T> for Stack<Inner, Outer>
where
    Inner: Accept<T>,
    Outer: Accept<Inner::Io>,
    T: AsyncRead + AsyncWrite,
{
    type Io = Outer::Io;

    fn accept(&self, source: &Source, io: T) -> Self::Io {
        let io = self.inner.accept(source, io);
        self.outer.accept(source, io)
    }
}
