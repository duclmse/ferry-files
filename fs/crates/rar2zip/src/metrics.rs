use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::queue::JobQueue;

// ---------------------------------------------------------------------------

pub struct Metrics {
    pub files_processed:     AtomicU64,
    pub bytes_transferred:   AtomicU64,
    pub conversion_failures: AtomicU64,
    pub transfer_failures:   AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            files_processed:     AtomicU64::new(0),
            bytes_transferred:   AtomicU64::new(0),
            conversion_failures: AtomicU64::new(0),
            transfer_failures:   AtomicU64::new(0),
        }
    }
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn prometheus_text(&self, queue_depth: i64) -> String {
        let fp = self.files_processed.load(Ordering::Relaxed);
        let bt = self.bytes_transferred.load(Ordering::Relaxed);
        let cf = self.conversion_failures.load(Ordering::Relaxed);
        let tf = self.transfer_failures.load(Ordering::Relaxed);
        format!(
            "# HELP rar2zip_files_processed_total Total files successfully processed\n\
             # TYPE rar2zip_files_processed_total counter\n\
             rar2zip_files_processed_total {fp}\n\
             # HELP rar2zip_bytes_transferred_total Total bytes sent to all destinations\n\
             # TYPE rar2zip_bytes_transferred_total counter\n\
             rar2zip_bytes_transferred_total {bt}\n\
             # HELP rar2zip_conversion_failures_total Total conversion failures\n\
             # TYPE rar2zip_conversion_failures_total counter\n\
             rar2zip_conversion_failures_total {cf}\n\
             # HELP rar2zip_transfer_failures_total Total transfer failures\n\
             # TYPE rar2zip_transfer_failures_total counter\n\
             rar2zip_transfer_failures_total {tf}\n\
             # HELP rar2zip_queue_depth Current job queue depth across all statuses\n\
             # TYPE rar2zip_queue_depth gauge\n\
             rar2zip_queue_depth {queue_depth}\n"
        )
    }
}

// ---------------------------------------------------------------------------
// HTTP server (tiny_http, runs in a dedicated thread)
// ---------------------------------------------------------------------------

pub fn serve(
    port:     u16,
    metrics:  Arc<Metrics>,
    queue:    Arc<JobQueue>,
    shutdown: Arc<AtomicBool>,
) {
    let addr = format!("0.0.0.0:{port}");
    let server = match tiny_http::Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("metrics server bind {addr} failed: {e}");
            return;
        }
    };
    tracing::info!("metrics server listening on http://{addr}");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        match server.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Some(req)) => handle_request(req, &metrics, &queue),
            Ok(None)      => {}
            Err(e) => {
                tracing::warn!("metrics recv error: {e}");
                break;
            }
        }
    }
}

fn handle_request(req: tiny_http::Request, metrics: &Metrics, queue: &JobQueue) {
    let url = req.url().to_string();
    let (status, content_type, body): (u16, &str, String) = match url.trim_end_matches('/') {
        "/metrics" => {
            let depth: i64 = queue
                .queue_depth()
                .map(|m| m.values().sum())
                .unwrap_or(0);
            (200, "text/plain; version=0.0.4; charset=utf-8", metrics.prometheus_text(depth))
        }
        "/health" => {
            let depth: i64 = queue
                .queue_depth()
                .map(|m| m.values().sum())
                .unwrap_or(0);
            (200, "application/json", format!(r#"{{"status":"ok","queue_depth":{depth}}}"#))
        }
        "/ui" => {
            let depth_map = queue.queue_depth().unwrap_or_default();
            let recent    = queue.recent_jobs(20).unwrap_or_default();
            (200, "text/html; charset=utf-8", crate::dashboard::render(&depth_map, &recent, metrics))
        }
        _ => (404, "text/plain", "not found\n".into()),
    };

    let header = tiny_http::Header::from_bytes(b"Content-Type", content_type.as_bytes())
        .expect("valid header");
    let response = tiny_http::Response::from_string(body)
        .with_status_code(status)
        .with_header(header);

    if let Err(e) = req.respond(response) {
        tracing::warn!("metrics response error: {e}");
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_text_contains_all_metrics() {
        let m = Metrics::default();
        m.files_processed.store(7, Ordering::Relaxed);
        m.bytes_transferred.store(2048, Ordering::Relaxed);
        m.conversion_failures.store(1, Ordering::Relaxed);
        m.transfer_failures.store(2, Ordering::Relaxed);
        let text = m.prometheus_text(5);
        assert!(text.contains("rar2zip_files_processed_total 7"));
        assert!(text.contains("rar2zip_bytes_transferred_total 2048"));
        assert!(text.contains("rar2zip_conversion_failures_total 1"));
        assert!(text.contains("rar2zip_transfer_failures_total 2"));
        assert!(text.contains("rar2zip_queue_depth 5"));
    }
}
