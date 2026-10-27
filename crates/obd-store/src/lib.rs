//! A single daemon owns the store flock. Registry publication is one SQLite
//! transaction; data artifacts are durable before their references are published.
use anyhow::{bail, ensure, Context, Result};
use obd_core_adapter::{durable_file, sync_directory, BlockImage, Geometry};
use obd_runtime::{atomic_json, lock_resource, read_json, Attachment, Command, Receipt, Resource};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub id: String,
    pub geometry: Geometry,
    pub generation: u64,
    pub layers: Vec<String>,
    pub upper: String,
    pub sealed: bool,
    pub storage_id: String,
    pub secret: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub id: String,
    pub geometry: Geometry,
    pub layers: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub instance: String,
    pub credential: String,
    pub writable: bool,
    pub ack: Option<String>,
    pub ready: bool,
    pub token: String,
    pub image: String,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRestack {
    pub operation: String,
    pub expected_generation: u64,
    pub image: Image,
    pub snapshot: Snapshot,
    pub durable: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    images: BTreeMap<String, Image>,
    snapshots: BTreeMap<String, Snapshot>,
    leases: BTreeMap<String, Lease>,
    #[serde(default)]
    pending_restacks: BTreeMap<String, PendingRestack>,
    #[serde(default)]
    devices: BTreeMap<u32, Value>,
    #[serde(default)]
    fork_jobs: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    remote_layers: BTreeMap<String, obd_core_adapter::remote::RemoteLayer>,
}
pub struct Store {
    root: PathBuf,
    db: Connection,
    _lock: File,
    state: State,
    port: Option<u16>,
    connected: BTreeSet<String>,
}
#[derive(Debug, Serialize)]
pub struct Usage {
    pub image: Image,
    pub valid_mapped_bytes: u64,
    pub upper_file_bytes: u64,
    pub upper_allocated_bytes: u64,
    pub shared_layer_file_bytes: u64,
    pub shared_layer_allocated_bytes: u64,
    pub layer_count: usize,
}
fn object_id() -> String {
    Uuid::new_v4().to_string()
}
fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "ID must contain 1..128 ASCII letters, digits, '-' or '_'"
    );
    Ok(())
}
fn validate_object(id: &str) -> Result<()> {
    Uuid::parse_str(id).context("invalid internal object ID")?;
    Ok(())
}
fn failpoint(name: &str) {
    // Opt-in process termination, used only by isolated crash-recovery tests.
    if std::env::var("OBD_TEST_CRASH_AT").as_deref() == Ok(name) {
        std::process::exit(86);
    }
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        if !root.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root)?;
        }
        let meta = fs::symlink_metadata(root)?;
        ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "store root must be a real directory"
        );
        // SAFETY: geteuid has no preconditions.
        ensure!(
            meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "store must be owned by current user with mode 0700"
        );
        let root = root.canonicalize()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join("store.lock"))?;
        // SAFETY: valid descriptor, flock is held until File drop.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("store is already owned by another process");
        }
        for name in ["images", "layers"] {
            let path = root.join(name);
            if !path.exists() {
                fs::DirBuilder::new().mode(0o700).create(&path)?;
            }
            ensure!(
                fs::symlink_metadata(&path)?.file_type().is_dir(),
                "invalid store object directory"
            );
        }
        for name in [
            "state.db",
            "state.db-wal",
            "state.db-shm",
            "state.db-journal",
        ] {
            if let Ok(m) = fs::symlink_metadata(root.join(name)) {
                ensure!(m.file_type().is_file(), "registry must be a regular file");
            }
        }
        let db = Connection::open(root.join("state.db"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;")?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        ensure!(
            version == 0 || version == 1,
            "unsupported registry schema {version}; create a new schema 1 store (no migration)"
        );
        db.execute_batch("CREATE TABLE IF NOT EXISTS registry (id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL); CREATE TABLE IF NOT EXISTS operations (id TEXT PRIMARY KEY, kind TEXT NOT NULL); PRAGMA user_version=1;")?;
        db.execute(
            "INSERT OR IGNORE INTO registry VALUES (1, ?1)",
            [serde_json::to_string(&State::default())?],
        )?;
        let body: String =
            db.query_row("SELECT body FROM registry WHERE id=1", [], |r| r.get(0))?;
        let state: State = serde_json::from_str(&body)?;
        for (id, remote) in &state.remote_layers {
            validate_object(id)?;
            remote.validate()?;
        }
        for image in state.images.values() {
            validate_name(&image.id)?;
            validate_object(&image.upper)?;
            validate_object(&image.storage_id)?;
            image.geometry.validate()?;
            for layer in &image.layers {
                validate_object(layer)?;
            }
        }
        for snapshot in state.snapshots.values() {
            validate_name(&snapshot.id)?;
            snapshot.geometry.validate()?;
            for layer in &snapshot.layers {
                validate_object(layer)?;
            }
        }
        for (id, pending) in &state.pending_restacks {
            ensure!(id == &pending.image.id, "pending restack image mismatch");
            validate_name(id)?;
            validate_name(&pending.snapshot.id)?;
            validate_object(&pending.image.upper)?;
            pending.image.geometry.validate()?;
            for layer in &pending.image.layers {
                validate_object(layer)?;
            }
            ensure!(
                pending.snapshot.layers == pending.image.layers,
                "pending restack layers mismatch"
            );
        }
        sync_directory(&root)?;
        let mut store = Self {
            root,
            db,
            _lock: lock,
            state,
            port: None,
            connected: BTreeSet::new(),
        };
        // Only a durably recorded successful upstream restack can be completed
        // automatically. Prepared/ambiguous intents remain quarantined.
        let ready: Vec<_> = store
            .state
            .pending_restacks
            .iter()
            .filter(|(_, p)| p.durable)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            store.finish_restack(&id)?;
        }
        let pending = store.pending_restacks();
        for p in pending {
            let path = store
                .directory(&p.image)
                .join("operations")
                .join(format!("{}.json", p.operation));
            if path.exists() {
                let r: Receipt = read_json(&path)?;
                if r.status == "success"
                    && r.operation == p.operation
                    && r.generation == p.expected_generation
                {
                    let mut next = store.state.clone();
                    next.pending_restacks.get_mut(&p.image.id).unwrap().durable = true;
                    store.publish(next, "recover-durable-restack")?;
                    store.finish_restack(&p.image.id)?;
                }
            }
        }
        for i in store.state.images.values() {
            store.manifest(i)?;
        }
        store.reconcile_closed()?;
        Ok(store)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn publish(&mut self, next: State, kind: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE registry SET body=?1 WHERE id=1",
            [serde_json::to_string(&next)?],
        )?;
        tx.execute(
            "INSERT INTO operations VALUES (?1, ?2)",
            params![object_id(), kind],
        )?;
        tx.commit()?;
        self.state = next;
        Ok(())
    }
    pub fn images(&self) -> Vec<Image> {
        self.state.images.values().cloned().collect()
    }
    pub fn snapshots(&self) -> Vec<Snapshot> {
        self.state.snapshots.values().cloned().collect()
    }
    pub fn leases(&mut self) -> Result<Vec<Lease>> {
        self.reconcile_closed()?;
        Ok(self.state.leases.values().cloned().collect())
    }
    pub fn image(&self, id: &str) -> Result<Image> {
        self.state
            .images
            .get(id)
            .cloned()
            .context("image not found")
    }
    fn healthy(&self, id: &str) -> Result<()> {
        ensure!(!self.state.pending_restacks.contains_key(id), "image has an incomplete live restack; inspect pending-restacks; preserve store for recovery");
        Ok(())
    }
    fn snapshot_name_available(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        ensure!(
            !self.state.snapshots.contains_key(name)
                && !self
                    .state
                    .pending_restacks
                    .values()
                    .any(|p| p.snapshot.id == name),
            "snapshot already exists or is pending"
        );
        Ok(())
    }
    pub fn pending_restacks(&self) -> Vec<PendingRestack> {
        self.state.pending_restacks.values().cloned().collect()
    }
    fn finish_restack(&mut self, id: &str) -> Result<Snapshot> {
        let mut next = self.state.clone();
        let pending = next
            .pending_restacks
            .remove(id)
            .context("restack intent not found")?;
        ensure!(pending.durable, "restack outcome is not durable");
        let layer = pending.image.layers.last().context("missing layer")?;
        let local = self.directory(&pending.image).join("layers").join(layer);
        let global = self.root.join("layers").join(layer);
        if !global.exists() {
            fs::hard_link(&local, &global)?;
            sync_directory(&self.root.join("layers"))?;
        }
        for lease in next.leases.values_mut().filter(|l| l.image == id) {
            lease.ack = Some(pending.operation.clone());
            lease.ready = false;
            lease.generation = pending.image.generation;
        }
        next.images.insert(id.into(), pending.image);
        next.snapshots
            .insert(pending.snapshot.id.clone(), pending.snapshot.clone());
        self.publish(next, "live-snapshot-published")?;
        self.manifest(&self.image(id)?)?;
        Ok(pending.snapshot)
    }
    fn detached(&mut self, id: &str) -> Result<()> {
        ensure!(
            !self.state.devices.values().any(|d| d["image"] == id),
            "image has a managed device; delete the device first"
        );
        self.unleased(id)
    }
    fn unleased(&mut self, id: &str) -> Result<()> {
        self.reconcile_closed()?;
        self.healthy(id)?;
        ensure!(
            !self.state.leases.values().any(|l| l.image == id),
            "image has an active or unrecovered RAW lease"
        );
        Ok(())
    }
    pub fn directory(&self, image: &Image) -> PathBuf {
        self.root.join("images").join(&image.storage_id)
    }
    fn manifest(&self, image: &Image) -> Result<()> {
        self.manifest_with_remotes(image, &self.state.remote_layers)
    }
    fn manifest_with_remotes(
        &self,
        image: &Image,
        remotes: &BTreeMap<String, obd_core_adapter::remote::RemoteLayer>,
    ) -> Result<()> {
        let dir = self.directory(image);
        for layer in &image.layers {
            let local = dir.join("layers").join(layer);
            if !local.exists() && self.root.join("layers").join(layer).exists() {
                fs::hard_link(self.root.join("layers").join(layer), &local)?;
            }
        }
        sync_directory(&dir.join("layers"))?;
        let layers = image
            .layers
            .iter()
            .map(|l| dir.join("layers").join(l))
            .collect::<Vec<_>>();
        let mut config = BlockImage::local_config(&dir.join("uppers").join(&image.upper), &layers)?;
        let mut remote_layers = BTreeMap::new();
        for (id, lower) in image.layers.iter().zip(config.lowers.iter_mut()) {
            if !dir.join("layers").join(id).exists() {
                let remote = remotes.get(id).context("missing layer")?;
                *lower = remote.config();
                remote_layers.insert(id.clone(), remote.clone());
            }
        }
        atomic_json(&dir.join("image.json"), &config)?;
        atomic_json(
            &dir.join("resource.json"),
            &Resource {
                version: obd_runtime::RESOURCE_VERSION,
                image: image.id.clone(),
                geometry: image.geometry,
                generation: image.generation,
                upper: image.upper.clone(),
                layers: image.layers.clone(),
                sealed: image.sealed,
                remote_layers,
            },
        )?;
        atomic_json(
            &dir.join("device-access.json"),
            &json!({"read_only": self.state.devices.values().any(|d|d["image"]==image.id && d["read_only"]==true)}),
        )?;
        if let Some(port) = self.port {
            atomic_json(
                &dir.join("control.json"),
                &json!({"version":1,"port":port,"secret":image.secret}),
            )?;
        }
        Ok(())
    }
    pub fn set_endpoint(&mut self, port: u16) -> Result<()> {
        self.port = Some(port);
        for image in self.state.images.values() {
            self.manifest(image)?;
        }
        Ok(())
    }
    pub fn devices(&self) -> &BTreeMap<u32, Value> {
        &self.state.devices
    }
    pub fn reserve_device(&mut self, id: u32, image: &str, record: Value) -> Result<()> {
        ensure!(
            !self.state.devices.contains_key(&id),
            "device ID already exists"
        );
        self.detached(image)?;
        let mut next = self.state.clone();
        next.devices.insert(id, record);
        self.publish(next, "device-reserved")?;
        self.manifest(&self.image(image)?)
    }
    pub fn save_device(&mut self, id: u32, record: Option<Value>) -> Result<()> {
        ensure!(self.state.devices.contains_key(&id), "device not found");
        let image = self.state.devices[&id]["image"]
            .as_str()
            .context("device image")?
            .to_owned();
        let removing = record.is_none();
        let mut next = self.state.clone();
        if let Some(record) = record {
            next.devices.insert(id, record);
        } else {
            next.devices.remove(&id);
        }
        self.publish(next, "device-updated")?;
        if removing {
            self.manifest(&self.image(&image)?)?;
        }
        Ok(())
    }
    pub fn authenticate(&self, secret: &str) -> Option<String> {
        self.state
            .images
            .values()
            .find(|i| i.secret == secret)
            .map(|i| i.id.clone())
    }
    pub fn resource(&self, id: &str) -> Result<PathBuf> {
        self.healthy(id)?;
        Ok(self.directory(&self.image(id)?))
    }
    async fn worker(job: Value) -> Result<Value> {
        let path = std::env::var_os("OBD_ENGINE_WORKER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let exe = std::env::current_exe().expect("executable path");
                let parent = exe.parent().expect("parent");
                if parent.ends_with("deps") {
                    parent.parent().unwrap().join("obd-engine-worker")
                } else {
                    parent.join("obd-engine-worker")
                }
            });
        let out = tokio::process::Command::new(path)
            .arg(serde_json::to_string(&job)?)
            .output()
            .await?;
        ensure!(
            out.status.success(),
            "offline worker failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(serde_json::from_slice(&out.stdout)?)
    }
    async fn new_upper(&mut self, storage: &str, geometry: Geometry) -> Result<String> {
        let dir = self.root.join("images").join(storage);
        for path in [
            dir.clone(),
            dir.join("uppers"),
            dir.join("layers"),
            dir.join("operations"),
        ] {
            if !path.exists() {
                fs::DirBuilder::new().mode(0o700).create(path)?;
            }
        }
        let id = object_id();
        Self::worker(json!({"op":"create","directory":dir,"upper":dir.join("uppers").join(&id),"geometry":geometry})).await?;
        sync_directory(&dir.join("uppers"))?;
        sync_directory(&self.root.join("images"))?;
        Ok(id)
    }
    pub async fn create(&mut self, id: &str, geometry: Geometry) -> Result<Image> {
        validate_name(id)?;
        geometry.validate()?;
        ensure!(
            !self.state.fork_jobs.contains_key(id),
            "image name reserved by fork operation"
        );
        ensure!(!self.state.images.contains_key(id), "image already exists");
        let storage_id = object_id();
        let upper = self.new_upper(&storage_id, geometry).await?;
        let image = Image {
            id: id.into(),
            geometry,
            generation: 0,
            layers: vec![],
            upper,
            sealed: false,
            storage_id,
            secret: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        self.manifest(&image)?;
        let mut next = self.state.clone();
        next.images.insert(id.into(), image.clone());
        self.publish(next, "create")?;
        Ok(image)
    }
    pub async fn fork(&mut self, snapshot: &str, id: &str) -> Result<Image> {
        ensure!(
            !self.state.fork_jobs.contains_key(id),
            "image name reserved by fork operation"
        );
        self.fork_inner(snapshot, id).await
    }
    async fn fork_inner(&mut self, snapshot: &str, id: &str) -> Result<Image> {
        validate_name(id)?;
        ensure!(!self.state.images.contains_key(id), "image already exists");
        let s = self
            .state
            .snapshots
            .get(snapshot)
            .context("snapshot not found")?
            .clone();
        let storage_id = object_id();
        let upper = self.new_upper(&storage_id, s.geometry).await?;
        let image = Image {
            id: id.into(),
            geometry: s.geometry,
            generation: 0,
            layers: s.layers,
            upper,
            sealed: false,
            storage_id,
            secret: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        self.manifest(&image)?;
        let mut next = self.state.clone();
        next.images.insert(id.into(), image.clone());
        if let Some(job) = next.fork_jobs.get_mut(id) {
            job["storage_id"] = json!(image.storage_id);
        }
        self.publish(next, "fork")?;
        Ok(image)
    }
    async fn materialize(&mut self, layers: &[String]) -> Result<()> {
        for id in layers {
            let dest = self.root.join("layers").join(id);
            if !dest.exists() {
                let remote = self.state.remote_layers.get(id).context("missing layer")?;
                Self::worker(json!({"op":"remote-fetch", "remote":remote, "output":dest})).await?;
            }
        }
        Ok(())
    }
    pub async fn fetch_image(&mut self, id: &str) -> Result<Value> {
        self.detached(id)?;
        let image = self.image(id)?;
        let _lock = lock_resource(&self.directory(&image))?;
        self.materialize(&image.layers).await?;
        self.manifest(&image)?;
        Ok(json!({"id":id,"materialized_layers":image.layers.len(),"offline_ready":true}))
    }
    pub async fn publish_snapshot(
        &mut self,
        id: &str,
        repository: &str,
        compression: &str,
    ) -> Result<Value> {
        obd_core_adapter::remote::url(repository)?;
        let scratch = self.root.join(format!("transfer-{}", object_id()));
        let result = async {
            self.export_snapshot(id, &scratch, compression).await?;
            Self::worker(json!({"op":"remote-publish", "config":scratch.join("image.json"),"repository":repository})).await
        }.await;
        if scratch.exists() {
            fs::remove_dir_all(&scratch)?;
        }
        result
    }
    pub async fn pull_image(&mut self, id: &str, manifest_url: &str) -> Result<Image> {
        validate_name(id)?;
        ensure!(
            !self.state.images.contains_key(id) && !self.state.fork_jobs.contains_key(id),
            "image name already exists or reserved"
        );
        let result = Self::worker(json!({"op":"remote-manifest","url":manifest_url})).await?;
        let remotes: Vec<obd_core_adapter::remote::RemoteLayer> = serde_json::from_value(result)?;
        let storage_id = object_id();
        let probe = self.root.join(format!("probe-{}", object_id()));
        let mut size = None;
        let probe_result: Result<()> = async {
            for remote in &remotes {
                let n = Self::worker(json!({"op":"remote-info","remote":remote,"cache":probe}))
                    .await?["virtual_size"]
                    .as_u64()
                    .context("remote geometry")?;
                ensure!(
                    size.is_none_or(|old| old == n),
                    "remote layer geometry mismatch"
                );
                size = Some(n);
            }
            Ok(())
        }
        .await;
        if probe.exists() {
            fs::remove_dir_all(&probe)?;
        }
        probe_result?;
        let geometry = Geometry {
            virtual_size: size.context("no layers")?,
            logical_block_size: 512,
        }
        .validate()?;
        let upper = self.new_upper(&storage_id, geometry).await?;
        let mut next = self.state.clone();
        let mut layers = vec![];
        for remote in remotes {
            let layer = next
                .remote_layers
                .iter()
                .find(|(_, r)| {
                    r.digest == remote.digest
                        && r.repo_blob_url == remote.repo_blob_url
                        && r.size == remote.size
                })
                .map(|(id, _)| id.clone())
                .unwrap_or_else(object_id);
            next.remote_layers.insert(layer.clone(), remote);
            layers.push(layer);
        }
        let image = Image {
            id: id.into(),
            geometry,
            generation: 0,
            layers,
            upper,
            sealed: false,
            storage_id,
            secret: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        self.manifest_with_remotes(&image, &next.remote_layers)?;
        Self::worker(json!({"op":"stat","directory":self.directory(&image)})).await?;
        // Publish only after the whole remote stack can be opened successfully.
        next.images.insert(id.into(), image.clone());
        self.publish(next, "pull-image")?;
        self.manifest(&image)?;
        Ok(image)
    }
    pub async fn import_image(&mut self, id: &str, config_path: &Path) -> Result<Image> {
        validate_name(id)?;
        ensure!(
            !self.state.images.contains_key(id) && !self.state.fork_jobs.contains_key(id),
            "image name already exists or reserved"
        );
        let config_path = config_path.canonicalize()?;
        ensure!(
            fs::metadata(&config_path)?.len() <= 1024 * 1024,
            "image config too large"
        );
        let raw: Value = serde_json::from_slice(&fs::read(&config_path)?)?;
        ensure!(
            raw.as_object()
                .is_some_and(|o| o.keys().all(|k| k == "lowers" || k == "upper")),
            "ENOTSUP: import supports only local lowers and empty upper"
        );
        let config: obd_core_adapter::UpstreamImageConfig = serde_json::from_value(raw.clone())?;
        ensure!(
            config.upper == Default::default(),
            "ENOTSUP: writable upper import requires a sealed snapshot export"
        );
        if let Some(upper) = raw.get("upper") {
            ensure!(
                upper.as_object().is_some_and(|o| o.is_empty()),
                "ENOTSUP: upper must be empty"
            );
        }
        ensure!(
            !config.lowers.is_empty() && config.lowers.len() < 253,
            "expected 1..252 sealed layers"
        );
        for lower in raw["lowers"].as_array().context("lowers")? {
            ensure!(
                lower
                    .as_object()
                    .is_some_and(|o| o.len() == 1 && o.contains_key("file")),
                "ENOTSUP: only local lower.file is supported"
            );
        }
        let mut layers = Vec::new();
        let mut size = None;
        for lower in config.lowers {
            let path = config_path
                .parent()
                .context("config parent")?
                .join(lower.file)
                .canonicalize()?;
            let before = fs::metadata(&path)?;
            let geometry = Self::worker(json!({"op":"layer-info","source":path})).await?
                ["virtual_size"]
                .as_u64()
                .context("layer geometry")?;
            ensure!(
                size.is_none_or(|s| s == geometry),
                "layer geometry mismatch"
            );
            size = Some(geometry);
            let layer = object_id();
            let dest = self.root.join("layers").join(&layer);
            let mut from = File::open(&path)?;
            let mut to = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&dest)?;
            std::io::copy(&mut from, &mut to)?;
            to.sync_all()?;
            durable_file(&dest)?;
            let after = fs::metadata(&path)?;
            ensure!(
                before.len() == after.len() && before.modified()? == after.modified()?,
                "source layer changed during import; use an immutable export"
            );
            ensure!(
                Self::worker(json!({"op":"layer-info","source":dest})).await?["virtual_size"]
                    .as_u64()
                    == Some(geometry),
                "copied layer invalid"
            );
            layers.push(layer);
        }
        sync_directory(&self.root.join("layers"))?;
        let geometry = Geometry {
            virtual_size: size.context("layer size")?,
            logical_block_size: 512,
        }
        .validate()?;
        let storage_id = object_id();
        let upper = self.new_upper(&storage_id, geometry).await?;
        let image = Image {
            id: id.into(),
            geometry,
            generation: 0,
            layers,
            upper,
            sealed: false,
            storage_id,
            secret: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        self.manifest(&image)?;
        Self::worker(json!({"op":"stat","directory":self.directory(&image)})).await?;
        let mut next = self.state.clone();
        next.images.insert(id.into(), image.clone());
        self.publish(next, "import-image")?;
        Ok(image)
    }
    /// Publish a new single-layer snapshot; source images and snapshots stay intact.
    pub async fn compact_snapshot(&mut self, source: &str, name: &str) -> Result<Snapshot> {
        self.snapshot_name_available(name)?;
        let old = self
            .state
            .snapshots
            .get(source)
            .context("snapshot not found")?
            .clone();
        self.materialize(&old.layers).await?;
        let layer = object_id();
        let paths: Vec<_> = old
            .layers
            .iter()
            .map(|l| self.root.join("layers").join(l))
            .collect();
        Self::worker(json!({"op":"compact-layers", "layers":paths,
            "geometry":old.geometry, "output":self.root.join("layers").join(&layer)}))
        .await?;
        sync_directory(&self.root.join("layers"))?;
        failpoint("compact-layer-durable");
        let snapshot = Snapshot {
            id: name.into(),
            geometry: old.geometry,
            layers: vec![layer],
        };
        let mut next = self.state.clone();
        next.snapshots.insert(name.into(), snapshot.clone());
        self.publish(next, "compact-snapshot")?;
        failpoint("compact-snapshot-published");
        Ok(snapshot)
    }

    /// Metadata-only sampling: safe for mounted images, never opens a second engine.
    pub fn space_usage(&self, kind: &str, id: &str) -> Result<Value> {
        let (geometry, layers, upper) = match kind {
            "image" => {
                let image = self.image(id)?;
                (
                    image.geometry,
                    image.layers.clone(),
                    Some(self.directory(&image).join("uppers").join(&image.upper)),
                )
            }
            "snapshot" => {
                let snap = self.state.snapshots.get(id).context("snapshot not found")?;
                (snap.geometry, snap.layers.clone(), None)
            }
            _ => anyhow::bail!("unknown usage kind"),
        };
        let mut details = Vec::new();
        let (mut layer_bytes, mut allocated, mut shared_bytes, mut shared_allocated) =
            (0u64, 0u64, 0u64, 0u64);
        for layer in &layers {
            let m = match fs::metadata(self.root.join("layers").join(layer)) {
                Ok(m) => Some(m),
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && self.state.remote_layers.contains_key(layer) =>
                {
                    None
                }
                Err(e) => return Err(e.into()),
            };
            let file_bytes = m.as_ref().map_or(0, |m| m.len());
            let allocated_bytes = m.as_ref().map_or(0, |m| m.blocks() * 512);
            let mut owners = BTreeSet::new();
            for image in self
                .state
                .images
                .values()
                .filter(|i| i.layers.contains(layer))
            {
                owners.insert(format!("image:{}", image.id));
            }
            for snap in self
                .state
                .snapshots
                .values()
                .filter(|s| s.layers.contains(layer))
            {
                owners.insert(format!("snapshot:{}", snap.id));
            }
            for pending in self.state.pending_restacks.values() {
                if pending.snapshot.layers.contains(layer) || pending.image.layers.contains(layer) {
                    owners.insert(format!("pending:{}", pending.operation));
                }
            }
            let shared = owners.len() > 1;
            layer_bytes += file_bytes;
            allocated += allocated_bytes;
            if shared {
                shared_bytes += file_bytes;
                shared_allocated += allocated_bytes;
            }
            details.push(
                json!({"id":layer,"file_bytes":file_bytes,"allocated_bytes":allocated_bytes,
                "remote":self.state.remote_layers.get(layer),"materialized":m.is_some(),
                "owners":owners,"shared":shared}),
            );
        }
        let remote_object_bytes: u64 = layers
            .iter()
            .filter_map(|id| self.state.remote_layers.get(id))
            .map(|r| r.size)
            .sum();
        fn cache_usage(path: &Path) -> Result<(u64, u64)> {
            if !path.exists() {
                return Ok((0, 0));
            }
            let m = fs::symlink_metadata(path)?;
            ensure!(!m.file_type().is_symlink(), "unexpected cache symlink");
            if m.is_file() {
                return Ok((m.len(), m.blocks() * 512));
            }
            let (mut size, mut allocated) = (0, 0);
            for e in fs::read_dir(path)? {
                let (s, a) = cache_usage(&e?.path())?;
                size += s;
                allocated += a;
            }
            Ok((size, allocated))
        }
        let (cache_file_bytes, cache_allocated_bytes) = match &upper {
            Some(dir) => cache_usage(&dir.join("index-cache"))?,
            None => (0, 0),
        };
        let (mut upper_bytes, mut upper_allocated) = (0, 0);
        if let Some(dir) = upper {
            for name in ["upper.data", "upper.index"] {
                let m = fs::metadata(dir.join(name))?;
                upper_bytes += m.len();
                upper_allocated += m.blocks() * 512;
            }
        }
        Ok(
            json!({"kind":kind,"id":id,"virtual_bytes":geometry.virtual_size,
            "logical_block_size":geometry.logical_block_size,"layer_count":layers.len(),"layers":details,
            "lower_file_bytes":layer_bytes,"lower_allocated_bytes":allocated,
            "remote_object_bytes":remote_object_bytes,"current_upper_cache_file_bytes":cache_file_bytes,"current_upper_cache_allocated_bytes":cache_allocated_bytes,
            "shared_lower_file_bytes":shared_bytes,"shared_lower_allocated_bytes":shared_allocated,
            "exclusive_lower_file_bytes":layer_bytes-shared_bytes,"exclusive_lower_allocated_bytes":allocated-shared_allocated,
            "upper_file_bytes":upper_bytes,"upper_allocated_bytes":upper_allocated,
            "referenced_file_bytes":layer_bytes+upper_bytes,"referenced_allocated_bytes":allocated+upper_allocated,
            "sampling":"non_atomic_metadata", "scope":"current_upper_and_referenced_layers",
            "allocation_note":"st_blocks; excludes metadata/retired files; APFS clone sharing and reclaimable bytes are not inferred"}),
        )
    }

    pub async fn export_snapshot(
        &mut self,
        id: &str,
        output: &Path,
        compression: &str,
    ) -> Result<Value> {
        ensure!(
            matches!(compression, "none" | "lz4" | "zstd"),
            "unsupported compression"
        );
        let snapshot = self
            .state
            .snapshots
            .get(id)
            .context("snapshot not found")?
            .clone();
        self.materialize(&snapshot.layers).await?;
        ensure!(!output.exists(), "export output must not exist");
        fs::DirBuilder::new().mode(0o700).create(output)?;
        let mut lowers = Vec::new();
        for layer in &snapshot.layers {
            let name = format!(
                "{layer}.{}",
                if compression == "none" {
                    "lsmt"
                } else {
                    "zfile"
                }
            );
            let dest = output.join(&name);
            if compression == "none" {
                let mut from = File::open(self.root.join("layers").join(layer))?;
                let mut to = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&dest)?;
                std::io::copy(&mut from, &mut to)?;
                to.sync_all()?;
                durable_file(&dest)?;
            } else {
                Self::worker(json!({"op":"compress-layer", "source":self.root.join("layers").join(layer), "output":dest, "compression":compression})).await?;
            }
            lowers.push(json!({"file":name}));
        }
        atomic_json(&output.join("image.json"), &json!({"lowers":lowers}))?;
        sync_directory(output)?;
        sync_directory(output.parent().context("export parent")?)?;
        Ok(
            json!({"image_config":output.join("image.json"),"virtual_size":snapshot.geometry.virtual_size}),
        )
    }
    pub fn image_fork_status(&self, id: Option<&str>) -> Result<Value> {
        if let Some(id) = id {
            ensure!(
                self.state.fork_jobs.contains_key(id),
                "fork operation not found"
            );
        }
        Ok(json!(self
            .state
            .fork_jobs
            .iter()
            .filter(|(name, _)| id.is_none_or(|id| id == name.as_str()))
            .map(|(target, j)| {
                let mut result = j.clone();
                result["target"] = json!(target);
                result["state"] = json!(if j["storage_id"].is_string() {
                    "completed"
                } else {
                    "pending"
                });
                result
            })
            .collect::<Vec<_>>()))
    }
    pub fn image_fork_mode(&self, id: &str) -> Option<bool> {
        self.state
            .fork_jobs
            .get(id)
            .and_then(|j| j["live"].as_bool())
    }
    pub fn cancel_image_fork(&mut self, id: &str) -> Result<()> {
        let job = self
            .state
            .fork_jobs
            .get(id)
            .context("fork intent not found")?;
        ensure!(
            job["storage_id"].is_null(),
            "completed fork: delete the image instead"
        );
        ensure!(
            !self
                .state
                .pending_restacks
                .values()
                .any(|p| job["snapshot"] == p.snapshot.id),
            "cannot cancel an unresolved live restack"
        );
        let mut next = self.state.clone();
        next.fork_jobs.remove(id);
        self.publish(next, "cancel-fork")
    }
    /// Reserve the target and checkpoint identity before starting a snapshot.
    /// A retry resumes the same operation, including after publication uncertainty.
    pub async fn prepare_image_fork(
        &mut self,
        source: &str,
        id: &str,
        live: bool,
    ) -> Result<String> {
        validate_name(id)?;
        ensure!(source != id, "source and target must differ");
        if let Some(job) = self.state.fork_jobs.get(id) {
            ensure!(
                job["source"] == source && job["live"] == live,
                "fork target belongs to a different operation"
            );
        } else {
            self.image(source)?;
            ensure!(!self.state.images.contains_key(id), "image already exists");
            if live {
                ensure!(
                    self.leases()?
                        .iter()
                        .any(|l| l.image == source && l.writable && l.ready),
                    "live fork requires a ready writable lease"
                );
            } else {
                self.detached(source)?;
            }
            let mut next = self.state.clone();
            next.fork_jobs.insert(id.into(), json!({"source":source,"live":live,"snapshot":format!("fork-{}",object_id()),"storage_id":null}));
            self.publish(next, "fork-intent")?;
        }
        let job = self.state.fork_jobs[id].clone();
        let name = job["snapshot"]
            .as_str()
            .context("fork checkpoint")?
            .to_owned();
        if job["storage_id"].is_string() || self.state.snapshots.contains_key(&name) {
            return Ok(name);
        }
        if live {
            if !self
                .state
                .pending_restacks
                .values()
                .any(|p| p.snapshot.id == name)
            {
                self.live_snapshot(source, &name)?;
            }
        } else {
            self.snapshot(source, &name, false).await?;
        }
        Ok(name)
    }
    pub async fn finish_image_fork(&mut self, id: &str) -> Result<Option<Image>> {
        let job = self
            .state
            .fork_jobs
            .get(id)
            .context("fork operation not found")?
            .clone();
        if let Some(storage) = job["storage_id"].as_str() {
            let image = self.image(id)?;
            ensure!(image.storage_id == storage, "fork target identity changed");
            return Ok(Some(image));
        }
        let name = job["snapshot"].as_str().context("fork checkpoint")?;
        if !self.state.snapshots.contains_key(name) {
            return Ok(None);
        }
        if job["live"] == true
            && self
                .state
                .leases
                .values()
                .any(|l| job["source"] == l.image && !l.ready)
        {
            return Ok(None);
        }
        Ok(Some(self.fork_inner(name, id).await?))
    }
    pub async fn snapshot(&mut self, id: &str, name: &str, seal: bool) -> Result<Snapshot> {
        self.snapshot_name_available(name)?;
        self.detached(id)?;
        let old = self.image(id)?;
        ensure!(
            old.layers.len() < 253,
            "layer limit reached; compaction required"
        );
        let layer = object_id();
        Self::worker(json!({"op":"export","directory":self.directory(&old),"output":self.root.join("layers").join(&layer)})).await?;
        failpoint("snapshot-layer-durable");
        let upper = self.new_upper(&old.storage_id, old.geometry).await?;
        failpoint("snapshot-upper-durable");
        let mut image = old;
        image.layers.push(layer);
        image.upper = upper;
        image.generation += 1;
        image.sealed = seal;
        let snapshot = Snapshot {
            id: name.into(),
            geometry: image.geometry,
            layers: image.layers.clone(),
        };
        let mut next = self.state.clone();
        next.images.insert(id.into(), image.clone());
        next.snapshots.insert(name.into(), snapshot.clone());
        self.publish(next, if seal { "seal" } else { "snapshot" })?;
        self.manifest(&image)?;
        failpoint("snapshot-published");
        Ok(snapshot)
    }
    /// Durable command publication only. The extension executes upstream restack.
    pub fn live_snapshot(&mut self, id: &str, name: &str) -> Result<PendingRestack> {
        self.healthy(id)?;
        self.snapshot_name_available(name)?;
        let mut image = self.image(id)?;
        ensure!(
            !image.sealed && image.layers.len() < 253,
            "EROFS or layer limit"
        );
        ensure!(
            self.state.leases.values().any(|l| l.image == id
                && l.writable
                && l.ready
                && self.connected.contains(&l.token)),
            "live snapshot requires writable attachment"
        );
        let expected_generation = image.generation;
        image.generation += 1;
        image.layers.push(object_id());
        let pending = PendingRestack {
            operation: object_id(),
            expected_generation,
            snapshot: Snapshot {
                id: name.into(),
                geometry: image.geometry,
                layers: image.layers.clone(),
            },
            image,
            durable: false,
        };
        let mut next = self.state.clone();
        next.pending_restacks.insert(id.into(), pending.clone());
        self.publish(next, "live-snapshot-prepared")?;
        failpoint("live-snapshot-prepared");
        Ok(pending)
    }
    pub fn claim(&mut self, id: &str, writable: bool, instance: &str) -> Result<Attachment> {
        self.reconcile_closed()?;
        self.unleased(id)?;
        ensure!(
            !writable
                || !self
                    .state
                    .devices
                    .values()
                    .any(|d| d["image"] == id && d["read_only"] == true),
            "EROFS: device is read-only"
        );
        Uuid::parse_str(instance)?;
        let image = self.image(id)?;
        ensure!(!writable || !image.sealed, "EROFS: sealed image");
        let lease = Lease {
            token: object_id(),
            image: id.into(),
            generation: image.generation,
            instance: instance.into(),
            credential: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            writable,
            ack: None,
            ready: true,
        };
        let a = Attachment {
            token: lease.token.clone(),
            instance: lease.instance.clone(),
            credential: lease.credential.clone(),
            generation: lease.generation,
        };
        let mut next = self.state.clone();
        next.leases.insert(lease.token.clone(), lease);
        self.publish(next, "claim")?;
        self.connected.insert(a.token.clone());
        Ok(a)
    }
    pub fn poll(
        &mut self,
        a: &Attachment,
        receipt: Option<Receipt>,
        closed: bool,
    ) -> Result<Value> {
        let lease = self
            .state
            .leases
            .get(&a.token)
            .context("attachment not found")?
            .clone();
        ensure!(
            lease.instance == a.instance && lease.credential == a.credential,
            "invalid attachment proof"
        );
        ensure!(
            a.generation == lease.generation
                || (a.generation.checked_add(1) == Some(lease.generation)
                    && receipt
                        .as_ref()
                        .is_some_and(|r| Some(&r.operation) == lease.ack.as_ref()
                            && r.generation == a.generation
                            && r.status == "success")),
            "stale attachment generation"
        );
        self.connected.insert(a.token.clone());
        if !lease.ready && a.generation == lease.generation && receipt.is_none() {
            let mut next = self.state.clone();
            next.leases.get_mut(&a.token).unwrap().ready = true;
            self.publish(next, "attachment-generation-acknowledged")?;
        }
        if closed {
            self.reconcile_closed()?;
            ensure!(
                !self.state.leases.contains_key(&a.token),
                "close receipt not durable or lock still held"
            );
            return Ok(json!({}));
        }
        if let Some(p) = self.state.pending_restacks.get(&lease.image).cloned() {
            if let Some(r) = receipt {
                ensure!(
                    r.operation == p.operation
                        && r.generation == p.expected_generation
                        && r.status == "success",
                    "invalid restack receipt"
                );
                let local: Receipt = read_json(
                    &self
                        .directory(&p.image)
                        .join("operations")
                        .join(format!("{}.json", p.operation)),
                )?;
                ensure!(
                    local.operation == r.operation
                        && local.status == "success"
                        && local.generation == r.generation,
                    "receipt not durable"
                );
                let mut next = self.state.clone();
                next.pending_restacks.get_mut(&lease.image).unwrap().durable = true;
                self.publish(next, "live-snapshot-durable")?;
                failpoint("live-snapshot-durable");
                self.finish_restack(&lease.image)?;
                failpoint("live-snapshot-published");
                return Ok(json!({"ack":p.operation,"generation":p.image.generation}));
            }
            return Ok(
                json!({"command":Command{operation:p.operation,generation:p.expected_generation,layer:p.image.layers.last().unwrap().clone()}}),
            );
        }
        if receipt.is_some() {
            self.manifest(&self.image(&lease.image)?)?;
        }
        Ok(json!({"ack":lease.ack,"generation":lease.generation}))
    }
    fn reconcile_closed(&mut self) -> Result<()> {
        let mut next = self.state.clone();
        for l in self.state.leases.values() {
            if self.state.pending_restacks.contains_key(&l.image) {
                continue;
            }
            let dir = self.directory(&self.image(&l.image)?);
            let path = dir
                .join("operations")
                .join(format!("closed-{}.json", l.token));
            if path.exists() {
                let a: Attachment = read_json(&path)?;
                ensure!(
                    a.token == l.token
                        && a.credential == l.credential
                        && a.instance == l.instance
                        && a.generation == l.generation,
                    "invalid close receipt"
                );
                if let Ok(_lock) = lock_resource(&dir) {
                    next.leases.remove(&l.token);
                }
            }
        }
        if next.leases.len() != self.state.leases.len() {
            self.publish(next, "reconcile-closed")?;
            self.connected
                .retain(|token| self.state.leases.contains_key(token));
        }
        Ok(())
    }
    pub fn delete_image(&mut self, id: &str) -> Result<()> {
        ensure!(
            !self
                .state
                .fork_jobs
                .values()
                .any(|j| j["source"] == id && j["storage_id"].is_null()),
            "image is source of an incomplete fork"
        );
        self.detached(id)?;
        let mut next = self.state.clone();
        ensure!(next.images.remove(id).is_some(), "image not found");
        next.fork_jobs.remove(id);
        self.publish(next, "delete-image")
    }
    pub fn delete_snapshot(&mut self, id: &str) -> Result<()> {
        ensure!(
            !self
                .state
                .fork_jobs
                .values()
                .any(|j| j["snapshot"] == id && j["storage_id"].is_null()),
            "snapshot is reserved by an incomplete fork"
        );
        let mut next = self.state.clone();
        ensure!(next.snapshots.remove(id).is_some(), "snapshot not found");
        self.publish(next, "delete-snapshot")
    }
    pub async fn recover_lease(&mut self, token: &str) -> Result<()> {
        let lease = self
            .state
            .leases
            .get(token)
            .context("lease not found")?
            .clone();
        self.healthy(&lease.image)?;
        Self::worker(
            json!({"op":"recover","directory":self.directory(&self.image(&lease.image)?)}),
        )
        .await?;
        let mut next = self.state.clone();
        next.leases.remove(token);
        self.publish(next, "offline-recovery")
    }
    pub async fn stat(&mut self, id: &str) -> Result<Usage> {
        let image = self.image(id)?;
        // Avoid opening a second LSMT writer while a live/recovery lease exists.
        self.detached(id)?;
        let stats = Self::worker(json!({"op":"stat","directory":self.directory(&image)})).await?;
        let (mut uf, mut ua, mut lf, mut la) = (0, 0, 0, 0);
        for name in ["upper.data", "upper.index"] {
            let m = fs::metadata(
                self.directory(&image)
                    .join("uppers")
                    .join(&image.upper)
                    .join(name),
            )?;
            uf += m.len();
            ua += m.blocks() * 512;
        }
        for layer in &image.layers {
            let m = match fs::metadata(self.root.join("layers").join(layer)) {
                Ok(m) => Some(m),
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && self.state.remote_layers.contains_key(layer) =>
                {
                    None
                }
                Err(e) => return Err(e.into()),
            };
            let file_bytes = m.as_ref().map_or(0, |m| m.len());
            let allocated_bytes = m.as_ref().map_or(0, |m| m.blocks() * 512);
            lf += file_bytes;
            la += allocated_bytes;
        }
        Ok(Usage {
            valid_mapped_bytes: stats["valid_mapped_bytes"]
                .as_u64()
                .context("invalid worker stat")?,
            upper_file_bytes: uf,
            upper_allocated_bytes: ua,
            shared_layer_file_bytes: lf,
            shared_layer_allocated_bytes: la,
            layer_count: image.layers.len(),
            image,
        })
    }
    /// Root scan, not reference-count guessing. Store lock excludes other daemons;
    /// orphaned transaction outputs and retired uppers are safe to collect.
    pub fn gc(&mut self) -> Result<Vec<String>> {
        ensure!(
            self.state.devices.is_empty(),
            "GC requires managed devices deleted"
        );
        self.reconcile_closed()?;
        ensure!(
            self.state.pending_restacks.is_empty(),
            "GC blocked by incomplete live restack"
        );
        ensure!(
            self.state.leases.is_empty(),
            "GC requires all RAW leases released/recovered"
        );
        self.reconcile_closed()?;
        let images: BTreeSet<_> = self
            .state
            .images
            .values()
            .map(|i| i.storage_id.clone())
            .collect();
        let layers: BTreeSet<_> = self
            .state
            .images
            .values()
            .flat_map(|i| i.layers.iter())
            .chain(self.state.snapshots.values().flat_map(|s| s.layers.iter()))
            .cloned()
            .collect();
        let mut removed = vec![];
        // Hold every surviving image lock throughout collection.
        let _locks = self
            .state
            .images
            .values()
            .map(|i| lock_resource(&self.directory(i)))
            .collect::<Result<Vec<_>>>()?;
        for image in self.state.images.values() {
            let dir = self.directory(image);
            for entry in fs::read_dir(dir.join("uppers"))? {
                let e = entry?;
                let id = e.file_name().to_string_lossy().into_owned();
                validate_object(&id)?;
                if id != image.upper {
                    ensure!(e.file_type()?.is_dir(), "invalid upper");
                    fs::remove_dir_all(e.path())?;
                    removed.push(format!("images/{}/uppers/{id}", image.storage_id));
                }
            }
            sync_directory(&dir.join("uppers"))?;
        }
        let mut next = self.state.clone();
        next.remote_layers.retain(|id, _| layers.contains(id));
        self.publish(next, "gc-remote-references")?;
        for (dir, roots) in [("images", images), ("layers", layers)] {
            for entry in fs::read_dir(self.root.join(dir))? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                validate_object(&name)?;
                if roots.contains(&name) {
                    continue;
                }
                ensure!(
                    !entry.file_type()?.is_symlink(),
                    "unexpected symlink in object store"
                );
                if dir == "images" {
                    fs::remove_dir_all(entry.path())?;
                } else {
                    fs::remove_file(entry.path())?;
                }
                removed.push(format!("{dir}/{name}"));
            }
            sync_directory(&self.root.join(dir))?;
        }
        // Interrupted network jobs may leave private probe/export directories.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(id) = name
                .strip_prefix("transfer-")
                .or_else(|| name.strip_prefix("probe-"))
            {
                validate_object(id)?;
                ensure!(entry.file_type()?.is_dir(), "invalid transfer directory");
                fs::remove_dir_all(entry.path())?;
                removed.push(name);
            }
        }
        sync_directory(&self.root)?;
        Ok(removed)
    }
    pub fn checkpoint(&self) -> Result<()> {
        self.db.execute_batch("PRAGMA wal_checkpoint(FULL)")?;
        durable_file(&self.root.join("state.db"))
    }
}
