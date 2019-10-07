use tokio::io::{AsyncRead, AsyncWrite};

use super::Source;

/// Wraps serverside transports with additional functionality.
pub trait WrapServerTransport<T: AsyncRead + AsyncWrite> {
    type Io: AsyncRead + AsyncWrite;

    fn wrap_server_transport(&self, source: &Source, inner: T) -> Self::Io;
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
    pub fn push<O>(self, outer: O) -> Builder<Stack<L, O>> {
        Builder {
            accept: Stack {
                inner: self.accept,
                outer,
            },
        }
    }
}

impl<L, T> WrapServerTransport<T> for Builder<L>
where
    L: WrapServerTransport<T>,
    T: AsyncRead + AsyncWrite,
{
    type Io = L::Io;

    fn wrap_server_transport(&self, source: &Source, io: T) -> Self::Io {
        self.accept.wrap_server_transport(source, io)
    }
}

/// The identity `Accept`.
impl<T> WrapServerTransport<T> for ()
where
    T: AsyncRead + AsyncWrite,
{
    type Io = T;

    fn wrap_server_transport(&self, _: &Source, inner: T) -> T {
        inner
    }
}

/// An `Accept` combinator.
#[derive(Clone, Debug)]
pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer, T> WrapServerTransport<T> for Stack<Inner, Outer>
where
    Inner: WrapServerTransport<T>,
    Outer: WrapServerTransport<Inner::Io>,
    T: AsyncRead + AsyncWrite,
{
    type Io = Outer::Io;

    fn wrap_server_transport(&self, source: &Source, io: T) -> Self::Io {
        let io = self.inner.wrap_server_transport(source, io);
        self.outer.wrap_server_transport(source, io)
    }
}
