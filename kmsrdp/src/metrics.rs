//! Global metrics aggregation and Prometheus text format export for KMSRDP.
//!
//! Provides lock-free atomic counters and gauges tracking session counts,
//! capture performance, encoding throughput, network transfer, and input injection.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub static GLOBAL_METRICS: LazyLock<GlobalMetrics> = LazyLock::new(GlobalMetrics::new);

#[derive(Debug, Default)]
pub struct GlobalMetrics {
    pub active_sessions: AtomicUsize,
    pub total_connections: AtomicU64,
    pub frames_captured: AtomicU64,
    pub frames_encoded_rdp6: AtomicU64,
    pub frames_encoded_nscodec: AtomicU64,
    pub frames_encoded_avc420: AtomicU64,
    pub bytes_sent_latency: AtomicU64,
    pub bytes_sent_bulk: AtomicU64,
    pub frames_dropped_bulk: AtomicU64,
    pub input_events_keyboard: AtomicU64,
    pub input_events_mouse: AtomicU64,
    pub input_events_unicode: AtomicU64,
}

impl GlobalMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn inc_active_sessions(&self) {
        self.active_sessions.fetch_add(1, Ordering::Relaxed);
        self.total_connections.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn dec_active_sessions(&self) {
        // Avoid underflow if decrement called without prior increment
        let _ = self
            .active_sessions
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |val| {
                Some(val.saturating_sub(1))
            });
    }

    #[inline]
    pub fn inc_frames_captured(&self) {
        self.frames_captured.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_frames_encoded(&self, codec: &str) {
        match codec {
            "avc420" => self.frames_encoded_avc420.fetch_add(1, Ordering::Relaxed),
            "nscodec" => self.frames_encoded_nscodec.fetch_add(1, Ordering::Relaxed),
            _ => self.frames_encoded_rdp6.fetch_add(1, Ordering::Relaxed),
        };
    }

    #[inline]
    pub fn add_bytes_sent(&self, is_latency: bool, bytes: usize) {
        if is_latency {
            self.bytes_sent_latency
                .fetch_add(bytes as u64, Ordering::Relaxed);
        } else {
            self.bytes_sent_bulk
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn inc_frames_dropped_bulk(&self) {
        self.frames_dropped_bulk.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_input_keyboard(&self) {
        self.input_events_keyboard.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_input_mouse(&self) {
        self.input_events_mouse.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_input_unicode(&self) {
        self.input_events_unicode.fetch_add(1, Ordering::Relaxed);
    }

    /// Exports all metrics formatted in standard Prometheus text exposition format (version 0.0.4).
    pub fn to_prometheus_text(&self) -> String {
        let active = self.active_sessions.load(Ordering::Relaxed);
        let total_conn = self.total_connections.load(Ordering::Relaxed);
        let captured = self.frames_captured.load(Ordering::Relaxed);
        let rdp6 = self.frames_encoded_rdp6.load(Ordering::Relaxed);
        let nscodec = self.frames_encoded_nscodec.load(Ordering::Relaxed);
        let avc420 = self.frames_encoded_avc420.load(Ordering::Relaxed);
        let bytes_lat = self.bytes_sent_latency.load(Ordering::Relaxed);
        let bytes_blk = self.bytes_sent_bulk.load(Ordering::Relaxed);
        let dropped_blk = self.frames_dropped_bulk.load(Ordering::Relaxed);
        let inp_kb = self.input_events_keyboard.load(Ordering::Relaxed);
        let inp_mouse = self.input_events_mouse.load(Ordering::Relaxed);
        let inp_uni = self.input_events_unicode.load(Ordering::Relaxed);

        let mut out = String::with_capacity(1024);

        out.push_str("# HELP kmsrdp_active_sessions Current number of active client sessions\n");
        out.push_str("# TYPE kmsrdp_active_sessions gauge\n");
        out.push_str(&format!("kmsrdp_active_sessions {}\n", active));

        out.push_str("# HELP kmsrdp_connections_total Total client connections accepted\n");
        out.push_str("# TYPE kmsrdp_connections_total counter\n");
        out.push_str(&format!("kmsrdp_connections_total {}\n", total_conn));

        out.push_str("# HELP kmsrdp_frames_captured_total Total DRM/KMS scanout frames captured\n");
        out.push_str("# TYPE kmsrdp_frames_captured_total counter\n");
        out.push_str(&format!("kmsrdp_frames_captured_total {}\n", captured));

        out.push_str("# HELP kmsrdp_frames_encoded_total Total frames encoded per codec\n");
        out.push_str("# TYPE kmsrdp_frames_encoded_total counter\n");
        out.push_str(&format!(
            "kmsrdp_frames_encoded_total{{codec=\"rdp6\"}} {}\n",
            rdp6
        ));
        out.push_str(&format!(
            "kmsrdp_frames_encoded_total{{codec=\"nscodec\"}} {}\n",
            nscodec
        ));
        out.push_str(&format!(
            "kmsrdp_frames_encoded_total{{codec=\"avc420\"}} {}\n",
            avc420
        ));

        out.push_str("# HELP kmsrdp_bytes_sent_total Total network bytes sent per priority\n");
        out.push_str("# TYPE kmsrdp_bytes_sent_total counter\n");
        out.push_str(&format!(
            "kmsrdp_bytes_sent_total{{priority=\"latency\"}} {}\n",
            bytes_lat
        ));
        out.push_str(&format!(
            "kmsrdp_bytes_sent_total{{priority=\"bulk\"}} {}\n",
            bytes_blk
        ));

        out.push_str("# HELP kmsrdp_frames_dropped_total Total frames dropped on congestion\n");
        out.push_str("# TYPE kmsrdp_frames_dropped_total counter\n");
        out.push_str(&format!(
            "kmsrdp_frames_dropped_total{{priority=\"bulk\"}} {}\n",
            dropped_blk
        ));

        out.push_str("# HELP kmsrdp_input_events_total Total client input events injected\n");
        out.push_str("# TYPE kmsrdp_input_events_total counter\n");
        out.push_str(&format!(
            "kmsrdp_input_events_total{{type=\"keyboard\"}} {}\n",
            inp_kb
        ));
        out.push_str(&format!(
            "kmsrdp_input_events_total{{type=\"mouse\"}} {}\n",
            inp_mouse
        ));
        out.push_str(&format!(
            "kmsrdp_input_events_total{{type=\"unicode\"}} {}\n",
            inp_uni
        ));

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_metrics_increments_and_formats_prometheus() {
        let metrics = GlobalMetrics::new();
        metrics.inc_active_sessions();
        metrics.inc_frames_captured();
        metrics.inc_frames_encoded("rdp6");
        metrics.inc_frames_encoded("avc420");
        metrics.add_bytes_sent(true, 128);
        metrics.add_bytes_sent(false, 4096);
        metrics.inc_input_keyboard();
        metrics.inc_input_mouse();
        metrics.inc_input_unicode();

        let text = metrics.to_prometheus_text();
        assert!(text.contains("kmsrdp_active_sessions 1\n"));
        assert!(text.contains("kmsrdp_connections_total 1\n"));
        assert!(text.contains("kmsrdp_frames_captured_total 1\n"));
        assert!(text.contains("kmsrdp_frames_encoded_total{codec=\"rdp6\"} 1\n"));
        assert!(text.contains("kmsrdp_frames_encoded_total{codec=\"avc420\"} 1\n"));
        assert!(text.contains("kmsrdp_bytes_sent_total{priority=\"latency\"} 128\n"));
        assert!(text.contains("kmsrdp_bytes_sent_total{priority=\"bulk\"} 4096\n"));
        assert!(text.contains("kmsrdp_input_events_total{type=\"keyboard\"} 1\n"));
        assert!(text.contains("kmsrdp_input_events_total{type=\"mouse\"} 1\n"));
        assert!(text.contains("kmsrdp_input_events_total{type=\"unicode\"} 1\n"));

        metrics.dec_active_sessions();
        assert_eq!(metrics.active_sessions.load(Ordering::Relaxed), 0);
    }
}
