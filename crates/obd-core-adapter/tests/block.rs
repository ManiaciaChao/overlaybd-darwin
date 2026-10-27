use obd_core_adapter::{BlockImage, Geometry, MAX_IO};
use std::sync::Arc;
fn geometry() -> Geometry {
    Geometry {
        virtual_size: 1024 * 1024,
        logical_block_size: 512,
    }
}
#[tokio::test]
async fn hybrid_reopen_overwrite_bounds_and_readonly() {
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    BlockImage::create_upper(&upper, geometry()).await.unwrap();
    let h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    assert_eq!(h.read_at(0, 8192).await.unwrap(), vec![0; 8192]);
    h.write_at(0, &vec![7; 4096]).await.unwrap();
    h.flush().await.unwrap();
    let lengths =
        || ["upper.data", "upper.index"].map(|n| std::fs::metadata(upper.join(n)).unwrap().len());
    let before = lengths();
    for i in 0..32 {
        h.write_at(0, &vec![i; 4096]).await.unwrap();
    }
    h.flush().await.unwrap();
    assert_eq!(lengths(), before);
    h.write_at(511, &[1, 2, 3]).await.unwrap();
    h.flush().await.unwrap();
    assert!(h
        .write_at(geometry().virtual_size - 1, &[1, 2])
        .await
        .is_err());
    assert!(h.read_at(0, MAX_IO + 1).await.is_err());
    assert!(h.write_at(u64::MAX, &[1]).await.is_err());
    assert_eq!(
        h.read_at(geometry().virtual_size - 1, 8).await.unwrap(),
        [0]
    );
    assert!(h.read_at(u64::MAX, 8).await.unwrap().is_empty());
    assert!(h.discard(0, 512).is_err());
    drop(h);
    let ro = BlockImage::open(&upper, &[], geometry(), false)
        .await
        .unwrap();
    assert_eq!(ro.read_at(510, 5).await.unwrap(), [31, 1, 2, 3, 31]);
    assert!(ro.write_at(0, &[1]).await.is_err());
}
#[tokio::test]
async fn lower_forks_zero_mask_and_concurrent_unaligned_rmw() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base");
    BlockImage::create_upper(&base, geometry()).await.unwrap();
    let h = BlockImage::open(&base, &[], geometry(), true)
        .await
        .unwrap();
    h.write_at(0, &vec![42; 8192]).await.unwrap();
    let layer = dir.path().join("layer");
    h.export_upper(&layer).await.unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    BlockImage::create_upper(&a, geometry()).await.unwrap();
    BlockImage::create_upper(&b, geometry()).await.unwrap();
    let branch = Arc::new(
        BlockImage::open(&a, std::slice::from_ref(&layer), geometry(), true)
            .await
            .unwrap(),
    );
    let mut jobs = vec![];
    for i in 0..128u64 {
        let branch = branch.clone();
        jobs.push(tokio::spawn(async move {
            branch.write_at(i, &[i as u8]).await.unwrap();
        }));
    }
    for job in jobs {
        job.await.unwrap();
    }
    branch.write_zeroes(4096, 512).await.unwrap();
    branch.flush().await.unwrap();
    drop(branch);
    let reopened = BlockImage::open(&a, std::slice::from_ref(&layer), geometry(), true)
        .await
        .unwrap();
    assert_eq!(
        reopened.read_at(0, 128).await.unwrap(),
        (0..128).collect::<Vec<u8>>()
    );
    assert_eq!(
        reopened.read_at(4096, 513).await.unwrap(),
        [vec![0; 512], vec![42]].concat()
    );
    let sibling = BlockImage::open(&b, &[layer], geometry(), true)
        .await
        .unwrap();
    assert_eq!(sibling.read_at(0, 8192).await.unwrap(), vec![42; 8192]);
}
#[tokio::test]
async fn random_bytes_match_reference_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    BlockImage::create_upper(&upper, geometry()).await.unwrap();
    let mut h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    let mut model = vec![0; geometry().virtual_size as usize];
    let mut rng = 17u64;
    for round in 0..240 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let len = 1 + (rng % 4095) as usize;
        let offset = (rng.rotate_left(27) as usize) % (model.len() - len);
        let bytes = vec![(rng >> 32) as u8; len];
        model[offset..offset + len].copy_from_slice(&bytes);
        h.write_at(offset as u64, &bytes).await.unwrap();
        if round % 40 == 0 {
            h.flush().await.unwrap();
            drop(h);
            h = BlockImage::open(&upper, &[], geometry(), true)
                .await
                .unwrap();
            assert_eq!(h.read_at(0, model.len()).await.unwrap(), model);
        }
    }
    h.flush().await.unwrap();
    assert_eq!(h.read_at(0, model.len()).await.unwrap(), model);
}
#[tokio::test]
async fn missing_or_truncated_index_is_not_recreated() {
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    BlockImage::create_upper(&upper, geometry()).await.unwrap();
    std::fs::remove_file(upper.join("upper.index")).unwrap();
    assert!(BlockImage::open(&upper, &[], geometry(), true)
        .await
        .is_err());
    assert!(!upper.join("upper.index").exists());
    std::fs::write(upper.join("upper.index"), [0; 7]).unwrap();
    assert!(BlockImage::open(&upper, &[], geometry(), true)
        .await
        .is_err());
}

/// Exercises the actual upstream live restack, not our offline registry export.
/// Publication/recovery and APFS quiescing remain outside this block-level test.
#[tokio::test]
async fn upstream_live_restack_retains_handle_and_freezes_fork() {
    use overlaybd::{
        config::{CacheConfig, GlobalConfig},
        image_file::ImageFile,
        image_service::ImageService,
        virtual_file::VirtualFile,
    };
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("source");
    BlockImage::create_upper(&upper, geometry()).await.unwrap();
    let config = BlockImage::local_config(&upper, &[]).unwrap();
    let service = ImageService::new(GlobalConfig {
        enable_audit: false,
        cache_config: CacheConfig {
            cache_dir: dir.path().join("cache").to_str().unwrap().into(),
            ..CacheConfig::default()
        },
        ..GlobalConfig::default()
    })
    .await
    .unwrap();
    let live = ImageFile::open(config, service, None).await.unwrap();
    live.write_at(0, &[17; 4096]).await.unwrap();
    live.write_at(0, &[23; 4096]).await.unwrap(); // Hybrid in-place overwrite
    live.sync().await.unwrap();
    let old_inode = std::fs::metadata(upper.join("upper.data")).unwrap().ino();
    let layer = dir.path().join("snapshot.layer");
    live.create_snapshot_and_restack(&layer).await.unwrap();
    // Upstream seals/renames the old Upper, rather than copying its full data.
    assert_eq!(std::fs::metadata(&layer).unwrap().ino(), old_inode);
    assert_ne!(
        std::fs::metadata(upper.join("upper.data")).unwrap().ino(),
        old_inode
    );
    assert_eq!(live.read_at(0, 4096).await.unwrap().as_ref(), &[23; 4096]);
    live.write_at(0, &[41; 4096]).await.unwrap();
    live.sync().await.unwrap();
    let branch_upper = dir.path().join("branch");
    BlockImage::create_upper(&branch_upper, geometry())
        .await
        .unwrap();
    let branch = BlockImage::open(
        &branch_upper,
        std::slice::from_ref(&layer),
        geometry(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(branch.read_at(0, 4096).await.unwrap(), vec![23; 4096]);
    branch.write_at(511, &[7, 8, 9]).await.unwrap();
    branch.flush().await.unwrap();
    assert_eq!(live.read_at(0, 4096).await.unwrap().as_ref(), &[41; 4096]);
    drop(branch);
    drop(live);
    let source = BlockImage::open(&upper, std::slice::from_ref(&layer), geometry(), true)
        .await
        .unwrap();
    let branch = BlockImage::open(
        &branch_upper,
        std::slice::from_ref(&layer),
        geometry(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(source.read_at(0, 4096).await.unwrap(), vec![41; 4096]);
    assert_eq!(branch.read_at(511, 3).await.unwrap(), vec![7, 8, 9]);
}

/// Repeated barriers are free only while no mutation follows a successful flush.
/// Restack must invalidate clean state and the cached durability handles.
#[tokio::test]
async fn clean_flush_elision_preserves_reopen_and_restack() {
    let dir = tempfile::tempdir().unwrap();
    let upper = dir.path().join("upper");
    BlockImage::create_upper(&upper, geometry()).await.unwrap();
    let h = BlockImage::open(&upper, &[], geometry(), true)
        .await
        .unwrap();
    h.flush().await.unwrap();
    h.flush().await.unwrap();
    assert_eq!(h.flush_profile().await.completed, 1);
    assert_eq!(h.flush_profile().await.skipped_clean, 1);
    h.write_at(511, &[17, 23, 41]).await.unwrap();
    h.flush().await.unwrap();
    h.flush().await.unwrap();
    assert_eq!(h.flush_profile().await.completed, 2);
    assert_eq!(h.flush_profile().await.skipped_clean, 2);
    let layer = dir.path().join("checkpoint");
    h.restack_upper(&layer).await.unwrap();
    h.flush().await.unwrap();
    assert_eq!(h.flush_profile().await.completed, 3);
    assert_eq!(h.flush_profile().await.descriptor_opens, 2);
    h.write_at(512, &[99]).await.unwrap();
    h.flush().await.unwrap();
    drop(h);
    let reopened = BlockImage::open(&upper, std::slice::from_ref(&layer), geometry(), true)
        .await
        .unwrap();
    reopened.flush().await.unwrap();
    assert_eq!(reopened.flush_profile().await.completed, 1);
    assert_eq!(reopened.read_at(511, 3).await.unwrap(), [17, 99, 41]);
    let fork_upper = dir.path().join("fork");
    BlockImage::create_upper(&fork_upper, geometry())
        .await
        .unwrap();
    let fork = BlockImage::open(&fork_upper, &[layer], geometry(), true)
        .await
        .unwrap();
    assert_eq!(fork.read_at(511, 3).await.unwrap(), [17, 23, 41]);
}
