use obd_core_adapter::Geometry;
use obd_runtime::{
    atomic_json, lock_resource, open_image, restack, runtime, Command, Engine, Receipt,
};
use obd_store::Store;
use std::os::unix::fs::MetadataExt;
fn geo() -> Geometry {
    Geometry {
        virtual_size: 1024 * 1024,
        logical_block_size: 512,
    }
}
fn write(s: &Store, id: &str, bytes: &[u8]) {
    let e = Engine::local(&s.resource(id).unwrap(), true).unwrap();
    e.write(511, bytes).unwrap();
    e.close().unwrap();
}
fn read(s: &Store, id: &str) -> Vec<u8> {
    let e = Engine::local(&s.resource(id).unwrap(), false).unwrap();
    let mut b = vec![0; 3];
    e.read(511, &mut b).unwrap();
    e.close().unwrap();
    b
}
#[test]
fn snapshot_fork_reopen_gc_and_seal() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime().block_on(s.create("a", geo())).unwrap();
    assert!(runtime().block_on(s.create("../escape", geo())).is_err());
    write(&s, "a", &[1, 2, 3]);
    runtime().block_on(s.snapshot("a", "s0", false)).unwrap();
    runtime().block_on(s.fork("s0", "b")).unwrap();
    let a = s.image("a").unwrap();
    let b = s.image("b").unwrap();
    let layer = &a.layers[0];
    assert_eq!(
        std::fs::metadata(s.directory(&a).join("layers").join(layer))
            .unwrap()
            .ino(),
        std::fs::metadata(s.directory(&b).join("layers").join(layer))
            .unwrap()
            .ino()
    );
    assert_eq!(
        std::fs::metadata(
            s.directory(&a)
                .join("uppers")
                .join(&a.upper)
                .join("upper.data")
        )
        .unwrap()
        .nlink(),
        1
    );
    write(&s, "a", &[4, 5, 6]);
    assert_eq!(read(&s, "b"), [1, 2, 3]);
    write(&s, "b", &[1, 0, 3]);
    runtime().block_on(s.snapshot("b", "s1", true)).unwrap();
    assert!(Engine::local(&s.resource("b").unwrap(), true).is_err());
    s.delete_image("a").unwrap();
    s.delete_snapshot("s0").unwrap();
    s.gc().unwrap();
    drop(s);
    let mut s = Store::open(&root).unwrap();
    assert_eq!(read(&s, "b"), [1, 0, 3]);
    runtime().block_on(s.fork("s1", "c")).unwrap();
    s.delete_image("b").unwrap();
    s.delete_snapshot("s1").unwrap();
    s.gc().unwrap();
    assert_eq!(read(&s, "c"), [1, 0, 3]);
    s.delete_image("c").unwrap();
    s.gc().unwrap();
    assert_eq!(std::fs::read_dir(root.join("layers")).unwrap().count(), 0);
}
#[test]
fn lease_owner_exclusion_and_restart_quarantine() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    assert!(Store::open(&root).is_err());
    runtime().block_on(s.create("a", geo())).unwrap();
    let dir = s.resource("a").unwrap();
    let engine = Engine::local(&dir, true).unwrap();
    let a = s
        .claim("a", true, &uuid::Uuid::new_v4().to_string())
        .unwrap();
    assert!(s
        .claim("a", false, &uuid::Uuid::new_v4().to_string())
        .is_err());
    assert!(runtime().block_on(s.snapshot("a", "bad", false)).is_err());
    assert!(s.delete_image("a").is_err());
    assert!(s.gc().is_err());
    assert!(runtime().block_on(s.recover_lease(&a.token)).is_err());
    engine.write(511, &[7, 8, 9]).unwrap();
    engine.flush().unwrap();
    drop(s);
    let mut s = Store::open(&root).unwrap();
    assert!(s.live_snapshot("a", "unknown").is_err());
    let mut bad = a.clone();
    bad.credential = "invalid".into();
    assert!(s.poll(&bad, None, false).is_err());
    s.poll(&a, None, false).unwrap();
    assert!(runtime().block_on(s.recover_lease(&a.token)).is_err());
    engine.close().unwrap();
    runtime().block_on(s.recover_lease(&a.token)).unwrap();
    assert_eq!(read(&s, "a"), [7, 8, 9]);
    assert!(s.poll(&a, None, false).is_err());
}
#[test]
fn repeated_live_restack_receipts_and_restart_publication() {
    check_live_restack_receipts_and_restart_publication("none");
}
#[test]
fn packed_live_restack_receipts_and_restart_quarantine() {
    for codec in ["packed-lz4", "packed-zstd-dedupe"] {
        check_live_restack_receipts_and_restart_publication(codec);
    }
}
fn check_live_restack_receipts_and_restart_publication(codec: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime()
        .block_on(s.create_compressed("source", geo(), codec))
        .unwrap();
    let dir = s.resource("source").unwrap();
    let _lock = lock_resource(&dir).unwrap();
    let h = runtime().block_on(open_image(&dir, true)).unwrap();
    let mut a = s
        .claim("source", true, &uuid::Uuid::new_v4().to_string())
        .unwrap();
    for n in 1..=2 {
        runtime().block_on(h.write_at(511, &[n; 3])).unwrap();
        let p = s.live_snapshot("source", &format!("s{n}")).unwrap();
        let cmd = Command {
            operation: p.operation.clone(),
            generation: p.expected_generation,
            layer: p.image.layers.last().unwrap().clone(),
        };
        let r = restack(&dir, &h, &cmd).unwrap();
        let ino = std::fs::metadata(dir.join("layers").join(&cmd.layer))
            .unwrap()
            .ino();
        restack(&dir, &h, &cmd).unwrap();
        assert_eq!(
            ino,
            std::fs::metadata(dir.join("layers").join(&cmd.layer))
                .unwrap()
                .ino()
        );
        if n == 2 {
            drop(s);
            s = Store::open(&root).unwrap();
        }
        assert_eq!(s.poll(&a, Some(r), false).unwrap()["ack"], p.operation);
        a.generation = n as u64;
        s.poll(&a, None, false).unwrap();
        assert_eq!(s.leases().unwrap()[0].generation, n as u64);
        runtime()
            .block_on(s.fork(&format!("s{n}"), &format!("b{n}")))
            .unwrap();
        assert_eq!(read(&s, &format!("b{n}")), [n; 3]);
    }
    assert!(s.pending_restacks().is_empty());
    let p = s.live_snapshot("source", "ambiguous").unwrap();
    atomic_json(
        &dir.join("operations").join(format!("{}.json", p.operation)),
        &Receipt {
            operation: p.operation,
            generation: p.expected_generation,
            status: "started".into(),
        },
    )
    .unwrap();
    drop(h);
    drop(_lock);
    drop(s);
    let mut s = Store::open(&root).unwrap();
    assert_eq!(s.pending_restacks().len(), 1);
    assert!(s.resource("source").is_err());
    assert!(s.gc().is_err());
    assert!(runtime().block_on(s.recover_lease(&a.token)).is_err());
}
#[test]
fn fresh_schema_one_and_unsupported_versions() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("store");
    drop(Store::open(&root).unwrap());
    let db = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let version: u32 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, 1);
    drop(db);
    drop(Store::open(&root).unwrap());
    for version in [2, 3] {
        let db = rusqlite::Connection::open(root.join("state.db")).unwrap();
        db.execute_batch(&format!("PRAGMA user_version={version}"))
            .unwrap();
        drop(db);
        assert!(Store::open(&root)
            .err()
            .unwrap()
            .to_string()
            .contains("no migration"));
    }
}

#[test]
fn managed_device_intent_survives_restart_and_blocks_offline_mutations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime().block_on(s.create("a", geo())).unwrap();
    let record = serde_json::json!({"image":"a","state":"creating"});
    s.reserve_device(1, "a", record.clone()).unwrap();
    assert!(s.reserve_device(2, "a", record.clone()).is_err());
    assert!(s.delete_image("a").is_err());
    assert!(runtime()
        .block_on(s.snapshot("a", "blocked", false))
        .is_err());
    assert!(s.gc().is_err());
    drop(s);
    let mut s = Store::open(&root).unwrap();
    assert_eq!(s.devices()[&1], record);
    // The extension must still be able to claim its reserved image.
    let instance = uuid::Uuid::new_v4().to_string();
    assert!(s.claim("a", true, &instance).is_ok());
    s.save_device(1, None).unwrap();
    assert!(s.delete_image("a").is_err()); // lease remains authoritative
}

#[test]
fn image_fork_intent_restarts_and_import_export_preserves_layers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime().block_on(s.create("source", geo())).unwrap();
    write(&s, "source", &[3, 4, 5]);
    let checkpoint = runtime()
        .block_on(s.prepare_image_fork("source", "branch", false))
        .unwrap();
    assert!(runtime().block_on(s.create("branch", geo())).is_err());
    assert!(s.delete_snapshot(&checkpoint).is_err());
    drop(s);
    let mut s = Store::open(&root).unwrap();
    let branch = runtime()
        .block_on(s.finish_image_fork("branch"))
        .unwrap()
        .unwrap();
    assert_eq!(read(&s, "branch"), [3, 4, 5]);
    assert_eq!(
        runtime()
            .block_on(s.prepare_image_fork("source", "branch", false))
            .unwrap(),
        checkpoint
    );
    assert_eq!(
        runtime()
            .block_on(s.finish_image_fork("branch"))
            .unwrap()
            .unwrap()
            .storage_id,
        branch.storage_id
    );
    let export = temp.path().join("export");
    runtime()
        .block_on(s.export_snapshot(&checkpoint, &export, "none"))
        .unwrap();
    runtime()
        .block_on(s.import_image("imported", &export.join("image.json")))
        .unwrap();
    assert_eq!(read(&s, "imported"), [3, 4, 5]);
    write(&s, "imported", &[9, 8, 7]);
    assert_eq!(read(&s, "branch"), [3, 4, 5]);
    assert!(runtime()
        .block_on(s.import_image("bad", &s.resource("source").unwrap().join("image.json")))
        .is_err());
    let pending = runtime()
        .block_on(s.prepare_image_fork("source", "cancelled", false))
        .unwrap();
    s.cancel_image_fork("cancelled").unwrap();
    s.delete_snapshot(&pending).unwrap();
    runtime().block_on(s.create("cancelled", geo())).unwrap();
}

#[test]
fn read_only_device_rejects_writable_claim_and_persists_access_descriptor() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime().block_on(s.create("a", geo())).unwrap();
    s.reserve_device(1, "a", serde_json::json!({"image":"a","read_only":true}))
        .unwrap();
    assert!(s
        .claim("a", true, &uuid::Uuid::new_v4().to_string())
        .is_err());
    let access: serde_json::Value = serde_json::from_slice(
        &std::fs::read(s.resource("a").unwrap().join("device-access.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(access["read_only"], true);
    drop(s);
    let mut s = Store::open(&root).unwrap();
    assert!(s
        .claim("a", true, &uuid::Uuid::new_v4().to_string())
        .is_err());
    assert!(s
        .claim("a", false, &uuid::Uuid::new_v4().to_string())
        .is_ok());
}

#[test]
fn zfile_archive_roundtrip_mixed_layers_and_reexport() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = Store::open(&temp.path().join("store")).unwrap();
    runtime().block_on(s.create("source", geo())).unwrap();
    let mut expected = vec![0x5a; 512 * 1024];
    let engine = Engine::local(&s.resource("source").unwrap(), true).unwrap();
    engine.write(0, &expected).unwrap();
    engine.close().unwrap();
    runtime()
        .block_on(s.snapshot("source", "base", false))
        .unwrap();
    expected[65530..65550].fill(0);
    let engine = Engine::local(&s.resource("source").unwrap(), true).unwrap();
    engine.write(65530, &[0; 20]).unwrap();
    engine.close().unwrap();
    runtime()
        .block_on(s.snapshot("source", "updated", false))
        .unwrap();
    for codec in ["lz4", "zstd"] {
        let export = temp.path().join(codec);
        runtime()
            .block_on(s.export_snapshot("updated", &export, codec))
            .unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(export.join("image.json")).unwrap()).unwrap();
        let bytes: u64 = config["lowers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                std::fs::metadata(export.join(v["file"].as_str().unwrap()))
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(bytes < expected.len() as u64 / 4);
        runtime()
            .block_on(s.import_image(codec, &export.join("image.json")))
            .unwrap();
        let engine = Engine::local(&s.resource(codec).unwrap(), true).unwrap();
        let mut actual = vec![0; expected.len()];
        engine.read(0, &mut actual).unwrap();
        assert_eq!(actual, expected);
        engine.write(511, &[1, 2, 3]).unwrap();
        engine.close().unwrap();
        let snap = format!("{codec}-mixed");
        runtime().block_on(s.snapshot(codec, &snap, false)).unwrap();
        let reexport = temp.path().join(&snap);
        runtime()
            .block_on(s.export_snapshot(&snap, &reexport, codec))
            .unwrap();
        let restored = format!("{codec}-restored");
        runtime()
            .block_on(s.import_image(&restored, &reexport.join("image.json")))
            .unwrap();
        assert_eq!(read(&s, &restored), [1, 2, 3]);
        assert_eq!(read(&s, "source"), [0x5a; 3]);
        // Reject a truncated compressed layer before publishing an image.
        let layer = export.join(config["lowers"][0]["file"].as_str().unwrap());
        std::fs::OpenOptions::new()
            .write(true)
            .open(layer)
            .unwrap()
            .set_len(600)
            .unwrap();
        let bad = format!("{codec}-bad");
        assert!(runtime()
            .block_on(s.import_image(&bad, &export.join("image.json")))
            .is_err());
        assert!(s.image(&bad).is_err());
    }
}

#[test]
fn compact_preserves_visible_bytes_and_usage_tracks_real_owners() {
    let temp = tempfile::tempdir().unwrap();
    let mut s = Store::open(&temp.path().join("store")).unwrap();
    runtime().block_on(s.create("source", geo())).unwrap();
    let mut expected = vec![0x33; 512 * 1024];
    let e = Engine::local(&s.resource("source").unwrap(), true).unwrap();
    e.write(0, &expected).unwrap();
    e.close().unwrap();
    runtime()
        .block_on(s.snapshot("source", "base", false))
        .unwrap();
    expected.fill(0x77);
    expected[511..66000].fill(0);
    let e = Engine::local(&s.resource("source").unwrap(), true).unwrap();
    e.write(0, &expected).unwrap();
    e.close().unwrap();
    let original = runtime()
        .block_on(s.snapshot("source", "top", false))
        .unwrap();
    let before = s.space_usage("snapshot", "top").unwrap();
    assert_eq!(before["layer_count"], 2);
    assert_eq!(before["exclusive_lower_file_bytes"], 0);
    let e = Engine::local(&s.resource("source").unwrap(), true).unwrap();
    assert!(
        s.space_usage("image", "source").unwrap()["upper_allocated_bytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    // A mounted writer does not prevent compaction of a published immutable snapshot.
    let compact = runtime()
        .block_on(s.compact_snapshot("top", "flat"))
        .unwrap();
    assert_eq!(compact.layers.len(), 1);
    e.write(511, &[9, 8, 7]).unwrap();
    e.close().unwrap();
    assert!(runtime()
        .block_on(s.compact_snapshot("top", "flat"))
        .is_err());
    let flat_usage = s.space_usage("snapshot", "flat").unwrap();
    assert_eq!(flat_usage["shared_lower_file_bytes"], 0);
    assert!(
        flat_usage["lower_file_bytes"].as_u64().unwrap()
            < before["lower_file_bytes"].as_u64().unwrap()
    );
    runtime().block_on(s.fork("flat", "branch")).unwrap();
    let e = Engine::local(&s.resource("branch").unwrap(), false).unwrap();
    let mut actual = vec![0; expected.len()];
    e.read(0, &mut actual).unwrap();
    e.close().unwrap();
    assert_eq!(actual, expected);
    let shared = s.space_usage("snapshot", "flat").unwrap();
    assert_eq!(
        shared["shared_lower_file_bytes"],
        shared["lower_file_bytes"]
    );
    let metadata =
        std::fs::metadata(temp.path().join("store/layers").join(&compact.layers[0])).unwrap();
    assert_eq!(shared["lower_allocated_bytes"], metadata.blocks() * 512);
    s.delete_image("branch").unwrap();
    assert_eq!(
        s.space_usage("snapshot", "flat").unwrap()["shared_lower_file_bytes"],
        0
    );
    assert_eq!(
        s.snapshots().iter().find(|v| v.id == "top").unwrap().layers,
        original.layers
    );
    // Merge a stack with compressed lowers and a fresh raw upper snapshot.
    let export = temp.path().join("export");
    runtime()
        .block_on(s.export_snapshot("top", &export, "zstd"))
        .unwrap();
    runtime()
        .block_on(s.import_image("compressed", &export.join("image.json")))
        .unwrap();
    write(&s, "compressed", &[4, 5, 6]);
    expected[511..514].copy_from_slice(&[4, 5, 6]);
    runtime()
        .block_on(s.snapshot("compressed", "mixed", false))
        .unwrap();
    runtime()
        .block_on(s.compact_snapshot("mixed", "mixed-flat"))
        .unwrap();
    runtime()
        .block_on(s.fork("mixed-flat", "mixed-branch"))
        .unwrap();
    let e = Engine::local(&s.resource("mixed-branch").unwrap(), false).unwrap();
    e.read(0, &mut actual).unwrap();
    e.close().unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn compressed_upper_policy_snapshot_exchange_and_switch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime()
        .block_on(s.create_compressed("a", geo(), "lz4"))
        .unwrap();
    write(&s, "a", &[1, 2, 3]);
    runtime()
        .block_on(s.snapshot("a", "from-a", false))
        .unwrap();
    let a = s.image("a").unwrap();
    assert_eq!(
        obd_core_adapter::upper_compression(&s.directory(&a).join("uppers").join(&a.upper))
            .unwrap(),
        "lz4"
    );
    runtime().block_on(s.fork("from-a", "b")).unwrap();
    write(&s, "b", &[4, 5, 6]);
    runtime()
        .block_on(s.snapshot("b", "from-b", false))
        .unwrap();
    runtime().block_on(s.fork("from-b", "returned")).unwrap();
    runtime()
        .block_on(s.set_compression("returned", "zstd"))
        .unwrap();
    assert_eq!(read(&s, "returned"), [4, 5, 6]);
    write(&s, "returned", &[7, 8, 9]);
    runtime()
        .block_on(s.snapshot("returned", "zstd-result", false))
        .unwrap();
    let export = temp.path().join("export");
    runtime()
        .block_on(s.export_snapshot("zstd-result", &export, "none"))
        .unwrap();
    assert_eq!(read(&s, "a"), [1, 2, 3]);
    assert_eq!(read(&s, "b"), [4, 5, 6]);
    runtime().block_on(s.compact_image("returned")).unwrap();
    let image = s.image("returned").unwrap();
    assert_eq!(
        &std::fs::read(root.join("layers").join(&image.layers[0])).unwrap()[..8],
        b"ZFile\0\x01\0"
    );
    drop(s);
    let mut s = Store::open(&root).unwrap();
    s.gc().unwrap();
    assert_eq!(read(&s, "returned"), [7, 8, 9]);
    runtime()
        .block_on(s.set_compression("returned", "none"))
        .unwrap();
    write(&s, "returned", &[10, 11, 12]);
    assert_eq!(read(&s, "returned"), [10, 11, 12]);
    runtime().block_on(s.compact_image("returned")).unwrap();
    s.gc().unwrap();
    assert_eq!(s.image("returned").unwrap().layers.len(), 1);
    assert_eq!(read(&s, "returned"), [10, 11, 12]);
    assert_eq!(read(&s, "a"), [1, 2, 3]);
}

#[test]
#[cfg(all(target_os = "macos", feature = "experimental-vdo"))]
fn vdo_upper_store_snapshot_fork_exchange_and_accounting() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime().block_on(s.create_compressed("a", geo(), "vdo-lz4")).unwrap();
    write(&s, "a", &[1,2,3]);
    runtime().block_on(s.snapshot("a", "vdo-snapshot", false)).unwrap();
    runtime().block_on(s.fork("vdo-snapshot", "raw-b")).unwrap();
    write(&s, "raw-b", &[4,5,6]);
    assert_eq!(read(&s,"a"),[1,2,3]);
    runtime().block_on(s.snapshot("raw-b", "raw-result", false)).unwrap();
    runtime().block_on(s.fork("raw-result", "vdo-returned")).unwrap();
    runtime().block_on(s.set_compression("vdo-returned", "vdo-lz4")).unwrap();
    assert_eq!(read(&s,"vdo-returned"),[4,5,6]);
    write(&s,"vdo-returned",&[7,8,9]);
    let image=s.image("vdo-returned").unwrap();
    let backing=s.directory(&image).join("uppers").join(&image.upper).join("upper.vdo");
    let stats=runtime().block_on(s.stat("vdo-returned")).unwrap();
    // The store must count VDO's backing, not only its 4 KiB marker.
    let metadata=std::fs::metadata(backing).unwrap();
    assert!(stats.upper_file_bytes >= metadata.len() + 8192);
    assert!(stats.upper_allocated_bytes >= metadata.blocks() * 512);
    runtime().block_on(s.compact_image("vdo-returned")).unwrap();
    drop(s);
    let mut s=Store::open(&root).unwrap();
    s.gc().unwrap();
    assert_eq!(read(&s,"vdo-returned"),[7,8,9]);
    assert_eq!(read(&s,"a"),[1,2,3]);
}

#[test]
fn write_policy_metadata_tracks_legacy_migration_and_mixed_packed_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("store");
    let mut s = Store::open(&root).unwrap();
    runtime().block_on(s.create("existing", geo())).unwrap();
    write(&s, "existing", &[1, 2, 3]);
    let raw = s.image("existing").unwrap();
    assert_eq!(
        s.space_usage("image", "existing").unwrap()["write_policy"],
        serde_json::json!({"backend":"legacy", "compression":"none", "dedupe":false})
    );
    runtime()
        .block_on(s.set_compression("existing", "packed-lz4"))
        .unwrap();
    let packed = s.image("existing").unwrap();
    assert_ne!(raw.upper, packed.upper);
    assert_eq!(read(&s, "existing"), [1, 2, 3]);
    assert_eq!(
        s.space_usage("image", "existing").unwrap()["write_policy"],
        serde_json::json!({"backend":"packed", "compression":"lz4", "dedupe":false})
    );
    runtime()
        .block_on(s.snapshot("existing", "before-new-writes", false))
        .unwrap();
    assert!(s.space_usage("snapshot", "before-new-writes").unwrap()["write_policy"].is_null());
    write(&s, "existing", &[4, 5, 6]);
    let upper = s.image("existing").unwrap().upper;
    runtime()
        .block_on(s.set_compression("existing", "packed-none"))
        .unwrap();
    assert_eq!(s.image("existing").unwrap().upper, upper);
    assert_eq!(read(&s, "existing"), [4, 5, 6]);
    assert_eq!(
        s.space_usage("image", "existing").unwrap()["write_policy"]["compression"],
        "none"
    );
    write(&s, "existing", &[7, 8, 9]);
    runtime()
        .block_on(s.set_compression("existing", "packed-zstd-dedupe"))
        .unwrap();
    assert_eq!(read(&s, "existing"), [7, 8, 9]);
    drop(s);
    let mut s = Store::open(&root).unwrap();
    assert_eq!(
        s.space_usage("image", "existing").unwrap()["write_policy"],
        serde_json::json!({"backend":"packed", "compression":"zstd", "dedupe":true})
    );
    runtime()
        .block_on(s.fork("before-new-writes", "independent"))
        .unwrap();
    assert_eq!(read(&s, "independent"), [1, 2, 3]);
    assert_eq!(read(&s, "existing"), [7, 8, 9]);
}

#[test]
fn packed_policy_is_local_and_snapshot_layers_remain_standard() {
 let t=tempfile::tempdir().unwrap();let mut s=Store::open(&t.path().join("store")).unwrap();
 runtime().block_on(s.create_compressed("packed",geo(),"packed-lz4-dedupe")).unwrap();
 write(&s,"packed",&[8,9,10]);let before=s.image("packed").unwrap();
 runtime().block_on(s.set_compression("packed","none")).unwrap();
 let after=s.image("packed").unwrap();assert_eq!(before.upper,after.upper);assert_eq!(before.layers,after.layers);
 assert_eq!(obd_core_adapter::upper_compression(&s.directory(&after).join("uppers").join(&after.upper)).unwrap(),"packed-none-dedupe");
 assert_eq!(read(&s,"packed"),[8,9,10]);write(&s,"packed",&[11,12,13]);
 runtime().block_on(s.stat("packed")).unwrap();
 runtime().block_on(s.snapshot("packed","mixed",false)).unwrap();
 runtime().block_on(s.fork("mixed","receiver")).unwrap();assert_eq!(read(&s,"receiver"),[11,12,13]);
 runtime().block_on(s.set_compression("packed","packed-zstd")).unwrap();assert_eq!(read(&s,"packed"),[11,12,13]);
}

#[test]
fn packed_compaction_preserves_policy_snapshots_and_reopen() {
    for codec in [
        "packed-none",
        "packed-lz4",
        "packed-zstd",
        "packed-none-dedupe",
        "packed-lz4-dedupe",
        "packed-zstd-dedupe",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("store");
        let mut s = Store::open(&root).unwrap();
        runtime()
            .block_on(s.create_compressed("a", geo(), codec))
            .unwrap();
        write(&s, "a", &[1, 2, 3]);
        runtime()
            .block_on(s.snapshot("a", "before", false))
            .unwrap();
        write(&s, "a", &[4, 0, 6]);
        runtime().block_on(s.compact_image("a")).unwrap();
        let image = s.image("a").unwrap();
        assert_eq!(image.layers.len(), 1);
        assert_eq!(
            obd_core_adapter::upper_compression(
                &s.directory(&image).join("uppers").join(&image.upper)
            )
            .unwrap(),
            codec
        );
        let bytes = std::fs::read(root.join("layers").join(&image.layers[0])).unwrap();
        assert_eq!(
            bytes.starts_with(b"ZFile\0\x01\0"),
            !codec.starts_with("packed-none")
        );
        drop(s);
        let mut s = Store::open(&root).unwrap();
        s.gc().unwrap();
        assert_eq!(read(&s, "a"), [4, 0, 6]);
        runtime().block_on(s.fork("before", "old")).unwrap();
        assert_eq!(read(&s, "old"), [1, 2, 3]);
        write(&s, "a", &[7, 8, 9]);
        assert_eq!(read(&s, "a"), [7, 8, 9]);
        assert_eq!(read(&s, "old"), [1, 2, 3]);
    }
}
