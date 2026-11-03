//! Policy adapter over pinned AgentENV ImageFile/ImageService.
pub mod remote;
mod writeback;
use anyhow::{bail, ensure, Context, Result};
use overlaybd::{
    backend::local::LocalFile,
    config::{ImageConfig, LayerConfig, UpperConfig, UpperMode},
    helper::prepare_runtime_upper,
    image_file::ImageFile,
    index_file::{CommitArgs, RwLayout},
    virtual_file::VirtualFile,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::{Mutex, RwLock};
use writeback::BufferedImage;
pub use writeback::WritebackProfile;

pub const MAX_IO: usize = 4 * 1024 * 1024;
// Upstream seal currently queries the index with a u32 sector count.
pub const MAX_CAPACITY: u64 = u32::MAX as u64 * 512;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Geometry {
    pub virtual_size: u64,
    pub logical_block_size: u32,
}
impl Geometry {
    pub fn validate(self) -> Result<Self> {
        ensure!(
            matches!(self.logical_block_size, 512 | 4096),
            "logical block size must be 512 or 4096"
        );
        ensure!(
            self.virtual_size > 0
                && self.virtual_size <= MAX_CAPACITY
                && self
                    .virtual_size
                    .is_multiple_of(self.logical_block_size as u64),
            "invalid or unsupported capacity"
        );
        Ok(self)
    }
}

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub write_zeroes: bool,
    pub discard: bool,
    pub atomic_block_write: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FlushProfile {
    pub completed: u64,
    pub skipped_clean: u64,
    pub shared_device_flushes: u64,
    pub descriptor_opens: u64,
    pub total_ns: u64,
    pub lock_wait_ns: u64,
    pub prepare_ns: u64,
    pub open_ns: u64,
    pub metadata_ns: u64,
    pub upstream_ns: u64,
    pub data_ns: u64,
    pub index_ns: u64,
}

/// Reads share the operation gate. Writes/RMW, flush and export are exclusive.
/// Store owns the process lease; packed uses its own range and lifecycle gates.
pub struct BlockImage {
    geometry: Geometry,
    inner: RwLock<BufferedImage>,
    packed: Option<Arc<ImageFile>>,
    flush_profile: Mutex<FlushProfile>,
    // Always acquired after inner; invalidated BEFORE upstream restack.
    durability_files: Mutex<Option<DurabilityFiles>>,
    // Accessed only under inner: successful barriers cover all preceding writes.
    dirty: AtomicBool,
    upper: PathBuf,
    writable: bool,
}

fn reserve(path: &Path) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    Ok(())
}
fn regular(path: &Path) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_file(),
        "expected regular file: {}",
        path.display()
    );
    Ok(())
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

/// fsync plus Apple's hardware-cache flush. A failure is never reported as durable.
pub fn durable_file(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    durable_descriptor(&file)
}

fn durable_descriptor(file: &File) -> Result<()> {
    file.sync_all()?;
    hardware_barrier(file)
}

fn hardware_barrier(_file: &File) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: valid borrowed descriptor, F_FULLFSYNC takes no third argument.
        if unsafe { libc::fcntl(_file.as_raw_fd(), libc::F_FULLFSYNC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

/// Upper descriptors are owned under inner; restack invalidates them first.
struct DurabilityFiles {
    data: File,
    index: File,
    shared_apfs: bool,
}
impl DurabilityFiles {
    fn open(upper: &Path) -> Result<(Self, u64, u64)> {
        let opened = Instant::now();
        let open = |name| -> Result<File> {
            Ok(OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(upper.join(name))?)
        };
        let data = open("upper.data")?;
        let index = open(if matches!(upper_compression(upper)?, "none" | "vdo-lz4") {
            "upper.index"
        } else {
            "upper.zlog"
        })?;
        let open_ns = opened.elapsed().as_nanos() as u64;
        let metadata_start = Instant::now();
        #[cfg(target_os = "macos")]
        let shared_apfs = {
            use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
            let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
            // SAFETY: valid descriptor and writable statfs storage.
            if unsafe { libc::fstatfs(index.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let stat = unsafe { stat.assume_init() };
            // SAFETY: kernel-populated fixed NUL-terminated filesystem type.
            let apfs = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_bytes()
                == b"apfs";
            apfs && data.metadata()?.dev() == index.metadata()?.dev()
        };
        #[cfg(not(target_os = "macos"))]
        let shared_apfs = false;
        Ok((
            Self {
                data,
                index,
                shared_apfs,
            },
            open_ns,
            metadata_start.elapsed().as_nanos() as u64,
        ))
    }
    /// ONLY after upstream fsync of both files under inner. macOS fcntl(2)
    /// guarantees F_FULLFSYNC persists prior fsync'd data on the same device.
    /// Other filesystems retain both complete barriers.
    fn after_upstream_sync(&self) -> Result<(u64, u64)> {
        if self.shared_apfs {
            let start = Instant::now();
            hardware_barrier(&self.index)?;
            return Ok((0, start.elapsed().as_nanos() as u64));
        }
        let start = Instant::now();
        durable_descriptor(&self.data)?;
        let data_ns = start.elapsed().as_nanos() as u64;
        let start = Instant::now();
        durable_descriptor(&self.index)?;
        Ok((data_ns, start.elapsed().as_nanos() as u64))
    }
}

pub fn packed_policy(selector:&str)->Result<overlaybd::packed::Policy> {
    let codec=selector.strip_prefix("packed-").context("invalid packed selector")?;
    let dedupe=codec.ends_with("-dedupe");let codec=codec.strip_suffix("-dedupe").unwrap_or(codec);
    Ok(overlaybd::packed::Policy {compression:match codec {"none"=>overlaybd::packed::Codec::None,"lz4"=>overlaybd::packed::Codec::Lz4,"zstd"=>overlaybd::packed::Codec::Zstd,_=>bail!("invalid packed compression")},dedupe})
}
pub fn valid_compression(value:&str)->bool {matches!(value,"none"|"lz4"|"zstd"|"vdo-lz4") || packed_policy(value).is_ok()}
impl BlockImage {
    pub async fn create_upper(dir: &Path, geometry: Geometry) -> Result<()> {
        Self::create_upper_compressed(dir, geometry, "none").await
    }
    pub async fn create_upper_compressed(
        dir: &Path,
        geometry: Geometry,
        compression: &str,
    ) -> Result<()> {
        ensure!(
            valid_compression(compression),
            "invalid upper compression"
        );
        geometry.validate()?;
        std::fs::create_dir(dir)?;
        if compression.starts_with("packed-") {
            let policy=packed_policy(compression)?;
            overlaybd::packed::PackedUpper::create(&dir.join("upper.packed"),geometry.virtual_size,policy)?;
            std::fs::write(dir.join("upper.policy"),serde_json::to_vec(&policy)?)?;
            durable_file(&dir.join("upper.policy"))?;sync_directory(dir)?;return Ok(());
        }
        let data = dir.join("upper.data");
        let index = dir.join("upper.index");
        reserve(&data)?;
        reserve(&index)?;
        prepare_runtime_upper(
            &data,
            Some(&index),
            geometry.virtual_size,
            if matches!(compression, "none" | "vdo-lz4") {
                UpperMode::HybridLogStructured
            } else {
                UpperMode::LogStructured
            },
        )?;
        if compression == "vdo-lz4" {
            overlaybd::backend::vdo::VdoFile::initialize_async(&data, geometry.virtual_size).await?;
            durable_file(&overlaybd::backend::vdo::backing_path(&data))?;
        } else if compression != "none" {
            overlaybd::zfile::writable::WritableZFile::initialize(&data, &index, compression)?;
            durable_file(&overlaybd::zfile::writable::journal_path(&data))?;
        }
        durable_file(&data)?;
        durable_file(&index)?;
        sync_directory(dir)?;
        Ok(())
    }

    /// Layers are ordered oldest to newest. Never creates missing files on reopen.
    pub async fn open(
        upper: &Path,
        layers: &[PathBuf],
        geometry: Geometry,
        writable: bool,
    ) -> Result<Self> {
        Self::open_config(
            upper,
            Self::local_config(upper, layers)?,
            geometry,
            writable,
        )
        .await
    }
    pub async fn open_config(
        upper: &Path,
        config: ImageConfig,
        geometry: Geometry,
        writable: bool,
    ) -> Result<Self> {
        geometry.validate()?;
        ensure!(
            config.lowers.len() < overlaybd::index_file::MAX_STACK_LAYERS,
            "layer limit reached; compact before adding a layer"
        );
        let data = upper.join("upper.data");
        let index = upper.join("upper.index");
        let is_packed=upper.join("upper.packed").try_exists()?;
        if is_packed {regular(&upper.join("upper.packed"))?;regular(&upper.join("upper.policy"))?;} else {
        regular(&data)?;
        regular(&index)?;
        if let Some(capacity) = overlaybd::backend::vdo::capacity(&data)? {
            ensure!(capacity == (geometry.virtual_size + 4096).div_ceil(4096) * 4096,
                "VDO geometry mismatch");
            regular(&overlaybd::backend::vdo::backing_path(&data))?;
            // Virtual data/index headers are validated together by ImageFile::open_upper.
        } else if overlaybd::zfile::writable::codec(&data)?.is_some() {
            regular(&overlaybd::zfile::writable::journal_path(&data))?;
        } else {
            overlaybd::index_file::validate_rw_header_pair_paths(
                &data,
                Some(&index),
                geometry.virtual_size,
                match config.upper.mode {
                    Some(UpperMode::LogStructured) => RwLayout::LogStructured,
                    _ => RwLayout::HybridLogStructured,
                },
            )?;
        }
        }
        for layer in &config.lowers {
            if !layer.file.is_empty() {
                ensure!(
                    archive_layer_size(Path::new(&layer.file)).await? == geometry.virtual_size,
                    "lower geometry mismatch"
                );
            } else {
                remote::RemoteLayer::from_config(layer)?.validate()?;
            }
        }
        let service = remote::service(
            &upper.join("index-cache"),
            config.lowers.iter().any(|l| l.file.is_empty()),
        )
        .await?;
        let inner=Arc::new(if is_packed {
            ImageFile::open_packed(config,service,packed_policy(upper_compression(upper)?)?).await?
        } else {ImageFile::open(config,service,None).await.context("open local image using AgentENV ImageFile")?});
        ensure!(
            inner.size_bytes() == geometry.virtual_size,
            "image geometry mismatch"
        );
        Ok(Self {
            geometry,
            packed: is_packed.then(||inner.clone()),
            inner: RwLock::new(BufferedImage::new(
                inner,
                if !is_packed && upper_compression(upper)? != "none" {
                    MAX_IO
                } else {
                    0
                },
            )),
            flush_profile: Mutex::new(FlushProfile::default()),
            durability_files: Mutex::new(None),
            // Reopen conservatively: no assumption about another owner's last flush.
            dirty: AtomicBool::new(true),
            upper: upper.to_owned(),
            writable,
        })
    }
    /// Upstream ImageConfig, not a parallel layer-configuration schema.
    pub fn local_config(upper: &Path, layers: &[PathBuf]) -> Result<ImageConfig> {
        fn path_string(path: &Path) -> Result<String> {
            Ok(path
                .to_str()
                .context("image paths must be UTF-8")?
                .to_owned())
        }
        let packed=upper.join("upper.packed").try_exists()?;
        Ok(ImageConfig {
            lowers: layers
                .iter()
                .map(|path| {
                    Ok(LayerConfig {
                        file: path_string(path)?,
                        ..LayerConfig::default()
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            upper: UpperConfig {
                mode: Some(
                    if !packed && overlaybd::zfile::writable::codec(&upper.join("upper.data"))?.is_some() {
                        UpperMode::LogStructured
                    } else {
                        UpperMode::HybridLogStructured
                    },
                ),
                data: path_string(&upper.join(if packed {"upper.packed"}else{"upper.data"}))?,
                index: if packed {String::new()}else{path_string(&upper.join("upper.index"))?},
                ..UpperConfig::default()
            },
            ..ImageConfig::default()
        })
    }
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            write_zeroes: self.writable,
            discard: false,
            atomic_block_write: false,
        }
    }
    fn range(&self, offset: u64, len: usize) -> Result<()> {
        ensure!(len <= MAX_IO, "request exceeds 4 MiB");
        ensure!(
            offset
                .checked_add(len as u64)
                .is_some_and(|end| end <= self.geometry.virtual_size),
            "I/O beyond fixed capacity"
        );
        Ok(())
    }
    pub async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; len.min(MAX_IO)];
        ensure!(len <= MAX_IO, "request exceeds 4 MiB");
        let n = self.read_at_into(offset, &mut data).await?;
        data.truncate(n);
        Ok(data)
    }
    /// Aligned reads land directly in the caller's buffer through upstream VirtualFile.
    pub async fn read_at_into(&self, offset: u64, dst: &mut [u8]) -> Result<usize> {
        ensure!(dst.len() <= MAX_IO, "request exceeds 4 MiB");
        if offset >= self.geometry.virtual_size || dst.is_empty() {
            return Ok(0);
        }
        let len = dst
            .len()
            .min((self.geometry.virtual_size - offset) as usize);
        if let Some(image)=&self.packed {return image.read_at_into(offset,&mut dst[..len]).await;}
        let inner = self.inner.read().await;
        if offset.is_multiple_of(512) && len.is_multiple_of(512) {
            let n = inner.read_at_into(offset, &mut dst[..len]).await?;
            ensure!(n == len, "short backing read");
        } else {
            let start = offset / 512 * 512;
            let end = (offset + len as u64).div_ceil(512) * 512;
            let bytes = inner.read_at(start, (end - start) as usize).await?;
            ensure!(bytes.len() == (end - start) as usize, "short backing read");
            let skip = (offset - start) as usize;
            dst[..len].copy_from_slice(&bytes[skip..skip + len]);
        }
        Ok(len)
    }
    pub async fn write_at(&self, offset: u64, data: &[u8]) -> Result<()> {
        ensure!(self.writable, "EROFS: image is read-only");
        self.range(offset, data.len())?;
        if data.is_empty() {
            return Ok(());
        }
        if let Some(image)=&self.packed {ensure!(image.write_at(offset,data).await?==data.len(),"short packed write");return Ok(());}
        let start = offset / 512 * 512;
        let end = (offset + data.len() as u64).div_ceil(512) * 512;
        let mut inner = self.inner.write().await;
        // Mark before mutation: partial/failed writes still require a barrier.
        self.dirty.store(true, Ordering::Relaxed);
        if start == offset && end == offset + data.len() as u64 {
            ensure!(
                inner.write_at(offset, data).await? == data.len(),
                "short backing write"
            );
        } else {
            let mut block = inner.read_at(start, (end - start) as usize).await?.to_vec();
            ensure!(block.len() == (end - start) as usize, "short RMW read");
            let skip = (offset - start) as usize;
            block[skip..skip + data.len()].copy_from_slice(data);
            ensure!(
                inner.write_at(start, &block).await? == block.len(),
                "short RMW write"
            );
        }
        Ok(())
    }
    pub async fn write_zeroes(&self, offset: u64, len: usize) -> Result<()> {
        self.range(offset, len)?;
        self.write_at(offset, &vec![0; len]).await
    }
    pub fn discard(&self, _offset: u64, _len: u64) -> Result<()> {
        bail!("ENOTSUP: discard is not validated")
    }
    /// Explicit discard for packed and experimental VDO uppers.
    /// The legacy synchronous API stays unsupported.
    pub async fn discard_vdo(&self, offset: u64, len: usize) -> Result<()> {
        self.range(offset, len)?;
        ensure!(self.writable, "read-only image");
        if let Some(image) = &self.packed {
            return image.discard(offset, len as u64).await;
        }
        ensure!(upper_compression(&self.upper)? == "vdo-lz4", "discard requires packed or VDO upper");
        ensure!(offset % 512 == 0 && len % 512 == 0, "unaligned discard");
        let mut inner = self.inner.write().await;
        self.dirty.store(true, Ordering::Relaxed);
        inner.drain().await?;
        inner.discard(offset, len as u64).await
    }
    pub async fn vdo_space_usage(&self) -> Result<Option<overlaybd::backend::vdo::SpaceUsage>> {
        self.inner.read().await.vdo_space_usage().await
    }
    pub async fn flush(&self) -> Result<()> {
        let entered = Instant::now();
        if let Some(image)=&self.packed {
            image.sync().await?;
            let mut p=self.flush_profile.lock().await;p.completed+=1;p.total_ns+=entered.elapsed().as_nanos()as u64;return Ok(());
        }
        let mut inner = self.inner.write().await;
        let lock_wait_ns = entered.elapsed().as_nanos() as u64;
        if !self.dirty.load(Ordering::Relaxed) {
            self.flush_profile.lock().await.skipped_clean += 1;
            return Ok(());
        }
        let start = Instant::now();
        inner.drain_and_sync().await?;
        let upstream_ns = start.elapsed().as_nanos() as u64;
        let start = Instant::now();
        let mut cached = self.durability_files.lock().await;
        let (open_ns, metadata_ns, descriptor_opens) = if cached.is_none() {
            let (files, open_ns, metadata_ns) = DurabilityFiles::open(&self.upper)?;
            *cached = Some(files);
            (open_ns, metadata_ns, 1)
        } else {
            (0, 0, 0)
        };
        let files = cached.as_ref().unwrap();
        let shared = files.shared_apfs;
        let (data_ns, index_ns) = files.after_upstream_sync()?;
        let prepare_ns = (start.elapsed().as_nanos() as u64).saturating_sub(data_ns + index_ns);
        let total_ns = entered.elapsed().as_nanos() as u64;
        // Clear only after upstream sync and all required hardware barriers succeed.
        self.dirty.store(false, Ordering::Relaxed);
        let mut profile = self.flush_profile.lock().await;
        profile.completed += 1;
        profile.descriptor_opens += descriptor_opens;
        profile.total_ns += total_ns;
        profile.lock_wait_ns += lock_wait_ns;
        profile.prepare_ns += prepare_ns;
        profile.open_ns += open_ns;
        profile.metadata_ns += metadata_ns;
        profile.shared_device_flushes += u64::from(shared);
        profile.upstream_ns += upstream_ns;
        profile.data_ns += data_ns;
        profile.index_ns += index_ns;
        Ok(())
    }
    /// Bound plaintext writeback per open image. Zero disables it. Changing the
    /// limit drains existing writes but does not replace the explicit flush barrier.
    pub async fn set_writeback_capacity(&self, bytes: usize) -> Result<()> {
        if self.packed.is_some() {ensure!(bytes==0,"packed upper owns its bounded buffer");return Ok(());}
        ensure!(
            bytes == 0 || (4096..=16 * MAX_IO).contains(&bytes) && bytes.is_multiple_of(4096),
            "invalid writeback capacity"
        );
        let mut inner = self.inner.write().await;
        inner.drain().await?;
        inner.capacity = bytes;
        Ok(())
    }
    pub async fn compression_write_profile(
        &self,
    ) -> Option<overlaybd::zfile::writable::WriteProfile> {
        self.inner.read().await.compression_write_profile().await
    }
    pub async fn writeback_profile(&self) -> WritebackProfile {
        self.inner.read().await.profile()
    }
    pub async fn flush_profile(&self) -> FlushProfile {
        self.flush_profile.lock().await.clone()
    }
    /// Non-destructive export: crash before registry commit leaves old upper usable.
    /// Copies the valid upper data; does not claim constant-time sealing.
    pub async fn export_upper(&self, path: &Path) -> Result<()> {
        if let Some(image)=&self.packed {
            reserve(path)?;let out:Arc<dyn VirtualFile>=Arc::new(LocalFile::open_rw(path,false)?);
            image.export_upper_as_sealed(CommitArgs::new(out.clone())).await?;out.sync().await?;
            durable_file(path)?;sync_directory(path.parent().context("layer parent")?)?;return Ok(());
        }
        let mut inner = self.inner.write().await;
        inner.drain_and_sync().await?;
        if matches!(upper_compression(&self.upper)?, "lz4" | "zstd") {
            ensure!(!path.try_exists()?, "snapshot output already exists");
            let scratch = tempfile::tempdir_in(path.parent().context("snapshot parent")?)?;
            let data = scratch.path().join("upper.data");
            let index = scratch.path().join("upper.index");
            for name in ["upper.data", "upper.index", "upper.zlog"] {
                std::fs::copy(self.upper.join(name), scratch.path().join(name))?;
            }
            let compressed = Arc::new(overlaybd::zfile::writable::WritableZFile::open(
                &data, &index,
            )?);
            let file = overlaybd::index_file::open_file_rw(
                compressed,
                Some(Arc::new(LocalFile::open_rw(&index, false)?)),
            )
            .await?;
            file.close_seal().await?;
            // Hard-link publication fails if the destination already exists.
            std::fs::hard_link(&data, path)?;
            durable_file(path)?;
            sync_directory(path.parent().context("snapshot parent")?)?;
            return Ok(());
        }
        reserve(path)?;
        let output: Arc<dyn VirtualFile> = Arc::new(LocalFile::open_rw(path, false)?);
        inner
            .export_upper_as_sealed(CommitArgs::new(output.clone()))
            .await?;
        output.sync().await?;
        durable_file(path)?;
        sync_directory(path.parent().context("layer has no parent")?)?;
        Ok(())
    }
    /// In-place upstream restack. Caller MUST persist an intent and block all
    /// further access if this fails: upstream may already have sealed/renamed
    /// the Upper. This method deliberately does not invent a second algorithm.
    pub async fn restack_upper(&self, path: &Path) -> Result<()> {
        ensure!(self.writable, "EROFS: image is read-only");
        ensure!(!path.try_exists()?, "snapshot output already exists");
        if let Some(image)=&self.packed {image.create_snapshot_and_restack(path).await?;image.sync().await?;sync_directory(&self.upper)?;return Ok(());}
        let mut inner = self.inner.write().await;
        // Invalidate BEFORE Upper can be renamed, including failed restacks.
        self.durability_files.lock().await.take();
        // Restack replaces the upper files; keep later barriers conservative even
        // when this operation fails partway through publication.
        self.dirty.store(true, Ordering::Relaxed);
        inner.drain_and_sync().await?;
        durable_file(&self.upper.join("upper.data"))?;
        durable_file(&self.upper.join("upper.index"))?;
        inner.create_snapshot_and_restack(path).await?;
        inner.drain_and_sync().await?;
        durable_file(path)?;
        durable_file(&self.upper.join("upper.data"))?;
        durable_file(&self.upper.join("upper.index"))?;
        sync_directory(&self.upper)?;
        sync_directory(path.parent().context("layer has no parent")?)?;
        Ok(())
    }
    pub async fn packed_stats(&self)->Result<Option<overlaybd::packed::Stats>> {
        if let Some(image)=&self.packed {image.packed_stats().await}else{Ok(None)}
    }
    pub async fn valid_data_bytes(&self) -> Result<u64> {
        if let Some(image)=&self.packed {return Ok(image.data_stat().await?.valid_data_size);}
        let mut inner = self.inner.write().await;
        inner.drain().await?;
        Ok(inner.data_stat().await?.valid_data_size)
    }
    pub async fn sampled_logical_data_bytes(&self) -> Result<u64> {
        if let Some(image) = &self.packed {
            return Ok(image.data_stat().await?.valid_data_size);
        }
        self.inner.read().await.logical_data_bytes().await
    }
}

#[cfg(all(test, target_os = "macos"))]
mod durability_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn geometry() -> Geometry {
        Geometry {
            virtual_size: 1024 * 1024,
            logical_block_size: 512,
        }
    }

    #[tokio::test]
    async fn logical_sampling_counts_pending_overlays_without_draining_or_flushing() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), "lz4")
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.sampled_logical_data_bytes().await.unwrap(), 0);
        h.write_at(511, &[1, 2, 3]).await.unwrap();
        let before = h.writeback_profile().await;
        assert!(before.pending_bytes > 0);
        assert_eq!(h.sampled_logical_data_bytes().await.unwrap(), 4096);
        assert_eq!(
            h.writeback_profile().await.pending_bytes,
            before.pending_bytes
        );
        assert_eq!(
            h.writeback_profile().await.backing_writes,
            before.backing_writes
        );
        assert_eq!(h.flush_profile().await.completed, 0);
        h.flush().await.unwrap();
        assert_eq!(h.sampled_logical_data_bytes().await.unwrap(), 4096);
        h.write_at(511, &[4, 5, 6]).await.unwrap();
        h.write_at(4096 + 511, &[7, 8, 9]).await.unwrap();
        let before = h.writeback_profile().await;
        assert_eq!(h.sampled_logical_data_bytes().await.unwrap(), 8192);
        assert_eq!(
            h.writeback_profile().await.pending_bytes,
            before.pending_bytes
        );
        assert_eq!(
            h.writeback_profile().await.backing_writes,
            before.backing_writes
        );
        assert_eq!(h.flush_profile().await.completed, 1);
        h.flush().await.unwrap();
        assert_eq!(h.sampled_logical_data_bytes().await.unwrap(), 8192);
    }

    #[tokio::test]
    async fn failed_hardware_barrier_stays_dirty_and_can_retry() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper(&upper, geometry()).await.unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        h.flush().await.unwrap();
        h.write_at(511, &[17, 23, 41]).await.unwrap();
        let saved = h.durability_files.lock().await.take();
        // Real syscall failure, after upstream sync: /dev/null rejects F_FULLFSYNC.
        *h.durability_files.lock().await = Some(DurabilityFiles {
            data: File::open(upper.join("upper.data")).unwrap(),
            index: OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null")
                .unwrap(),
            shared_apfs: true,
        });
        assert!(h.flush().await.is_err());
        assert!(h.flush().await.is_err());
        assert!(h.dirty.load(Ordering::Relaxed));
        assert_eq!(h.flush_profile().await.completed, 1);
        assert_eq!(h.flush_profile().await.skipped_clean, 0);
        *h.durability_files.lock().await = saved;
        h.flush().await.unwrap();
        h.flush().await.unwrap();
        assert_eq!(h.flush_profile().await.completed, 2);
        assert_eq!(h.flush_profile().await.skipped_clean, 1);
        drop(h);
        let reopened = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(reopened.read_at(511, 3).await.unwrap(), [17, 23, 41]);
    }

    #[tokio::test]
    async fn restack_rebinds_durability_descriptors_to_new_upper() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper(&upper, geometry()).await.unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        h.write_at(0, &[7; 4096]).await.unwrap();
        h.flush().await.unwrap();
        let old = h
            .durability_files
            .lock()
            .await
            .as_ref()
            .unwrap()
            .data
            .metadata()
            .unwrap()
            .ino();
        h.restack_upper(&dir.path().join("layer")).await.unwrap();
        assert!(h.durability_files.lock().await.is_none());
        h.write_at(0, &[9; 4096]).await.unwrap();
        h.flush().await.unwrap();
        let cached = h.durability_files.lock().await;
        let files = cached.as_ref().unwrap();
        assert_ne!(files.data.metadata().unwrap().ino(), old);
        assert_eq!(
            files.data.metadata().unwrap().ino(),
            std::fs::metadata(upper.join("upper.data")).unwrap().ino()
        );
        assert_eq!(
            files.index.metadata().unwrap().ino(),
            std::fs::metadata(upper.join("upper.index")).unwrap().ino()
        );
    }
}

/// Use the pinned upstream configuration and layer metadata parser for imports.
pub use overlaybd::config::ImageConfig as UpstreamImageConfig;
pub fn sealed_layer_size(path: &Path) -> Result<u64> {
    regular(path)?;
    overlaybd::layer_metadata::read_overlaybd_layer_virtual_size(path)
}

/// Open raw or ZFile-wrapped sealed LSMT using the upstream parsers.
async fn decoded_layer(path: &Path) -> Result<Arc<dyn VirtualFile>> {
    regular(path)?;
    let file: Arc<dyn VirtualFile> = Arc::new(LocalFile::open_ro(path)?);
    if overlaybd::zfile::is_zfile(file.clone()).await? == 1 {
        overlaybd::zfile::zfile_open_ro_vfile(file, true).await
    } else {
        Ok(file)
    }
}

pub async fn archive_layer_size(path: &Path) -> Result<u64> {
    let layer = overlaybd::index_file::LSMTReadOnlyFile::open(decoded_layer(path).await?).await?;
    layer.size().await
}

pub async fn compress_layer(source: &Path, dest: &Path, codec: &str) -> Result<()> {
    use overlaybd::zfile::{zfile_compress, CompressArgs, CompressOptions};
    let algo = match codec {
        "lz4" => CompressOptions::LZ4,
        "zstd" => CompressOptions::ZSTD,
        _ => bail!("unsupported compression: {codec}"),
    };
    let size = archive_layer_size(source).await?;
    // Decode an already compressed source on demand; never nest ZFile wrappers.
    let source = decoded_layer(source).await?;
    reserve(dest)?;
    let output: Arc<dyn VirtualFile> = Arc::new(LocalFile::open_rw(dest, false)?);
    let args = CompressArgs::new(CompressOptions::new(algo, 65536, 1));
    zfile_compress(source, output.clone(), &args).await?;
    output.sync().await?;
    durable_file(dest)?;
    ensure!(
        archive_layer_size(dest).await? == size,
        "compressed layer geometry mismatch"
    );
    Ok(())
}

/// Same merge/flatten implementation used by upstream ImageFile::compact.
/// Inputs are immutable sealed layers, ordered bottom to top.
pub async fn compact_layers(paths: &[PathBuf], output: &Path, geometry: Geometry) -> Result<()> {
    compact_layers_compressed(paths, output, geometry, "none").await
}
pub async fn compact_layers_compressed(
    paths: &[PathBuf],
    output: &Path,
    geometry: Geometry,
    codec: &str,
) -> Result<()> {
    geometry.validate()?;
    ensure!(!paths.is_empty(), "no layers to compact");
    let mut files = Vec::new();
    for path in paths {
        ensure!(
            archive_layer_size(path).await? == geometry.virtual_size,
            "layer geometry mismatch"
        );
        files.push(decoded_layer(path).await?);
    }
    let image = overlaybd::index_file::open_files_ro(&files).await?;
    reserve(output)?;
    let dest: Arc<dyn VirtualFile> = Arc::new(LocalFile::open_rw(output, false)?);
    if codec == "none" {
        image.flatten(dest.clone()).await?;
    } else {
        use overlaybd::zfile::{CompressArgs, CompressOptions, ZFileCompactWriter};
        let algorithm = match codec {
            "lz4" => CompressOptions::LZ4,
            "zstd" => CompressOptions::ZSTD,
            _ => bail!("invalid compression"),
        };
        let writer = ZFileCompactWriter::new(
            dest.clone(),
            &CompressArgs::new(CompressOptions::new(algorithm, 4096, 1)),
        )
        .await?;
        image
            .flatten_with_args(CommitArgs::from_writer(Arc::new(writer)))
            .await?;
    }
    dest.sync().await?;
    durable_file(output)?;
    ensure!(
        archive_layer_size(output).await? == geometry.virtual_size,
        "compacted geometry mismatch"
    );
    Ok(())
}

/// The persisted upper format, never the receiving machine's write default.
pub fn upper_compression(dir: &Path) -> Result<&'static str> {
    if dir.join("upper.packed").try_exists()? {
        regular(&dir.join("upper.policy"))?;
        let policy:overlaybd::packed::Policy=serde_json::from_slice(&std::fs::read(dir.join("upper.policy"))?)?;
        use overlaybd::packed::Codec;
        return Ok(match (policy.compression,policy.dedupe) {
            (Codec::None,false)=>"packed-none",(Codec::Lz4,false)=>"packed-lz4",(Codec::Zstd,false)=>"packed-zstd",
            (Codec::None,true)=>"packed-none-dedupe",(Codec::Lz4,true)=>"packed-lz4-dedupe",(Codec::Zstd,true)=>"packed-zstd-dedupe",
        });
    }
    if overlaybd::backend::vdo::capacity(&dir.join("upper.data"))?.is_some() {
        return Ok("vdo-lz4");
    }
    Ok(overlaybd::zfile::writable::codec(&dir.join("upper.data"))?.unwrap_or("none"))
}
