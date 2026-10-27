use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use obd_core_adapter::{Geometry, MAX_IO};
use obd_protocol::{call, Operation};
use std::{io::Write, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "obd",
    about = "Local OverlayBD image control; FSKit integration tests are in tests/integration/"
)]
struct Args {
    #[arg(long, env = "OBD_SOCKET")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    #[command(subcommand)]
    Device(DeviceCommand),
    #[command(subcommand)]
    Image(ImageCommand),
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Diagnostic block reads/writes, only when no RAW lease exists.
    #[command(subcommand)]
    Block(BlockCommand),
    /// List durable RAW leases, including interrupted sessions.
    Leases,
    /// Inspect interrupted in-place restacks. Do not delete their files.
    PendingRestacks,
    /// Release an interrupted lease only after verifying all consumers are detached.
    Recover {
        token: String,
        #[arg(long, required = true)]
        confirmed_offline: bool,
    },
    /// Reclaim unreferenced artifacts. Requires no RAW leases.
    Gc,
}
#[derive(Subcommand)]
enum DeviceCommand {
    /// Query the implemented macOS controller capabilities.
    Features,
    Mount {
        dev_id: u32,
        #[arg(long)]
        mountpoint: PathBuf,
        #[arg(long)]
        volume: Option<String>,
        /// Hide the mounted volume from Finder.
        #[arg(long)]
        nobrowse: bool,
    },
    Unmount {
        dev_id: u32,
        #[arg(long)]
        volume: Option<String>,
    },
    /// Snapshot a managed source and create an independent runtime image/device.
    CreateRuntime {
        dev_id: u32,
        #[arg(long)]
        source_image_config: PathBuf,
        #[arg(long)]
        image: String,
        #[arg(long)]
        read_only: bool,
    },
    /// Present an existing image without formatting or mounting its filesystem.
    #[command(visible_alias = "add")]
    Create {
        dev_id: u32,
        #[command(subcommand)]
        target: DeviceTarget,
    },
    /// Detach the virtual device; retain its image data.
    #[command(visible_alias = "del")]
    Delete { dev_id: u32 },
    /// Inspect persisted intent and current operating-system resources.
    Status { dev_id: Option<u32> },
}
#[derive(Subcommand)]
enum DeviceTarget {
    Overlaybd {
        #[arg(
            long,
            required_unless_present = "image_config",
            conflicts_with = "image_config"
        )]
        image: Option<String>,
        /// Must identify an image managed by this store.
        #[arg(long)]
        image_config: Option<PathBuf>,
        /// Validate explicit local-profile settings; unsupported settings are rejected.
        #[arg(long)]
        global_config: Option<PathBuf>,
        #[arg(long)]
        read_only: bool,
    },
}
#[derive(Subcommand)]
enum ImageCommand {
    /// Create a writable image backed by lazily read HTTP(S) CAS layers.
    Pull {
        id: String,
        #[arg(long)]
        manifest: String,
    },
    /// Fully download and SHA-256 verify remote layers for offline use (detached).
    Fetch {
        id: String,
    },
    /// Sample physical layer/upper sizes, including shared ownership, while mounted or detached.
    Usage {
        id: String,
    },
    ForkStatus {
        id: Option<String>,
    },
    /// Import immutable local layers from an upstream image configuration.
    Import {
        id: String,
        #[arg(long)]
        image_config: PathBuf,
    },
    /// Release an incomplete fork intent only when no restack is pending.
    ForkCancel {
        id: String,
    },
    Create {
        id: String,
        #[arg(long, value_parser=parse_size)]
        size: u64,
        #[arg(long, default_value_t = 512)]
        block_size: u32,
    },
    Fork {
        /// Snapshot ID, or source image ID with --live/--from-image.
        snapshot: String,
        id: String,
        #[arg(long, conflicts_with = "from_image")]
        live: bool,
        #[arg(long)]
        from_image: bool,
    },
    List,
    Resource {
        id: String,
    },
    Stat {
        id: String,
    },
    Delete {
        id: String,
    },
}
#[derive(Subcommand)]
enum SnapshotCommand {
    /// Publish immutable blobs, then an upstream-shaped content-addressed manifest.
    Publish {
        id: String,
        #[arg(long)]
        repository: String,
        #[arg(long, default_value="zstd", value_parser=["none","lz4","zstd"])]
        compression: String,
    },
    /// Flatten an immutable snapshot into a new single-layer snapshot.
    #[command(visible_alias = "flatten")]
    Compact {
        id: String,
        #[arg(long)]
        name: String,
    },
    /// Report referenced layer sizes and shared ownership.
    Stat {
        id: String,
    },
    /// Export immutable layers and an upstream-compatible local image.json.
    Export {
        id: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "none", value_parser = ["none", "lz4", "zstd"])]
        compression: String,
    },
    Create {
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        seal: bool,
        /// Keep the device active. Block-point-in-time only; does not freeze apps/filesystems.
        #[arg(long, conflicts_with = "seal")]
        live: bool,
    },
    List,
    Delete {
        id: String,
    },
}
#[derive(Subcommand)]
enum BlockCommand {
    Read {
        id: String,
        offset: u64,
        length: usize,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Write {
        id: String,
        offset: u64,
        #[arg(long)]
        input: PathBuf,
    },
    Zero {
        id: String,
        offset: u64,
        length: usize,
    },
}
fn parse_size(value: &str) -> Result<u64, String> {
    let (digits, scale) = match value.as_bytes().last() {
        Some(b'K') => (&value[..value.len() - 1], 1024u64),
        Some(b'M') => (&value[..value.len() - 1], 1024u64.pow(2)),
        Some(b'G') => (&value[..value.len() - 1], 1024u64.pow(3)),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .map_err(|e| e.to_string())?
        .checked_mul(scale)
        .ok_or_else(|| "size overflow".into())
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut stream = tokio::net::UnixStream::connect(&args.socket).await?;
    if let Command::Block(cmd) = args.command {
        let (id, writable) = match &cmd {
            BlockCommand::Read { id, .. } => (id, false),
            BlockCommand::Write { id, .. } | BlockCommand::Zero { id, .. } => (id, true),
        };
        let value = call(&mut stream, Operation::Resource { id: id.clone() }).await?;
        let dir = PathBuf::from(
            value["directory"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing resource"))?,
        );
        tokio::task::spawn_blocking(move || -> Result<()> {
            let e = obd_runtime::Engine::attached(&dir, writable)?;
            let result = (|| -> Result<()> {
                match cmd {
                    BlockCommand::Read {
                        offset,
                        length,
                        output,
                        ..
                    } => {
                        ensure!(length <= MAX_IO, "request exceeds 4 MiB");
                        let mut data = vec![0; length];
                        let n = e.read(offset, &mut data)?;
                        data.truncate(n);
                        if let Some(path) = output {
                            std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(path)?
                                .write_all(&data)?;
                        } else {
                            std::io::stdout().lock().write_all(&data)?;
                        }
                    }
                    BlockCommand::Write { offset, input, .. } => {
                        ensure!(
                            std::fs::metadata(&input)?.len() <= MAX_IO as u64,
                            "input exceeds 4 MiB"
                        );
                        e.write(offset, &std::fs::read(input)?)?;
                    }
                    BlockCommand::Zero { offset, length, .. } => {
                        ensure!(length <= MAX_IO, "request exceeds 4 MiB");
                        e.write(offset, &vec![0; length])?;
                    }
                };
                Ok(())
            })();
            let close = e.close();
            result?;
            close?;
            Ok(())
        })
        .await??;
        return Ok(());
    }
    let mut live_name = None;
    let op = match args.command {
        Command::Device(cmd) => match cmd {
            DeviceCommand::Features => Operation::DeviceFeatures,
            DeviceCommand::Mount {
                dev_id,
                mountpoint,
                volume,
                nobrowse,
            } => Operation::DeviceMount {
                dev_id,
                mountpoint: std::path::absolute(mountpoint)?,
                volume,
                nobrowse,
            },
            DeviceCommand::Unmount { dev_id, volume } => {
                Operation::DeviceUnmount { dev_id, volume }
            }
            DeviceCommand::CreateRuntime {
                dev_id,
                source_image_config,
                image,
                read_only,
            } => Operation::DeviceRuntimeCreate {
                dev_id,
                source_image_config: std::path::absolute(source_image_config)?,
                image,
                read_only,
            },
            DeviceCommand::Create {
                dev_id,
                target:
                    DeviceTarget::Overlaybd {
                        image,
                        image_config,
                        global_config,
                        read_only,
                    },
            } => Operation::DeviceCreate {
                dev_id,
                image,
                image_config: image_config.map(std::path::absolute).transpose()?,
                global_config: global_config.map(std::path::absolute).transpose()?,
                read_only,
            },
            DeviceCommand::Delete { dev_id } => Operation::DeviceDelete { dev_id },
            DeviceCommand::Status { dev_id } => Operation::DeviceStatus { dev_id },
        },
        Command::Image(cmd) => match cmd {
            ImageCommand::ForkStatus { id } => Operation::ForkStatus { id },
            ImageCommand::Pull { id, manifest } => Operation::PullImage { id, manifest },
            ImageCommand::Fetch { id } => Operation::FetchImage { id },
            ImageCommand::Import { id, image_config } => Operation::ImportImage {
                id,
                image_config: std::path::absolute(image_config)?,
            },
            ImageCommand::ForkCancel { id } => Operation::CancelFork { id },
            ImageCommand::Create {
                id,
                size,
                block_size,
            } => Operation::Create {
                id,
                geometry: Geometry {
                    virtual_size: size,
                    logical_block_size: block_size,
                }
                .validate()?,
            },
            ImageCommand::Fork {
                snapshot,
                id,
                live,
                from_image,
            } => {
                if live || from_image {
                    Operation::ImageFork {
                        source: snapshot,
                        id,
                        live,
                    }
                } else {
                    Operation::Fork { snapshot, id }
                }
            }
            ImageCommand::List => Operation::List,
            ImageCommand::Resource { id } => Operation::Resource { id },
            ImageCommand::Usage { id } => Operation::SpaceUsage {
                kind: "image".into(),
                id,
            },
            ImageCommand::Stat { id } => Operation::Stat { id },
            ImageCommand::Delete { id } => Operation::Delete { id },
        },
        Command::Snapshot(cmd) => match cmd {
            SnapshotCommand::Publish {
                id,
                repository,
                compression,
            } => Operation::PublishSnapshot {
                id,
                repository,
                compression,
            },
            SnapshotCommand::Compact { id, name } => {
                Operation::CompactSnapshot { source: id, name }
            }
            SnapshotCommand::Stat { id } => Operation::SpaceUsage {
                kind: "snapshot".into(),
                id,
            },
            SnapshotCommand::Export {
                id,
                output,
                compression,
            } => Operation::ExportSnapshot {
                compression: Some(compression),
                id,
                output: std::path::absolute(output)?,
            },
            SnapshotCommand::Create {
                id,
                name,
                seal,
                live,
            } => {
                if live {
                    live_name = Some((id.clone(), name.clone()));
                    Operation::LiveSnapshot { id, name }
                } else {
                    Operation::Snapshot { id, name, seal }
                }
            }
            SnapshotCommand::List => Operation::Snapshots,
            SnapshotCommand::Delete { id } => Operation::DeleteSnapshot { id },
        },
        Command::Block(_) => unreachable!(),
        Command::PendingRestacks => Operation::PendingRestacks,
        Command::Leases => Operation::Leases,
        Command::Recover {
            token,
            confirmed_offline,
        } => Operation::Recover {
            token,
            confirmed_offline,
        },
        Command::Gc => Operation::Gc,
    };
    let mut value = call(&mut stream, op).await?;
    if let Some((id, name)) = live_name {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let snapshots = call(&mut stream, Operation::Snapshots).await?;
            if let Some(s) = snapshots
                .as_array()
                .and_then(|items| items.iter().find(|s| s["id"] == name))
            {
                let leases = call(&mut stream, Operation::Leases).await?;
                if leases
                    .as_array()
                    .is_some_and(|ls| ls.iter().any(|l| l["image"] == id && l["ready"] == true))
                {
                    value = s.clone();
                    break;
                }
            }
            ensure!(tokio::time::Instant::now()<deadline,"snapshot remains pending; inspect pending-restacks (do not retry with another operation)");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_device_spelling_and_explicit_unsupported_flags() {
        assert!(Args::try_parse_from([
            "obd",
            "--socket",
            "/tmp/s",
            "device",
            "add",
            "1",
            "overlaybd",
            "--image-config",
            "image.json",
            "--global-config",
            "global.json"
        ])
        .is_ok());
        assert!(Args::try_parse_from(["obd", "--socket", "/tmp/s", "device", "del", "1"]).is_ok());
        assert!(Args::try_parse_from([
            "obd",
            "--socket",
            "/tmp/s",
            "device",
            "create",
            "--nr-queues",
            "1",
            "1",
            "overlaybd",
            "--image",
            "a"
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "obd",
            "--socket",
            "/tmp/s",
            "device",
            "create",
            "1",
            "overlaybd"
        ])
        .is_err());
        assert!(Args::try_parse_from([
            "obd",
            "--socket",
            "/tmp/s",
            "device",
            "create",
            "1",
            "overlaybd",
            "--image",
            "a",
            "--image-config",
            "a.json"
        ])
        .is_err());
    }
}
