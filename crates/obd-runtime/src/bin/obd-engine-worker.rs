//! Short-lived offline engine. Its stdout contains metadata only.
use anyhow::{Context, Result};
use obd_core_adapter::{BlockImage, Geometry};
use obd_runtime::{lock_resource, open_image, runtime};
use serde_json::{json, Value};
use std::path::Path;
fn main() -> Result<()> {
    let job: Value = serde_json::from_str(&std::env::args().nth(1).context("JSON job required")?)?;
    if job["op"]
        .as_str()
        .is_some_and(|op| op.starts_with("remote-"))
    {
        use obd_core_adapter::remote;
        let value = runtime().block_on(async {
            Ok::<Value, anyhow::Error>(match job["op"].as_str().unwrap() {
                "remote-publish" => remote::publish(Path::new(job["config"].as_str().context("config")?),job["repository"].as_str().context("repository")?).await?,
                "remote-manifest" => json!(remote::manifest(job["url"].as_str().context("url")?).await?),
                "remote-info" => json!({"virtual_size":remote::layer_size(&serde_json::from_value(job["remote"].clone())?, Path::new(job["cache"].as_str().context("cache")?)).await?}),
                "remote-fetch" => {remote::fetch(&serde_json::from_value(job["remote"].clone())?,Path::new(job["output"].as_str().context("output")?)).await?; json!({})},
                _ => anyhow::bail!("unknown remote job"),
            })
        })?;
        println!("{value}");
        return Ok(());
    }
    if job["op"] == "compact-layers" {
        let paths: Vec<std::path::PathBuf> = serde_json::from_value(job["layers"].clone())?;
        let geometry: Geometry = serde_json::from_value(job["geometry"].clone())?;
        runtime().block_on(obd_core_adapter::compact_layers(
            &paths,
            Path::new(job["output"].as_str().context("output")?),
            geometry,
        ))?;
        println!("{{}}");
        return Ok(());
    }
    if matches!(job["op"].as_str(), Some("layer-info" | "compress-layer")) {
        let source = Path::new(job["source"].as_str().context("source")?);
        if job["op"] == "compress-layer" {
            runtime().block_on(obd_core_adapter::compress_layer(
                source,
                Path::new(job["output"].as_str().context("output")?),
                job["compression"].as_str().context("compression")?,
            ))?;
        }
        let size = runtime().block_on(obd_core_adapter::archive_layer_size(source))?;
        println!("{}", json!({"virtual_size":size}));
        return Ok(());
    }
    let dir = Path::new(job["directory"].as_str().context("directory")?);
    let _lock = lock_resource(dir)?;
    let result = match job["op"].as_str().context("op")? {
        "create" => {
            let g: Geometry = serde_json::from_value(job["geometry"].clone())?;
            runtime().block_on(BlockImage::create_upper(
                Path::new(job["upper"].as_str().context("upper")?),
                g,
            ))?;
            json!({})
        }
        "export" => {
            let h = runtime().block_on(open_image(dir, false))?;
            runtime().block_on(async {
                h.flush().await?;
                h.export_upper(Path::new(job["output"].as_str().context("output")?))
                    .await
            })?;
            json!({})
        }
        "stat" => {
            let h = runtime().block_on(open_image(dir, false))?;
            json!({"valid_mapped_bytes":runtime().block_on(h.valid_data_bytes())?})
        }
        "recover" => {
            let h = runtime().block_on(open_image(dir, false))?;
            runtime().block_on(h.flush())?;
            json!({})
        }
        _ => anyhow::bail!("unknown worker job"),
    };
    println!("{result}");
    Ok(())
}
