use futures::prelude::*;
use linkerd_error::{Error, Recover, Result};
use linkerd_exp_backoff::{ExponentialBackoff, ExponentialBackoffStream};
use linkerd_identity::DerX509;
use linkerd_identity::Id;
use linkerd_proxy_http as http;
use linkerd_tonic_watch::StreamWatch;
use spiffe_proto::client::{
    self as api, spiffe_workload_api_client::SpiffeWorkloadApiClient as Client,
};
use std::collections::HashMap;
use tower::Service;
use tracing::error;

#[derive(Clone, Debug)]
pub struct Svid {
    pub spiffe_id: Id,
    pub leaf: DerX509,
    pub private_key: Vec<u8>,
    pub intermediates: Vec<DerX509>,
}

#[derive(Clone, Debug)]
pub struct SvidUpdate {
    pub svids: HashMap<Id, Svid>,
}

#[derive(Clone, Debug)]
pub(crate) struct Api<S> {
    client: Client<S>,
}

#[derive(Clone)]
pub(crate) struct GrpcRecover(ExponentialBackoff);

pub(crate) type Watch<S> = StreamWatch<GrpcRecover, Api<S>>;

// === impl Svid ===

impl TryFrom<api::X509svid> for Svid {
    type Error = Error;
    fn try_from(proto: api::X509svid) -> Result<Self, Self::Error> {
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
            leaf: leaf.clone(),
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
    pub(super) fn watch(client: S, backoff: ExponentialBackoff) -> Watch<S> {
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
            let rsp = client.fetch_x509svid(tonic::Request::new(req)).await?;
            Ok(rsp.map(|svids| {
                svids
                    .map_ok(move |s| {
                        let svids = s
                            .svids
                            .into_iter()
                            .filter_map(|proto| {
                                let svid: Option<Svid> = proto
                                    .try_into()
                                    .map_err(|err| error!("could not parse SVID: {}", err))
                                    .ok();

                                svid.map(|svid| (svid.spiffe_id.clone(), svid))
                            })
                            .collect();

                        SvidUpdate { svids }
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
        if status.code() == tonic::Code::InvalidArgument
            || status.code() == tonic::Code::FailedPrecondition
        {
            return Err(status);
        }

        tracing::warn!(
            grpc.status = %status.code(),
            grpc.message = status.message(),
            "Unexpected policy SPIRE Workload API response; retrying with a backoff",
        );
        Ok(self.0.stream())
    }
}
