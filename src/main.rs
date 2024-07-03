//! Run with
//!
//! ```not_rust
//! $ cargo run -p example-http-proxy
//! ```
//!
//! In another terminal:
//!
//! ```not_rust
//! $ curl -v -x "127.0.0.1:3000" https://tokio.rs
//! ```
//!
//! Example is based on <https://github.com/hyperium/hyper/blob/master/examples/http_proxy.rs>

pub mod crontab;
pub mod config;
pub mod request_id;
pub mod stats;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use log::{debug, error, info, warn};
use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use hyper::{body::Incoming, header::{PROXY_AUTHORIZATION, WWW_AUTHENTICATE}};
use hyper::server::conn::http1;
use hyper::upgrade::Upgraded;
use request_id::RequestId;
use std::{process::exit, sync::Arc};
use tokio::{net::{TcpListener, TcpStream}, time::Instant};
use tower::Service;
use tower::ServiceExt;

use hyper_util::rt::TokioIo;


#[tokio::main]
async fn main() {
    log4rs::init_file("log4rs.yml", Default::default()).unwrap();

    let config = config::Config::from_config_file("config.yml").unwrap();
    let config = Arc::new(config);
    let config_clone = Arc::clone(&config);

    let router_svc = Router::new().route("/", get(|| async { "This is not a web server, it is a proxy server" }));

    let tower_service = tower::service_fn(move |req: Request<_>| {
        let mut req = req.map(Body::new);
        let req_id = RequestId::new();
        req.extensions_mut().insert(req_id);
        let router_svc = router_svc.clone();

        let config = Arc::clone(&config);
        let config_clone_2 = Arc::clone(&config);
        async move {
            let req_id = req.extensions().get::<RequestId>().unwrap().clone();
            if req.method() == Method::CONNECT {
                req.headers().iter().for_each(|(k, v)| {
                    debug!("{} header {} => {}", req_id, k.as_str(), v.to_str().unwrap());
                });
                let authorized = check_authorization(&req, config);
                let unauthorized_response = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(WWW_AUTHENTICATE, "Basic realm=\"HTTP Proxy\"")
                    .body(Body::empty())
                    .unwrap();
    
                if ! authorized {
                    info!("{} rejecting request because of authentication issue", req_id);
                    return Ok(unauthorized_response);
                } else {
                    info!("{} user auth OK", req_id);
                }

                let target_host = req.uri().authority().map(|auth| auth.to_string());
                match target_host {
                    None => {
                        error!("{} bad request. no host to connect to...", req_id);
                        return Ok(Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Body::empty())
                            .unwrap());
                    },
                    Some(host) => {
                        let access = config_clone_2.check_access(&req_id, &host).await;
                        if access {
                            info!("{} allowing connection to {}", req_id, host);
                            let result = proxy(req).await;
                            return result;
                        } else {
                            info!("{} rejecting connection to {}", req_id, host);
                            return Ok(Response::builder()
                                .status(StatusCode::FORBIDDEN)
                                .body(Body::empty())
                                .unwrap());
                        }
                    }
                }
            } else {
                router_svc.oneshot(req).await.map_err(|err| match err {})
            }
        }
    });

    let hyper_service = hyper::service::service_fn(move |request: Request<Incoming>| {
        tower_service.clone().call(request)
    });

    let addr = format!("{}:{}", config_clone.get_bind_address(), config_clone.get_bind_port());
    info!("listening on {}", addr);

    let listener_result = TcpListener::bind(&addr).await;
    match listener_result {
        Ok(listener) => {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let io = TokioIo::new(stream);
                let hyper_service = hyper_service.clone();
                tokio::task::spawn(async move {
                    if let Err(err) = http1::Builder::new()
                        .preserve_header_case(true)
                        .title_case_headers(true)
                        .serve_connection(io, hyper_service)
                        .with_upgrades()
                        .await
                    {
                        error!("failed to serve connection: {:?}", err);
                    }
                });
            }
        },
        Err(e) => {
            error!("failed to bind to address {}: {}", addr, e);
            exit(1);
        }
    }
}

fn decode_user_password(req:&Request) -> Result<(String, String), Box<dyn std::error::Error>>{
    let authorization = req.headers().get(PROXY_AUTHORIZATION);
    match authorization {
        None => {
            return Err("no authorization header found".into());
        },
        Some(value) => {
            let input = value.to_str()?;
            let tokens:Vec<&str> = input.split_ascii_whitespace().collect();
            if tokens.len() != 2 {
                return Err(format!("invalid authorization header: `{}`. must be space separated, 2 tokens exactly", input).into());
            }
            let input = tokens[1];
            let decoded = STANDARD.decode(input.as_bytes())?;
            let decoded_str = String::from_utf8(decoded)?;
            let index = decoded_str.find(":");
            match index {
                None => {
                    return Err(format!("invalid user:password combo - no `:` found in `{}`", decoded_str).into());
                },
                Some(index) => {
                    let username = &decoded_str[..index];
                    let password = &decoded_str[index+1..];
                    return Ok((username.to_string(), password.to_string()));
                }
            }
        }
    }
}

fn check_authorization(req:&Request, cfg:Arc<config::Config>) -> bool {
    let default_user = "anonymous";
    let default_password = "anonymous";
    let decoded = decode_user_password(req);
    let req_id = req.extensions().get::<RequestId>().unwrap();
    match decoded {
        Ok((username, password)) => {
            info!("{} authenticating User:{}", req_id, username);
            return cfg.authenticate_user(&username, &password);
        },
        Err(cause) => {
            error!("{} no authorization header found({}). using default User:{} and default password {}", 
                req_id, cause, default_user, default_password);
            return cfg.authenticate_user(default_user, default_password);
        }
    }
}
async fn proxy(req: Request) -> Result<Response, hyper::Error> {
    let req_id = req.extensions().get::<RequestId>().unwrap().clone();
    if let Some(host_addr) = req.uri().authority().map(|auth| auth.to_string()) {
        tokio::task::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    if let Err(e) = tunnel(req_id.clone(), upgraded, host_addr).await {
                        warn!("{} server io error: {}", req_id, e);
                    };
                }
                Err(e) => warn!("{} upgrade error: {}", req_id, e),
            }
        });

        Ok(Response::new(Body::empty()))
    } else {
        warn!("CONNECT host is not socket addr: {:?}", req.uri());
        Ok((
            StatusCode::BAD_REQUEST,
            "CONNECT must be to a socket address",
        )
            .into_response())
    }
}

async fn tunnel(req_id:RequestId, upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    let start = Instant::now();
    info!("{req_id} tunnel start");
    stats::incr_conn();
    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;
    let elapsed = start.elapsed();
    info!("{req_id} ended. up {from_client} bytes, down {from_server} bytes, time {elapsed:?}");
    stats::incr_downloaded(from_server as usize);
    stats::incr_uploaded(from_client as usize);
    stats::decr_conn();
    let stat = stats::get_stats();
    info!("{req_id} stats {stat:?}");
    Ok(())
}