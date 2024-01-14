use futures::prelude::*;
use linkerd_error::{Error, Recover, Result};
use linkerd_exp_backoff::{ExponentialBackoff, ExponentialBackoffStream};
use linkerd_identity::DerX509;
use linkerd_identity::{Credentials, Id};
use linkerd_proxy_http as http;
use linkerd_tonic_watch::StreamWatch;
use spiffe_proto::client::{
    self as api, spiffe_workload_api_client::SpiffeWorkloadApiClient as Client,
};
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};
use tower::Service;
use tracing::error;

const SPIFFE_HEADER_KEY: &str = "workload.spiffe.io";
const SPIFFE_HEADER_VALUE: &str = "true";

#[derive(Debug, thiserror::Error)]
#[error("no matching SVID found")]
pub struct NoMatchingSVIDFound(());

#[derive(Clone)]
pub struct Svid {
    pub(super) spiffe_id: Id,
    leaf: DerX509,
    private_key: Vec<u8>,
    intermediates: Vec<DerX509>,
}

#[derive(Clone)]
pub struct SvidUpdate {
    svids: HashMap<Id, Svid>,
}

#[derive(Clone, Debug)]
pub struct Api<S> {
    client: Client<S>,
}

#[derive(Clone)]
pub struct GrpcRecover(ExponentialBackoff);

pub type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

// === impl Svid ===

impl SvidUpdate {
    pub(super) fn new(svids: Vec<Svid>) -> Self {
        let mut svids_map = HashMap::default();
        for svid in svids.into_iter() {
            svids_map.insert(svid.spiffe_id.clone(), svid);
        }

        SvidUpdate { svids: svids_map }
    }
}

// === impl Svid ===

impl Svid {
    #[cfg(test)]
    pub(super) fn new(
        spiffe_id: Id,
        leaf: DerX509,
        private_key: Vec<u8>,
        intermediates: Vec<DerX509>,
    ) -> Self {
        Self {
            spiffe_id,
            leaf,
            private_key,
            intermediates,
        }
    }
}

impl TryFrom<api::X509svid> for Svid {
    // TODO: Use bundles from response to compare against
    // what is provided at bootstrap time

    type Error = Error;
    fn try_from(proto: api::X509svid) -> Result<Self, Self::Error> {
        if proto.x509_svid_key.is_empty() {
            return Err("empty private key".into());
        }

        let cert_der_blocks = asn1::from_der(&proto.x509_svid)?;
        let (leaf, intermediates) = match cert_der_blocks.split_first() {
            None => return Err("empty cert chain".into()),
            Some((leaf_block, intermediates_block)) => {
                let leaf = DerX509(asn1::to_der(leaf_block)?);
                let mut intermediates = vec![];
                for block in intermediates_block.iter() {
                    let cert_der = asn1::to_der(block)?;
                    intermediates.push(DerX509(cert_der));
                }
                (leaf, intermediates)
            }
        };

        let spiffe_id = Id::parse_uri(&proto.spiffe_id)?;

        Ok(Svid {
            spiffe_id,
            leaf,
            private_key: proto.x509_svid_key,
            intermediates: intermediates.to_vec(),
        })
    }
}

// === impl Api ===

impl<S> Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody> + Clone,
    S::Error: Into<Error>,
    S::ResponseBody: Default + http::HttpBody<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as http::HttpBody>::Error: Into<Error> + Send,
{
    pub fn watch(client: S, backoff: ExponentialBackoff) -> Watch<S> {
        let client = Client::new(client);
        StreamWatch::new(GrpcRecover(backoff), Self { client })
    }
}

impl<S> Service<()> for Api<S>
where
    S: tonic::client::GrpcService<tonic::body::BoxBody> + Clone,
    S: Clone + Send + Sync + 'static,
    S::ResponseBody: Default + http::HttpBody<Data = tonic::codegen::Bytes> + Send + 'static,
    <S::ResponseBody as http::HttpBody>::Error: Into<Error> + Send,
    S::Future: Send + 'static,
{
    type Response =
        tonic::Response<futures::stream::BoxStream<'static, Result<SvidUpdate, tonic::Status>>>;
    type Error = tonic::Status;
    type Future = futures::future::BoxFuture<'static, Result<Self::Response, tonic::Status>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: ()) -> Self::Future {
        let req = api::X509svidRequest {};
        let mut client = self.client.clone();
        Box::pin(async move {
            let parsed_header = SPIFFE_HEADER_VALUE
                .parse()
                .map_err(|e| tonic::Status::internal(format!("Failed to parse header: {}", e)))?;

            let mut req = tonic::Request::new(req);
            req.metadata_mut().insert(SPIFFE_HEADER_KEY, parsed_header);

            let rsp = client.fetch_x509svid(req).await?;
            Ok(rsp.map(|svids| {
                svids
                    .map_ok(move |s| {
                        let svids = s
                            .svids
                            .into_iter()
                            .filter_map(|proto| {
                                proto
                                    .try_into()
                                    .map_err(|err| error!("could not parse SVID: {}", err))
                                    .ok()
                            })
                            .collect();

                        SvidUpdate::new(svids)
                    })
                    .boxed()
            }))
        })
    }
}

// === impl GrpcRecover ===

impl Recover<tonic::Status> for GrpcRecover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, status: tonic::Status) -> Result<Self::Backoff, tonic::Status> {
        // Non retriable conditions described in:
        // https://github.com/spiffe/spiffe/blob/a5b6456ff1bcdb6935f61ed7f83e8ee533a325a3/standards/SPIFFE_Workload_API.md#client-state-machine
        if status.code() == tonic::Code::InvalidArgument {
            return Err(status);
        }

        tracing::warn!(
            grpc.status = %status.code(),
            grpc.message = status.message(),
            "Unexpected SPIRE Workload API response; retrying with a backoff",
        );
        Ok(self.0.stream())
    }
}

pub(super) fn process_svid<C>(credentials: &mut C, mut update: SvidUpdate, id: &Id) -> Result<()>
where
    C: Credentials,
{
    if let Some(svid) = update.svids.remove(id) {
        use x509_parser::prelude::*;

        let (_, parsed_cert) = X509Certificate::from_der(&svid.leaf.0)?;
        let exp: u64 = parsed_cert.validity().not_after.timestamp().try_into()?;
        let exp = UNIX_EPOCH + Duration::from_secs(exp);

        return credentials.set_certificate(svid.leaf, svid.intermediates, svid.private_key, exp);
    }

    Err(NoMatchingSVIDFound(()).into())
}

#[cfg(test)]
mod tests {
    use crate::api::Svid;
    use rcgen::{Certificate, CertificateParams, SanType};
    use spiffe_proto::client as api;

    fn gen_svid_pb(id: String, subject_alt_names: Vec<SanType>) -> api::X509svid {
        let mut params = CertificateParams::default();
        params.subject_alt_names = subject_alt_names;
        let cert = Certificate::from_params(params).expect("should generate cert");

        api::X509svid {
            spiffe_id: id,
            x509_svid: cert.serialize_der().expect("should serialize"),
            x509_svid_key: cert.serialize_private_key_der(),
            bundle: Vec::default(),
        }
    }

    #[test]
    fn can_parse_valid_proto() {
        let id = "spiffe://some-domain/some-workload";
        let svid_pb = gen_svid_pb(id.into(), vec![SanType::URI(id.into())]);
        assert!(Svid::try_from(svid_pb).is_ok());
    }

    #[test]
    fn cannot_parse_non_spiffe_id() {
        let id = "some-domain.some-workload";
        let svid_pb = gen_svid_pb(id.into(), vec![SanType::DnsName(id.into())]);
        assert!(Svid::try_from(svid_pb).is_err());
    }

    #[test]
    fn cannot_parse_empty_cert() {
        let id = "spiffe://some-domain/some-workload";
        let mut svid_pb = gen_svid_pb(id.into(), vec![SanType::URI(id.into())]);
        svid_pb.x509_svid = Vec::default();
        assert!(Svid::try_from(svid_pb).is_err());
    }

    #[test]
    fn cannot_parse_empty_key() {
        let id = "spiffe://some-domain/some-workload";
        let mut svid_pb = gen_svid_pb(id.into(), vec![SanType::URI(id.into())]);
        svid_pb.x509_svid_key = Vec::default();
        assert!(Svid::try_from(svid_pb).is_err());
    }
}
