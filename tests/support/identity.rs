use support::*;

use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use linkerd2_proxy_api::identity as pb;

pub struct Identity {
    pub env: app::config::TestEnv,
    pub certify_rsp: pb::CertifyResponse,
}

#[derive(Clone)]
pub struct Controller {
    expect_calls: Arc<Mutex<VecDeque<Certify>>>,
}

type Certify = Box<
    Fn(
            pb::CertifyRequest,
        )
            -> Box<Future<Item = grpc::Response<pb::CertifyResponse>, Error = grpc::Status> + Send>
        + Send,
>;

pub fn rsp_from_cert<P>(p: P) -> Result<pb::CertifyResponse, io::Error>
where
    P: AsRef<Path>,
{
    let f = fs::File::open(p)?;
    let mut r = io::BufReader::new(f);
    let certs = rustls::internal::pemfile::certs(&mut r)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "rustls error reading certs"))?;
    let leaf_certificate = certs
        .get(0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no certs in pemfile"))?
        .as_ref()
        .into();
    let intermediate_certificates = certs[1..].iter().map(|i| i.as_ref().into()).collect();
    let rsp = pb::CertifyResponse {
        leaf_certificate,
        intermediate_certificates,
        ..Default::default()
    };
    Ok(rsp)
}

impl Identity {
    pub fn new(dir: &'static str, local_name: String) -> Self {
        let (id_dir, token, trust_anchors, certify_rsp) = {
            let path_to_string = |path: &PathBuf| {
                path.as_path()
                    .to_owned()
                    .into_os_string()
                    .into_string()
                    .unwrap()
            };
            let mut id = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            id.push("tests");
            id.push("support");
            id.push("data");

            id.push("ca1.pem");
            let trust_anchors = fs::read_to_string(&id).expect("read trust anchors");

            id.set_file_name(dir);
            let id_dir = path_to_string(&id);

            id.push("token.txt");
            let token = path_to_string(&id);

            id.set_file_name("ca1-cert.pem");
            let rsp = rsp_from_cert(&id).expect("read cert");

            (id_dir, token, trust_anchors, rsp)
        };

        let mut env = app::config::TestEnv::new();

        env.put(app::config::ENV_IDENTITY_DIR, id_dir);
        env.put(app::config::ENV_IDENTITY_TOKEN_FILE, token);
        env.put(app::config::ENV_IDENTITY_TRUST_ANCHORS, trust_anchors);
        env.put(
            app::config::ENV_IDENTITY_IDENTITY_LOCAL_NAME,
            local_name,
        );

        Self { env, certify_rsp }
    }

    pub fn service(&self) -> Controller {
        let rsp = self.certify_rsp.clone();
        Controller::new().certify(move |_req| {
            let expiry = SystemTime::now() + Duration::from_secs(666);
            pb::CertifyResponse {
                valid_until: Some(expiry.into()),
                ..rsp.clone()
            }
        })
    }
}

impl Controller {
    pub fn new() -> Self {
        Self {
            expect_calls: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

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
            let fut = f(req)
                .into_future()
                .map(grpc::Response::new)
                .map_err(Into::into);
            Box::new(fut)
        });
        self.expect_calls.lock().unwrap().push_back(func);
        self
    }

    pub fn run(self) -> controller::Listening {
        println!("running support identity service");
        controller::run(
            pb::server::IdentityServer::new(self),
            "support identity service",
            None,
        )
    }
}

impl pb::server::Identity for Controller {
    type CertifyFuture =
        Box<Future<Item = grpc::Response<pb::CertifyResponse>, Error = grpc::Status> + Send>;
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
