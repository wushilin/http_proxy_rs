use std::sync::atomic::AtomicUsize;

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};


lazy_static! {
    pub static ref ACTIVE_CONNECTIONS:AtomicUsize = AtomicUsize::new(0);
    pub static ref UPLOADED:AtomicUsize = AtomicUsize::new(0);
    pub static ref DOWNLOADED:AtomicUsize = AtomicUsize::new(0);
    pub static ref TOTAL_CONNECTIONS:AtomicUsize = AtomicUsize::new(0);
}

pub fn incr_conn() {
    ACTIVE_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    TOTAL_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

pub fn decr_conn() {
    ACTIVE_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
}

pub fn incr_uploaded(bytes:usize) {
    UPLOADED.fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
}

pub fn incr_downloaded(bytes:usize) {
    DOWNLOADED.fetch_add(bytes, std::sync::atomic::Ordering::SeqCst);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub active_connections:usize,
    pub uploaded:usize,
    pub downloaded:usize,
    pub total_connections:usize,
}
pub fn get_stats() -> Stats {
    Stats {
        active_connections: ACTIVE_CONNECTIONS.load(std::sync::atomic::Ordering::SeqCst),
        uploaded: UPLOADED.load(std::sync::atomic::Ordering::SeqCst),
        downloaded: DOWNLOADED.load(std::sync::atomic::Ordering::SeqCst),
        total_connections: TOTAL_CONNECTIONS.load(std::sync::atomic::Ordering::SeqCst),
    }
}