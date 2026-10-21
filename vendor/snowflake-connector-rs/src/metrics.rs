//! Process-wide timing of result-chunk downloads and parsing (rustflake measurements). Chunks are
//! fetched and parsed concurrently, so these are sums of per-chunk times, not wall time.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

static DOWNLOAD_NS: AtomicU64 = AtomicU64::new(0);
static PARSE_NS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static CHUNKS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default)]
pub struct ChunkTimings {
    pub download: Duration,
    pub parse: Duration,
    /// Bytes as received (gzip when Snowflake compressed them).
    pub bytes: u64,
    pub chunks: u64,
}

pub fn chunk_timings() -> ChunkTimings {
    ChunkTimings {
        download: Duration::from_nanos(DOWNLOAD_NS.load(Relaxed)),
        parse: Duration::from_nanos(PARSE_NS.load(Relaxed)),
        bytes: BYTES.load(Relaxed),
        chunks: CHUNKS.load(Relaxed),
    }
}

pub fn reset() {
    for c in [&DOWNLOAD_NS, &PARSE_NS, &BYTES, &CHUNKS] {
        c.store(0, Relaxed);
    }
}

pub(crate) fn record_download(took: Duration, bytes: usize) {
    DOWNLOAD_NS.fetch_add(took.as_nanos() as u64, Relaxed);
    BYTES.fetch_add(bytes as u64, Relaxed);
    CHUNKS.fetch_add(1, Relaxed);
}

pub(crate) fn record_parse(took: Duration) {
    PARSE_NS.fetch_add(took.as_nanos() as u64, Relaxed);
}
