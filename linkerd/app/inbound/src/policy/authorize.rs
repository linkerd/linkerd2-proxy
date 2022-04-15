mod http;
mod tcp;

pub use self::{http::NewAuthorizeHttp, tcp::NewAuthorizeTcp};
