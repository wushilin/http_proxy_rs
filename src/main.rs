use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use anycache::{FileParseError, FromWatchedFile};
use bytes::Bytes;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use config::Config;
use http::header::{PROXY_AUTHORIZATION, WWW_AUTHENTICATE};
use http::StatusCode;
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

    let cfg = config.get().unwrap();

    let bind_addr = format!("{}:{}", cfg.get_bind_address(), cfg.get_bind_port());
    let listener_result = TcpListener::bind(&bind_addr).await;
    match listener_result {
        Ok(listener) => {
            info!("Listening on http://{}", bind_addr);

            loop {
                let accept_result = listener.accept().await;
                let cfg = config.get().unwrap();
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
                                .serve_connection(io, service_fn(|req| proxy(req_id.clone(),Arc::clone(&cfg),  req)))
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
    config: Arc<Config>,
    mut req: Request<hyper::body::Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    req.extensions_mut().insert(req_id.clone());
    info!("{} request begin", req_id);
    let config_clone = Arc::clone(&config);
    let authorized = check_authorization(&req, config);
    
    let unauthorized_response = Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(WWW_AUTHENTICATE, "Basic realm=\"HTTP Proxy\"")
        .body(empty())
        .unwrap();

    if !authorized {
        info!(
            "{} rejecting request because of authentication issue",
            req_id
        );
        return Ok(unauthorized_response);
    } else {
        info!("{} user auth OK", req_id);
    }

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
            let access = config_clone.check_access(&req_id, &addr).await;
            if access {
                info!("{} allowing connection to {}", req_id, addr);
                tokio::task::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            if let Err(e) = tunnel(req_id_clone.clone(), upgraded, addr).await {
                                error!("{} server io error: {}", req_id_clone, e);
                            } else {
                                info!("{} upgrade completed", req_id_clone);
                            }
                        }
                        Err(e) => {
                            error!("{} upgrade error: {}", req_id_clone, e);   
                        }
                    }
                });

                // has to return empty response to allow upgrade to complete
                return Ok(Response::new(empty()));
            } else {
                info!("{} rejecting connection to {}", req_id, addr);
                return Ok(Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(empty())
                    .unwrap());
            }
        } else {
            let mut resp = Response::new(full("CONNECT must be to a socket address"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            info!("{} request ended no socket addr for CONNECT", req_id);
            return Ok(resp);
        }
    } else {
        let host_opt = req.uri().host();
        
        if host_opt.is_none() {
            let mut resp = Response::new(full("This is a proxy, not a web server. no HOST found to connect to"));
            *resp.status_mut() = http::StatusCode::BAD_REQUEST;
            info!("{} request ended no host to connect", req_id);
            return Ok(resp);
        }
        let host = host_opt.unwrap().to_string();
        let port = req.uri().port_u16().unwrap_or(80);
        let addr = format!("{}:{}", host, port);
        let access = config_clone.check_access(&req_id, &addr).await;
        if access {
            info!("{} allowing connection to {}", req_id, addr);
        } else {
            info!("{} rejecting connection to {}", req_id, addr);
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(empty())
                .unwrap());
        }

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
                    info!("{} handshake result acquired.", req_id_clone);
                },
                Err(err) => {
                    warn!("{} handshake result failed: {:?}", req_id_clone, err);
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
        "{} completed. client wrote {} bytes and received {} bytes",
        req_id,
        from_client, 
        from_server
    );

    Ok(())
}

fn decode_user_password(req: &Request<hyper::body::Incoming>) -> Result<(String, String), Box<dyn std::error::Error>> {
    let authorization = req.headers().get(PROXY_AUTHORIZATION);
    match authorization {
        None => {
            return Err("no authorization header found".into());
        }
        Some(value) => {
            let input = value.to_str()?;
            let tokens: Vec<&str> = input.split_ascii_whitespace().collect();
            if tokens.len() != 2 {
                return Err(format!(
                    "invalid authorization header: `{}`. must be space separated, 2 tokens exactly",
                    input
                )
                .into());
            }
            let input = tokens[1];
            let decoded = STANDARD.decode(input.as_bytes())?;
            let decoded_str = String::from_utf8(decoded)?;
            let index = decoded_str.find(":");
            match index {
                None => {
                    return Err(format!(
                        "invalid user:password combo - no `:` found in `{}`",
                        decoded_str
                    )
                    .into());
                }
                Some(index) => {
                    let username = &decoded_str[..index];
                    let password = &decoded_str[index + 1..];
                    return Ok((username.to_string(), password.to_string()));
                }
            }
        }
    }
}

fn check_authorization(req: &Request<hyper::body::Incoming>, cfg: Arc<config::Config>) -> bool {
    let default_user = "anonymous";
    let default_password = "anonymous";
    let decoded = decode_user_password(req);
    let req_id = req.extensions().get::<RequestId>().unwrap();
    match decoded {
        Ok((username, password)) => {
            info!("{} authenticating User:{}", req_id, username);
            return cfg.authenticate_user(&username, &password);
        }
        Err(cause) => {
            error!("{} no authorization header found({}). using default User:{} and default password {}", 
                req_id, cause, default_user, default_password);
            return cfg.authenticate_user(default_user, default_password);
        }
    }
}