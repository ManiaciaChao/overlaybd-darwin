mod devices;
use anyhow::{ensure, Context, Result};
use obd_protocol::{receive, send, Operation, Request, Response, VERSION};
use obd_store::Store;
use serde_json::{json, Value};
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, UnixListener},
    sync::{Mutex, Semaphore},
};

async fn execute(store: &mut Store, _owner: u64, op: Operation) -> Result<Value> {
    let claim = matches!(&op, Operation::Claim { .. });
    let mut value = match op {
        Operation::PublishSnapshot {
            id,
            repository,
            compression,
        } => {
            store
                .publish_snapshot(&id, &repository, &compression)
                .await?
        }
        Operation::PullImage { id, manifest } => json!(store.pull_image(&id, &manifest).await?),
        Operation::FetchImage { id } => store.fetch_image(&id).await?,
        Operation::CompactSnapshot { source, name } => {
            json!(store.compact_snapshot(&source, &name).await?)
        }
        Operation::SpaceUsage { kind, id } => store.space_usage(&kind, &id)?,
        Operation::ForkStatus { id } => store.image_fork_status(id.as_deref())?,
        Operation::ImportImage { id, image_config } => {
            json!(store.import_image(&id, &image_config).await?)
        }
        Operation::ExportSnapshot {
            id,
            output,
            compression,
        } => {
            store
                .export_snapshot(&id, &output, compression.as_deref().unwrap_or("none"))
                .await?
        }
        Operation::CancelFork { id } => {
            store.cancel_image_fork(&id)?;
            json!({})
        }
        Operation::DeviceCreate { .. }
        | Operation::DeviceDelete { .. }
        | Operation::DeviceStatus { .. }
        | Operation::DeviceFeatures
        | Operation::DeviceMount { .. }
        | Operation::DeviceUnmount { .. }
        | Operation::DeviceRuntimeCreate { .. }
        | Operation::ImageFork { .. } => anyhow::bail!("device dispatch required"),
        Operation::Create { id, geometry } => json!(store.create(&id, geometry).await?),
        Operation::List => json!(store.images()),
        Operation::Stat { id } => json!(store.stat(&id).await?),
        Operation::Snapshot { id, name, seal } => json!(store.snapshot(&id, &name, seal).await?),
        Operation::LiveSnapshot { id, name } => json!(store.live_snapshot(&id, &name)?),
        Operation::PendingRestacks => json!(store.pending_restacks()),
        Operation::Snapshots => json!(store.snapshots()),
        Operation::Fork { snapshot, id } => json!(store.fork(&snapshot, &id).await?),
        Operation::Delete { id } => {
            store.delete_image(&id)?;
            json!({})
        }
        Operation::DeleteSnapshot { id } => {
            store.delete_snapshot(&id)?;
            json!({})
        }
        Operation::Resource { id } => json!({"directory":store.resource(&id)?}),
        Operation::Gc => json!(store.gc()?),
        Operation::Claim {
            id,
            writable,
            instance,
        } => json!(store.claim(&id, writable, &instance)?),
        Operation::Poll {
            attachment,
            receipt,
            closed,
        } => store.poll(&attachment, receipt, closed)?,
        Operation::Leases => json!(store.leases()?),
        Operation::Recover {
            token,
            confirmed_offline,
        } => {
            ensure!(
                confirmed_offline,
                "confirm all device consumers are detached before recovery"
            );
            store.recover_lease(&token).await?;
            json!({})
        }
    };
    // Credentials belong only in private descriptors and the claim response.
    fn redact(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.remove("secret");
                fields.remove("credential");
                for v in fields.values_mut() {
                    redact(v);
                }
            }
            Value::Array(values) => {
                for v in values {
                    redact(v);
                }
            }
            _ => {}
        }
    }
    if !claim {
        redact(&mut value);
    }
    Ok(value)
}
fn error_response(e: anyhow::Error) -> Response {
    let message = format!("{e:#}");
    let errno = e
        .chain()
        .find_map(|e| {
            e.downcast_ref::<std::io::Error>()
                .and_then(|io| io.raw_os_error())
        })
        .unwrap_or_else(|| {
            if message.contains("EROFS") {
                libc::EROFS
            } else if message.contains("ENOTSUP") {
                libc::ENOTSUP
            } else if message.contains("lease") || message.contains("already") {
                libc::EBUSY
            } else if message.contains("not found") {
                libc::ENOENT
            } else if message.contains("capacity")
                || message.contains("request exceeds")
                || message.contains("version")
            {
                libc::EINVAL
            } else {
                libc::EIO
            }
        });
    Response::failure(errno, message)
}
async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    store: Arc<Mutex<Store>>,
    owner: u64,
    devices: Arc<Mutex<()>>,
    scope: Option<String>,
) -> Result<()> {
    loop {
        // Idle sessions are allowed. Bound time to finish a frame after first byte.
        let mut first = [0u8; 1];
        use tokio::io::AsyncReadExt;
        if stream.read(&mut first).await? == 0 {
            return Ok(());
        }
        let prefix: &[u8] = &first;
        let mut reader = prefix.chain(&mut stream);
        let req: Request =
            tokio::time::timeout(Duration::from_secs(30), receive(&mut reader)).await??;
        let result = if req.version != VERSION {
            Err(anyhow::anyhow!("unsupported protocol version"))
        } else {
            let allowed = match (&scope, &req.operation) {
                (None, _) => true,
                (Some(id), Operation::Claim { id: target, .. }) => id == target,
                (Some(id), Operation::Poll { attachment, .. }) => store
                    .lock()
                    .await
                    .leases()?
                    .iter()
                    .any(|l| l.image == *id && l.token == attachment.token),
                _ => false,
            };
            if allowed {
                if matches!(
                    &req.operation,
                    Operation::DeviceCreate { .. }
                        | Operation::DeviceDelete { .. }
                        | Operation::DeviceStatus { .. }
                        | Operation::DeviceFeatures
                        | Operation::DeviceMount { .. }
                        | Operation::DeviceUnmount { .. }
                        | Operation::DeviceRuntimeCreate { .. }
                        | Operation::ImageFork { .. }
                ) {
                    if matches!(&req.operation, Operation::DeviceStatus { .. }) {
                        devices::execute(store.clone(), req.operation).await
                    } else {
                        let _guard = devices.lock().await;
                        devices::execute(store.clone(), req.operation).await
                    }
                } else {
                    execute(&mut *store.lock().await, owner, req.operation).await
                }
            } else {
                Err(anyhow::anyhow!("unauthorized control operation"))
            }
        };
        let response = match result {
            Ok(value) => Response::success(value),
            Err(e) => error_response(e),
        };
        tokio::time::timeout(Duration::from_secs(30), send(&mut stream, &response)).await??;
    }
}
#[tokio::main]
async fn main() -> Result<()> {
    // Restrict SQLite and all runtime artifacts from their first creation.
    unsafe {
        libc::umask(0o077);
    }
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .context("usage: overlaybd-macosd STORE_DIRECTORY [RESOURCE_DIRECTORY]")?,
    );
    let mut store = Store::open(&root)?;
    // Optional scoped IPC directory keeps layer files out of the FSKit grant.
    let endpoint = if let Some(path) = std::env::args_os().nth(2) {
        let path = PathBuf::from(path);
        std::fs::create_dir_all(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o077 == 0,
            "resource directory must be owned, private, and not a symlink"
        );
        std::fs::canonicalize(path)?
    } else {
        store.root().to_path_buf()
    };
    // A distinct endpoint cannot be stolen by another daemon/store.
    let endpoint_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(endpoint.join("endpoint.lock"))?;
    endpoint_lock.try_lock()?;
    let socket = endpoint.join("control.sock");
    ensure!(
        socket.as_os_str().len() < 104,
        "store path too long for macOS Unix socket"
    );
    if let Ok(m) = std::fs::symlink_metadata(&socket) {
        ensure!(m.file_type().is_socket(), "refusing to remove non-socket");
        std::fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    // Opt-in demo IPC, bound to IPv4 loopback only. The scoped directory
    // grants the extension access to this per-run random bearer credential.
    let tcp = Some(TcpListener::bind("127.0.0.1:0").await?);
    store.set_endpoint(tcp.as_ref().unwrap().local_addr()?.port())?;
    // Readiness descriptor has no credential. Each image has its own scoped secret.
    std::fs::write(endpoint.join("connection.json"), b"{\"version\":1}")?;
    let store = Arc::new(Mutex::new(store));
    let devices = Arc::new(Mutex::new(()));
    let limit = Arc::new(Semaphore::new(64));
    let mut sequence = 0u64;
    eprintln!("listening on {}", socket.display());
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            accepted = async { match &tcp { Some(listener) => listener.accept().await, None => std::future::pending().await } } => {
                let (mut stream, address) = accepted?;
                if !address.ip().is_loopback() { continue; }
                let Ok(permit) = limit.clone().try_acquire_owned() else { continue; };
                sequence = sequence.checked_add(1).context("session sequence exhausted")?;
                let owner = sequence; let state = store.clone(); let devices = devices.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let result: Result<()> = async {
                        let auth: Value = tokio::time::timeout(Duration::from_secs(5), receive(&mut stream)).await??;
                        ensure!(auth["version"] == 1 && auth["operation"]["op"] == "authenticate", "unauthorized IPC client");
                        let scope=state.lock().await.authenticate(auth["operation"]["secret"].as_str().unwrap_or("")).context("unauthorized image resource")?;
                        stream.set_nodelay(true)?;
                        tokio::time::timeout(Duration::from_secs(5), send(&mut stream, &Response::success(json!({})))).await??;
                        session(stream, state.clone(), owner, devices, Some(scope)).await
                    }.await;
                    if let Err(e) = result { eprintln!("IPC session {owner}: {e:#}"); }

                });
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } { continue; }
                let Ok(permit) = limit.clone().try_acquire_owned() else { drop(stream); continue; };
                sequence = sequence.checked_add(1).context("session sequence exhausted")?;
                let owner = sequence; let state = store.clone(); let devices = devices.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = session(stream, state.clone(), owner, devices, None).await { eprintln!("session {owner}: {e:#}"); }

                });
            }
        }
    }
    // Unreleased RAW leases remain durable and block snapshots after restart.
    store.lock().await.checkpoint()?;
    std::fs::remove_file(socket)?;
    if tcp.is_some() {
        std::fs::remove_file(endpoint.join("connection.json"))?;
    }
    Ok(())
}
