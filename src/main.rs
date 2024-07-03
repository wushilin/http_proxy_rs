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
use base64::{engine::general_purpose::STANDARD, Engine as _};

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use hyper::{body::Incoming, header::{AUTHORIZATION, PROXY_AUTHORIZATION, WWW_AUTHENTICATE}};
use hyper::server::conn::http1;
use hyper::upgrade::Upgraded;
use std::{net::SocketAddr, process::exit, sync::Arc};
use tokio::net::{TcpListener, TcpStream};
use tower::Service;
use tower::ServiceExt;

use hyper_util::rt::TokioIo;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};


#[tokio::main]
async fn main() {
    let config = config::Config::from_config_file("config.yml").unwrap();
    let config = Arc::new(config);
    let config_clone = Arc::clone(&config);
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "http_proxy=trace,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let router_svc = Router::new().route("/", get(|| async { "This is not a web server, it is a proxy server" }));

    let tower_service = tower::service_fn(move |req: Request<_>| {
        let router_svc = router_svc.clone();
        let req = req.map(Body::new);
        let config = Arc::clone(&config);
        let config_clone_2 = Arc::clone(&config);
        async move {

            if req.method() == Method::CONNECT {
                req.headers().iter().for_each(|(k, v)| {
                    println!("{}: {}", k.as_str(), v.to_str().unwrap());
                    tracing::debug!("{}: {}", k.as_str(), v.to_str().unwrap());
                });
                let authorized = check_authorization(config, req.headers().get(PROXY_AUTHORIZATION));
                let unauthorized_response = Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .header(WWW_AUTHENTICATE, "Basic realm=\"HTTP Proxy\"")
                    .body(Body::empty())
                    .unwrap();
    
                if ! authorized {
                    return Ok(unauthorized_response);
                }

                let target_host = req.uri().authority().map(|auth| auth.to_string());
                match target_host {
                    None => {
                        println!("Bad request. no host to connect to...");
                        return Ok(Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .body(Body::empty())
                            .unwrap());
                    },
                    Some(host) => {
                        let access = config_clone_2.check_access(&host).await;
                        if access {
                            proxy(req).await
                        } else {
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
    tracing::debug!("listening on {}", addr);

    let listener = TcpListener::bind(addr).await.unwrap();
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
                println!("Failed to serve connection: {:?}", err);
            }
        });
    }
}

fn check_authorization(cfg:Arc<config::Config>, authorization: Option<&HeaderValue>) -> bool {
    let default_user = "anonymous";
    let default_password = "anonymous";
    match authorization {
        None => {
            return cfg.authenticate_user(default_user, default_password);
        }, 
        Some(value) => {
            let str_value = value.to_str();
            match str_value {
                Ok(value) => {
                    let parts: Vec<&str> = value.split_whitespace().collect();
                    if parts.len() != 2 {
                        return cfg.authenticate_user(default_user, default_password);;
                    }
                    let decoded = STANDARD.decode(parts[1]);
                    match decoded {
                        Ok(decoded) => {
                            let decoded_str = String::from_utf8(decoded);
                            match decoded_str {
                                Ok(decoded_str) => {
                                    let index = decoded_str.find(":");
                                    match index {
                                        None => {
                                            println!("Invalid user:password combo: {}", decoded_str);
                                            return cfg.authenticate_user(default_user, default_password);
                                        },
                                        Some(index) => {
                                            let username = &decoded_str[..index];
                                            let password = &decoded_str[index+1..];

                                            let result = cfg.authenticate_user(username, password);
                                            println!("User: {username} result {result}");
                                            return result;
                                        }
                                    }
                                },
                                Err(cause) => {
                                    println!("Failed to get as string after base 64: {} -> {:?}", parts[1], cause);
                                    return cfg.authenticate_user(default_user, default_password);
                                }
                            }
                        },
                        Err(cause) => {
                            println!("Failed to decode base64: {} -> {:?}", parts[1], cause);
                            return cfg.authenticate_user(default_user, default_password);
                        }
                    }
                },
                Err(cause) => {
                    println!("Failed to get header value as string:  {:?}", cause);
                    return cfg.authenticate_user(default_user, default_password);
                }
            
            }
        }
    }
    true
}
async fn proxy(req: Request) -> Result<Response, hyper::Error> {
    tracing::trace!(?req);

    if let Some(host_addr) = req.uri().authority().map(|auth| auth.to_string()) {
        tokio::task::spawn(async move {
            match hyper::upgrade::on(req).await {
                Ok(upgraded) => {
                    if let Err(e) = tunnel(upgraded, host_addr).await {
                        tracing::warn!("server io error: {}", e);
                    };
                }
                Err(e) => tracing::warn!("upgrade error: {}", e),
            }
        });

        Ok(Response::new(Body::empty()))
    } else {
        tracing::warn!("CONNECT host is not socket addr: {:?}", req.uri());
        Ok((
            StatusCode::BAD_REQUEST,
            "CONNECT must be to a socket address",
        )
            .into_response())
    }
}

async fn tunnel(upgraded: Upgraded, addr: String) -> std::io::Result<()> {
    let mut server = TcpStream::connect(addr).await?;
    let mut upgraded = TokioIo::new(upgraded);

    let (from_client, from_server) =
        tokio::io::copy_bidirectional(&mut upgraded, &mut server).await?;

    println!("handled 1 conn {from_client} {from_server}");
    tracing::debug!(
        "client wrote {} bytes and received {} bytes",
        from_client,
        from_server
    );

    Ok(())
}