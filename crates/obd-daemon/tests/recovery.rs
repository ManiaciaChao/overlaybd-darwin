use obd_runtime::Engine;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn start(root: &Path, phase: Option<&str>) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_overlaybd-macosd"));
    cmd.arg(root).stdout(Stdio::null()).stderr(Stdio::inherit());
    if let Some(phase) = phase {
        cmd.env("OBD_TEST_CRASH_AT", phase);
    }
    let mut d = Daemon(cmd.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if request(root, json!({"op":"list"})).is_ok() {
            return d;
        }
        assert!(d.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn request(root: &Path, op: Value) -> anyhow::Result<Value> {
    let mut s = UnixStream::connect(root.join("control.sock"))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let b = serde_json::to_vec(&json!({"version":1,"operation":op}))?;
    s.write_all(&(b.len() as u32).to_be_bytes())?;
    s.write_all(&b)?;
    let mut size = [0; 4];
    s.read_exact(&mut size)?;
    let mut b = vec![0; u32::from_be_bytes(size) as usize];
    s.read_exact(&mut b)?;
    let v: Value = serde_json::from_slice(&b)?;
    anyhow::ensure!(v["error"].is_null(), "{}", v["error"]);
    Ok(v["result"].clone())
}
fn create(root: &Path, id: &str) -> PathBuf {
    request(
        root,
        json!({"op":"create","id":id,"geometry":{"virtual_size":1048576,"logical_block_size":512}}),
    )
    .unwrap();
    resource(root, id)
}
fn resource(root: &Path, id: &str) -> PathBuf {
    PathBuf::from(
        request(root, json!({"op":"resource","id":id})).unwrap()["directory"]
            .as_str()
            .unwrap(),
    )
}
fn read(e: &Engine) -> Vec<u8> {
    let mut b = vec![0; 3];
    e.read(511, &mut b).unwrap();
    b
}
fn wait_snapshot(root: &Path, name: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if request(root, json!({"op":"snapshots"}))
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == name)
        {
            return;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
}
#[test]
fn controller_death_does_not_stop_io_and_reconnect_preserves_exclusion() {
    let t = tempfile::Builder::new()
        .prefix("obd-store-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().join("store");
    let d = start(&root, None);
    let dir = create(&root, "a");
    let e = Engine::attached(&dir, true).unwrap();
    e.write(511, &[1, 2, 3]).unwrap();
    let before = request(&root, json!({"op":"leases"})).unwrap();
    assert!(before[0].get("credential").is_none());
    assert!(request(&root, json!({"op":"list"})).unwrap()[0]
        .get("secret")
        .is_none());
    drop(d);
    e.write(511, &[4, 5, 6]).unwrap();
    e.flush().unwrap();
    assert_eq!(read(&e), [4, 5, 6]);
    assert!(Engine::local(&dir, true).is_err());
    let d = start(&root, None);
    std::thread::sleep(Duration::from_millis(500));
    let after = request(&root, json!({"op":"leases"})).unwrap();
    assert_eq!(before[0]["token"], after[0]["token"]);
    assert!(Engine::attached(&dir, true).is_err());
    assert!(request(&root, json!({"op":"gc"})).is_err());
    assert!(request(
        &root,
        json!({"op":"recover","token":before[0]["token"],"confirmed_offline":true})
    )
    .is_err());
    request(&root, json!({"op":"live_snapshot","id":"a","name":"s"})).unwrap();
    wait_snapshot(&root, "s");
    e.write(511, &[7, 8, 9]).unwrap();
    request(&root, json!({"op":"fork","snapshot":"s","id":"b"})).unwrap();
    let b = Engine::attached(&resource(&root, "b"), true).unwrap();
    assert_eq!(read(&b), [4, 5, 6]);
    b.write(511, &[9, 9, 9]).unwrap();
    assert_eq!(read(&e), [7, 8, 9]);
    b.close().unwrap();
    drop(d);
    e.write(0, &[1]).unwrap();
    e.close().unwrap(); // controller absent clean close
    let _d = start(&root, None);
    assert!(request(&root, json!({"op":"leases"}))
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    let e = Engine::attached(&dir, true).unwrap();
    assert_eq!(read(&e), [7, 8, 9]);
    e.close().unwrap();
}
#[test]
fn snapshot_process_crash_at_every_publish_boundary() {
    for phase in [
        "snapshot-layer-durable",
        "snapshot-upper-durable",
        "snapshot-published",
    ] {
        let t = tempfile::Builder::new()
            .prefix("obd-store-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = t.path().join("store");
        let mut d = start(&root, Some(phase));
        let dir = create(&root, "a");
        let e = Engine::attached(&dir, true).unwrap();
        e.write(511, &[1, 2, 3]).unwrap();
        e.close().unwrap();
        assert!(request(
            &root,
            json!({"op":"snapshot","id":"a","name":"s","seal":false})
        )
        .is_err());
        assert_eq!(d.0.wait().unwrap().code(), Some(86));
        drop(d);
        let _d = start(&root, None);
        let e = Engine::attached(&dir, true).unwrap();
        assert_eq!(read(&e), [1, 2, 3]);
        e.close().unwrap();
        let snapshots = request(&root, json!({"op":"snapshots"})).unwrap();
        assert_eq!(
            snapshots.as_array().unwrap().len(),
            usize::from(phase == "snapshot-published")
        );
        if phase != "snapshot-published" {
            request(
                &root,
                json!({"op":"snapshot","id":"a","name":"s","seal":false}),
            )
            .unwrap();
        }
        request(&root, json!({"op":"fork","snapshot":"s","id":"b"})).unwrap();
        let b = Engine::attached(&resource(&root, "b"), false).unwrap();
        assert_eq!(read(&b), [1, 2, 3]);
        b.close().unwrap();
    }
}
#[test]
fn protocol_rejects_data_plane_and_scopes_resource_credentials() {
    let t = tempfile::Builder::new()
        .prefix("obd-store-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().join("store");
    let _d = start(&root, None);
    let dir = create(&root, "a");
    create(&root, "b");
    for op in [
        "read",
        "write",
        "read_lease",
        "write_lease",
        "flush",
        "discard",
    ] {
        assert!(request(&root, json!({"op":op})).is_err());
    }
    let endpoint: Value = obd_runtime::read_json(&dir.join("control.json")).unwrap();
    fn tcp_call(s: &mut std::net::TcpStream, op: Value) -> Value {
        let b = serde_json::to_vec(&json!({"version":1,"operation":op})).unwrap();
        s.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
        s.write_all(&b).unwrap();
        let mut n = [0; 4];
        s.read_exact(&mut n).unwrap();
        let mut b = vec![0; u32::from_be_bytes(n) as usize];
        s.read_exact(&mut b).unwrap();
        serde_json::from_slice(&b).unwrap()
    }
    let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{}", endpoint["port"])).unwrap();
    assert!(tcp_call(
        &mut s,
        json!({"op":"authenticate","secret":endpoint["secret"]})
    )["error"]
        .is_null());
    assert!(!tcp_call(&mut s, json!({"op":"list"}))["error"].is_null());
    assert!(!tcp_call(
        &mut s,
        json!({"op":"claim","id":"b","writable":true,"instance":uuid::Uuid::new_v4().to_string()})
    )["error"]
        .is_null());
    let mut bad = UnixStream::connect(root.join("control.sock")).unwrap();
    bad.write_all(&u32::MAX.to_be_bytes()).unwrap();
    assert_eq!(bad.read(&mut [0]).unwrap(), 0);
}

#[test]
fn live_snapshot_controller_crash_replays_operation_once() {
    for phase in [
        "live-snapshot-prepared",
        "live-snapshot-durable",
        "live-snapshot-published",
    ] {
        let t = tempfile::Builder::new()
            .prefix("obd-live-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = t.path().join("store");
        let mut d = start(&root, Some(phase));
        let dir = create(&root, "a");
        let e = Engine::attached(&dir, true).unwrap();
        e.write(511, &[1, 2, 3]).unwrap();
        let before = request(&root, json!({"op":"leases"})).unwrap();
        let _ = request(&root, json!({"op":"live_snapshot","id":"a","name":"live"}));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = d.0.try_wait().unwrap() {
                assert_eq!(status.code(), Some(86));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(d);
        let _d = start(&root, None);
        wait_snapshot(&root, "live");
        // The gate remains closed until the returned ack is consumed.
        let (tx, rx) = std::sync::mpsc::channel();
        let other = e.clone();
        let writer = std::thread::spawn(move || {
            tx.send(other.write(511, &[4, 5, 6])).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        writer.join().unwrap();
        let after = request(&root, json!({"op":"leases"})).unwrap();
        assert_eq!(before[0]["token"], after[0]["token"]);
        assert_eq!(after[0]["generation"], 1);
        assert_eq!(std::fs::read_dir(dir.join("layers")).unwrap().count(), 1);
        request(&root, json!({"op":"fork","snapshot":"live","id":"b"})).unwrap();
        let b = Engine::attached(&resource(&root, "b"), false).unwrap();
        assert_eq!(read(&b), [1, 2, 3]);
        b.close().unwrap();
        e.close().unwrap();
    }
}

#[test]
fn compact_crash_keeps_source_and_publishes_only_complete_snapshot() {
    for phase in ["compact-layer-durable", "compact-snapshot-published"] {
        let t = tempfile::Builder::new()
            .prefix("obd-compact-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = t.path().join("store");
        let mut d = start(&root, Some(phase));
        let dir = create(&root, "a");
        let e = Engine::attached(&dir, true).unwrap();
        e.write(511, &[1, 2, 3]).unwrap();
        e.close().unwrap();
        request(
            &root,
            json!({"op":"snapshot","id":"a","name":"source","seal":false}),
        )
        .unwrap();
        assert!(request(
            &root,
            json!({"op":"compact_snapshot","source":"source","name":"flat"})
        )
        .is_err());
        assert_eq!(d.0.wait().unwrap().code(), Some(86));
        drop(d);
        let _d = start(&root, None);
        let snaps = request(&root, json!({"op":"snapshots"})).unwrap();
        let published = snaps.as_array().unwrap().iter().any(|v| v["id"] == "flat");
        assert_eq!(published, phase == "compact-snapshot-published");
        if !published {
            request(
                &root,
                json!({"op":"compact_snapshot","source":"source","name":"flat"}),
            )
            .unwrap();
        }
        request(&root, json!({"op":"gc"})).unwrap();
        request(&root, json!({"op":"fork","snapshot":"flat","id":"b"})).unwrap();
        let e = Engine::attached(&resource(&root, "b"), false).unwrap();
        assert_eq!(read(&e), [1, 2, 3]);
        e.close().unwrap();
        let e = Engine::attached(&dir, false).unwrap();
        assert_eq!(read(&e), [1, 2, 3]);
        e.close().unwrap();
    }
}
