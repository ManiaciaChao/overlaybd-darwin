//! Direct adapter diagnostic. FSKit acceptance is a separate end-to-end run.
use anyhow::{ensure, Result};
use obd_core_adapter::{BlockImage, Geometry};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::Barrier;

#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() -> Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("fresh output directory"));
    std::fs::create_dir(&root)?;
    let geometry = Geometry {
        virtual_size: 64 * 1024 * 1024,
        logical_block_size: 512,
    };
    let upper = root.join("upper");
    BlockImage::create_upper(&upper, geometry).await?;
    let image = Arc::new(BlockImage::open(&upper, &[], geometry, true).await?);
    let mut seed = 7331u64;
    let mut data = vec![0; 1024 * 1024];
    for byte in &mut data {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = seed as u8;
    }
    for offset in (0..geometry.virtual_size).step_by(data.len()) {
        image.write_at(offset, &data).await?;
    }
    image.flush().await?;
    let mut rows = Vec::new();
    for repeat in 0..5 {
        for (name, write, block, count) in [
            ("read_4k", false, 4096, 4096),
            ("read_128k", false, 131072, 512),
            ("write_4k", true, 4096, 1024),
        ] {
            for depth in [1usize, 4, 16] {
                let start_gate = Arc::new(Barrier::new(depth + 1));
                let mut tasks = Vec::new();
                for lane in 0..depth {
                    let image = image.clone();
                    let gate = start_gate.clone();
                    tasks.push(tokio::spawn(async move {
                        let mut buffer = vec![lane as u8 + 1; block];
                        let mut latencies = Vec::new();
                        gate.wait().await;
                        for i in 0..count / depth {
                            let index = ((i * 97 + lane * 23) % (64 * 1024 * 1024 / block)) as u64;
                            let started = Instant::now();
                            if write {
                                image.write_at(index * block as u64, &buffer).await?;
                            } else {
                                ensure!(
                                    image
                                        .read_at_into(index * block as u64, &mut buffer)
                                        .await?
                                        == block,
                                    "short read"
                                );
                            }
                            latencies.push(started.elapsed().as_nanos() as u64);
                            std::hint::black_box(&buffer);
                        }
                        anyhow::Ok(latencies)
                    }));
                }
                let start = Instant::now();
                start_gate.wait().await;
                let mut ns = Vec::new();
                for task in tasks {
                    ns.extend(task.await??);
                }
                if write {
                    image.flush().await?;
                }
                let elapsed = start.elapsed().as_secs_f64();
                ns.sort_unstable();
                let row = json!({"repeat":repeat,"workload":name,"depth":depth,"operations":ns.len(),"seconds":elapsed,
                    "iops":ns.len() as f64/elapsed,"p50_us":ns[ns.len()/2] as f64/1000.,"p99_us":ns[ns.len()*99/100] as f64/1000.,
                    "write_timing_includes_final_fullsync":write});
                println!("{row}");
                rows.push(row);
            }
        }
    }
    std::fs::write(root.join("report.json"), serde_json::to_vec_pretty(&rows)?)?;
    Ok(())
}
