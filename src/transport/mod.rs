mod connect;
mod connection;
mod addr_info;
mod io;
mod prefixed;
pub mod tls;

#[cfg(test)]
mod connection_tests;

pub use self::{
    addr_info::{
        AddrInfo,
        GetOriginalDst,
        SoOriginalDst
    },
    connect::{
        Connect,
        DnsNameAndPort, Host, HostAndPort, HostAndPortError,
        LookupAddressAndConnect,
    },
    connection::{
        BoundPort,
        Connection,
        Peek,
    },
    io::BoxedIo,
};
