//! Bounded plaintext writeback. BlockImage permits concurrent immutable reads;
//! mutations and drains require its exclusive operation gate.
//! Entries are complete logical blocks, including bytes inherited from lowers.
use anyhow::{ensure, Context, Result};
use overlaybd::{image_file::ImageFile, virtual_file::VirtualFile};
use serde::Serialize;
use std::{collections::BTreeMap, ops::Deref, time::Instant};

const BLOCK: usize = 4096;

#[derive(Clone, Debug, Default, Serialize)]
pub struct WritebackProfile {
    pub accepted_bytes: u64,
    pub overwritten_blocks: u64,
    pub backing_bytes: u64,
    pub backing_writes: u64,
    pub drain_ns: u64,
    pub pending_bytes: usize,
    pub peak_pending_bytes: usize,
}

pub(crate) struct BufferedImage {
    image: std::sync::Arc<ImageFile>,
    blocks: BTreeMap<u64, Vec<u8>>,
    pub capacity: usize,
    stats: WritebackProfile,
}
impl Deref for BufferedImage {
    type Target = ImageFile;
    fn deref(&self) -> &ImageFile {
        &self.image
    }
}
impl BufferedImage {
    pub fn new(image: std::sync::Arc<ImageFile>, capacity: usize) -> Self {
        Self {
            image,
            capacity,
            blocks: BTreeMap::new(),
            stats: WritebackProfile::default(),
        }
    }
    pub fn profile(&self) -> WritebackProfile {
        let mut p = self.stats.clone();
        p.pending_bytes = self.blocks.len() * BLOCK;
        p
    }
    pub async fn logical_data_bytes(&self) -> Result<u64> {
        let mut bytes = self.image.data_stat().await?.valid_data_size;
        // Pending entries replace complete logical blocks. Count only their
        // additional mapped bytes, without draining the cache or reading payloads.
        for (&offset, block) in &self.blocks {
            let replaced = self
                .image
                .mapped_data_bytes_in_range(offset, block.len() as u64)
                .await?;
            bytes = bytes
                .checked_sub(replaced)
                .context("invalid mapped byte count")?
                + block.len() as u64;
        }
        Ok(bytes)
    }
    pub async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut dst = vec![0; len];
        let n = self.read_at_into(offset, &mut dst).await?;
        dst.truncate(n);
        Ok(dst)
    }
    pub async fn read_at_into(&self, offset: u64, dst: &mut [u8]) -> Result<usize> {
        if self.blocks.is_empty() {
            return self.image.read_at_into(offset, dst).await;
        }
        let mut done = 0;
        while done < dst.len() {
            let at = offset + done as u64;
            let base = at / BLOCK as u64 * BLOCK as u64;
            let skip = (at - base) as usize;
            let n = (BLOCK - skip).min(dst.len() - done);
            if let Some(block) = self.blocks.get(&base) {
                dst[done..done + n].copy_from_slice(&block[skip..skip + n]);
                done += n;
            } else {
                // Coalesce clean runs instead of splitting every read into 4 KiB I/O.
                let end = offset + dst.len() as u64;
                let next = self
                    .blocks
                    .range(at..)
                    .next()
                    .map_or(end, |(&k, _)| k.min(end));
                let n = (next - at) as usize;
                ensure!(
                    self.image
                        .read_at_into(at, &mut dst[done..done + n])
                        .await?
                        == n,
                    "short writeback backing read"
                );
                done += n;
            }
        }
        Ok(done)
    }
    pub async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<usize> {
        self.stats.accepted_bytes += data.len() as u64;
        if self.capacity == 0 || data.len() >= 64 * 1024 {
            // Large requests already provide a compression/I/O batch. Avoid
            // copying a sequential stream through thousands of cache entries.
            self.drain().await?;
            let started = Instant::now();
            let n = self.image.write_at(offset, data).await?;
            self.stats.backing_bytes += n as u64;
            self.stats.backing_writes += 1;
            self.stats.drain_ns += started.elapsed().as_nanos() as u64;
            return Ok(n);
        }
        let mut done = 0;
        while done < data.len() {
            let at = offset + done as u64;
            let base = at / BLOCK as u64 * BLOCK as u64;
            let skip = (at - base) as usize;
            let size = (self.image.size_bytes() - base).min(BLOCK as u64) as usize;
            let n = (size - skip).min(data.len() - done);
            if self.blocks.contains_key(&base) {
                self.stats.overwritten_blocks += 1;
            } else {
                if self.blocks.len() * BLOCK >= self.capacity {
                    // Bound eviction work charged to one small write. Full
                    // drains made rare requests absorb the entire 4 MiB cache.
                    self.drain_blocks(16).await?;
                }
                let mut block = vec![0; size];
                if skip != 0 || n != size {
                    ensure!(
                        self.image.read_at_into(base, &mut block).await? == size,
                        "short writeback RMW read"
                    );
                }
                self.blocks.insert(base, block);
            }
            self.blocks.get_mut(&base).unwrap()[skip..skip + n]
                .copy_from_slice(&data[done..done + n]);
            done += n;
            self.stats.peak_pending_bytes =
                self.stats.peak_pending_bytes.max(self.blocks.len() * BLOCK);
        }
        Ok(data.len())
    }
    pub async fn drain(&mut self) -> Result<()> {
        self.drain_blocks(usize::MAX).await
    }
    async fn drain_blocks(&mut self, mut budget: usize) -> Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }
        let started = Instant::now();
        while let Some((&start, _)) = self.blocks.first_key_value() {
            let mut bytes = Vec::new();
            let mut keys = Vec::new();
            for (&at, block) in self.blocks.range(start..) {
                if keys.len() >= budget
                    || at != start + bytes.len() as u64
                    || bytes.len() + block.len() > crate::MAX_IO
                {
                    break;
                }
                bytes.extend_from_slice(block);
                keys.push(at);
            }
            // Retain entries on failure (or future cancellation); only completed
            // backing writes may remove dirty data from the cache.
            ensure!(
                self.image.write_at(start, &bytes).await? == bytes.len(),
                "short writeback drain"
            );
            self.stats.backing_bytes += bytes.len() as u64;
            self.stats.backing_writes += 1;
            budget -= keys.len();
            for key in keys {
                self.blocks.remove(&key);
            }
            if budget == 0 {
                break;
            }
        }
        self.stats.drain_ns += started.elapsed().as_nanos() as u64;
        Ok(())
    }
    pub async fn drain_and_sync(&mut self) -> Result<()> {
        self.drain().await?;
        self.image.sync().await
    }
}
