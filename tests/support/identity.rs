use crate::support::*;

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
    pub client_config: Arc<rustls::ClientConfig>,
    pub server_config: Arc<rustls::ServerConfig>,
}

#[derive(Clone)]
pub struct Controller {
    expect_calls: Arc<Mutex<VecDeque<Certify>>>,
}

type Certify = Box<
    FnMut(
            pb::CertifyRequest,
        )
            -> Box<Future<Item = grpc::Response<pb::CertifyResponse>, Error = grpc::Status> + Send>
        + Send,
>;

const TLS_VERSIONS: &[rustls::ProtocolVersion] = &[rustls::ProtocolVersion::TLSv1_2];

struct Certificates {
    pub leaf: Vec<u8>,
    pub intermediates: Vec<Vec<u8>>,
}

impl Certificates {
    pub fn load<P>(p: P) -> Result<Certificates, io::Error>
    where
        P: AsRef<Path>,
    {
        let f = fs::File::open(p)?;
        let mut r = io::BufReader::new(f);
        let certs = rustls::internal::pemfile::certs(&mut r)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "rustls error reading certs"))?;
        let leaf = certs
            .get(0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no certs in pemfile"))?
            .as_ref()
            .into();
        let intermediates = certs[1..].iter().map(|i| i.as_ref().into()).collect();

        Ok(Certificates {
            leaf,
            intermediates,
        })
    }

    pub fn chain(&self) -> Vec<rustls::Certificate> {
        let mut chain = Vec::with_capacity(self.intermediates.len() + 1);
        chain.push(self.leaf.clone());
        chain.extend(self.intermediates.clone());
        chain.into_iter().map(rustls::Certificate).collect()
    }

    pub fn response(&self) -> pb::CertifyResponse {
        pb::CertifyResponse {
            leaf_certificate: self.leaf.clone(),
            intermediate_certificates: self.intermediates.clone(),
            ..Default::default()
        }
    }
}

impl Identity {
    fn load_key<P>(p: P) -> rustls::PrivateKey
    where
        P: AsRef<Path>,
    {
        let p8 = fs::read(&p).expect("read key");
        rustls::PrivateKey(p8)
    }

    fn configs(
        trust_anchors: &str,
        certs: &Certificates,
        key: rustls::PrivateKey,
    ) -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
        use std::io::Cursor;
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add_pem_file(&mut Cursor::new(trust_anchors))
            .expect("add pem file");

        let mut client_config = rustls::ClientConfig::new();
        client_config.root_store = roots;

        let mut server_config = rustls::ServerConfig::new(
            rustls::AllowAnyAnonymousOrAuthenticatedClient::new(client_config.root_store.clone()),
        );

        server_config.versions = TLS_VERSIONS.to_vec();
        server_config
            .set_single_cert(certs.chain(), key)
            .expect("set server resover");

        (Arc::new(client_config), Arc::new(server_config))
    }

    pub fn new(dir: &'static str, local_name: String) -> Self {
        let (id_dir, token, trust_anchors, certs, key) = {
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
            let certs = Certificates::load(&id).expect("read cert");

            id.set_file_name("key.p8");
            let key = Identity::load_key(&id);

            (id_dir, token, trust_anchors, certs, key)
        };

        let certify_rsp = certs.response();
        let (client_config, server_config) = Identity::configs(&trust_anchors, &certs, key);
        let mut env = app::config::TestEnv::new();

        env.put(app::config::ENV_IDENTITY_DIR, id_dir);
        env.put(app::config::ENV_IDENTITY_TOKEN_FILE, token);
        env.put(app::config::ENV_IDENTITY_TRUST_ANCHORS, trust_anchors);
        env.put(app::config::ENV_IDENTITY_IDENTITY_LOCAL_NAME, local_name);

        Self {
            env,
            certify_rsp,
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
        self.certify_async(move |req| Ok::<_, grpc::Status>(f(req)))
    }

    pub fn certify_async<F, U>(self, f: F) -> Self
    where
        F: FnOnce(pb::CertifyRequest) -> U + Send + 'static,
        U: IntoFuture<Item = pb::CertifyResponse> + Send + 'static,
        U::Future: Send + 'static,
        <U::Future as Future>::Error: fmt::Display + Send,
    {
        let mut f = Some(f);
        let func: Certify = Box::new(move |req| {
            // It's a shame how `FnBox` isn't actually a thing yet, otherwise this
            // closure could be one (and not a `FnMut`).
            let f = f.take().expect("called twice?");
            let fut = f(req)
                .into_future()
                .map(grpc::Response::new)
                .map_err(|e| grpc::Status::new(grpc::Code::Internal, format!("{}", e)));
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
        if let Some(mut f) = self.expect_calls.lock().unwrap().pop_front() {
            return f(req.into_inner());
        }

        Box::new(future::err(grpc::Status::new(
            grpc::Code::Unavailable,
            "unit test identity service has no results",
        )))
    }
}
