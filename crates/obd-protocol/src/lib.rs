//! v1: big-endian u32 byte length followed by UTF-8 JSON; bounded frames.
use anyhow::{ensure, Result};
use obd_core_adapter::Geometry;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub operation: Operation,
}
impl Request {
    pub fn new(operation: Operation) -> Self {
        Self {
            version: VERSION,
            operation,
        }
    }
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    PublishSnapshot {
        id: String,
        repository: String,
        compression: String,
    },
    PullImage {
        id: String,
        manifest: String,
    },
    FetchImage {
        id: String,
    },
    CompactSnapshot {
        source: String,
        name: String,
    },
    SpaceUsage {
        kind: String,
        id: String,
    },
    ForkStatus {
        id: Option<String>,
    },
    ImportImage {
        id: String,
        image_config: std::path::PathBuf,
    },
    ExportSnapshot {
        id: String,
        output: std::path::PathBuf,
        #[serde(default)]
        compression: Option<String>,
    },
    CancelFork {
        id: String,
    },
    DeviceFeatures,
    DeviceMount {
        dev_id: u32,
        mountpoint: std::path::PathBuf,
        volume: Option<String>,
        #[serde(default)]
        nobrowse: bool,
    },
    DeviceUnmount {
        dev_id: u32,
        volume: Option<String>,
    },
    DeviceRuntimeCreate {
        dev_id: u32,
        source_image_config: std::path::PathBuf,
        image: String,
        #[serde(default)]
        read_only: bool,
    },
    ImageFork {
        source: String,
        id: String,
        #[serde(default)]
        live: bool,
    },
    DeviceCreate {
        dev_id: u32,
        image: Option<String>,
        image_config: Option<std::path::PathBuf>,
        global_config: Option<std::path::PathBuf>,
        #[serde(default)]
        read_only: bool,
    },
    DeviceDelete {
        dev_id: u32,
    },
    DeviceStatus {
        dev_id: Option<u32>,
    },
    Create {
        id: String,
        geometry: Geometry,
    },
    List,
    Stat {
        id: String,
    },
    Snapshot {
        id: String,
        name: String,
        #[serde(default)]
        seal: bool,
    },
    LiveSnapshot {
        id: String,
        name: String,
    },
    PendingRestacks,
    Snapshots,
    Fork {
        snapshot: String,
        id: String,
    },
    Delete {
        id: String,
    },
    DeleteSnapshot {
        id: String,
    },
    Resource {
        id: String,
    },
    Gc,
    Claim {
        id: String,
        writable: bool,
        instance: String,
    },
    Poll {
        attachment: obd_runtime::Attachment,
        receipt: Option<obd_runtime::Receipt>,
        closed: bool,
    },
    Leases,
    Recover {
        token: String,
        confirmed_offline: bool,
    },
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    pub result: Option<serde_json::Value>,
    pub error: Option<RemoteError>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct RemoteError {
    pub errno: i32,
    pub message: String,
}
impl Response {
    pub fn success(value: serde_json::Value) -> Self {
        Self {
            version: VERSION,
            result: Some(value),
            error: None,
        }
    }
    pub fn failure(errno: i32, message: String) -> Self {
        Self {
            version: VERSION,
            result: None,
            error: Some(RemoteError { errno, message }),
        }
    }
    pub fn into_result(self) -> Result<serde_json::Value> {
        ensure!(self.version == VERSION, "protocol version mismatch");
        if let Some(e) = self.error {
            anyhow::bail!("errno {}: {}", e.errno, e.message);
        }
        self.result
            .ok_or_else(|| anyhow::anyhow!("missing response result"))
    }
}
pub async fn receive<R: AsyncRead + Unpin, T: DeserializeOwned>(stream: &mut R) -> Result<T> {
    let len = stream.read_u32().await? as usize;
    ensure!(len > 0 && len <= MAX_FRAME, "invalid frame length");
    let mut data = vec![0; len];
    stream.read_exact(&mut data).await?;
    Ok(serde_json::from_slice(&data)?)
}
pub async fn send<W: AsyncWrite + Unpin, T: Serialize>(stream: &mut W, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "frame too large");
    stream.write_u32(bytes.len() as u32).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}
pub async fn call(stream: &mut tokio::net::UnixStream, op: Operation) -> Result<serde_json::Value> {
    send(stream, &Request::new(op)).await?;
    receive::<_, Response>(stream).await?.into_result()
}
