use obd_core_adapter::{BlockImage, Geometry};
fn geometry() -> Geometry {
    Geometry {
        virtual_size: 8 * 1024 * 1024,
        logical_block_size: 512,
    }
}

#[tokio::test]
async fn compressed_write_reopen_seal_and_mixed_layers() {
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        let mut expected = vec![0; 32768];
        for i in 0..80usize {
            let at = (i * 389) % 28000;
            let bytes = vec![i as u8; 1 + i * 7];
            h.write_at(at as u64, &bytes).await.unwrap();
            expected[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
        h.flush().await.unwrap();
        drop(h);
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
        let layer = dir.path().join("sealed");
        h.restack_upper(&layer).await.unwrap();
        assert_eq!(&std::fs::read(&layer).unwrap()[..8], b"ZFile\0\x01\0");
        assert_eq!(
            obd_core_adapter::archive_layer_size(&layer).await.unwrap(),
            geometry().virtual_size
        );
        h.write_at(0, &[99; 512]).await.unwrap();
        h.flush().await.unwrap();
        drop(h);
        let b = dir.path().join("b");
        BlockImage::create_upper(&b, geometry()).await.unwrap();
        let h = BlockImage::open(&b, std::slice::from_ref(&layer), geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
        h.write_at(512, &[77; 512]).await.unwrap();
        expected[512..1024].fill(77);
        let raw = dir.path().join("raw");
        h.export_upper(&raw).await.unwrap();
        drop(h);
        let a = dir.path().join("a");
        BlockImage::create_upper_compressed(&a, geometry(), codec)
            .await
            .unwrap();
        let h = BlockImage::open(&a, &[layer, raw], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
    }
}

#[tokio::test]
async fn compressed_recovery_ignores_uncommitted_tail_and_detects_corruption() {
    use std::os::unix::fs::FileExt;
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        h.write_at(512, &[10; 512]).await.unwrap();
        h.flush().await.unwrap();
        let committed_size = std::fs::metadata(upper.join("upper.data")).unwrap().len();
        h.write_at(512, &[20; 512]).await.unwrap();
        h.write_at(8192, &[30; 4096]).await.unwrap();
        drop(h);
        let log = std::fs::OpenOptions::new()
            .write(true)
            .open(upper.join("upper.zlog"))
            .unwrap();
        // A commit interrupted before the body was complete.
        log.write_all_at(&[24, 0, 0, 0, 1, 2], log.metadata().unwrap().len())
            .unwrap();
        drop(log);
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(512, 512).await.unwrap(), [10; 512]);
        assert_eq!(h.read_at(8192, 4096).await.unwrap(), [0; 4096]);
        assert_eq!(
            std::fs::metadata(upper.join("upper.data")).unwrap().len(),
            committed_size
        );
        h.write_at(1024, &[40; 512]).await.unwrap();
        h.flush().await.unwrap();
        drop(h);
        // The ordinary LSMT index is a cache; recovery rebuilds corrupted bytes.
        let index = std::fs::OpenOptions::new()
            .write(true)
            .open(upper.join("upper.index"))
            .unwrap();
        index.write_all_at(&[255; 16], 4096).unwrap();
        drop(index);
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(1024, 512).await.unwrap(), [40; 512]);
        drop(h);
        // Corrupting the authoritative committed WAL must fail closed.
        let log = std::fs::OpenOptions::new()
            .write(true)
            .open(upper.join("upper.zlog"))
            .unwrap();
        log.write_all_at(&[255; 8], 16).unwrap();
        drop(log);
        assert!(BlockImage::open(&upper, &[], geometry(), true)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn compressed_offline_export_is_non_destructive() {
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        h.write_at(0, &[42; 8192]).await.unwrap();
        h.flush().await.unwrap();
        let before = std::fs::read(upper.join("upper.data")).unwrap();
        let layer = dir.path().join("export");
        h.export_upper(&layer).await.unwrap();
        assert_eq!(std::fs::read(upper.join("upper.data")).unwrap(), before);
        let sealed = std::fs::read(&layer).unwrap();
        // Existing encoded blocks are copied, not recompressed/recontainerized.
        assert_eq!(&sealed[..before.len()], &before);
        assert_eq!(
            obd_core_adapter::archive_layer_size(&layer).await.unwrap(),
            geometry().virtual_size
        );
        h.write_at(0, &[99; 512]).await.unwrap();
        h.flush().await.unwrap();
        let child = dir.path().join("child");
        BlockImage::create_upper(&child, geometry()).await.unwrap();
        let child = BlockImage::open(&child, &[layer], geometry(), true)
            .await
            .unwrap();
        assert_eq!(child.read_at(0, 8192).await.unwrap(), [42; 8192]);
    }
}

/// Synthetic, cache-warm kernel comparison; not an FSKit/physical-SSD claim.
#[tokio::test]
#[ignore = "release-only performance experiment"]
async fn compressed_upper_benchmark() {
    use std::os::unix::fs::MetadataExt;
    use std::time::Instant;
    if cfg!(debug_assertions) {
        panic!("run this benchmark with --release");
    }
    let g = Geometry {
        virtual_size: 64 * 1024 * 1024,
        logical_block_size: 512,
    };
    let mut rows = Vec::new();
    let mut codecs = vec![
        "none",
        "raw-log",
        "raw-log-buffered",
        "lz4-direct",
        "lz4",
        "zstd-direct",
        "zstd",
    ];
    if std::env::var_os("OBD_BENCH_REVERSE").is_some() {
        codecs.reverse();
    }
    for codec in codecs {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        let h = if codec.starts_with("raw-log") {
            use overlaybd::{config::UpperMode, helper::prepare_runtime_upper};
            std::fs::create_dir(&upper).unwrap();
            for name in ["upper.data", "upper.index"] {
                std::fs::File::create(upper.join(name)).unwrap();
            }
            prepare_runtime_upper(
                &upper.join("upper.data"),
                Some(&upper.join("upper.index")),
                g.virtual_size,
                UpperMode::LogStructured,
            )
            .unwrap();
            let mut config = BlockImage::local_config(&upper, &[]).unwrap();
            config.upper.mode = Some(UpperMode::LogStructured);
            BlockImage::open_config(&upper, config, g, true)
                .await
                .unwrap()
        } else {
            BlockImage::create_upper_compressed(&upper, g, codec.trim_end_matches("-direct"))
                .await
                .unwrap();
            BlockImage::open(&upper, &[], g, true).await.unwrap()
        };
        if codec.ends_with("-direct") {
            h.set_writeback_capacity(0).await.unwrap();
        }
        if codec == "raw-log-buffered" {
            h.set_writeback_capacity(4 * 1024 * 1024).await.unwrap();
        }
        let mut payload = vec![0u8; 1024 * 1024];
        let mut rng = 42u64;
        for block in payload.chunks_mut(4096) {
            let random_bytes = if std::env::var_os("OBD_BENCH_ENTROPY").is_some() {
                4096
            } else {
                2048
            };
            for b in &mut block[..random_bytes] {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                *b = rng as u8;
            }
            for i in random_bytes..4096 {
                block[i] = block[i % 256];
            }
        }
        for op in [
            "seq_write_1m",
            "seq_read_1m",
            "random_write_4k",
            "random_read_4k",
            "hot_overwrite_4k",
            "write_4k_fsync",
        ] {
            let count = if op.starts_with("seq") || op.contains("fsync") {
                64
            } else {
                4096
            };
            let len = if op.starts_with("seq") {
                1024 * 1024
            } else {
                4096
            };
            let mut latencies = Vec::new();
            let mut read = vec![0; len];
            fn cpu_seconds() -> f64 {
                let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
                // SAFETY: writable rusage storage, process-local query.
                assert_eq!(
                    unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
                    0
                );
                let u = unsafe { usage.assume_init() };
                (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as f64
                    + (u.ru_utime.tv_usec + u.ru_stime.tv_usec) as f64 / 1e6
            }
            let writeback_before = h.writeback_profile().await;
            let compression_before = h.compression_write_profile().await;
            let cpu_start = cpu_seconds();
            let start = Instant::now();
            for i in 0..count {
                let at = if op.starts_with("seq") {
                    i * len
                } else if op == "hot_overwrite_4k" {
                    (i % 64) * 4096
                } else {
                    ((i * 7919) % (g.virtual_size as usize / 4096)) * 4096
                };
                let t = Instant::now();
                if op.contains("write") {
                    h.write_at(at as u64, &payload[..len]).await.unwrap();
                } else {
                    h.read_at_into(at as u64, &mut read).await.unwrap();
                }
                if op.contains("fsync") {
                    h.flush().await.unwrap();
                }
                latencies.push(t.elapsed().as_nanos() as u64);
            }
            let seconds = start.elapsed().as_secs_f64();
            let drain_start = Instant::now();
            h.flush().await.unwrap();
            let flush_seconds = drain_start.elapsed().as_secs_f64();
            let cpu_total = cpu_seconds() - cpu_start;
            let writeback_after = h.writeback_profile().await;
            let compression_after = h.compression_write_profile().await;
            latencies.sort_unstable();
            rows.push(serde_json::json!({"codec":codec,"workload":op,"operations":count,"cpu_seconds":cpu_total,"flush_seconds":flush_seconds,"durable_mib_per_second":count as f64*len as f64/1048576.0/(seconds+flush_seconds),"writeback_before":writeback_before,"writeback_after":writeback_after,"compression_before":compression_before,"compression_after":compression_after,"mib_per_second":count as f64*len as f64/1048576.0/seconds,"p50_us":latencies[count/2] as f64/1000.0,"p99_us":latencies[(count*99/100).min(count-1)] as f64/1000.0,"p999_us":latencies[(count*999/1000).min(count-1)] as f64/1000.0,"max_us":latencies[count-1] as f64/1000.0}));
        }
        h.flush().await.unwrap();
        let mut bytes = 0;
        let mut allocated = 0;
        for name in ["upper.data", "upper.index", "upper.zlog"] {
            if let Ok(m) = std::fs::metadata(upper.join(name)) {
                bytes += m.len();
                allocated += m.blocks() * 512;
            }
        }
        rows.push(
            serde_json::json!({"codec":codec,"file_bytes":bytes,"allocated_bytes":allocated}),
        );
    }
    println!("{}", serde_json::to_string_pretty(&rows).unwrap());
}

#[tokio::test]
async fn compressed_crash_child() {
    let Ok(path) = std::env::var("OBD_COMPRESSED_CRASH_CHILD") else {
        return;
    };
    let h = BlockImage::open(std::path::Path::new(&path), &[], geometry(), true)
        .await
        .unwrap();
    h.write_at(0, &[51; 4096]).await.unwrap();
    h.flush().await.unwrap();
    h.write_at(0, &[99; 4096]).await.unwrap();
    // Bypass destructors: test a process dying with a valid durable prefix.
    std::process::exit(77);
}

#[tokio::test]
async fn compressed_process_death_recovers_last_flush() {
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "compressed_crash_child", "--nocapture"])
            .env("OBD_COMPRESSED_CRASH_CHILD", &upper)
            .status()
            .unwrap();
        assert_eq!(child.code(), Some(77));
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, 4096).await.unwrap(), [51; 4096]);
        h.write_at(4096, &[52; 4096]).await.unwrap();
        h.flush().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "writes retained fixtures for the independent upstream probe"]
async fn compressed_interop_fixtures() {
    let root = std::path::PathBuf::from(std::env::var("OBD_COMPRESSED_FIXTURES").unwrap());
    std::fs::create_dir(&root).unwrap();
    let g = Geometry {
        virtual_size: 1024 * 1024,
        logical_block_size: 512,
    };
    for codec in ["lz4", "zstd"] {
        let dir = root.join(codec);
        std::fs::create_dir(&dir).unwrap();
        let upper = dir.join("upper");
        BlockImage::create_upper_compressed(&upper, g, codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], g, true).await.unwrap();
        let mut expected = vec![0u8; g.virtual_size as usize];
        for (at, bytes) in [
            (0, vec![51; 65536]),
            (512, vec![77; 512]),
            (4096, vec![0; 4096]),
            (g.virtual_size as usize - 512, vec![99; 512]),
        ] {
            h.write_at(at as u64, &bytes).await.unwrap();
            expected[at..at + bytes.len()].copy_from_slice(&bytes);
        }
        h.flush().await.unwrap();
        h.restack_upper(&dir.join("layer.zfile")).await.unwrap();
        std::fs::write(dir.join("expected.raw"), expected).unwrap();
        std::fs::write(
            dir.join("image.json"),
            br#"{"lowers":[{"file":"layer.zfile"}]}"#,
        )
        .unwrap();
    }
}

#[tokio::test]
async fn compressed_discard_masks_lower_after_reopen_and_seal() {
    use overlaybd::{
        backend::local::LocalFile,
        index_file::{open_file_rw, open_files_ro, stack_files, LSMTReadOnlyFile},
        virtual_file::VirtualFile,
        zfile::{writable::WritableZFile, zfile_open_ro_vfile},
    };
    use std::sync::Arc;
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        BlockImage::create_upper(&base, geometry()).await.unwrap();
        let b = BlockImage::open(&base, &[], geometry(), true)
            .await
            .unwrap();
        b.write_at(0, &[42; 8192]).await.unwrap();
        let layer = dir.path().join("base.layer");
        b.export_upper(&layer).await.unwrap();
        drop(b);
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let raw: Arc<dyn VirtualFile> = Arc::new(LocalFile::open_ro(&layer).unwrap());
        let lower = open_files_ro(&[raw]).await.unwrap();
        let data = Arc::new(
            WritableZFile::open(&upper.join("upper.data"), &upper.join("upper.index")).unwrap(),
        );
        let rw = open_file_rw(
            data.clone(),
            Some(Arc::new(
                LocalFile::open_rw(upper.join("upper.index"), false).unwrap(),
            )),
        )
        .await
        .unwrap();
        let image = stack_files(&rw, &lower, false).await.unwrap();
        image.write_at(0, &[51; 512]).await.unwrap();
        image.discard_range(512, 4096).await.unwrap();
        image.sync().await.unwrap();
        drop(image);
        drop(rw);
        drop(data);
        let h = BlockImage::open(&upper, std::slice::from_ref(&layer), geometry(), true)
            .await
            .unwrap();
        let mut expected = [42; 8192];
        expected[..512].fill(51);
        expected[512..4608].fill(0);
        assert_eq!(h.read_at(0, 8192).await.unwrap(), expected);
        let seal = dir.path().join("seal");
        h.restack_upper(&seal).await.unwrap();
        drop(h);
        let decoded = zfile_open_ro_vfile(Arc::new(LocalFile::open_ro(&seal).unwrap()), true)
            .await
            .unwrap();
        let sealed = LSMTReadOnlyFile::open(decoded).await.unwrap();
        assert_eq!(
            sealed.read_at(512, 4096).await.unwrap().as_ref(),
            &[0; 4096]
        );
        let child = dir.path().join("child");
        BlockImage::create_upper(&child, geometry()).await.unwrap();
        let h = BlockImage::open(&child, &[layer, seal], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, 8192).await.unwrap(), expected);
    }
}

#[tokio::test]
async fn writeback_merges_overwrites_and_snapshots_pending_data() {
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        let before = h.compression_write_profile().await.unwrap().encoded_blocks;
        for byte in 0..100u8 {
            h.write_at(0, &[byte; 4096]).await.unwrap();
        }
        assert_eq!(h.read_at(0, 4096).await.unwrap(), [99; 4096]);
        assert_eq!(
            h.compression_write_profile().await.unwrap().encoded_blocks,
            before
        );
        assert_eq!(h.writeback_profile().await.overwritten_blocks, 99);
        let snapshot = dir.path().join("snapshot");
        h.export_upper(&snapshot).await.unwrap();
        assert_eq!(
            h.compression_write_profile().await.unwrap().encoded_blocks - before,
            1
        );
        assert_eq!(h.writeback_profile().await.pending_bytes, 0);
        h.write_zeroes(511, 1026).await.unwrap();
        let mut expected = vec![99; 4096];
        expected[511..1537].fill(0);
        assert_eq!(h.read_at(0, 4096).await.unwrap(), expected);
        let second = dir.path().join("second");
        h.restack_upper(&second).await.unwrap();
        assert_eq!(h.read_at(0, 4096).await.unwrap(), expected);
        let receiver = dir.path().join("receiver");
        BlockImage::create_upper(&receiver, geometry())
            .await
            .unwrap();
        let old = BlockImage::open(&receiver, &[snapshot], geometry(), false)
            .await
            .unwrap();
        assert_eq!(old.read_at(0, 4096).await.unwrap(), [99; 4096]);
    }
}

#[tokio::test]
async fn writeback_eviction_partial_blocks_and_capacity_changes() {
    for codec in ["lz4", "zstd"] {
        let g = Geometry {
            virtual_size: 33 * 512,
            logical_block_size: 512,
        };
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        BlockImage::create_upper(&base, g).await.unwrap();
        let h = BlockImage::open(&base, &[], g, true).await.unwrap();
        let mut expected = vec![17; g.virtual_size as usize];
        h.write_at(0, &expected).await.unwrap();
        let layer = dir.path().join("layer");
        h.export_upper(&layer).await.unwrap();
        drop(h);
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, g, codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, std::slice::from_ref(&layer), g, true)
            .await
            .unwrap();
        h.set_writeback_capacity(8192).await.unwrap();
        assert!(h.set_writeback_capacity(1).await.is_err());
        for i in 0..80usize {
            let at = (i * 4079) % (expected.len() - 701);
            let bytes = vec![i as u8; 701];
            h.write_at(at as u64, &bytes).await.unwrap();
            expected[at..at + 701].copy_from_slice(&bytes);
            assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
        }
        let last = g.virtual_size - 1;
        h.write_at(last, &[201]).await.unwrap();
        *expected.last_mut().unwrap() = 201;
        assert!(h.writeback_profile().await.peak_pending_bytes <= 8192);
        h.set_writeback_capacity(0).await.unwrap();
        assert_eq!(h.writeback_profile().await.pending_bytes, 0);
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
        h.flush().await.unwrap();
        drop(h);
        let h = BlockImage::open(&upper, &[layer], g, true).await.unwrap();
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
    }
}

#[tokio::test]
async fn writeback_concurrent_writes_and_barriers_recover() {
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let h = std::sync::Arc::new(
            BlockImage::open(&upper, &[], geometry(), true)
                .await
                .unwrap(),
        );
        h.set_writeback_capacity(8192).await.unwrap();
        let mut jobs = Vec::new();
        for worker in 0..8u64 {
            let h = h.clone();
            jobs.push(tokio::spawn(async move {
                for generation in 1..=16u8 {
                    h.write_at(worker * 4096, &[generation; 4096])
                        .await
                        .unwrap();
                    h.flush().await.unwrap();
                    assert_eq!(
                        h.read_at(worker * 4096, 4096).await.unwrap(),
                        [generation; 4096]
                    );
                }
            }));
        }
        for job in jobs {
            job.await.unwrap();
        }
        drop(h);
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, 8 * 4096).await.unwrap(), vec![16; 8 * 4096]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepared_appends_publish_atomically_with_concurrent_sync() {
    use overlaybd::{
        backend::local::LocalFile, index_file::open_file_rw, virtual_file::VirtualFile,
        zfile::writable::WritableZFile,
    };
    use std::sync::Arc;
    for codec in ["lz4", "zstd"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec)
            .await
            .unwrap();
        let data = Arc::new(
            WritableZFile::open(&upper.join("upper.data"), &upper.join("upper.index")).unwrap(),
        );
        let image = Arc::new(
            open_file_rw(
                data,
                Some(Arc::new(
                    LocalFile::open_rw(upper.join("upper.index"), false).unwrap(),
                )),
            )
            .await
            .unwrap(),
        );
        image.set_index_group_commit(65536).unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(5));
        let mut jobs = Vec::new();
        for worker in 0..4u64 {
            let image = image.clone();
            let barrier = barrier.clone();
            jobs.push(tokio::spawn(async move {
                barrier.wait().await;
                for byte in 1..=32u8 {
                    image
                        .write_at(worker * 65536, &vec![byte; 65536])
                        .await
                        .unwrap();
                    assert_eq!(
                        image.read_at(worker * 65536, 65536).await.unwrap().as_ref(),
                        &vec![byte; 65536]
                    );
                }
            }));
        }
        barrier.wait().await;
        for _ in 0..16 {
            image.sync().await.unwrap();
            tokio::task::yield_now().await;
        }
        for job in jobs {
            job.await.unwrap();
        }
        image.sync().await.unwrap();
        drop(image);
        let h = BlockImage::open(&upper, &[], geometry(), true)
            .await
            .unwrap();
        assert_eq!(h.read_at(0, 4 * 65536).await.unwrap(), vec![32; 4 * 65536]);
    }
}

#[tokio::test]
async fn small_write_pressure_does_not_drain_whole_cache() {
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    BlockImage::create_upper_compressed(&upper, geometry(), "lz4")
        .await
        .unwrap();
    let h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    h.set_writeback_capacity(128 * 1024).await.unwrap();
    for i in 0..32u64 {
        h.write_at(i * 4096, &[71; 4096]).await.unwrap();
    }
    let before = h.writeback_profile().await;
    h.write_at(32 * 4096, &[71; 4096]).await.unwrap();
    let after = h.writeback_profile().await;
    assert!(after.backing_bytes > before.backing_bytes);
    assert!(after.backing_bytes - before.backing_bytes <= 64 * 1024);
    assert!(after.pending_bytes > 64 * 1024);
    assert!(after.peak_pending_bytes <= 128 * 1024);
    assert_eq!(h.read_at(0, 33 * 4096).await.unwrap(), vec![71; 33 * 4096]);
    h.flush().await.unwrap();
    drop(h);
    let h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    assert_eq!(h.read_at(0, 33 * 4096).await.unwrap(), vec![71; 33 * 4096]);
}
