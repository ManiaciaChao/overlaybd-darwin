use obd_core_adapter::{BlockImage,Geometry};
use std::sync::Arc;
fn geometry()->Geometry {Geometry {virtual_size:32*1024*1024,logical_block_size:512}}
#[tokio::test(flavor="multi_thread",worker_threads=4)]
async fn packed_concurrent_overwrite_flush_restack_raw_interop() -> anyhow::Result<()> {
 for codec in ["packed-none","packed-lz4","packed-zstd","packed-lz4-dedupe"] {
  let dir=tempfile::tempdir()?;let upper=dir.path().join("upper");BlockImage::create_upper_compressed(&upper,geometry(),codec).await?;
  let h=Arc::new(BlockImage::open(&upper,&[],geometry(),true).await?);
  let mut tasks=Vec::new();for sector in 0..8 {let h=h.clone();tasks.push(tokio::spawn(async move {
   for generation in 0..20 {h.write_at(sector*512,&[generation+sector as u8;512]).await?;h.flush().await?;}
   anyhow::Ok(())
  }));}for task in tasks {task.await??;}
  let mut expected=vec![0;4096];for sector in 0..8 {expected[sector*512..sector*512+512].fill(19+sector as u8);}
  assert_eq!(h.read_at(0,4096).await?,expected);
  let layer=dir.path().join("snapshot");h.restack_upper(&layer).await?;
  h.write_at(2,&[99;3]).await?;h.flush().await?;drop(h);
  let fresh=BlockImage::open(&upper,std::slice::from_ref(&layer),geometry(),true).await?;
  let mut modified=expected.clone();modified[2..5].fill(99);assert_eq!(fresh.read_at(0,4096).await?,modified);
  fresh.discard_vdo(512,512).await?;fresh.flush().await?;modified[512..1024].fill(0);assert_eq!(fresh.read_at(0,4096).await?,modified);drop(fresh);
  let raw=dir.path().join("raw");BlockImage::create_upper(&raw,geometry()).await?;
  let receiver=BlockImage::open(&raw,&[layer],geometry(),true).await?;assert_eq!(receiver.read_at(0,4096).await?,expected);
 }
 Ok(())
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore = "release profiling; FSKit acceptance is experiments/packed-upper/fskit.py"]
async fn packed_profile_continuous() -> anyhow::Result<()> {
 let dir=tempfile::tempdir()?;
 let geometry=Geometry {virtual_size:128*1024*1024,logical_block_size:512};
 let mut data=vec![0u8;4*1024*1024];let mut seed=17u64;
 for block in data.chunks_mut(4096) {for b in &mut block[..2048] {seed^=seed<<13;seed^=seed>>7;seed^=seed<<17;*b=seed as u8;}let (head,tail)=block.split_at_mut(2048);for i in 0..2048 {tail[i]=head[i%256];}}
 for codec in ["none","packed-lz4"] {
  let upper=dir.path().join(codec);BlockImage::create_upper_compressed(&upper,geometry,codec).await?;
  let image=BlockImage::open(&upper,&[],geometry,true).await?;let start=std::time::Instant::now();
  for iteration in 0..16 {for i in 0..16 {image.write_at(i*data.len()as u64,&data).await?;}if iteration==15 {image.flush().await?;}}
  eprintln!("{codec}: {} MiB/s {:?}",1024.0/start.elapsed().as_secs_f64(),image.packed_stats().await?);
 }Ok(())
}

#[tokio::test(flavor="multi_thread",worker_threads=4)]
#[ignore = "same-workload before/after profiling, not FSKit acceptance"]
async fn packed_profile_growth() -> anyhow::Result<()> {
 let dir=tempfile::tempdir()?;let geometry=Geometry {virtual_size:2*1024*1024*1024,logical_block_size:512};
 let mut data=vec![0u8;4*1024*1024];let mut seed=17u64;
 for block in data.chunks_mut(4096) {for b in &mut block[..2048] {seed^=seed<<13;seed^=seed>>7;seed^=seed<<17;*b=seed as u8;}let(head,tail)=block.split_at_mut(2048);for i in 0..2048{tail[i]=head[i%256];}}
 let upper=dir.path().join("upper");BlockImage::create_upper_compressed(&upper,geometry,"packed-lz4").await?;
 let image=BlockImage::open(&upper,&[],geometry,true).await?;let start=std::time::Instant::now();
 for i in 0..256 {image.write_at(i*data.len()as u64,&data).await?;}image.flush().await?;
 let seconds=start.elapsed().as_secs_f64();let mut latency=Vec::new();
 for i in 0..16 {let start=std::time::Instant::now();image.write_at(i*4096,&data[..4096]).await?;image.flush().await?;latency.push(start.elapsed().as_secs_f64()*1000.0);}
 latency.sort_by(f64::total_cmp);eprintln!("GROWTH sequential_mib_s={} sync_p50_ms={} sync_p99_ms={} stats={:?}",1024.0/seconds,latency[8],latency[15],image.packed_stats().await?);Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn packed_batch_reads_mix_lower_zero_dirty_and_durable_data() -> anyhow::Result<()> {
    for codec in ["packed-none", "packed-lz4", "packed-zstd", "packed-lz4-dedupe"] {
        let dir = tempfile::tempdir()?;
        let upper = dir.path().join("upper");
        BlockImage::create_upper_compressed(&upper, geometry(), codec).await?;
        let h = BlockImage::open(&upper, &[], geometry(), true).await?;
        let mut expected = vec![0; 4096 * 544];
        for (i, block) in expected.chunks_mut(4096).enumerate() { block.fill((i % 251 + 1) as u8); }
        h.write_at(0, &expected).await?;
        let layer = dir.path().join("snapshot");
        h.restack_upper(&layer).await?;
        for i in (0..544).step_by(5) {
            h.write_at((i * 4096) as u64, &[111; 4096]).await?;
            expected[i * 4096..(i + 1) * 4096].fill(111);
        }
        h.flush().await?;
        for i in (1..544).step_by(7) {
            h.discard_vdo((i * 4096) as u64, 4096).await?;
            expected[i * 4096..(i + 1) * 4096].fill(0);
        }
        h.write_at(255 * 4096 + 7, &[222; 8192]).await?;
        expected[255 * 4096 + 7..257 * 4096 + 7].fill(222);
        // Cover two batch boundaries plus unaligned head/tail, before and after
        // the dirty frontend is drained. Lower-only runs must remain coalescible.
        for (offset, len) in [(0, expected.len()), (511, expected.len() - 1023)] {
            assert_eq!(h.read_at(offset as u64, len).await?, expected[offset..offset + len]);
        }
        h.flush().await?;
        assert_eq!(h.read_at(0, expected.len()).await?, expected);
        drop(h);
        let h = BlockImage::open(&upper, &[layer], geometry(), true).await?;
        assert_eq!(h.read_at(0, expected.len()).await?, expected);
    }
    Ok(())
}
