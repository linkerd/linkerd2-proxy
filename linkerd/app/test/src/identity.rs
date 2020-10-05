use linkerd2_proxy_identity::*;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

const TLS_VERSIONS: &[rustls::ProtocolVersion] = &[rustls::ProtocolVersion::TLSv1_2];
pub struct Identity {
    pub id_dir: PathBuf,
    pub token_path: PathBuf,
    pub trust_anchors: String,
    pub certs: Certificates,
    pub client_config: Arc<rustls::ClientConfig>,
    pub server_config: Arc<rustls::ServerConfig>,
    pub key: Vec<u8>,
    pub csr: Vec<u8>,
    _p: (),
}

pub struct Certificates {
    pub leaf: Vec<u8>,
    pub intermediates: Vec<Vec<u8>>,
    _p: (),
}

impl Identity {
    pub fn new(dir: &'static str) -> Self {
        let (id_dir, token_path, trust_anchors, certs, key, csr) = {
            let mut id = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            id.push("src");
            id.push("data");

            id.push("ca1.pem");
            let trust_anchors = fs::read_to_string(&id).expect("read trust anchors");

            id.set_file_name(dir);
            let id_dir = id.clone();

            id.push("token.txt");
            let token = id.clone();

            id.set_file_name("ca1-cert.pem");
            let certs = Certificates::load(&id).expect("read cert");

            id.set_file_name("key.p8");
            let key = fs::read(&id).expect("read key");

            id.set_file_name("csr.der");
            let csr = fs::read(&id).expect("read CSR");

            (id_dir, token, trust_anchors, certs, key, csr)
        };

        let (client_config, server_config) =
            configs(&trust_anchors, &certs, rustls::PrivateKey(key.clone()));

        Self {
            id_dir,
            token_path,
            trust_anchors,
            certs,
            client_config,
            server_config,
            key,
            csr,
            _p: (),
        }
    }

    pub fn config(&self, local_name: &[u8]) -> certify::Config {
        let token =
            TokenSource::if_nonempty_file(self.token_path.clone().to_string_lossy().to_string())
                .expect("token exists");
        certify::Config {
            trust_anchors: TrustAnchors::from_pem(self.trust_anchors.as_ref())
                .expect("trust anchors are valid"),
            key: Key::from_pkcs8(&self.key[..]).expect("key is valid"),
            csr: Csr::from_der(self.csr.clone()).expect("CSR is okay"),
            local_name: Name::from_hostname(local_name).expect("hostname is valid"),
            min_refresh: Duration::from_secs(1),
            max_refresh: Duration::from_secs(4096),
            token,
        }
    }

    pub fn local_identity(&self, local_name: &[u8]) -> Local {
        let config = self.config(local_name);
        let crt = Crt::new(
            Name::from_hostname(local_name).expect("hostname is valid"),
            self.certs.leaf.clone(),
            self.certs.intermediates.clone(),
            SystemTime::now() + Duration::from_secs(4092),
        );
        let crt_key = config
            .trust_anchors
            .certify(config.key.clone(), crt)
            .expect("crt is okay");
        let (local, sender) = Local::new(&self.config(local_name));
        sender.broadcast(Some(crt_key)).expect("rx not dropped");
        local
    }
}

impl Certificates {
    fn load<P>(p: P) -> Result<Certificates, io::Error>
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

        Ok(Self {
            leaf,
            intermediates,
            _p: (),
        })
    }

    fn chain(&self) -> Vec<rustls::Certificate> {
        let mut chain = Vec::with_capacity(self.intermediates.len() + 1);
        chain.push(self.leaf.clone());
        chain.extend(self.intermediates.clone());
        chain.into_iter().map(rustls::Certificate).collect()
    }
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
