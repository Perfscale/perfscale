//! Minimal gRPC echo server for trying out `examples/grpc.test.yaml`:
//!
//! ```sh
//! cargo run -p perfscale-core --example grpc_echo_server
//! # in another terminal:
//! cargo run -p perfscale-cli -- run -f examples/grpc.test.yaml
//! ```
//!
//! Listens plaintext on 127.0.0.1:50051 with server reflection enabled.
//! Dev tool only — the load tests use the in-crate server in
//! `src/testsupport.rs` (same proto, richer service).

use tonic::{Request, Response, Status, Streaming};

pub mod echo {
    include!(concat!(env!("OUT_DIR"), "/perfscale.test.v1.rs"));
}

use echo::echo_server::{Echo, EchoServer};
use echo::{EchoRequest, EchoResponse};

#[derive(Debug, Default)]
struct EchoSvc;

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

    type ServerStreamStream = tokio_stream::wrappers::ReceiverStream<Result<EchoResponse, Status>>;

    async fn server_stream(
        &self,
        request: Request<EchoRequest>,
    ) -> Result<Response<Self::ServerStreamStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
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
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
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

    type BidiStream = tokio_stream::wrappers::ReceiverStream<Result<EchoResponse, Status>>;

    async fn bidi(
        &self,
        request: Request<Streaming<EchoRequest>>,
    ) -> Result<Response<Self::BidiStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
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
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/echo_descriptor.bin"
        )))
        .build_v1()?;

    let addr = "127.0.0.1:50051".parse()?;
    println!("gRPC echo server (with reflection) listening on {addr}");
    tonic::transport::Server::builder()
        .add_service(EchoServer::new(EchoSvc))
        .add_service(reflection)
        .serve(addr)
        .await?;
    Ok(())
}
