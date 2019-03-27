use support::*;

use std::{
    collections::{VecDeque},
    sync::{Arc, Mutex},
};

use linkerd2_proxy_api::identity as pb;

#[derive(Clone)]
pub struct Identity {
    expect_calls: Arc<Mutex<VecDeque<Certify>>>,
}

pub struct Listening {
    pub addr: SocketAddr,
    shutdown: super::Shutdown,
}

type Certify = Box<
    Fn(pb::CertifyRequest) -> Box<
        Future<Item=grpc::Response<pb::CertifyResponse>, Error=grpc::Status> + Send
    >
+ Send>;

impl Identity {

    pub fn certify<F>(self, f: F) -> Self
    where
        F: Fn(pb::CertifyRequest) -> pb::CertifyResponse + Send + 'static,
    {
        self.certify_async(move |req| Ok::<_, grpc::Status>(f(req)))
    }

    pub fn certify_async<F, U>(self, f: F) -> Self
    where
        F: Fn(pb::CertifyRequest) -> U + Send + 'static,
        U: IntoFuture<Item = pb::CertifyResponse> + Send + 'static,
        U::Future: Send + 'static,
        <U::Future as Future>::Error: Into<grpc::Status> + Send,
    {
        let func: Certify = Box::new(move |req| {
            let fut = f(req).into_future()
                .map(grpc::Response::new)
                .map_err(Into::into);
            Box::new(fut)
        });
        self.expect_calls.lock().unwrap().push_back(func);
        self
    }

    pub fn run(self) -> super::Listening {
        super::run(pb::server::IdentityServer::new(self), "support identity service", None)
    }
}

impl pb::server::Identity for Identity {
    type CertifyFuture = Box<Future<Item = grpc::Response<pb::CertifyResponse>, Error = grpc::Status> + Send>;
    fn certify(&mut self, req: grpc::Request<pb::CertifyRequest>) -> Self::CertifyFuture {
        if let Some(f) = self.expect_calls.lock().unwrap().pop_front() {
            return f(req.into_inner());
        }

        Box::new(future::err(grpc::Status::new(
            grpc::Code::Unavailable,
            "unit test identity service has no results",
        )))
    }
}
