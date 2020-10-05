use super::*;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use linkerd2_app_test::identity as app_test;
use linkerd2_proxy_api::identity as pb;
use tonic as grpc;

pub struct Identity {
    pub env: TestEnv,
    pub certify_rsp: pb::CertifyResponse,
    pub client_config: Arc<rustls::ClientConfig>,
    pub server_config: Arc<rustls::ServerConfig>,
}

#[derive(Clone)]
pub struct Controller {
    expect_calls: Arc<Mutex<VecDeque<Certify>>>,
}

type Certify = Box<
    dyn FnMut(
            pb::CertifyRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<grpc::Response<pb::CertifyResponse>, grpc::Status>>
                    + Send,
            >,
        > + Send,
>;

impl Identity {
    pub fn new(dir: &'static str, local_name: String) -> Self {
        let app_test::Identity {
            id_dir,
            token_path,
            trust_anchors,
            certs:
                app_test::Certificates {
                    leaf,
                    intermediates,
                    ..
                },
            client_config,
            server_config,
            ..
        } = app_test::Identity::new(dir);
        let path_to_string = |path: PathBuf| path.into_os_string().into_string().unwrap();
        let mut env = TestEnv::new();

        env.put(app::env::ENV_IDENTITY_DIR, path_to_string(id_dir));
        env.put(
            app::env::ENV_IDENTITY_TOKEN_FILE,
            path_to_string(token_path),
        );
        env.put(app::env::ENV_IDENTITY_TRUST_ANCHORS, trust_anchors);
        env.put(app::env::ENV_IDENTITY_IDENTITY_LOCAL_NAME, local_name);

        Self {
            env,
            certify_rsp: pb::CertifyResponse {
                leaf_certificate: leaf,
                intermediate_certificates: intermediates,
                ..Default::default()
            },
            client_config,
            server_config,
        }
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
        F: FnOnce(pb::CertifyRequest) -> pb::CertifyResponse + Send + 'static,
    {
        self.certify_async(move |req| async { Ok::<_, grpc::Status>(f(req)) })
    }

    pub fn certify_async<F, U>(self, f: F) -> Self
    where
        F: FnOnce(pb::CertifyRequest) -> U + Send + 'static,
        U: TryFuture<Ok = pb::CertifyResponse> + Send + 'static,
        U::Error: fmt::Display + Send,
    {
        let mut f = Some(f);
        let func: Certify = Box::new(move |req| {
            // It's a shame how `FnBox` isn't actually a thing yet, otherwise this
            // closure could be one (and not a `FnMut`).
            let f = f.take().expect("called twice?");
            let fut = f(req)
                .map_ok(grpc::Response::new)
                .map_err(|e| grpc::Status::new(grpc::Code::Internal, format!("{}", e)));
            Box::pin(fut)
        });
        self.expect_calls.lock().unwrap().push_back(func);
        self
    }

    pub async fn run(self) -> controller::Listening {
        println!("running support identity service");
        controller::run(
            pb::identity_server::IdentityServer::new(self),
            "support identity service",
            None,
        )
        .await
    }
}

#[tonic::async_trait]
impl pb::identity_server::Identity for Controller {
    async fn certify(
        &self,
        req: grpc::Request<pb::CertifyRequest>,
    ) -> Result<grpc::Response<pb::CertifyResponse>, grpc::Status> {
        let f = self
            .expect_calls
            .lock()
            .unwrap()
            .pop_front()
            .map(|mut f| f(req.into_inner()));
        if let Some(f) = f {
            return f.await;
        }

        Err(grpc::Status::new(
            grpc::Code::Unavailable,
            "unit test identity service has no results",
        ))
    }
}
