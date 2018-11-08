pub mod connect;
mod connection;
mod addr_info;
mod io;
pub mod metrics;
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
    connect::Connect,
    connection::{
        BoundPort,
        Connection,
        Peek,
    },
    io::BoxedIo,
};
