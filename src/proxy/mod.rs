//! Tools for building a transparent TCP/HTTP proxy.

use tokio::io::{AsyncRead, AsyncWrite};

pub mod buffer;
pub mod http;
pub mod limit;
mod protocol;
mod reconnect;
pub mod resolve;
pub mod server;
mod tcp;
pub mod timeout;

pub use self::reconnect::Reconnect;
pub use self::resolve::{Resolve, Resolution};
pub use self::server::{Server, Source};

/// Wraps serverside transports with additional functionality.
pub trait Accept<T: AsyncRead + AsyncWrite> {
    type Io: AsyncRead + AsyncWrite;

    fn accept(&self, inner: T) -> Self::Io;
}

/// The identity `Accept`.
impl<T> Accept<T> for ()
where
    T: AsyncRead + AsyncWrite,
{
    type Io = T;

    #[inline]
    fn accept(&self, inner: T) -> T {
        inner
    }
}
