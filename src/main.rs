use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use anycache::{FileParseError, FromWatchedFile};
use bytes::Bytes;

use config::Config;
use http_body_util::BodyExt;
use http_body_util::{combinators::BoxBody, Empty, Full};
use hyper::client::conn::http1::Builder;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::upgrade::Upgraded;
use hyper::{Method, Request, Response};
use tokio::sync::oneshot::{self, Sender};

use hyper_util::rt::TokioIo;
use log::{debug, error, info, warn};
use metered_io::MeteredIo;
use request_id::RequestId;
use tokio::net::{TcpListener, TcpStream};
pub mod crontab;
pub mod config;
pub mod request_id;
pub mod stats;
pub mod metered_io;

// To try this example:
// 1. cargo run --example http_proxy
// 2. config http_proxy in command line
//    $ export http_proxy=http://127.0.0.1:8100
//    $ export https_proxy=http://127.0.0.1:8100
// 3. send requests
//    $ curl -i https://www.some_domain.com/
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    log4rs::init_file("log4rs.yml", Default::default()).unwrap();
    fn load_config(data: &[u8]) -> Result<Config, FileParseError> {
        info!("loading config as source file is modified");
        let config = config::Config::from_bytes(data);
        match config {
            Ok(config) => Ok(config),
            Err(e) => {
                error!("failed to load config: {}", e);
                Err(e.into())
            }
        }
    }

    let config = Arc::new(FromWatchedFile::new(
        "config.yml",
        load_config,
        Duration::from_secs(5),
    ));

    let xx = config.get().unwrap();

    info!("Config is {:?}", xx);
    let bind_addr = format!("{}:{}", xx.get_bind_address(), xx.get_bind_port());
    let listener_result = TcpListener::bind(&bind_addr).await;
    match listener_result {
        Ok(listener) => {
            info!("Listening on http://{}", bind_addr);

            loop {
                let accept_result = listener.accept().await;
                match accept_result {
                    Ok((stream, _)) => {
                        let read_count = Arc::new(AtomicUsize::new(0));
                        let written_count = Arc::new(AtomicUsize::new(0));
                        let read_count_c = Arc::clone(&read_count);
                        let written_count_c = Arc::clone(&written_count);
                        let metered_stream = MeteredIo::new(stream, read_count_c, written_count_c);
                        let io = TokioIo::new(metered_stream);
                        let start = Instant::now();
                        tokio::task::spawn(async move {
                            let req_id = RequestId::new();
                            let result = http1::Builder::new()
                                .preserve_header_case(true)
                                .title_case_headers(true)
                                .serve_connection(io, service_fn(|x| proxy(req_id.clone(), x)))
                                .with_upgrades()
                                .await;
                            match result {
                                Ok(()) => {
                                    info!("{} connection completed. duration {:?}", req_id, start.elapsed());
                                }
                                Err(err) => {
                                    info!("{} connection competed with error: {:?}. duration {:?}", req_id, err, start.elapsed());
                                }
                            }
                            info!("{} from client {} bytes, to client {} bytes", 
                                req_id, 
                                read_count.load(std::sync::atomic::Ordering::Relaxed), 
                                written_count.load(std::sync::atomic::Ordering::Relaxed))
                        });
                    },
                    Err(cause) => {
                        error!("unable to accept new connection: {}", cause);
                    }
                }
            }
        },
        Err(cause) => {
            error!("unable to bind to {}: {}", bind_addr, cause);
            return Ok(());
        }
    }
}

async fn proxy(
    req_id: RequestId,
    mut req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    req.extensions_mut().insert(req_id.clone());
    info!("{} request begin", req_id);
    if Method::CONNECT == req.method() {
        debug!("{} request is CONNECT", req_id);
        // Received an HTTP request like:
        // ```
        // CONNECT www.domain.com:443 HTTP/1.1
        // Host: www.domain.com:443
        // Proxy-Connection: Keep-Alive
        // ```
        //
        // When HTTP method is CONNECT we should return an empty body
        // then we can eventually upgrade the connection and talk a new protocol.
        //
        // Note: only after client received an empty body with STATUS_OK can the
        // connection be upgraded, so we can't return a response inside
        // `on_upgrade` future.
        if let Some(addr) = host_addr(req.uri()) {
            let req_id_clone = req_id.clone();
            tokio::task::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        if let Err(e) = tunnel(req_id_clone.clone(), upgraded, addr).await {
                            error!("{} server io error: {}", req_id_clone, e);
                        };
                    }
                    Err(e) => {
                        error!("{} upgrade error: {}", req_id_clone, e);   
                    }
                }
            });
            return Ok(Response::new(empty()));
        } else {
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            info!("{} request ended no socket addr for CONNECT", req_id);
            return Ok(resp);
        }
    } else {
        let host_opt = req.uri().host();
        
        
        if host_opt.is_none() {
            let mut resp = Response::new(full("no HOST found to connect to"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            info!("{} request ended no host to connect", req_id);
            return Ok(resp);
        }
        let host = host_opt.unwrap().to_string();
        let port = req.uri().port_u16().unwrap_or(80);

        let stream = TcpStream::connect((host.clone(), port)).await;
        info!("{} connecting to {}:{}", req_id, host, port);
        if stream.is_err() {
            let err = stream.unwrap_err();
            let mut resp = Response::new(full(format!("connect to ({}:{}) failed: {}", host, port, err)));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            info!("{} request ended connection error: {}", req_id, err);
            return Ok(resp);
        }
        info!("{} stream opened", req_id);
        let stream = stream.unwrap();

        let io = TokioIo::new(stream);

        let handshake_result = Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await;
        if handshake_result.is_err() {
            let err = handshake_result.unwrap_err();
            let mut resp = Response::new(full(format!("handshake with ({}:{}) failed: {}", host, port, err)));
            *resp.status_mut() = http::StatusCode::BAD_GATEWAY;
            info!("{} request ended handshake error: {}", req_id, err);
            return Ok(resp);
        }
        info!("{} handshake successful", req_id);
        let (mut sender, conn) = handshake_result.unwrap();
        let req_id_clone = req_id.clone();
        tokio::task::spawn(async move {
            let result = conn.await;
            match result {
                Ok(()) => {
                    info!("{} connection completed.", req_id_clone);
                },
                Err(err) => {
                    warn!("{} connection failed: {:?}", req_id_clone, err);
                }
            }
        });

        let resp = sender.send_request(req).await;
        if resp.is_err() {
            let err = resp.unwrap_err();
            let mut resp = Response::new(full(format!("send_request with ({}:{}) failed: {}", host, port, err)));
            *resp.status_mut() = http::StatusCode::BAD_GATEWAY;
            info!("{} request ended send_request error: {}", req_id, err);
            return Ok(resp);
        }
        info!("{} response streaming started", req_id);
        let resp = resp.unwrap();
        Ok(resp.map(|b| b.boxed()))
    }
}

fn host_addr(uri: &http::Uri) -> Option<String> {
    uri.authority().and_then(|auth| Some(auth.to_string()))
}

fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full<T: Into<Bytes>>(chunk: T) -> BoxBody<Bytes, hyper::Error> {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

// Create a TCP connection to host:port, build a tunnel between the connection and
// the upgraded connection
async fn tunnel(req_id:RequestId, upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    // Connect to remote server
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    // Proxying data
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    // Print message when done
    info!(
        "{} client wrote {} bytes and received {} bytes",
        req_id,
        from_client, 
        from_server
    );

    Ok(())
}