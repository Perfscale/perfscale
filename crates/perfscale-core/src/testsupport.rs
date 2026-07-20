//! Test-support gRPC echo server — compiled from `proto/echo.proto` by
//! build.rs and exercised through the real `execute_action` dispatch.
//!
//! Only compiled under `cfg(test)`; the `grpc_echo_server` example carries
//! its own minimal copy of this service.

#[allow(dead_code)] // the generated client stub is unused — tests dispatch actions
pub(crate) mod echo {
    include!(concat!(env!("OUT_DIR"), "/perfscale.test.v1.rs"));
}

/// Serialized `FileDescriptorSet` for echo.proto — feeds the reflection
/// service and the `descriptor_set` tests.
pub(crate) const ECHO_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/echo_descriptor.bin"));

use echo::echo_server::{Echo, EchoServer};
use echo::{EchoRequest, EchoResponse};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status, Streaming};

/// The echo service: every method is deterministic and side-effect free.
#[derive(Debug, Default)]
pub(crate) struct EchoSvc;

#[tonic::async_trait]
impl Echo for EchoSvc {
    async fn unary(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        let req = request.into_inner();
        Ok(Response::new(EchoResponse {
            message: req.message,
            seq: 0,
            padding: vec![],
        }))
    }

    type ServerStreamStream = ReceiverStream<Result<EchoResponse, Status>>;

    async fn server_stream(
        &self,
        request: Request<EchoRequest>,
    ) -> Result<Response<Self::ServerStreamStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            for i in 1..=req.count.max(0) {
                let msg = EchoResponse {
                    message: req.message.clone(),
                    seq: i,
                    padding: vec![],
                };
                if tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn client_stream(
        &self,
        request: Request<Streaming<EchoRequest>>,
    ) -> Result<Response<EchoResponse>, Status> {
        let mut stream = request.into_inner();
        let mut n = 0;
        let mut last = String::new();
        while let Some(req) = stream.message().await? {
            n += 1;
            last = req.message;
        }
        Ok(Response::new(EchoResponse {
            message: format!("{n} messages, last: {last}"),
            seq: n,
            padding: vec![],
        }))
    }

    type BidiStream = ReceiverStream<Result<EchoResponse, Status>>;

    async fn bidi(
        &self,
        request: Request<Streaming<EchoRequest>>,
    ) -> Result<Response<Self::BidiStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            let mut seq = 0;
            while let Ok(Some(req)) = stream.message().await {
                seq += 1;
                let msg = EchoResponse {
                    message: req.message,
                    seq,
                    padding: vec![],
                };
                if tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn fail(&self, _request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        Err(Status::invalid_argument("deliberate failure"))
    }

    async fn large(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        let req = request.into_inner();
        Ok(Response::new(EchoResponse {
            message: "large".into(),
            seq: 0,
            padding: vec![0u8; req.size.max(0) as usize],
        }))
    }
}

/// Base64 of [`ECHO_DESCRIPTOR_SET`], ready for the `descriptor_set` param.
pub(crate) fn descriptor_set_base64() -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(ECHO_DESCRIPTOR_SET)
}

fn reflection_service() -> tonic_reflection::server::v1::ServerReflectionServer<
    impl tonic_reflection::server::v1::ServerReflection,
> {
    tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(ECHO_DESCRIPTOR_SET)
        .build_v1()
        .expect("echo descriptor set is valid")
}

/// Start the plaintext echo server (reflection enabled) on 127.0.0.1:0.
/// Returns the bound port.
pub(crate) async fn start_echo_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        Server::builder()
            .add_service(EchoServer::new(EchoSvc))
            .add_service(reflection_service())
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    port
}

/// Start the TLS echo server with a fresh self-signed cert (`localhost`).
/// Default-verifying clients must reject it; `skipTLSVerify` must accept.
pub(crate) async fn start_echo_server_tls() -> u16 {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let identity = Identity::from_pem(cert.cert.pem(), cert.key_pair.serialize_pem());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        Server::builder()
            .tls_config(ServerTlsConfig::new().identity(identity))
            .unwrap()
            .add_service(EchoServer::new(EchoSvc))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    port
}
