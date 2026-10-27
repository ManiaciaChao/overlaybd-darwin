//! Policy adapter over pinned AgentENV ImageFile/ImageService.
pub mod remote;
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
use tokio::sync::Mutex;

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

/// The mutex serializes RMW, reads, flush and export. Store owns the process lease.
pub struct BlockImage {
    geometry: Geometry,
    inner: Mutex<ImageFile>,
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
        let index = open("upper.index")?;
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

impl BlockImage {
    pub async fn create_upper(dir: &Path, geometry: Geometry) -> Result<()> {
        geometry.validate()?;
        std::fs::create_dir(dir)?;
        let data = dir.join("upper.data");
        let index = dir.join("upper.index");
        reserve(&data)?;
        reserve(&index)?;
        prepare_runtime_upper(
            &data,
            Some(&index),
            geometry.virtual_size,
            UpperMode::HybridLogStructured,
        )?;
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
        regular(&data)?;
        regular(&index)?;
        overlaybd::index_file::validate_rw_header_pair_paths(
            &data,
            Some(&index),
            geometry.virtual_size,
            RwLayout::HybridLogStructured,
        )?;
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
        let inner = ImageFile::open(config, service, None)
            .await
            .context("open local Hybrid image using AgentENV ImageFile")?;
        ensure!(
            inner.size_bytes() == geometry.virtual_size,
            "image geometry mismatch"
        );
        Ok(Self {
            geometry,
            inner: Mutex::new(inner),
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
                mode: Some(UpperMode::HybridLogStructured),
                data: path_string(&upper.join("upper.data"))?,
                index: path_string(&upper.join("upper.index"))?,
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
        let inner = self.inner.lock().await;
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
        let start = offset / 512 * 512;
        let end = (offset + data.len() as u64).div_ceil(512) * 512;
        let inner = self.inner.lock().await;
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
    pub async fn flush(&self) -> Result<()> {
        let entered = Instant::now();
        let inner = self.inner.lock().await;
        let lock_wait_ns = entered.elapsed().as_nanos() as u64;
        if !self.dirty.load(Ordering::Relaxed) {
            self.flush_profile.lock().await.skipped_clean += 1;
            return Ok(());
        }
        let start = Instant::now();
        inner.sync().await?;
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
    pub async fn flush_profile(&self) -> FlushProfile {
        self.flush_profile.lock().await.clone()
    }
    /// Non-destructive export: crash before registry commit leaves old upper usable.
    /// Copies the valid upper data; does not claim constant-time sealing.
    pub async fn export_upper(&self, path: &Path) -> Result<()> {
        let inner = self.inner.lock().await;
        inner.sync().await?;
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
        let inner = self.inner.lock().await;
        // Invalidate BEFORE Upper can be renamed, including failed restacks.
        self.durability_files.lock().await.take();
        // Restack replaces the upper files; keep later barriers conservative even
        // when this operation fails partway through publication.
        self.dirty.store(true, Ordering::Relaxed);
        inner.sync().await?;
        durable_file(&self.upper.join("upper.data"))?;
        durable_file(&self.upper.join("upper.index"))?;
        inner.create_snapshot_and_restack(path).await?;
        inner.sync().await?;
        durable_file(path)?;
        durable_file(&self.upper.join("upper.data"))?;
        durable_file(&self.upper.join("upper.index"))?;
        sync_directory(&self.upper)?;
        sync_directory(path.parent().context("layer has no parent")?)?;
        Ok(())
    }
    pub async fn valid_data_bytes(&self) -> Result<u64> {
        Ok(self.inner.lock().await.data_stat().await?.valid_data_size)
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
    image.flatten(dest.clone()).await?;
    dest.sync().await?;
    durable_file(output)?;
    ensure!(
        archive_layer_size(output).await? == geometry.virtual_size,
        "compacted geometry mismatch"
    );
    Ok(())
}
