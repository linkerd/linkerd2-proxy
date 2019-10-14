use futures::{try_ready, Poll};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::reactor;

pub struct Listen {
    local_addr: SocketAddr,
    keepalive: Option<Duration>,
    state: State,
}

enum State {
    Init(Option<std::net::TcpListener>),
    Bound(tokio::net::TcpListener),
}

impl Listen {
    pub fn bind(addr: SocketAddr, keepalive: Option<Duration>) -> std::io::Result<Self> {
        let tcp = std::net::TcpListener::bind(addr)?;
        let local_addr = tcp.local_addr()?;
        Ok(Self {
            local_addr,
            keepalive,
            state: State::Init(Some(tcp)),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl linkerd2_proxy_core::listen::Listen for Listen {
    type Connection = (tokio::net::TcpStream, SocketAddr);
    type Error = std::io::Error;

    fn poll_accept(&mut self) -> Poll<Self::Connection, Self::Error> {
        loop {
            self.state = match self.state {
                State::Init(ref mut std) => {
                    // Create the TCP listener lazily, so that it's not bound to a
                    // reactor until the future is run. This will avoid
                    // `Handle::current()` creating a new thread for the global
                    // background reactor if `polled before the runtime is
                    // initialized.
                    tracing::trace!("listening on {}", self.local_addr);
                    let listener = tokio::net::TcpListener::from_std(
                        std.take().expect("illegal state"),
                        &reactor::Handle::current(),
                    )?;
                    State::Bound(listener)
                }
                State::Bound(ref mut listener) => {
                    tracing::trace!("accepting on {}", self.local_addr);
                    let (mut tcp, remote_addr) = try_ready!(listener.poll_accept());
                    // TODO: On Linux and most other platforms it would be better
                    // to set the `TCP_NODELAY` option on the bound socket and
                    // then have the listening sockets inherit it. However, that
                    // doesn't work on all platforms and also the underlying
                    // libraries don't have the necessary API for that, so just
                    // do it here.
                    super::set_nodelay_or_warn(&tcp);
                    super::set_keepalive_or_warn(&mut tcp, self.keepalive);
                    return Ok((tcp, remote_addr).into());
                }
            };
        }
    }
}
