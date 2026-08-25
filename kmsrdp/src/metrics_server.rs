//! Lightweight async Prometheus HTTP exporter endpoint for KMSRDP.
//!
//! Serves `/metrics` in standard Prometheus text exposition format and `/healthz`
//! without heavy web framework dependencies.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info};

use crate::metrics::GLOBAL_METRICS;

pub async fn run_metrics_server(
    listener: TcpListener,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    info!(addr = %listener.local_addr().map(|a| a.to_string()).unwrap_or_default(), "metrics HTTP server listening");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    debug!("metrics server shutting down");
                    break;
                }
            }
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, peer)) => {
                        debug!(%peer, "metrics request accepted");
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream).await {
                                debug!(%peer, "metrics connection closed: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!("metrics listener error: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");

    if method != "GET" && method != "HEAD" {
        let resp =
            "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    let (status, content_type, body) = match path {
        "/metrics" | "/" => (
            "200 OK",
            "text/plain; version=0.0.4; charset=utf-8",
            GLOBAL_METRICS.to_prometheus_text(),
        ),
        "/health" | "/healthz" => ("200 OK", "text/plain; charset=utf-8", "OK\n".to_string()),
        _ => (
            "404 Not Found",
            "text/plain; charset=utf-8",
            "Not Found\n".to_string(),
        ),
    };

    let response = if method == "HEAD" {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            content_type,
            body.len()
        )
    } else {
        format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status,
            content_type,
            body.len(),
            body
        )
    };

    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metrics_server_serves_metrics_and_healthz() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        let server_task = tokio::spawn(async move {
            run_metrics_server(listener, shutdown_rx).await;
        });

        // Test /healthz
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf);
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("OK\n"));

        // Test /metrics
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf);
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.contains("kmsrdp_active_sessions"));

        // Shutdown server
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
    }
}
