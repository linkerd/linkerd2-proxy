use super::*;
use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use linkerd2_proxy_api::identity as pb;
use tokio_rustls::rustls;
use tonic as grpc;

pub struct Identity {
    pub env: TestEnv,
    pub certify_rsp: pb::CertifyResponse,
    pub client_config: Arc<rustls::ClientConfig>,
    pub server_config: Arc<rustls::ServerConfig>,
}

#[derive(Clone, Default)]
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

static TLS_VERSIONS: &[&rustls::SupportedProtocolVersion] = &[&rustls::version::TLS13];
static TLS_SUPPORTED_CIPHERSUITES: &[rustls::SupportedCipherSuite] =
    &[rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256];

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
        let mut certs = rustls_pemfile::certs(&mut r)
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "rustls error reading certs"))?;
        let mut certs = certs.drain(..);
        let leaf = certs.next().expect("no leaf cert in pemfile");
        let intermediates = certs.collect();

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
        let trust_anchors =
            rustls_pemfile::certs(&mut Cursor::new(trust_anchors)).expect("error parsing pemfile");
        let (added, skipped) = roots.add_parsable_certificates(&trust_anchors[..]);
        assert_ne!(added, 0, "trust anchors must include at least one cert");
        assert_eq!(skipped, 0, "no certs in pemfile should be invalid");

        let client_config = rustls::ClientConfig::builder()
            .with_cipher_suites(TLS_SUPPORTED_CIPHERSUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(TLS_VERSIONS)
            .expect("client config must be valid")
            .with_root_certificates(roots.clone())
            .with_no_client_auth();

        let server_config = rustls::ServerConfig::builder()
            .with_cipher_suites(TLS_SUPPORTED_CIPHERSUITES)
            .with_safe_default_kx_groups()
            .with_protocol_versions(TLS_VERSIONS)
            .expect("server config must be valid")
            .with_client_cert_verifier(rustls::server::AllowAnyAnonymousOrAuthenticatedClient::new(
                roots,
            ))
            .with_single_cert(certs.chain(), key)
            .unwrap();

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
            id.push("src");
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
        let mut env = TestEnv::default();

        env.put(app::env::ENV_IDENTITY_DIR, id_dir);
        env.put(app::env::ENV_IDENTITY_TOKEN_FILE, token);
        env.put(app::env::ENV_IDENTITY_TRUST_ANCHORS, trust_anchors);
        env.put(app::env::ENV_IDENTITY_IDENTITY_LOCAL_NAME, local_name);

        Self {
            env,
            certify_rsp,
            client_config,
            server_config,
        }
    }

    pub fn service(&self) -> Controller {
        let rsp = self.certify_rsp.clone();
        Controller::default().certify(move |_req| {
            let expiry = SystemTime::now() + Duration::from_secs(666);
            pb::CertifyResponse {
                valid_until: Some(expiry.into()),
                ..rsp
            }
        })
    }
}

impl Controller {
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
        tracing::debug!("running support identity service");
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
