//! Implement our traits {AddrInfo, SetKeepalive, Io} for tokio_rustls types.

use crate::transport::io::internal::Io;
use crate::transport::AddrInfo;
use bytes::Buf;
use futures::Poll;
use std::fmt::Debug;
use std::io;
use std::net::SocketAddr;
use tokio::io::AsyncWrite;

// impl server

impl<S> AddrInfo for tokio_rustls::server::TlsStream<S>
where
    S: AddrInfo + Debug,
{
    fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        self.get_ref().0.remote_addr()
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.get_ref().0.local_addr()
    }

    fn get_original_dst(&self) -> Option<SocketAddr> {
        self.get_ref().0.get_original_dst()
    }
}

impl<S> Io for tokio_rustls::server::TlsStream<S>
where
    S: Io + Debug,
{
    fn shutdown_write(&mut self) -> Result<(), io::Error> {
        self.get_mut().0.shutdown_write()
    }

    fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize, io::Error> {
        self.write_buf(&mut buf)
    }
}

// impl client

impl<S> AddrInfo for tokio_rustls::client::TlsStream<S>
where
    S: AddrInfo + Debug,
{
    fn remote_addr(&self) -> Result<SocketAddr, io::Error> {
        self.get_ref().0.remote_addr()
    }

    fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.get_ref().0.local_addr()
    }

    fn get_original_dst(&self) -> Option<SocketAddr> {
        self.get_ref().0.get_original_dst()
    }
}

impl<S> Io for tokio_rustls::client::TlsStream<S>
where
    S: Io + Debug,
{
    fn shutdown_write(&mut self) -> Result<(), io::Error> {
        self.get_mut().0.shutdown_write()
    }

    fn write_buf_erased(&mut self, mut buf: &mut dyn Buf) -> Poll<usize, io::Error> {
        self.write_buf(&mut buf)
    }
}
