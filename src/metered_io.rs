use std::{pin::Pin, sync::{atomic::AtomicUsize, Arc}, task::{Context, Poll}};

use tokio::io::{AsyncRead, AsyncWrite};

pub struct MeteredIo<T> where 
    T: AsyncRead + AsyncWrite + Unpin
{
    io: T,
    read: Arc<AtomicUsize>,
    written: Arc<AtomicUsize>,
}

impl<T> MeteredIo<T> where 
T: AsyncRead + AsyncWrite + Unpin{
    pub fn get_written_bytes(&self) -> usize {
        return self.written.load(std::sync::atomic::Ordering::Relaxed);
    }

    pub fn get_read_bytes(&self) -> usize {
        return self.read.load(std::sync::atomic::Ordering::Relaxed);
    }

    pub fn new(value: T, read: Arc<AtomicUsize>, written: Arc<AtomicUsize>) -> Self {
        Self {
            io: value,
            read,
            written,
        }
    }
}

impl<T> AsyncRead for MeteredIo<T> where 
T: AsyncRead + AsyncWrite + Unpin
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let filled1 = buf.filled().len();
        let result = Pin::new(&mut self.io).poll_read(cx, buf);
        let filled2 = buf.filled().len();
        let read_count = filled2 - filled1;
        if read_count > 0 {
            println!("????{}", read_count);
            self.read.fetch_add(read_count, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }
}

impl<T> AsyncWrite for MeteredIo<T> where T: AsyncRead + AsyncWrite + Unpin {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let result = Pin::new(&mut self.io).poll_write(cx, buf);
        match result {
            Poll::Ready(read) => {
                match read.as_ref() {
                    Ok(read) => {
                        self.written.fetch_add(*read, std::sync::atomic::Ordering::SeqCst);
                    },
                    Err(_) => {}
                }
                Poll::Ready(read)
            },
            Poll::Pending => {
                Poll::Pending
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}