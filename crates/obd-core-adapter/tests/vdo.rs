#![cfg(target_os = "macos")]
use obd_core_adapter::{BlockImage, Geometry};
fn geometry() -> Geometry {
    Geometry {
        virtual_size: 8 * 1024 * 1024,
        logical_block_size: 512,
    }
}

#[tokio::test]
async fn vdo_hybrid_overwrite_reopen_export_restack_and_raw_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    BlockImage::create_upper_compressed(&upper, geometry(), "vdo-lz4")
        .await
        .unwrap();
    assert_eq!(
        obd_core_adapter::upper_compression(&upper).unwrap(),
        "vdo-lz4"
    );
    let h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    // Disable the Rust cache: this test must exercise actual VDO overwrites.
    h.set_writeback_capacity(0).await.unwrap();
    let mut expected = vec![0; 128 * 1024];
    for generation in 1..=12u8 {
        let at = 13 + generation as usize * 173;
        let bytes = vec![generation; 24 * 1024 + generation as usize];
        h.write_at(at as u64, &bytes).await.unwrap();
        expected[at..at + bytes.len()].copy_from_slice(&bytes);
        h.flush().await.unwrap();
        assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
    }
    drop(h); // Worker exits without VDO shutdown: reopen must recover.
    let h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
    let exported = dir.path().join("exported.layer");
    h.export_upper(&exported).await.unwrap();
    assert_eq!(
        obd_core_adapter::archive_layer_size(&exported)
            .await
            .unwrap(),
        geometry().virtual_size
    );
    let snapshot = dir.path().join("live.layer");
    h.restack_upper(&snapshot).await.unwrap();
    assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
    h.write_at(512, &[91; 512]).await.unwrap();
    h.write_zeroes(1024, 512).await.unwrap();
    h.flush().await.unwrap();
    let old = expected.clone();
    expected[512..1024].fill(91);
    expected[1024..1536].fill(0);
    drop(h);
    let h = BlockImage::open(&upper, &[snapshot.clone()], geometry(), true)
        .await
        .unwrap();
    assert_eq!(h.read_at(0, expected.len()).await.unwrap(), expected);
    let raw = dir.path().join("receiver");
    BlockImage::create_upper(&raw, geometry()).await.unwrap();
    let receiver = BlockImage::open(&raw, &[snapshot], geometry(), true)
        .await
        .unwrap();
    assert_eq!(receiver.read_at(0, old.len()).await.unwrap(), old);
    receiver.write_at(0, &[33; 512]).await.unwrap();
    receiver.flush().await.unwrap();
    let returned = dir.path().join("returned.layer");
    receiver.export_upper(&returned).await.unwrap();
    let third = dir.path().join("return-receiver");
    BlockImage::create_upper_compressed(&third, geometry(), "vdo-lz4")
        .await
        .unwrap();
    let third = BlockImage::open(&third, &[exported, returned], geometry(), true)
        .await
        .unwrap();
    let mut returned_expected = old;
    returned_expected[..512].fill(33);
    assert_eq!(
        third.read_at(0, returned_expected.len()).await.unwrap(),
        returned_expected
    );
}

#[tokio::test]
async fn vdo_rejects_wrong_index_and_duplicate_open() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    BlockImage::create_upper_compressed(&a, geometry(), "vdo-lz4")
        .await
        .unwrap();
    BlockImage::create_upper_compressed(&b, geometry(), "vdo-lz4")
        .await
        .unwrap();
    let h = BlockImage::open(&a, &[], geometry(), true).await.unwrap();
    assert!(BlockImage::open(&a, &[], geometry(), true).await.is_err());
    h.flush().await.unwrap();
    drop(h);
    std::fs::copy(b.join("upper.index"), a.join("upper.index")).unwrap();
    assert!(BlockImage::open(&a, &[], geometry(), true).await.is_err());
}

/// Opt-in real compiler artifact corpus; includes flush and host allocated bytes.
#[tokio::test]
#[ignore = "release benchmark requiring OBD_VDO_CORPUS"]
async fn vdo_rust_cache_benchmark() {
    use sha2::{Digest, Sha256};
    use std::{io::Read, os::unix::fs::MetadataExt, time::Instant};
    assert!(!cfg!(debug_assertions), "run with --release");
    let corpus = std::env::var("OBD_VDO_CORPUS").expect("compiler artifact path");
    let mut source = Vec::new();
    std::fs::File::open(&corpus)
        .unwrap()
        .take(
            std::env::var("OBD_VDO_CORPUS_MIB")
                .ok()
                .map(|s| s.parse::<u64>().unwrap())
                .unwrap_or(32)
                * 1024
                * 1024,
        )
        .read_to_end(&mut source)
        .unwrap();
    assert!(source.len() >= 8 * 1024 * 1024, "corpus too small");
    source.truncate(source.len() / 4096 * 4096);
    let digest = format!("{:x}", Sha256::digest(&source));
    let mut rows = Vec::new();
    let mut deletion = None;
    for codec in ["none", "vdo-lz4"] {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("upper");
        let g = Geometry {
            virtual_size: (source.len() as u64 * 2).max(64 * 1024 * 1024),
            logical_block_size: 512,
        };
        BlockImage::create_upper_compressed(&upper, g, codec)
            .await
            .unwrap();
        let h = BlockImage::open(&upper, &[], g, true).await.unwrap();
        let mut expected = source.clone();
        for generation in 0..std::env::var("OBD_VDO_GENERATIONS")
            .ok()
            .map(|s| s.parse::<u8>().unwrap())
            .unwrap_or(4)
        {
            if generation > 0 {
                // Modify every block without replacing the corpus by a synthetic pattern.
                let stride = std::env::var("OBD_VDO_CHANGE_STRIDE")
                    .ok()
                    .map(|s| s.parse::<usize>().unwrap())
                    .unwrap_or(1);
                assert!(stride > 0);
                for (i, block) in expected.chunks_mut(4096).enumerate() {
                    if i % stride == generation as usize % stride {
                        block[17] = generation;
                    }
                }
            }
            let started = Instant::now();
            for (i, chunk) in expected.chunks(1024 * 1024).enumerate() {
                h.write_at((i * 1024 * 1024) as u64, chunk).await.unwrap();
            }
            h.flush().await.unwrap();
            let write_seconds = started.elapsed().as_secs_f64();
            let started = Instant::now();
            for (i, chunk) in expected.chunks(1024 * 1024).enumerate() {
                assert_eq!(
                    h.read_at((i * 1024 * 1024) as u64, chunk.len())
                        .await
                        .unwrap(),
                    chunk
                );
            }
            let read_seconds = started.elapsed().as_secs_f64();
            let allocated: u64 = std::fs::read_dir(&upper)
                .unwrap()
                .map(|e| e.unwrap().metadata().unwrap())
                .filter(|m| m.is_file())
                .map(|m| m.blocks() * 512)
                .sum();
            let space = h.vdo_space_usage().await.unwrap();
            rows.push(serde_json::json!({"space":space,"codec":codec,"generation":generation,"bytes":source.len(),"write_seconds":write_seconds,"read_seconds":read_seconds,"allocated_bytes":allocated}));
        }
        drop(h);
        let reopened = BlockImage::open(&upper, &[], g, true).await.unwrap();
        for (i, chunk) in expected.chunks(1024 * 1024).enumerate() {
            assert_eq!(
                reopened
                    .read_at((i * 1024 * 1024) as u64, chunk.len())
                    .await
                    .unwrap(),
                chunk
            );
        }
        if codec == "vdo-lz4" {
            for (i, chunk) in expected.chunks(1024 * 1024).enumerate() {
                reopened
                    .discard_vdo((i * 1024 * 1024) as u64, chunk.len())
                    .await
                    .unwrap();
            }
            reopened.flush().await.unwrap();
            deletion = Some(reopened.vdo_space_usage().await.unwrap().unwrap());
            drop(reopened);
            let empty = BlockImage::open(&upper, &[], g, true).await.unwrap();
            for (i, chunk) in expected.chunks(1024 * 1024).enumerate() {
                assert_eq!(
                    empty
                        .read_at((i * 1024 * 1024) as u64, chunk.len())
                        .await
                        .unwrap(),
                    vec![0; chunk.len()]
                );
            }
        }
    }
    let require_savings = std::env::var_os("OBD_VDO_REQUIRE_SAVINGS").is_some();
    let raw = rows
        .iter()
        .filter(|r| r["codec"] == "none")
        .map(|r| r["allocated_bytes"].as_u64().unwrap())
        .max()
        .unwrap();
    let peak = rows
        .iter()
        .filter(|r| r["codec"] == "vdo-lz4")
        .map(|r| r["allocated_bytes"].as_u64().unwrap())
        .max()
        .unwrap();
    let report = serde_json::json!({"corpus":corpus,"source_bytes":source.len(),"source_sha256":digest,"rows":rows,"reopen_verified":true,
        "post_discard":deletion,"discard_reopen_verified":true,"host_savings_all_rounds":peak < raw,
        "change_stride":std::env::var("OBD_VDO_CHANGE_STRIDE").unwrap_or_else(|_| "1".into())});
    std::fs::write(
        std::env::var("OBD_VDO_BENCH_OUTPUT").expect("output path"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    if require_savings {
        assert!(
            peak < raw,
            "host savings failed: VDO peak {peak}, raw {raw}"
        );
    }
}

/// Host allocation, lower masking, partial-block discard, dedupe references,
/// rewrite after trim, and process-death reopen are checked together.
#[tokio::test]
async fn vdo_discard_reclaims_host_space_and_preserves_shared_and_lower_data() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    let g = Geometry {
        virtual_size: 64 * 1024 * 1024,
        logical_block_size: 512,
    };
    let lower_upper = dir.path().join("lower-upper");
    BlockImage::create_upper(&lower_upper, g).await.unwrap();
    let lower = BlockImage::open(&lower_upper, &[], g, true).await.unwrap();
    lower.write_at(0, &vec![73; 4 * 1024 * 1024]).await.unwrap();
    lower
        .write_at(20 * 1024 * 1024, &vec![74; 4096])
        .await
        .unwrap();
    let sealed = dir.path().join("lower.layer");
    lower.export_upper(&sealed).await.unwrap();
    drop(lower);
    BlockImage::create_upper_compressed(&upper, g, "vdo-lz4")
        .await
        .unwrap();
    let h = BlockImage::open(&upper, &[sealed.clone()], g, true)
        .await
        .unwrap();
    // Deterministic high-entropy bytes force real data allocation, not just
    // metadata or zero optimizations. Duplicate one region to test refcounts.
    let mut bytes = vec![0u8; 4 * 1024 * 1024];
    let mut x = 0x83ad571abcde7u64;
    for b in &mut bytes {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    for i in 0..4 {
        h.write_at(i * bytes.len() as u64, &bytes).await.unwrap();
    }
    h.flush().await.unwrap();
    let allocated = || std::fs::metadata(upper.join("upper.vdo")).unwrap().blocks() * 512;
    let before = allocated();

    h.discard_vdo(512, 3 * 4096).await.unwrap();
    h.flush().await.unwrap();
    let mut expected = bytes.clone();
    expected[512..512 + 3 * 4096].fill(0);
    assert_eq!(h.read_at(0, bytes.len()).await.unwrap(), expected);
    assert_eq!(
        h.read_at(bytes.len() as u64, bytes.len()).await.unwrap(),
        bytes
    );
    for i in 0..4 {
        h.discard_vdo(i * bytes.len() as u64, bytes.len())
            .await
            .unwrap();
    }
    // Discard a lower-only gap too: must mask, never inherit the old lower.
    h.discard_vdo(20 * 1024 * 1024, 4096).await.unwrap();
    h.flush().await.unwrap();
    let after = allocated();

    assert!(
        after + 2 * 1024 * 1024 < before,
        "host space not reclaimed: {before} -> {after}"
    );
    let stats = h.vdo_space_usage().await.unwrap().unwrap();
    assert!(stats.reclaimed_bytes >= 2 * 1024 * 1024);
    drop(h);
    let h = BlockImage::open(&upper, &[sealed.clone()], g, true)
        .await
        .unwrap();
    for i in 0..4 {
        assert_eq!(
            h.read_at(i * bytes.len() as u64, bytes.len())
                .await
                .unwrap(),
            vec![0; bytes.len()]
        );
    }
    assert_eq!(
        h.read_at(20 * 1024 * 1024, 4096).await.unwrap(),
        vec![0; 4096]
    );
    h.write_at(1024, &bytes[..8192]).await.unwrap();
    h.flush().await.unwrap();
    drop(h);
    let h = BlockImage::open(&upper, &[sealed], g, true).await.unwrap();
    assert_eq!(h.read_at(1024, 8192).await.unwrap(), bytes[..8192]);
    assert_eq!(h.read_at(0, 1024).await.unwrap(), vec![0; 1024]);
}
