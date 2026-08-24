use tracing::{debug, info};

use crate::encode::BitmapWireStats;

#[derive(Debug, Default)]
pub struct SessionBitmapMetrics {
    pub frames: u64,
    pub tiles: u64,
    pub compressed_tiles: u64,
    pub raw_tiles: u64,
    pub encoded_bytes: u64,
    pub update_batches: u64,
}

impl SessionBitmapMetrics {
    pub fn record(&mut self, stats: BitmapWireStats) {
        self.frames += 1;
        self.tiles += u64::from(stats.tiles);
        self.compressed_tiles += u64::from(stats.compressed_tiles);
        self.raw_tiles += u64::from(stats.raw_tiles);
        self.encoded_bytes += stats.encoded_bytes as u64;
        self.update_batches += u64::from(stats.update_batches);
        if self.frames.is_multiple_of(30) {
            self.emit_debug("periodic");
        }
    }

    pub fn log(&self, reason: &'static str) {
        if self.frames == 0 {
            return;
        }
        info!(
            reason,
            frames = self.frames,
            tiles = self.tiles,
            compressed_tiles = self.compressed_tiles,
            raw_tiles = self.raw_tiles,
            encoded_bytes = self.encoded_bytes,
            update_batches = self.update_batches,
            "session bitmap metrics"
        );
    }

    fn emit_debug(&self, reason: &'static str) {
        debug!(
            reason,
            frames = self.frames,
            tiles = self.tiles,
            compressed_tiles = self.compressed_tiles,
            raw_tiles = self.raw_tiles,
            encoded_bytes = self.encoded_bytes,
            update_batches = self.update_batches,
            "session bitmap metrics"
        );
    }
}
