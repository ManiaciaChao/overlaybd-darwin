//! Optional in-memory timing; never writes diagnostics on the block I/O path.
use anyhow::Result;
use obd_core_adapter::BlockImage;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Default)]
struct Io {
    calls: AtomicU64,
    ns: AtomicU64,
    max_ns: AtomicU64,
}
impl Io {
    fn snapshot(&self) -> Value {
        json!({"calls": self.calls.load(Ordering::Relaxed), "ns": self.ns.load(Ordering::Relaxed),
            "max_ns": self.max_ns.load(Ordering::Relaxed)})
    }
}
#[derive(Default, Serialize)]
struct Log {
    dropped: usize,
    flushes: VecDeque<Value>,
    io_dropped: usize,
    io: VecDeque<IoEvent>,
}
#[derive(Serialize)]
struct IoEvent {
    wall_start_ns: u128,
    ns: u64,
    write: bool,
    offset: u64,
    len: usize,
    ok: bool,
}
impl Log {
    fn io(&mut self, event: IoEvent) {
        if self.io.len() == 65_536 {
            self.io.pop_front();
            self.io_dropped += 1;
        }
        self.io.push_back(event);
    }
}
pub(crate) struct Diagnostics {
    read: Io,
    write: Io,
    log: Mutex<Log>,
}
impl Diagnostics {
    pub fn open(dir: &Path) -> Option<Self> {
        dir.join("io-trace.enabled").is_file().then(|| Self {
            read: Io::default(),
            write: Io::default(),
            log: Mutex::new(Log::default()),
        })
    }
    pub fn measure<T>(
        trace: Option<&Self>,
        write: bool,
        offset: u64,
        len: usize,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let Some(trace) = trace else {
            return action();
        };
        let wall_start_ns = Self::wall_ns();
        let start = Instant::now();
        let result = action();
        let elapsed = start.elapsed().as_nanos() as u64;
        let io = if write { &trace.write } else { &trace.read };
        io.calls.fetch_add(1, Ordering::Relaxed);
        io.ns.fetch_add(elapsed, Ordering::Relaxed);
        io.max_ns.fetch_max(elapsed, Ordering::Relaxed);
        if let Ok(mut log) = trace.log.lock() {
            log.io(IoEvent {
                wall_start_ns,
                ns: elapsed,
                write,
                offset,
                len,
                ok: result.is_ok(),
            });
        }
        result
    }
    pub fn snapshot(&self, image: &BlockImage) -> Value {
        let (packed, flush) = crate::runtime().block_on(async {
            (
                image.packed_stats().await.ok().flatten(),
                image.flush_profile().await,
            )
        });
        json!({"packed": packed, "flush": flush, "read": self.read.snapshot(), "write": self.write.snapshot()})
    }
    pub fn wall_ns() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }
    pub fn record(&self, row: Value) {
        if let Ok(mut log) = self.log.lock() {
            // Diagnostic memory is bounded even if a mount is left open.
            if log.flushes.len() == 512 {
                log.flushes.pop_front();
                log.dropped += 1;
            }
            log.flushes.push_back(row);
        }
    }
    pub fn save(&self, dir: &Path) {
        if let Ok(log) = self.log.lock() {
            if let Err(error) = crate::atomic_json(&dir.join("flush-trace.json"), &*log) {
                eprintln!("OverlayBD diagnostics: {error:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn io_timeline_is_bounded_and_preserves_latest_order() {
        let mut log = Log::default();
        for offset in 0..65_540 {
            log.io(IoEvent {
                wall_start_ns: 0,
                ns: 1,
                write: false,
                offset,
                len: 4096,
                ok: true,
            });
        }
        assert_eq!(log.io.len(), 65_536);
        assert_eq!(log.io_dropped, 4);
        assert_eq!(log.io.front().unwrap().offset, 4);
        assert_eq!(log.io.back().unwrap().offset, 65_539);
    }
}
