//! Independent reader/writer: depends only on pinned upstream, never obd adapters.
use anyhow::{ensure, Context, Result};
use overlaybd::{
    backend::local::LocalFile,
    index_file::{create_file_rw, open_files_ro, LayerInfo, RwLayout},
    virtual_file::VirtualFile,
    zfile::{is_zfile, zfile_compress, zfile_open_ro_vfile, CompressArgs, CompressOptions},
};
use std::{path::Path, sync::Arc};
const SIZE: usize = 1024 * 1024;
fn ro(path: &Path) -> Result<Arc<dyn VirtualFile>> {
    Ok(Arc::new(LocalFile::open_ro(path)?))
}
fn new(path: &Path) -> Result<Arc<dyn VirtualFile>> {
    Ok(Arc::new(LocalFile::new(path)?))
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("check") => {
            let config = Path::new(args.get(2).context("config")?);
            let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(config)?)?;
            let mut files = Vec::new();
            for lower in doc["lowers"].as_array().context("lowers")? {
                let file = ro(&config
                    .parent()
                    .context("parent")?
                    .join(lower["file"].as_str().context("file")?))?;
                files.push(if is_zfile(file.clone()).await? == 1 {
                    zfile_open_ro_vfile(file, true).await?
                } else {
                    file
                });
            }
            let image = open_files_ro(&files).await?;
            let expected = std::fs::read(args.get(3).context("expected")?)?;
            ensure!(
                image.size().await? == expected.len() as u64,
                "capacity mismatch"
            );
            let actual = image.read_at(0, expected.len()).await?;
            ensure!(
                actual.as_ref() == expected.as_slice(),
                "visible data mismatch"
            );
            println!(
                "{}",
                serde_json::json!({"check":config,"bytes":expected.len(),"passed":true,"os":std::env::consts::OS})
            );
        }
        Some("generate") => {
            let dir = Path::new(args.get(2).context("output directory")?);
            std::fs::create_dir(dir)?;
            let mut expected = vec![0; SIZE];
            expected[..SIZE / 2].fill(0x35);
            for n in 0..2 {
                let data = new(&dir.join(format!("{n}.data")))?;
                let index = new(&dir.join(format!("{n}.index")))?;
                let mut info = LayerInfo::new(data, Some(index), SIZE as u64);
                info.rw_layout = RwLayout::LogStructured;
                let file = create_file_rw(info).await?;
                if n == 0 {
                    file.write_at(0, &expected[..SIZE / 2]).await?;
                } else {
                    expected[..SIZE / 4].fill(0x73);
                    expected[65536..131072].fill(0);
                    expected[SIZE - 512..].fill(0xa5);
                    file.write_at(0, &expected[..SIZE / 4]).await?;
                    file.write_at((SIZE - 512) as u64, &expected[SIZE - 512..])
                        .await?;
                }
                file.commit(new(&dir.join(format!("{n}.lsmt")))?).await?;
            }
            std::fs::write(dir.join("expected.raw"), expected)?;
            for codec in ["none", "lz4", "zstd"] {
                let sub = dir.join(codec);
                std::fs::create_dir(&sub)?;
                let mut lowers = Vec::new();
                for n in 0..2 {
                    let source = dir.join(format!("{n}.lsmt"));
                    let name = format!("{n}.layer");
                    let dest = sub.join(&name);
                    if codec == "none" {
                        std::fs::copy(source, dest)?;
                    } else {
                        let algo = if codec == "lz4" {
                            CompressOptions::LZ4
                        } else {
                            CompressOptions::ZSTD
                        };
                        zfile_compress(
                            ro(&source)?,
                            new(&dest)?,
                            &CompressArgs::new(CompressOptions::new(algo, 65536, 1)),
                        )
                        .await?;
                    }
                    lowers.push(serde_json::json!({"file":name}));
                }
                std::fs::write(
                    sub.join("image.json"),
                    serde_json::to_vec(&serde_json::json!({"lowers":lowers}))?,
                )?;
            }
            println!(
                "{}",
                serde_json::json!({"generated":dir,"os":std::env::consts::OS})
            );
        }
        _ => anyhow::bail!("usage: probe check IMAGE_JSON EXPECTED_RAW | generate NEW_DIRECTORY"),
    }
    Ok(())
}
