use linkerd_app_core::{
    proxy::{api_resolve::Metadata, tcp::balance},
    NameAddr,
};
use std::net::SocketAddr;

pub mod concrete;
pub mod forward;
pub mod logical;

#[derive(Clone, Debug)]
pub enum Dispatch {
    Balance(NameAddr, balance::EwmaConfig),
    Forward(SocketAddr, Metadata),
}
