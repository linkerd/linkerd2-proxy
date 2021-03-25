pub use crate::app_core::transport::*;

#[derive(Clone, Debug)]
pub struct NoGetAddrs;

impl<T> listen::GetAddrs<T> for NoGetAddrs {
    type Addrs = listen::Addrs;
    fn addrs(&self, _: &T) -> std::io::Result<Self::Addrs> {
        panic!("GetAddrs should not be called in this test")
    }
}
