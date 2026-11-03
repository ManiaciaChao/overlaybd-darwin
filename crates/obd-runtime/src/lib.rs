//! In-process engine and durable per-image ownership. No block data is sent over IPC.
mod diagnostics;
pub mod storage_stats;
use diagnostics::Diagnostics;
use anyhow::{ensure, Context, Result};
use obd_core_adapter::{durable_file, sync_directory, BlockImage, Geometry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex, OnceLock,
    },
    time::Duration,
};

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    })
}
pub const RESOURCE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resource {
    pub version: u32,
    pub image: String,
    pub geometry: Geometry,
    pub generation: u64,
    pub upper: String,
    pub layers: Vec<String>,
    pub sealed: bool,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub remote_layers: std::collections::BTreeMap<String, obd_core_adapter::remote::RemoteLayer>,
}
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    durable_file(&temp)?;
    fs::rename(&temp, path)?;
    sync_directory(path.parent().context("parent")?)
}
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        f.metadata()?.is_file() && f.metadata()?.len() <= 1024 * 1024,
        "invalid descriptor"
    );
    let mut b = vec![];
    f.read_to_end(&mut b)?;
    Ok(serde_json::from_slice(&b)?)
}
/// Exclusive ownership ends with this guard, even if a concurrent process
/// launch temporarily inherited the underlying open-file description.
pub struct OwnershipLock(File);
impl OwnershipLock {
    pub fn acquire(file: File) -> Result<Self> {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}
impl Drop for OwnershipLock {
    fn drop(&mut self) {
        // close alone can leave the flock alive in a forked child's duplicate
        // until exec. Explicit unlock releases only this owner's description.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN); }
    }
}
pub fn lock_resource(dir: &Path) -> Result<OwnershipLock> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("runtime.lock"))?;
    OwnershipLock::acquire(f)
}
fn contained(dir: &Path, p: &Path) -> Result<PathBuf> {
    let p = p.canonicalize()?;
    ensure!(p.starts_with(dir), "path escapes image resource");
    Ok(p)
}
pub async fn open_image(dir: &Path, writable: bool) -> Result<BlockImage> {
    let dir = dir.canonicalize()?;
    let r: Resource = read_json(&dir.join("resource.json"))?;
    ensure!(
        matches!(r.version, RESOURCE_VERSION | 3 | 4),
        "unsupported resource schema"
    );
    ensure!(!writable || !r.sealed, "EROFS: sealed image");
    uuid::Uuid::parse_str(&r.upper)?;
    let upper = contained(&dir, &dir.join("uppers").join(&r.upper))?;
    let files:&[&str]=if upper.join("upper.packed").try_exists()? {&["upper.packed","upper.policy"]}else{&["upper.data","upper.index"]};
    for n in files {contained(&dir,&upper.join(n))?;}
    match obd_core_adapter::upper_compression(&upper)? {
        "vdo-lz4" => { contained(&dir, &upper.join("upper.vdo"))?; }
        "lz4" | "zstd" => { contained(&dir, &upper.join("upper.zlog"))?; }
        _ => {}
    }
    if upper.join("index-cache").exists() {
        contained(&dir, &upper.join("index-cache"))?;
    }
    ensure!(
        matches!(r.version, RESOURCE_VERSION | 4) || r.remote_layers.is_empty(),
        "remote layers require schema 4"
    );
    let mut config = BlockImage::local_config(&upper, &[])?;
    for id in &r.layers {
        uuid::Uuid::parse_str(id)?;
        if let Some(remote) = r.remote_layers.get(id) {
            remote.validate()?;
            config.lowers.push(remote.config());
        } else {
            let path = contained(&dir, &dir.join("layers").join(id))?;
            config
                .lowers
                .extend(BlockImage::local_config(&upper, &[path])?.lowers);
        }
    }
    BlockImage::open_config(&upper, config, r.geometry, writable).await
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub operation: String,
    pub generation: u64,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Command {
    pub operation: String,
    pub generation: u64,
    pub layer: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub token: String,
    pub instance: String,
    pub credential: String,
    pub generation: u64,
}

pub fn restack(dir: &Path, image: &BlockImage, command: &Command) -> Result<Receipt> {
    uuid::Uuid::parse_str(&command.operation)?;
    uuid::Uuid::parse_str(&command.layer)?;
    let path = dir
        .join("operations")
        .join(format!("{}.json", command.operation));
    if path.exists() {
        let r: Receipt = read_json(&path)?;
        ensure!(
            r.operation == command.operation && r.generation == command.generation,
            "receipt mismatch"
        );
        ensure!(r.status == "success", "ambiguous restack; quarantined");
        return Ok(r);
    }
    let r: Resource = read_json(&dir.join("resource.json"))?;
    ensure!(r.generation == command.generation, "stale generation");
    let mut receipt = Receipt {
        operation: command.operation.clone(),
        generation: command.generation,
        status: "started".into(),
    };
    atomic_json(&path, &receipt)?;
    runtime().block_on(image.restack_upper(&dir.join("layers").join(&command.layer)))?;
    receipt.status = "success".into();
    atomic_json(&path, &receipt)?;
    Ok(receipt)
}
struct State {
    generation: u64,
    image: Option<Arc<BlockImage>>,
    lock: Option<OwnershipLock>,
    frozen: bool,
    receipt: Option<Receipt>,
    closed: bool,
    closing: bool,
    active: BTreeSet<u64>,
    next_io: u64,
}
struct ActiveIo<'a> {
    engine: &'a Engine,
    image: Arc<BlockImage>,
    sequence: u64,
}
impl ActiveIo<'_> {
    fn wait_preceding(&self) -> Result<()> {
        // Do not use Engine::state here: close/restack may already be waiting
        // for this lease. Later I/O may run without extending this boundary.
        let mut state = self.engine.state.lock().map_err(|_| anyhow::anyhow!("poisoned engine"))?;
        while state.active.first().is_some_and(|sequence| *sequence < self.sequence) {
            state = self.engine.ready.wait(state).map_err(|_| anyhow::anyhow!("poisoned engine"))?;
        }
        Ok(())
    }
}
impl Drop for ActiveIo<'_> {
    fn drop(&mut self) {
        let mut state = self.engine.state.lock().unwrap();
        state.active.remove(&self.sequence);
        self.engine.ready.notify_all();
    }
}
pub struct Engine {
    storage_report: Mutex<Option<storage_stats::StorageReport>>,
    diagnostics: Option<Diagnostics>,
    dir: PathBuf,
    state: Mutex<State>,
    ready: Condvar,
    attachment: Option<Attachment>,
}
impl Engine {
    pub fn local(dir: &Path, writable: bool) -> Result<Arc<Self>> {
        let dir = dir.canonicalize()?;
        let lock = lock_resource(&dir)?;
        let image = runtime().block_on(open_image(&dir, writable))?;
        Ok(Arc::new(Self {
            storage_report: Mutex::new(None),
            diagnostics: Diagnostics::open(&dir),
            dir,
            state: Mutex::new(State {
                generation: 0,
                image: Some(Arc::new(image)),
                lock: Some(lock),
                frozen: false,
                receipt: None,
                closed: false,
                closing: false,
                active: BTreeSet::new(),
                next_io: 0,
            }),
            ready: Condvar::new(),
            attachment: None,
        }))
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned engine"))?;
        while s.frozen && !s.closed {
            s = self
                .ready
                .wait(s)
                .map_err(|_| anyhow::anyhow!("poisoned engine"))?;
        }
        ensure!(!s.closed && !s.closing, "closed engine");
        Ok(s)
    }
    fn begin_io(&self)->Result<ActiveIo<'_>> {
        let mut s = self.state()?;
        let image = s.image.as_ref().context("closed engine")?.clone();
        let sequence = s.next_io;
        s.next_io = sequence.checked_add(1).context("engine I/O sequence overflow")?;
        s.active.insert(sequence);
        Ok(ActiveIo { engine: self, image, sequence })
    }
    pub fn read(&self,offset:u64,dst:&mut[u8])->Result<usize> {
        Diagnostics::measure(self.diagnostics.as_ref(), false, offset, dst.len(), || {
            let io=self.begin_io()?;runtime().block_on(io.image.read_at_into(offset,dst))
        })
    }
    pub fn write(&self,offset:u64,src:&[u8])->Result<()> {
        Diagnostics::measure(self.diagnostics.as_ref(), true, offset, src.len(), || {
            let io=self.begin_io()?;runtime().block_on(io.image.write_at(offset,src))
        })
    }
    pub fn discard_vdo(&self,offset:u64,length:usize)->Result<()> {let io=self.begin_io()?;runtime().block_on(io.image.discard_vdo(offset,length))}
    pub fn flush(&self) -> Result<()> {
        let Some(trace) = &self.diagnostics else {
            let io = self.begin_io()?;
            io.wait_preceding()?;
            return runtime().block_on(io.image.flush());
        };
        let wall_start_ns = Diagnostics::wall_ns();
        let start = std::time::Instant::now();
        let io = self.begin_io()?;
        io.wait_preceding()?;
        let preceding_ns = start.elapsed().as_nanos() as u64;
        let before = trace.snapshot(&io.image);
        let start = std::time::Instant::now();
        let result = runtime().block_on(io.image.flush());
        let flush_ns = start.elapsed().as_nanos() as u64;
        let after = trace.snapshot(&io.image);
        trace.record(json!({"wall_start_ns":wall_start_ns, "wall_end_ns":Diagnostics::wall_ns(),
            "preceding_ns":preceding_ns, "flush_ns":flush_ns, "ok":result.is_ok(),
            "before":before, "after":after}));
        result
    }
    pub fn close(&self) -> Result<()> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned engine"))?;
        if s.closed {
            return Ok(());
        }
        ensure!(!s.frozen, "EBUSY: unresolved restack");
        while s.closing && !s.closed {s=self.ready.wait(s).map_err(|_|anyhow::anyhow!("poisoned"))?;}
        if s.closed {return Ok(());}
        s.closing=true;
        while !s.active.is_empty() {s=self.ready.wait(s).map_err(|_|anyhow::anyhow!("poisoned"))?;}
        if let Err(e)=runtime().block_on(s.image.as_ref().context("closed engine")?.flush()) {s.closing=false;self.ready.notify_all();return Err(e);}

        // Opt-in benchmark diagnostics are written only at close, outside timed I/O.
        // A diagnostics failure must not change the engine's durability contract.
        if self.dir.join("io-profile.enabled").is_file() {
            let profile = runtime().block_on(s.image.as_ref().unwrap().flush_profile());
            if let Ok(Some(stats))=runtime().block_on(s.image.as_ref().unwrap().packed_stats()) {let _=atomic_json(&self.dir.join("packed-profile.json"),&stats);}
            if let Err(error) = atomic_json(&self.dir.join("io-profile.json"), &profile) {
                eprintln!("OverlayBD diagnostics: {error:#}");
            }
        }
        if let Some(trace) = &self.diagnostics { trace.save(&self.dir); }
        if let Some(a) = &self.attachment {
            let mut a = a.clone();
            a.generation = s.generation;
            if let Err(error) = atomic_json(
                &self.dir.join("operations").join(format!("closed-{}.json", a.token)),
                &a,
            ) {
                s.closing = false;
                self.ready.notify_all();
                return Err(error);
            }
        }
        s.image.take();
        s.closed = true;
        s.lock.take();
        self.ready.notify_all();
        Ok(())
    }
    pub fn attached(dir: &Path, writable: bool) -> Result<Arc<Self>> {
        let dir = dir.canonicalize()?;
        let r: Resource = read_json(&dir.join("resource.json"))?;
        let instance = uuid::Uuid::new_v4().to_string();
        let value = control(
            &dir,
            json!({"op":"claim","id":r.image,"writable":writable,"instance":instance}),
        )?;
        let a: Attachment = serde_json::from_value(value)?;
        let lock = lock_resource(&dir)?;
        // A failed open deliberately retains the durable attachment for explicit recovery.
        let image = runtime().block_on(open_image(&dir, writable))?;
        let e = Arc::new(Self {
            storage_report: Mutex::new(None),
            diagnostics: Diagnostics::open(&dir),
            dir,
            state: Mutex::new(State {
                generation: a.generation,
                image: Some(Arc::new(image)),
                lock: Some(lock),
                frozen: false,
                receipt: None,
                closed: false,
                closing: false,
                active: BTreeSet::new(),
                next_io: 0,
            }),
            ready: Condvar::new(),
            attachment: Some(a),
        });
        let weak = Arc::downgrade(&e);
        std::thread::Builder::new()
            .name("obd-control".into())
            .spawn(move || {
                while let Some(e) = weak.upgrade() {
                    if e.state.lock().map(|s| s.closed).unwrap_or(true) {
                        let _ = e.poll();
                        break;
                    }
                    if let Err(error) = e.poll() {
                        eprintln!("OverlayBD control: {error:#}");
                    }
                    drop(e);
                    std::thread::sleep(Duration::from_millis(200));
                }
            })?;
        Ok(e)
    }
    fn poll(&self) -> Result<()> {
        let mut a = self.attachment.as_ref().context("no attachment")?.clone();
        let (receipt, closed) = {
            let s = self.state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            a.generation = s.generation;
            (s.receipt.clone(), s.closed)
        };
        // No image mutex is held during any network operation.
        let mut request = json!({"op":"poll","attachment":a,"receipt":receipt,"closed":closed});
        // Only add the optional field after a server requested it. Older servers
        // reject unknown fields, so ordinary polls retain their original shape.
        if let Some(report) = self.storage_report.lock().unwrap().as_ref() {
            request["storage_report"] = serde_json::to_value(report)?;
        }
        let value = control(&self.dir, request)?;
        if closed {
            return Ok(());
        }
        if let Some(command) = value.get("command").filter(|v| !v.is_null()) {
            let command: Command = serde_json::from_value(command.clone())?;
            let mut s = self.state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            if s.closed || s.closing {
                return Ok(());
            }
            s.frozen = true;
            while !s.active.is_empty() {s=self.ready.wait(s).map_err(|_|anyhow::anyhow!("poisoned"))?;}
            let receipt = restack(&self.dir, s.image.as_ref().context("closed")?, &command)?;
            s.receipt = Some(receipt);
        } else if value["ack"].as_str().is_some() {
            let mut s = self.state.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            if s.receipt
                .as_ref()
                .is_some_and(|r| Some(r.operation.as_str()) == value["ack"].as_str())
            {
                let generation = value["generation"]
                    .as_u64()
                    .context("missing ack generation")?;
                ensure!(generation == s.generation + 1, "invalid ack generation");
                s.generation = generation;
                s.frozen = false;
                s.receipt = None;
                self.ready.notify_all();
            }
        }
        self.poll_storage_stats(&value);
        Ok(())
    }
}
fn control(dir: &Path, op: Value) -> Result<Value> {
    let endpoint: Value = read_json(&dir.join("control.json"))?;
    let port = endpoint["port"].as_u64().context("missing control port")?;
    ensure!(port > 0 && port <= 65535, "invalid port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.set_nodelay(true)?;
    fn call(s: &mut TcpStream, op: Value) -> Result<Value> {
        let data = serde_json::to_vec(&json!({"version":1,"operation":op}))?;
        s.write_all(&(data.len() as u32).to_be_bytes())?;
        s.write_all(&data)?;
        let mut size = [0; 4];
        s.read_exact(&mut size)?;
        let n = u32::from_be_bytes(size) as usize;
        ensure!(n > 0 && n <= 1048576, "invalid frame");
        let mut data = vec![0; n];
        s.read_exact(&mut data)?;
        let value: Value = serde_json::from_slice(&data)?;
        ensure!(
            value["version"] == 1 && value["error"].is_null(),
            "control failure: {}",
            value["error"]
        );
        Ok(value["result"].clone())
    }
    call(
        &mut s,
        json!({"op":"authenticate","secret":endpoint["secret"]}),
    )?;
    call(&mut s, op)
}

// Integer handles prevent use-after-free on concurrent close. In-flight calls hold an Arc.
fn handles() -> &'static Mutex<HashMap<u64, Arc<Engine>>> {
    static H: OnceLock<Mutex<HashMap<u64, Arc<Engine>>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}
fn handle(id: u64) -> Result<Arc<Engine>> {
    handles()
        .lock()
        .map_err(|_| anyhow::anyhow!("poisoned handles"))?
        .get(&id)
        .cloned()
        .context("closed engine")
}
fn ffi(f: impl FnOnce() -> Result<()>) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            eprintln!("OverlayBD engine: {e:#}");
            e.chain()
                .find_map(|x| {
                    x.downcast_ref::<std::io::Error>()
                        .and_then(|e| e.raw_os_error())
                })
                .unwrap_or_else(|| {
                    let m = e.to_string();
                    if m.contains("EROFS") {
                        libc::EROFS
                    } else if m.contains("EBUSY") {
                        libc::EBUSY
                    } else if m.contains("capacity") || m.contains("4 MiB") {
                        libc::EINVAL
                    } else {
                        libc::EIO
                    }
                })
        }
        Err(_) => libc::EIO,
    }
}
/// # Safety
/// C string and output pointers must be valid for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn obd_open(
    path: *const libc::c_char,
    writable: bool,
    out: *mut u64,
    capacity: *mut u64,
) -> i32 {
    ffi(|| {
        ensure!(
            !path.is_null() && !out.is_null() && !capacity.is_null(),
            "null pointer"
        );
        let dir = Path::new(std::ffi::CStr::from_ptr(path).to_str()?);
        let e = Engine::attached(dir, writable)?;
        let r: Resource = read_json(&dir.join("resource.json"))?;
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        handles()
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .insert(id, e);
        *out = id;
        *capacity = r.geometry.virtual_size;
        Ok(())
    })
}
/// # Safety
/// Buffer must be writable for len bytes, actual must be a valid output pointer.
#[no_mangle]
pub unsafe extern "C" fn obd_read(
    id: u64,
    offset: u64,
    buffer: *mut u8,
    len: usize,
    actual: *mut usize,
) -> i32 {
    ffi(|| {
        ensure!(
            !actual.is_null() && (!buffer.is_null() || len == 0),
            "null pointer"
        );
        *actual = 0;
        let dst = if len == 0 {
            &mut []
        } else {
            std::slice::from_raw_parts_mut(buffer, len)
        };
        *actual = handle(id)?.read(offset, dst)?;
        Ok(())
    })
}
/// # Safety
/// Buffer must be readable for len bytes, actual must be a valid output pointer.
#[no_mangle]
pub unsafe extern "C" fn obd_write(
    id: u64,
    offset: u64,
    buffer: *const u8,
    len: usize,
    actual: *mut usize,
) -> i32 {
    ffi(|| {
        ensure!(
            !actual.is_null() && (!buffer.is_null() || len == 0),
            "null pointer"
        );
        *actual = 0;
        let src = if len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(buffer, len)
        };
        handle(id)?.write(offset, src)?;
        *actual = len;
        Ok(())
    })
}
/// Experimental explicit range discard. FSKit/DiskImages does not currently
/// forward guest filesystem TRIM through the raw-file bridge.
#[no_mangle]
pub extern "C" fn obd_discard_vdo(id: u64, offset: u64, length: usize) -> i32 {
    ffi(|| handle(id)?.discard_vdo(offset, length))
}
#[no_mangle]
pub extern "C" fn obd_flush(id: u64) -> i32 {
    ffi(|| handle(id)?.flush())
}
#[no_mangle]
pub extern "C" fn obd_close(id: u64) -> i32 {
    ffi(|| {
        handle(id)?.close()?;
        handles()
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .remove(&id);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Arc<Engine>) {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        for name in ["uppers", "layers", "operations"] {
            fs::create_dir(dir.join(name)).unwrap();
        }
        let upper = uuid::Uuid::new_v4().to_string();
        let geometry = Geometry {
            virtual_size: 1024 * 1024,
            logical_block_size: 512,
        };
        runtime()
            .block_on(BlockImage::create_upper(
                &dir.join("uppers").join(&upper),
                geometry,
            ))
            .unwrap();
        atomic_json(
            &dir.join("resource.json"),
            &Resource {
                version: RESOURCE_VERSION,
                image: "test".into(),
                geometry,
                generation: 0,
                upper,
                layers: vec![],
                sealed: false,
                remote_layers: Default::default(),
            },
        )
        .unwrap();
        let e = Engine::local(dir, true).unwrap();
        (t, e)
    }
    #[test]
    fn optional_trace_preserves_io_results_and_writes_only_at_close() -> Result<()> {
        let (dir, engine) = setup();
        engine.close()?;
        fs::write(dir.path().join("io-trace.enabled"), [])?;
        let engine = Engine::local(dir.path(), true)?;
        engine.write(0, &[37; 512])?;
        assert!(engine.write(1024 * 1024, &[1; 512]).is_err());
        let mut read = [0; 512];
        engine.read(0, &mut read)?;
        assert_eq!(read, [37; 512]);
        engine.flush()?;
        assert!(!dir.path().join("flush-trace.json").exists());
        engine.close()?;
        let trace: Value = read_json(&dir.path().join("flush-trace.json"))?;
        assert_eq!(trace["flushes"][0]["ok"], true);
        assert_eq!(trace["flushes"][0]["after"]["write"]["calls"], 2);
        assert_eq!(trace["io"].as_array().unwrap().len(), 3);
        assert_eq!(trace["io"][0]["write"], true);
        assert_eq!(trace["io"][0]["len"], 512);
        assert_eq!(trace["io"][1]["ok"], false);
        assert_eq!(trace["io"][2]["write"], false);
        // A diagnostic output error must never turn durable I/O into failure.
        fs::remove_file(dir.path().join("flush-trace.json"))?;
        fs::create_dir(dir.path().join("flush-trace.json"))?;
        let engine = Engine::local(dir.path(), true)?;
        engine.read(0, &mut read)?;
        assert_eq!(read, [37; 512]);
        engine.flush()?;
        engine.close()?;
        Ok(())
    }
    #[test]
    fn abi_buffers_bounds_eof_readonly_and_closed_handle() {
        let (t, e) = setup();
        let id = 9001;
        handles().lock().unwrap().insert(id, e.clone());
        let src = [1, 2, 3];
        let mut actual = 99;
        let mut dst = [0; 3];
        unsafe {
            assert_eq!(obd_write(id, 511, src.as_ptr(), 3, &mut actual), 0);
            assert_eq!(actual, 3);
            assert_eq!(obd_read(id, 511, dst.as_mut_ptr(), 3, &mut actual), 0);
            assert_eq!(dst, src);
            assert_eq!(
                obd_read(id, 1024 * 1024, dst.as_mut_ptr(), 3, &mut actual),
                0
            );
            assert_eq!(actual, 0);
            assert_eq!(
                obd_write(id, 1024 * 1024, src.as_ptr(), 3, &mut actual),
                libc::EINVAL
            );
            assert_eq!(actual, 0);
            assert_eq!(obd_read(id, 0, std::ptr::null_mut(), 0, &mut actual), 0);
        }
        assert_eq!(obd_flush(id), 0);
        assert_eq!(obd_close(id), 0);
        assert_ne!(obd_flush(id), 0);
        let ro = Engine::local(t.path(), false).unwrap();
        assert!(ro.write(0, &src).is_err());
        ro.close().unwrap();
    }
    #[test]
    fn close_waits_for_inflight_and_rejects_later_io() {
        let (_t, e) = setup();
        let gate = e.state.lock().unwrap();
        let other = e.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let r = other.close();
            tx.send(r).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(gate);
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        thread.join().unwrap();
        assert!(e.write(0, &[1]).is_err());
    }
    #[test]
    fn flush_fences_preceding_io_without_waiting_for_later_leases() {
        let (_t, e) = setup();
        let first = e.begin_io().unwrap();
        let second = e.begin_io().unwrap();
        let other = e.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || tx.send(other.flush()).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while e.state.lock().unwrap().next_io <= second.sequence + 1 {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        let later = e.begin_io().unwrap();
        drop(second); // Out-of-order completion cannot satisfy the fence.
        assert!(rx.recv_timeout(Duration::from_millis(20)).is_err());
        runtime().block_on(first.image.write_at(0, &[37; 4096])).unwrap();
        drop(first);
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        thread.join().unwrap();
        drop(later);
        e.close().unwrap();
        let reopened = Engine::local(&_t.path(), false).unwrap();
        let mut bytes = [0; 4096];
        reopened.read(0, &mut bytes).unwrap();
        assert_eq!(bytes, [37; 4096]);
        reopened.close().unwrap();
    }
    #[test]
    fn resource_ownership_ends_before_inherited_descriptor_closes() {
        let t = tempfile::tempdir().unwrap();
        let owner = lock_resource(t.path()).unwrap();
        let inherited = owner.0.try_clone().unwrap();
        assert!(lock_resource(t.path()).is_err());
        drop(owner);
        let next = lock_resource(t.path()).unwrap();
        drop(inherited);
        assert!(lock_resource(t.path()).is_err());
        drop(next);
        assert!(lock_resource(t.path()).is_ok());
    }
    #[test]
    fn symlink_escape_is_rejected() {
        let (t, e) = setup();
        e.close().unwrap();
        let r: Resource = read_json(&t.path().join("resource.json")).unwrap();
        let original = t.path().join("uppers").join(r.upper).join("upper.data");
        let outside = tempfile::tempdir().unwrap();
        fs::rename(&original, outside.path().join("data")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("data"), original).unwrap();
        assert!(Engine::local(t.path(), true).is_err());
    }
}
