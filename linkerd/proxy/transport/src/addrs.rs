use std::{
    fmt, io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
};

/// The address of a client.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientAddr(pub SocketAddr);

/// The address for a listener to bind on.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ListenAddr(pub SocketAddr);

/// The address of a server.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerAddr(pub SocketAddr);

/// An SO_ORIGINAL_DST address.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct OrigDstAddr(pub SocketAddr);

/// Wraps an address type to indicate it describes an address describing this
/// process.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Local<T>(pub T);

/// Wraps an address type to indicate it describes another process.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Remote<T>(pub T);

// === impl ClientAddr ===

impl std::ops::Deref for ClientAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl From<ClientAddr> for SocketAddr {
    fn from(ClientAddr(addr): ClientAddr) -> SocketAddr {
        addr
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ClientAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

// === impl ListenAddr ===

impl std::ops::Deref for ListenAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl From<ListenAddr> for SocketAddr {
    fn from(ListenAddr(addr): ListenAddr) -> SocketAddr {
        addr
    }
}

impl ToSocketAddrs for ListenAddr {
    type Iter = std::option::IntoIter<SocketAddr>;

    fn to_socket_addrs(&self) -> io::Result<Self::Iter> {
        Ok(Some(self.0).into_iter())
    }
}

impl fmt::Display for ListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ListenAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

// === impl ServerAddr ===

impl std::ops::Deref for ServerAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl From<ServerAddr> for SocketAddr {
    fn from(ServerAddr(addr): ServerAddr) -> SocketAddr {
        addr
    }
}

impl fmt::Display for ServerAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ServerAddr {
    pub fn ip(&self) -> IpAddr {
        self.0.ip()
    }

    pub fn port(&self) -> u16 {
        self.0.port()
    }
}

// === impl OrigDstAddr ===

impl std::ops::Deref for OrigDstAddr {
    type Target = SocketAddr;

    fn deref(&self) -> &SocketAddr {
        &self.0
    }
}

impl From<OrigDstAddr> for SocketAddr {
    fn from(OrigDstAddr(addr): OrigDstAddr) -> SocketAddr {
        addr
    }
}

impl fmt::Display for OrigDstAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl Local ===

impl<T: std::ops::Deref> std::ops::Deref for Local<T> {
    type Target = T::Target;

    fn deref(&self) -> &T::Target {
        self.0.deref()
    }
}

impl<T: Into<SocketAddr>> From<Local<T>> for SocketAddr {
    fn from(Local(addr): Local<T>) -> SocketAddr {
        addr.into()
    }
}

impl<T: fmt::Display> fmt::Display for Local<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// === impl Remote ===

impl<T: std::ops::Deref> std::ops::Deref for Remote<T> {
    type Target = T::Target;

    fn deref(&self) -> &T::Target {
        self.0.deref()
    }
}

impl<T: Into<SocketAddr>> From<Remote<T>> for SocketAddr {
    fn from(Remote(addr): Remote<T>) -> SocketAddr {
        addr.into()
    }
}

impl<T: fmt::Display> fmt::Display for Remote<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
