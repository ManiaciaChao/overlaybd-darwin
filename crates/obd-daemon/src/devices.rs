//! Durable control-plane orchestration. Never handles block I/O.
use anyhow::{bail, ensure, Context, Result};
use obd_protocol::Operation;
use obd_store::Store;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command, sync::Mutex};

async fn run(program: &str, args: &[&str]) -> Result<Vec<u8>> {
    let result = tokio::time::timeout(
        Duration::from_secs(90),
        Command::new(program).args(args).kill_on_drop(true).output(),
    )
    .await
    .context("system command timed out; inspect device status")??;
    ensure!(
        result.status.success(),
        "{program}: {}{}",
        String::from_utf8_lossy(&result.stderr),
        String::from_utf8_lossy(&result.stdout)
    );
    Ok(result.stdout)
}
async fn plist(program: &str, args: &[&str]) -> Result<Value> {
    let bytes = run(program, args).await?;
    let mut child = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .context("stdin")?
        .write_all(&bytes)
        .await?;
    let out = child.wait_with_output().await?;
    ensure!(out.status.success(), "invalid system plist");
    Ok(serde_json::from_slice(&out.stdout)?)
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("missing device field {key}"))
}
fn whole_disk(s: &str) -> bool {
    s.strip_prefix("/dev/disk")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}
#[cfg(target_os = "macos")]
fn mount_source(path: &Path) -> Result<Option<String>> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let point = unsafe { std::ffi::CStr::from_ptr(stat.f_mntonname.as_ptr()) }.to_str()?;
    if Path::new(point) != path {
        return Ok(None);
    }
    let kind = unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_str()?;
    ensure!(kind == "obdraw", "foreign filesystem on managed mountpoint");
    Ok(Some(
        unsafe { std::ffi::CStr::from_ptr(stat.f_mntfromname.as_ptr()) }
            .to_str()?
            .to_owned(),
    ))
}
#[cfg(not(target_os = "macos"))]
fn mount_source(_: &Path) -> Result<Option<String>> {
    bail!("ENOTSUP: macOS device backend")
}

fn source_path(source: &str) -> Result<PathBuf> {
    // FSKit reports a percent-escaped file URL, not a POSIX source name.
    let source = source
        .strip_prefix("file://")
        .context("unexpected FSKit source URL")?;
    ensure!(source.starts_with('/'), "non-local FSKit source URL");
    let bytes = source.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            ensure!(i + 2 < bytes.len(), "invalid source URL escape");
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])?;
            decoded.push(u8::from_str_radix(hex, 16)?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Ok(std::fs::canonicalize(PathBuf::from(
        std::ffi::OsString::from_vec(decoded),
    ))?)
}
async fn observe(record: &Value) -> Result<Value> {
    let proxy = Path::new(text(record, "proxy")?);
    let source = if proxy.exists() {
        mount_source(proxy)?
    } else {
        None
    };
    if let Some(source) = &source {
        ensure!(
            source_path(source)? == Path::new(text(record, "resource")?),
            "FSKit source identity mismatch: {source}"
        );
    }
    let raw = proxy.join("disk.raw");
    let info = plist("/usr/bin/hdiutil", &["info", "-plist"]).await?;
    let matches: Vec<_> = info["images"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| {
            i["image-path"]
                .as_str()
                .is_some_and(|s| Path::new(s) == raw)
        })
        .collect();
    ensure!(
        matches.len() <= 1,
        "multiple DiskImages attachments; manual inspection required"
    );
    let disk = matches
        .first()
        .and_then(|i| i["system-entities"].as_array())
        .and_then(|entities| {
            entities
                .iter()
                .filter_map(|e| e["dev-entry"].as_str())
                .find(|s| whole_disk(s))
        });
    ensure!(
        matches.is_empty() || disk.is_some(),
        "attachment has no identifiable whole disk"
    );
    if let Some(disk) = disk {
        ensure!(
            source.is_some(),
            "DiskImages attachment without expected FSKit mount"
        );
        let info = plist("/usr/sbin/diskutil", &["info", "-plist", disk]).await?;
        ensure!(
            info["WholeDisk"] == true && info["VirtualOrPhysical"] == "Virtual",
            "not a whole virtual disk"
        );
    }
    Ok(json!({"fskit_mounted":source.is_some(), "device_path":disk}))
}
// Called only after observing both presentation layers offline. The recovery
// worker also takes runtime.lock; a surviving engine must keep blocking cleanup.
async fn recover_offline_device_leases(
    store: &mut Store,
    image: &str,
    observed: &Value,
) -> Result<()> {
    ensure!(
        observed.get("device_path") == Some(&Value::Null)
            && observed.get("fskit_mounted") == Some(&Value::Bool(false)),
        "device consumers are not confirmed offline; refusing RAW lease recovery"
    );
    let leases = store.leases()?;
    for lease in leases.iter().filter(|lease| lease.image == image) {
        store.recover_lease(&lease.token).await.context(
            "cannot recover RAW lease after unmount; engine may still be active or recovery requires inspection",
        )?;
    }
    Ok(())
}

fn validate_global_profile(config: &Value) -> Result<()> {
    // Explicit assertions of the profile actually used by the in-process engine.
    // No unsupported settings are silently accepted or forwarded to network code.
    let object = config
        .as_object()
        .context("global config must be an object")?;
    for (key, value) in object {
        match key.as_str() {
            "enableAudit" => ensure!(
                *value == false,
                "ENOTSUP: audit is disabled in the local backend"
            ),
            "remoteIoWorkers" => ensure!(*value == 0, "ENOTSUP: remote I/O is disabled"),
            "cacheConfig" => ensure!(
                value
                    .as_object()
                    .is_some_and(|o| o.iter().all(|(k, v)| k == "cacheSizeGB" && *v == 0)),
                "ENOTSUP: only cacheSizeGB=0 is supported"
            ),
            _ => bail!("ENOTSUP: unsupported global config field {key}"),
        }
    }
    Ok(())
}
fn partition_of(identifier: &str, disk: &str) -> bool {
    identifier == disk
        || identifier.strip_prefix(disk).is_some_and(|tail| {
            tail.starts_with('s') && tail[1..].bytes().all(|b| b.is_ascii_digit()) && tail.len() > 1
        })
}
async fn volumes(record: &Value) -> Result<Vec<Value>> {
    let observed = observe(record).await?;
    volumes_for_device(text(&observed, "device_path")?).await
}
async fn volumes_for_device(device: &str) -> Result<Vec<Value>> {
    crate::disk_arbitration::volumes(device.to_owned()).await
}

async fn select_mount_volume(record: &Value, selector: Option<&str>) -> Result<Value> {
    let observed = observe(record).await?;
    let device = text(&observed, "device_path")?;
    select_mount_volume_for_device(device, selector).await
}

async fn select_mount_volume_for_device(device: &str, selector: Option<&str>) -> Result<Value> {
    let disk = device.strip_prefix("/dev/").context("disk identifier")?;
    let selected = if let Some(path) = selector.filter(|s| s.starts_with("/dev/")) {
        // Explicit device paths need no global discovery. Ownership is still
        // derived from the current RAW attachment, not a persisted disk number.
        json!({"device_path":path})
    } else {
        let candidates = volumes_for_device(device).await?;
        select_volume(&candidates, selector)?.clone()
    };
    let path = text(&selected, "device_path")?;
    let id = path.strip_prefix("/dev/").context("volume identifier")?;
    let info = plist("/usr/sbin/diskutil", &["info", "-plist", path]).await?;
    ensure!(
        info["DeviceIdentifier"] == id && volume_belongs_to(&info, disk),
        "selected volume ownership changed"
    );
    ensure!(
        info["FilesystemType"].is_string(),
        "selected device has no filesystem"
    );
    let current = volume_info(id, &info);
    if selected["uuid"].is_string() {
        ensure!(
            current["uuid"] == selected["uuid"] && current["filesystem"] == selected["filesystem"],
            "selected volume identity changed"
        );
    }
    Ok(current)
}
fn volume_info(id: &str, info: &Value) -> Value {
    json!({"device_path":format!("/dev/{id}"),"uuid":info["VolumeUUID"],"name":info["VolumeName"],
        "partition_content":info["Content"],"filesystem":info["FilesystemType"],
        "mountpoint":info["MountPoint"],"read_only":info["ReadOnlyVolume"]})
}
fn volume_belongs_to(info: &Value, disk: &str) -> bool {
    if info["DeviceIdentifier"]
        .as_str()
        .is_some_and(|id| partition_of(id, disk))
    {
        return true;
    }
    // APFS volumes live on synthesized disks. Verify their physical stores,
    // rather than trusting a volume name, UUID, or reused disk number.
    info["FilesystemType"] == "apfs"
        && info["APFSPhysicalStores"].as_array().is_some_and(|stores| {
            !stores.is_empty()
                && stores.iter().all(|store| {
                    store["APFSPhysicalStore"]
                        .as_str()
                        .is_some_and(|id| partition_of(id, disk))
                })
        })
}
fn select_volume<'a>(list: &'a [Value], selector: Option<&str>) -> Result<&'a Value> {
    let matches: Vec<_> = list
        .iter()
        .filter(|v| match selector {
            Some(s) => v["uuid"] == s || v["device_path"] == s,
            // GPT images can have a FAT-formatted EFI System Partition beside
            // their data volume. Keep it visible and explicitly selectable, but
            // never let it make automatic data-volume selection ambiguous.
            None => v["partition_content"] != "EFI",
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "specify --volume UUID from device status; expected one filesystem volume, found {}",
        matches.len()
    );
    Ok(matches[0])
}
async fn fork_image(
    store: &Arc<Mutex<Store>>,
    source: &str,
    id: &str,
    live: bool,
) -> Result<Value> {
    store
        .lock()
        .await
        .prepare_image_fork(source, id, live)
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(image) = store.lock().await.finish_image_fork(id).await? {
            return Ok(
                json!({"id":image.id,"generation":image.generation,"storage_id":image.storage_id}),
            );
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "fork remains pending; retry the same command to resume, inspect pending-restacks"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
async fn managed_config_image(store: &Arc<Mutex<Store>>, path: &Path) -> Result<String> {
    let path = std::fs::canonicalize(path)?;
    let s = store.lock().await;
    s.images()
        .into_iter()
        .find(|i| s.directory(i).join("image.json") == path)
        .map(|i| i.id)
        .context("image-config must be imported into this store first")
}
async fn save(store: &Arc<Mutex<Store>>, id: u32, r: &Value) -> Result<()> {
    store.lock().await.save_device(id, Some(r.clone()))
}
pub async fn execute(store: Arc<Mutex<Store>>, op: Operation) -> Result<Value> {
    ensure!(cfg!(target_os = "macos"), "ENOTSUP: macOS device backend");
    match op {
        Operation::DeviceFeatures => Ok(json!({"backend":"fskit-diskimages","protocol_version":1,
            "create_overlaybd":true,"create_overlaybd_runtime_device":true,"restack_snapshot":true,
            "mount":true,"unmount":true,"read_only":true,"live_fork":true,
            "update_size":false,"warm_pool":false,"startup_pack":false,"remote_layers":true,"remote_backend":"http-cas","remote_cache_gib_per_upper":1,
            "external_config_import":"immutable_local_layers","snapshot_export_compression":["none","lz4","zstd"],"zfile_local_layers":true,"snapshot_compact":true,"space_usage":true,"runtime_upper_modes":["hybridLogStructured","logStructured"],"upper_backends":["legacy","packed"],"upper_compression":["none","lz4","zstd"],"upper_compression_switch":"detached_checkpoint"})),
        Operation::ImageFork { source, id, live } => fork_image(&store, &source, &id, live).await,
        Operation::DeviceRuntimeCreate {
            dev_id,
            source_image_config,
            image,
            read_only,
        } => {
            let source = managed_config_image(&store, &source_image_config).await?;
            let live = {
                let mut s = store.lock().await;
                if let Some(existing) = s.devices().get(&dev_id) {
                    ensure!(
                        existing["image"] == image
                            && existing["read_only"].as_bool().unwrap_or(false) == read_only,
                        "device ID already belongs to another image or access mode"
                    );
                }
                match s.image_fork_mode(&image) {
                    Some(live) => live,
                    None => s.leases()?.iter().any(|l| l.image == source && l.writable),
                }
            };
            fork_image(&store, &source, &image, live).await?;
            // Separate device publication: a failed attach retains the independent runtime image.
            Box::pin(execute(
                store,
                Operation::DeviceCreate {
                    dev_id,
                    image: Some(image),
                    image_config: None,
                    global_config: None,
                    read_only,
                },
            ))
            .await
        }
        Operation::DeviceMount {
            dev_id,
            mountpoint,
            volume,
            nobrowse,
        } => {
            let mut record = store
                .lock()
                .await
                .devices()
                .get(&dev_id)
                .cloned()
                .context("device not found")?;
            let selected = select_mount_volume(&record, volume.as_deref()).await?;
            if !mountpoint.exists() {
                std::fs::create_dir(&mountpoint)?;
            }
            ensure!(
                !std::fs::symlink_metadata(&mountpoint)?
                    .file_type()
                    .is_symlink(),
                "mountpoint must not be a symlink"
            );
            let mountpoint = mountpoint.canonicalize()?;
            ensure!(
                !mountpoint.starts_with(store.lock().await.root()),
                "mountpoint must be outside the store"
            );
            if selected["mountpoint"]
                .as_str()
                .is_some_and(|p| !p.is_empty())
            {
                ensure!(
                    Path::new(text(&selected, "mountpoint")?) == mountpoint,
                    "volume already mounted elsewhere; unmount first"
                );
                return Ok(selected);
            }
            ensure!(
                std::fs::read_dir(&mountpoint)?.next().is_none(),
                "mountpoint must be empty"
            );
            record["state"] = json!("mounting");
            record["mount_intent"] = json!({"volume":selected["uuid"],"mountpoint":mountpoint});
            save(&store, dev_id, &record).await?;
            let result = async {
                let device = text(&selected, "device_path")?;
                let point = mountpoint.to_str().context("mountpoint UTF-8")?;
                crate::disk_arbitration::mount(
                    device.to_owned(),
                    mountpoint.clone(),
                    record["read_only"] == true,
                    nobrowse,
                )
                .await?;
                // Revalidate the RAW attachment and the selected mounted volume
                // directly. A second full partition/container scan is unnecessary.
                let info_args = ["info", "-plist", device];
                let (observed, info) =
                    tokio::try_join!(observe(&record), plist("/usr/sbin/diskutil", &info_args),)?;
                let disk = text(&observed, "device_path")?
                    .strip_prefix("/dev/")
                    .context("disk identifier")?;
                let id = device.strip_prefix("/dev/").context("volume identifier")?;
                ensure!(
                    info["DeviceIdentifier"] == id && volume_belongs_to(&info, disk),
                    "mounted volume ownership changed"
                );
                let mounted = volume_info(id, &info);
                ensure!(
                    mounted["uuid"] == selected["uuid"]
                        && mounted["filesystem"] == selected["filesystem"],
                    "mounted volume identity changed"
                );
                ensure!(
                    mounted["mountpoint"] == point,
                    "mount did not reach requested path"
                );
                record["state"] = json!("presented");
                record["error"] = Value::Null;
                save(&store, dev_id, &record).await?;
                Ok::<_, anyhow::Error>(mounted)
            }
            .await;
            if let Err(e) = &result {
                record["error"] = json!(format!("{e:#}"));
                save(&store, dev_id, &record).await?;
            }
            result
        }
        Operation::DeviceUnmount { dev_id, volume } => {
            let mut record = store
                .lock()
                .await
                .devices()
                .get(&dev_id)
                .cloned()
                .context("device not found")?;
            let list = volumes(&record).await?;
            let selected = if volume.is_some() {
                vec![select_volume(&list, volume.as_deref())?.clone()]
            } else {
                list
            };
            record["state"] = json!("unmounting");
            save(&store, dev_id, &record).await?;
            let result = async {
                for v in selected {
                    if v["mountpoint"].as_str().is_some_and(|p| !p.is_empty()) {
                        run("/usr/sbin/diskutil", &["unmount", text(&v, "device_path")?]).await?;
                    }
                }
                record["state"] = json!("presented");
                record["error"] = Value::Null;
                save(&store, dev_id, &record).await?;
                Ok::<_, anyhow::Error>(json!({"dev_id":dev_id,"volumes":volumes(&record).await?}))
            }
            .await;
            if let Err(e) = &result {
                record["error"] = json!(format!("{e:#}"));
                save(&store, dev_id, &record).await?;
            }
            result
        }
        Operation::DeviceStatus { dev_id } => {
            let records = store.lock().await.devices().clone();
            if let Some(id) = dev_id {
                ensure!(records.contains_key(&id), "device not found");
            }
            let mut result = Vec::new();
            for (id, mut record) in records {
                if dev_id.is_some_and(|wanted| id != wanted) {
                    continue;
                }
                record["observed"] = match observe(&record).await {
                    Ok(mut v) => {
                        if v["device_path"].is_string() {
                            v["volumes"] = match volumes_for_device(text(&v, "device_path")?).await
                            {
                                Ok(v) => json!(v),
                                Err(e) => json!({"error":format!("{e:#}")}),
                            };
                        }
                        v
                    }
                    Err(e) => json!({"error":format!("{e:#}")}),
                };
                result.push(record);
            }
            Ok(json!(result))
        }
        Operation::DeviceCreate {
            dev_id,
            image,
            image_config,
            global_config,
            read_only,
        } => {
            ensure!(
                image.is_some() != image_config.is_some(),
                "provide exactly one of image or image-config"
            );
            if let Some(path) = global_config {
                let config: Value = serde_json::from_slice(&std::fs::read(path)?)?;
                validate_global_profile(&config)?;
            }
            let mut record = {
                let mut s = store.lock().await;
                let image = if let Some(id) = image {
                    id
                } else {
                    let path = std::fs::canonicalize(image_config.context("image config")?)?;
                    s.images().into_iter().find(|i| s.directory(i).join("image.json") == path)
                        .context("ENOTSUP: image-config must belong to this store; external import not implemented")?.id
                };
                if let Some(existing) = s.devices().get(&dev_id).cloned() {
                    ensure!(
                        existing["image"] == image
                            && existing["read_only"].as_bool().unwrap_or(false) == read_only,
                        "device ID already belongs to another image"
                    );
                    ensure!(
                        existing["state"] == "presented",
                        "device operation incomplete; inspect status and delete before retrying"
                    );
                    drop(s);
                    let observed = observe(&existing).await?;
                    ensure!(
                        observed["fskit_mounted"] == true && observed["device_path"].is_string(),
                        "device resources changed; inspect status and delete before retrying"
                    );
                    let mut existing = existing;
                    existing["device_path"] = observed["device_path"].clone();
                    return Ok(existing);
                }
                let resource = s.resource(&image)?;
                let operation = uuid::Uuid::new_v4().to_string();
                let proxy = s.root().join("devices").join(&operation).join("proxy");
                let r = json!({"dev_id":dev_id,"image":image,"resource":resource,"proxy":proxy,
                    "operation_id":operation,"state":"creating","device_path":null,"read_only":read_only});
                s.reserve_device(dev_id, &image, r.clone())?;
                r
            };
            let result = async {
                let proxy = PathBuf::from(text(&record, "proxy")?);
                std::fs::create_dir_all(&proxy)?;
                run(
                    "/sbin/mount",
                    &[
                        "-w", // Control receipts need directory writes; data access is enforced by lease + engine.
                        // The proxy is an internal staging filesystem; keep it out of Finder and the Desktop.
                        "-o",
                        "nobrowse",
                        "-F",
                        "-t",
                        "obdraw",
                        text(&record, "resource")?,
                        proxy.to_str().context("proxy path")?,
                    ],
                )
                .await?;
                record["state"] = json!("attaching");
                save(&store, dev_id, &record).await?;
                let raw = proxy.join("disk.raw");
                plist(
                    "/usr/bin/hdiutil",
                    &[
                        "attach",
                        "-plist",
                        if read_only { "-readonly" } else { "-readwrite" },
                        "-nomount",
                        "-imagekey",
                        "diskimage-class=CRawDiskImage",
                        raw.to_str().context("raw path")?,
                    ],
                )
                .await?;
                let observed = observe(&record).await?;
                ensure!(observed["device_path"].is_string(), "device did not appear");
                record["device_path"] = observed["device_path"].clone();
                record["state"] = json!("presented");
                save(&store, dev_id, &record).await?;
                Ok::<_, anyhow::Error>(record.clone())
            }
            .await;
            if let Err(e) = &result {
                record["error"] = json!(format!("{e:#}"));
                record["state"] = json!("needs_attention");
                save(&store, dev_id, &record).await?;
            }
            result
        }
        Operation::DeviceDelete { dev_id } => {
            let mut record = match store.lock().await.devices().get(&dev_id).cloned() {
                Some(r) => r,
                None => return Ok(json!({"dev_id":dev_id,"state":"detached"})),
            };
            record["state"] = json!("detaching");
            save(&store, dev_id, &record).await?;
            let result = async {
                let observed = observe(&record).await?;
                if let Some(disk) = observed["device_path"].as_str() {
                    // Derive from current image ownership, never from stale persisted disk numbers.
                    run("/usr/bin/hdiutil", &["detach", disk]).await?;
                }
                let observed = observe(&record).await?;
                ensure!(observed["device_path"].is_null(), "device still attached");
                if observed["fskit_mounted"] == true {
                    run("/sbin/umount", &[text(&record, "proxy")?]).await?;
                }
                let image = text(&record, "image")?;
                for attempt in 0..100 {
                    let leases = store.lock().await.leases()?;
                    if !leases.iter().any(|l| l.image == image) {
                        break;
                    }
                    if attempt == 99 {
                        // Reobserve after the grace period, rather than trusting the
                        // pre-unmount observation or persisted device numbers.
                        let offline = observe(&record).await?;
                        recover_offline_device_leases(&mut *store.lock().await, image, &offline)
                            .await?;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                store.lock().await.save_device(dev_id, None)?;
                Ok::<_, anyhow::Error>(json!({"dev_id":dev_id,"state":"detached"}))
            }
            .await;
            if let Err(e) = &result {
                record["error"] = json!(format!("{e:#}"));
                save(&store, dev_id, &record).await?;
            }
            result
        }
        _ => bail!("not a device operation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "creates and detaches disposable APFS images"]
    async fn native_volume_discovery_checks_topology_and_ambiguity() -> Result<()> {
        let temp = tempfile::tempdir()?.keep();
        println!("Native discovery fixtures: {}", temp.display());
        let mut attached_disks = Vec::new();
        let result: Result<()> = async {
            for index in 0..2 {
                let path = temp.join(format!("disk-{index}.sparseimage"));
                run(
                    "/usr/bin/hdiutil",
                    &[
                        "create",
                        "-size",
                        "1g",
                        "-fs",
                        "APFS",
                        "-volname",
                        "Same name",
                        "-type",
                        "SPARSE",
                        "-o",
                        path.to_str().unwrap(),
                    ],
                )
                .await?;
                let reply = plist(
                    "/usr/bin/hdiutil",
                    &["attach", "-nomount", "-plist", path.to_str().unwrap()],
                )
                .await?;
                let disk = reply["system-entities"]
                    .as_array()
                    .context("entities")?
                    .iter()
                    .filter_map(|v| v["dev-entry"].as_str())
                    .find(|d| whole_disk(d))
                    .context("whole disk")?
                    .to_owned();
                attached_disks.push(disk);
            }
            let start = std::time::Instant::now();
            let first = volumes_for_device(&attached_disks[0]).await?;
            println!("Native discovery: {:?}", start.elapsed());
            let second = volumes_for_device(&attached_disks[1]).await?;
            ensure!(
                first.len() == 1 && second.len() == 1,
                "expected one APFS volume per image"
            );
            ensure!(first[0]["name"] == second[0]["name"], "same-label fixture");
            ensure!(
                first[0]["device_path"] != second[0]["device_path"],
                "foreign volume leaked into discovery"
            );
            let foreign = text(&second[0], "device_path")?;
            ensure!(
                select_mount_volume_for_device(&attached_disks[0], Some(foreign))
                    .await
                    .is_err(),
                "foreign storage accepted"
            );
            let info = plist(
                "/usr/sbin/diskutil",
                &["info", "-plist", text(&first[0], "device_path")?],
            )
            .await?;
            let container = text(&info, "APFSContainerReference")?;
            run(
                "/usr/sbin/diskutil",
                &[
                    "apfs",
                    "addVolume",
                    container,
                    "APFS",
                    "Second volume",
                    "-nomount",
                ],
            )
            .await?;
            let multiple = volumes_for_device(&attached_disks[0]).await?;
            ensure!(
                multiple.len() == 2,
                "newly added APFS volume missing from native discovery"
            );
            ensure!(
                select_volume(&multiple, None).is_err(),
                "ambiguous automatic selection accepted"
            );
            ensure!(
                select_mount_volume_for_device(&attached_disks[0], None)
                    .await
                    .is_err(),
                "ambiguous mount accepted"
            );
            for volume in &multiple {
                let selected =
                    select_mount_volume_for_device(&attached_disks[0], Some(text(volume, "uuid")?))
                        .await?;
                ensure!(
                    selected["device_path"] == volume["device_path"],
                    "UUID selection changed devices"
                );
            }
            Ok(())
        }
        .await;
        let mut cleanup = Ok(());
        for disk in attached_disks.iter().rev() {
            if let Err(error) = run("/usr/bin/hdiutil", &["detach", disk]).await {
                cleanup = Err(error);
            }
        }
        result?;
        cleanup?;
        std::fs::remove_dir_all(temp)?;
        Ok(())
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "creates, mounts, and detaches disposable APFS images"]
    async fn benchmark_disk_arbitration_mount() -> Result<()> {
        // Retain backing images on any failure, including a failed detach.
        let temp = tempfile::tempdir()?.keep();
        for round in 0..2 {
            for native in [true, false] {
                let label = format!("OBD-DA-Bench-{round}-{native}");
                let image = temp.join(format!("{label}.sparseimage"));
                run(
                    "/usr/bin/hdiutil",
                    &[
                        "create",
                        "-size",
                        "1g",
                        "-fs",
                        "APFS",
                        "-volname",
                        &label,
                        "-type",
                        "SPARSE",
                        "-o",
                        image.to_str().unwrap(),
                    ],
                )
                .await?;
                let attached = plist(
                    "/usr/bin/hdiutil",
                    &["attach", "-nomount", "-plist", image.to_str().unwrap()],
                )
                .await?;
                let entities = attached["system-entities"]
                    .as_array()
                    .context("attachment entities")?;
                let disk = entities
                    .iter()
                    .filter_map(|v| v["dev-entry"].as_str())
                    .find(|s| whole_disk(s))
                    .context("whole disk")?;
                let device = entities
                    .iter()
                    .find(|v| v["volume-kind"] == "apfs")
                    .and_then(|v| v["dev-entry"].as_str())
                    .context("APFS volume")?;
                let point = temp.join(&label);
                std::fs::create_dir(&point)?;
                let result: Result<()> = async {
                    for cycle in 0..3 {
                        let start = std::time::Instant::now();
                        if native {
                            crate::disk_arbitration::mount(device.to_owned(), point.clone(), false, false).await?;
                        } else {
                            run("/usr/sbin/diskutil", &["mount", "-mountPoint", point.to_str().unwrap(), device]).await?;
                        }
                        let mounted = start.elapsed();
                        // Include the existing post-mount external queries: a
                        // fast callback alone is insufficient if validation
                        // subsequently waits for StorageKit's refresh anyway.
                        let start = std::time::Instant::now();
                        let whole_args = ["info", "-plist", disk];
                        let volume_args = ["info", "-plist", device];
                        let (_, whole, volume) = tokio::try_join!(
                            plist("/usr/bin/hdiutil", &["info", "-plist"]),
                            plist("/usr/sbin/diskutil", &whole_args),
                            plist("/usr/sbin/diskutil", &volume_args),
                        )?;
                        let validated = start.elapsed();
                        ensure!(whole["WholeDisk"] == true && whole["VirtualOrPhysical"] == "Virtual", "whole disk identity");
                        ensure!(volume_belongs_to(&volume, disk.strip_prefix("/dev/").unwrap()), "volume storage");
                        ensure!(volume["MountPoint"].as_str() == point.canonicalize()?.to_str(), "mountpoint");
                        let start = std::time::Instant::now();
                        let new_label = format!("{label}-{cycle}");
                        run("/usr/sbin/diskutil", &["renameVolume", device, &new_label]).await?;
                        let renamed = start.elapsed();
                        let info = plist("/usr/sbin/diskutil", &volume_args).await?;
                        ensure!(info["VolumeName"] == new_label && info["MountPoint"] == volume["MountPoint"], "rename identity");
                        println!("round={round} native={native} cycle={cycle} mount={mounted:?} validation={validated:?} rename={renamed:?}");
                        run("/usr/sbin/diskutil", &["unmount", device]).await?;
                    }
                    if native {
                        crate::disk_arbitration::mount(device.to_owned(), point.clone(), true, true).await?;
                        use std::os::unix::ffi::OsStrExt;
                        let path = std::ffi::CString::new(point.as_os_str().as_bytes())?;
                        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
                        ensure!(unsafe { libc::statfs(path.as_ptr(), &mut stat) } == 0, "statfs");
                        ensure!(stat.f_flags & libc::MNT_RDONLY as u32 != 0, "read-only option lost");
                        ensure!(stat.f_flags & libc::MNT_DONTBROWSE as u32 != 0, "nobrowse option lost");
                        run("/usr/sbin/diskutil", &["unmount", device]).await?;
                        let invalid = crate::disk_arbitration::mount(device.to_owned(), temp.join("missing/point"), false, false).await;
                        ensure!(invalid.is_err(), "invalid mountpoint unexpectedly succeeded");
                    }
                    Ok(())
                }.await;
                let detached = run("/usr/bin/hdiutil", &["detach", disk]).await;
                result?;
                detached?;
            }
        }
        std::fs::remove_dir_all(&temp)?;
        Ok(())
    }

    // Read-only native discovery/checked-selection comparison, excluding the
    // unchanged FSKit/RAW ownership observation. Never mounts or renames a disk.
    #[test]
    #[ignore = "set OBD_BENCH_DEVICE to an attached disposable APFS whole disk"]
    fn benchmark_mount_selection() {
        let device = std::env::var("OBD_BENCH_DEVICE").unwrap();
        let selector = std::env::var("OBD_BENCH_VOLUME").ok();
        assert!(whole_disk(&device));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            for _ in 0..5 {
                let start = std::time::Instant::now();
                let old = volumes_for_device(&device).await.unwrap();
                let ambiguous = select_volume(&old, None).is_err();
                let old = select_volume(&old, selector.as_deref()).unwrap();
                let old_elapsed = start.elapsed();
                let start = std::time::Instant::now();
                let new = select_mount_volume_for_device(&device, selector.as_deref())
                    .await
                    .unwrap();
                let new_elapsed = start.elapsed();
                for key in ["device_path", "uuid", "name", "filesystem", "partition_content"] {
                    assert_eq!(old[key], new[key], "{key}");
                }
                let start = std::time::Instant::now();
                let direct = select_mount_volume_for_device(&device, new["device_path"].as_str())
                    .await
                    .unwrap();
                assert_eq!(new, direct);
                println!(
                    "native_discovery={old_elapsed:?} checked_selection={new_elapsed:?} explicit={:?}",
                    start.elapsed()
                );
                if ambiguous {
                    assert!(select_mount_volume_for_device(&device, None).await.is_err());
                }
            }
            if let Ok(foreign) = std::env::var("OBD_BENCH_FOREIGN_VOLUME") {
                assert!(select_mount_volume_for_device(&device, Some(&foreign))
                    .await
                    .is_err());
            }
        });
    }
    #[test]
    fn offline_cleanup_recovers_only_target_and_preserves_live_leases() {
        use obd_runtime::{runtime, Engine};
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(&temp.path().join("store")).unwrap();
        let geometry = obd_core_adapter::Geometry {
            virtual_size: 1024 * 1024,
            logical_block_size: 512,
        };
        runtime()
            .block_on(store.create("target", geometry))
            .unwrap();
        runtime().block_on(store.create("other", geometry)).unwrap();
        let target = store
            .claim("target", true, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        let other = store
            .claim("other", true, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        for observation in [
            json!({}),
            json!({"device_path":"/dev/disk99","fskit_mounted":false}),
            json!({"device_path":null,"fskit_mounted":true}),
        ] {
            assert!(runtime()
                .block_on(recover_offline_device_leases(
                    &mut store,
                    "target",
                    &observation
                ))
                .is_err());
            assert_eq!(store.leases().unwrap().len(), 2);
        }
        let engine = Engine::local(&store.resource("target").unwrap(), true).unwrap();
        engine.write(511, &[1, 2, 3]).unwrap();
        engine.flush().unwrap();
        let offline = json!({"device_path":null,"fskit_mounted":false});
        assert!(runtime()
            .block_on(recover_offline_device_leases(
                &mut store, "target", &offline
            ))
            .is_err());
        assert!(store
            .leases()
            .unwrap()
            .iter()
            .any(|l| l.token == target.token));
        engine.close().unwrap();
        runtime()
            .block_on(recover_offline_device_leases(
                &mut store, "target", &offline,
            ))
            .unwrap();
        let remaining = store.leases().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].token, other.token);
        let engine = Engine::local(&store.resource("target").unwrap(), false).unwrap();
        let mut bytes = [0; 3];
        engine.read(511, &mut bytes).unwrap();
        assert_eq!(bytes, [1, 2, 3]);
        engine.close().unwrap();
        runtime()
            .block_on(recover_offline_device_leases(
                &mut store, "target", &offline,
            ))
            .unwrap();
    }
    #[test]
    fn source_urls_are_local_and_decoded() {
        let path = std::env::temp_dir().join("obd-device source-url-test");
        std::fs::create_dir_all(&path).unwrap();
        let url = format!("file://{}/", path.display()).replace(' ', "%20");
        assert_eq!(source_path(&url).unwrap(), path.canonicalize().unwrap());
        assert!(source_path("file://remote/tmp/a").is_err());
        assert!(source_path("file:///tmp/%GG").is_err());
        assert!(source_path("/tmp/a").is_err());
        std::fs::remove_dir(&path).unwrap();
    }
    #[test]
    fn volume_selection_and_local_profile_are_explicit() {
        assert!(partition_of("disk4s2", "disk4"));
        assert!(!partition_of("disk40s2", "disk4"));
        assert!(!partition_of("disk4s2s1", "disk4"));
        let list = vec![
            json!({"uuid":"same","device_path":"/dev/disk4s1"}),
            json!({"uuid":"same","device_path":"/dev/disk4s2"}),
        ];
        assert!(select_volume(&list, None).is_err());
        assert!(select_volume(&list, Some("same")).is_err());
        assert!(select_volume(&list, Some("/dev/disk4s1")).is_ok());
        assert!(validate_global_profile(
            &json!({"enableAudit":false,"remoteIoWorkers":0,"cacheConfig":{"cacheSizeGB":0}})
        )
        .is_ok());
        assert!(validate_global_profile(&json!({"cacheConfig":{"cacheSizeGB":5}})).is_err());
        assert!(validate_global_profile(&json!({"unknown":false})).is_err());
    }
    #[test]
    fn automatic_mount_ignores_efi_partition_but_not_other_data_volumes() {
        let efi = json!({"uuid":"efi","device_path":"/dev/disk4s1",
            "filesystem":"msdos","partition_content":"EFI","name":"EFI"});
        let data = json!({"uuid":"data","device_path":"/dev/disk5s1",
            "filesystem":"apfs","partition_content":"41504653-0000-11AA-AA11-00306543ECAC"});
        let list = vec![efi.clone(), data.clone()];
        assert_eq!(select_volume(&list, None).unwrap(), &data);
        assert_eq!(select_volume(&list, Some("efi")).unwrap(), &efi);
        assert_eq!(select_volume(&list, Some("/dev/disk4s1")).unwrap(), &efi);
        assert!(select_volume(&[efi], None).is_err());
        let ordinary_fat = json!({"uuid":"other","device_path":"/dev/disk4s3",
            "filesystem":"msdos","partition_content":"Microsoft Basic Data","name":"EFI"});
        assert!(select_volume(&[data, ordinary_fat], None).is_err());
    }
    #[test]
    fn mounted_volume_requires_matching_physical_storage() {
        let mut apfs = json!({"DeviceIdentifier":"disk5s1", "FilesystemType":"apfs",
            "APFSPhysicalStores":[{"APFSPhysicalStore":"disk4s2"}]});
        assert!(volume_belongs_to(&apfs, "disk4"));
        assert!(!volume_belongs_to(&apfs, "disk40"));
        apfs["APFSPhysicalStores"] = json!([]);
        assert!(!volume_belongs_to(&apfs, "disk4"));
        apfs["APFSPhysicalStores"] =
            json!([{"APFSPhysicalStore":"disk4s2"},{"APFSPhysicalStore":"disk7s2"}]);
        assert!(!volume_belongs_to(&apfs, "disk4"));
        apfs["APFSPhysicalStores"] = Value::Null;
        assert!(!volume_belongs_to(&apfs, "disk4"));
        assert!(volume_belongs_to(
            &json!({"DeviceIdentifier":"disk4s1", "FilesystemType":"hfs"}),
            "disk4"
        ));
        assert!(!volume_belongs_to(
            &json!({"DeviceIdentifier":"disk40s1", "FilesystemType":"hfs"}),
            "disk4"
        ));
    }
    #[test]
    fn only_whole_disks() {
        for s in ["/dev/disk0", "/dev/disk123"] {
            assert!(whole_disk(s));
        }
        for s in ["/dev/disk", "/dev/disk1s1", "/dev/rdisk1", "/dev/disk-1"] {
            assert!(!whole_disk(s));
        }
    }
}
