//! Mount via Disk Arbitration's completion callback, without waiting for
//! StorageKit's post-mount space/metadata refresh performed by diskutil.
use anyhow::Result;
use std::path::PathBuf;

pub async fn volumes(device: String) -> Result<Vec<serde_json::Value>> {
    #[cfg(target_os = "macos")]
    return tokio::task::spawn_blocking(move || native::volumes(&device)).await?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device;
        anyhow::bail!("ENOTSUP: macOS Disk Arbitration discovery")
    }
}

pub async fn mount(device: String, point: PathBuf, read_only: bool, nobrowse: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    return tokio::task::spawn_blocking(move || {
        native::mount(&device, &point, read_only, nobrowse)
    })
    .await?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (device, point, read_only, nobrowse);
        anyhow::bail!("ENOTSUP: macOS Disk Arbitration mount")
    }
}

#[cfg(target_os = "macos")]
mod native {
    use anyhow::{ensure, Context, Result};
    use serde_json::{json, Value};
    use std::{
        cell::Cell,
        ffi::{c_char, c_void, CString},
        os::unix::ffi::OsStrExt,
        path::Path,
        ptr::{null, NonNull},
        time::{Duration, Instant},
    };

    type Ref = *const c_void;
    type Callback = extern "C" fn(Ref, Ref, *mut c_void);

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(object: Ref);
        fn CFStringCreateWithCString(allocator: Ref, text: *const c_char, encoding: u32) -> Ref;
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: Ref,
            bytes: *const u8,
            length: isize,
            directory: u8,
        ) -> Ref;
        fn CFRunLoopGetCurrent() -> Ref;
        fn CFRunLoopRunInMode(mode: Ref, seconds: f64, return_after_source: u8) -> i32;
        fn CFGetTypeID(value: Ref) -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFStringGetLength(value: Ref) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(value: Ref, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFDictionaryGetValue(dictionary: Ref, key: Ref) -> Ref;
        fn CFBooleanGetValue(value: Ref) -> u8;
        fn CFUUIDCreateString(allocator: Ref, value: Ref) -> Ref;
        fn CFURLCopyFileSystemPath(url: Ref, style: isize) -> Ref;
    }
    #[link(name = "DiskArbitration", kind = "framework")]
    extern "C" {
        fn DASessionCreate(allocator: Ref) -> Ref;
        fn DASessionScheduleWithRunLoop(session: Ref, run_loop: Ref, mode: Ref);
        fn DASessionUnscheduleFromRunLoop(session: Ref, run_loop: Ref, mode: Ref);
        fn DADiskCreateFromBSDName(allocator: Ref, session: Ref, name: *const c_char) -> Ref;
        fn DADiskMountWithArguments(
            disk: Ref,
            path: Ref,
            options: u32,
            callback: Callback,
            context: *mut c_void,
            arguments: *const Ref,
        );
        fn DADissenterGetStatus(dissenter: Ref) -> i32;
        fn DADiskCopyDescription(disk: Ref) -> Ref;
        fn DADiskCopyIOMedia(disk: Ref) -> u32;
        fn DADiskCreateFromIOMedia(allocator: Ref, session: Ref, media: u32) -> Ref;
        static kDADiskDescriptionVolumeKindKey: Ref;
        static kDADiskDescriptionVolumeNameKey: Ref;
        static kDADiskDescriptionVolumePathKey: Ref;
        static kDADiskDescriptionVolumeUUIDKey: Ref;
        static kDADiskDescriptionMediaBSDNameKey: Ref;
        static kDADiskDescriptionMediaContentKey: Ref;
        static kDADiskDescriptionMediaWholeKey: Ref;
        static kDADiskDescriptionMediaWritableKey: Ref;
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOObjectRelease(object: u32) -> i32;
        fn IOObjectConformsTo(object: u32, class: *const c_char) -> u8;
        fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
        fn IORegistryEntryCreateIterator(
            entry: u32,
            plane: *const c_char,
            options: u32,
            iterator: *mut u32,
        ) -> i32;
        fn IOIteratorNext(iterator: u32) -> u32;
        fn IOIteratorIsValid(iterator: u32) -> u8;
    }

    struct Io(u32);
    impl Io {
        fn new(value: u32) -> Result<Self> {
            ensure!(value != 0, "disk media disappeared during discovery");
            Ok(Self(value))
        }
        fn id(&self) -> Result<u64> {
            let mut id = 0;
            ensure!(
                unsafe { IORegistryEntryGetRegistryEntryID(self.0, &mut id) } == 0,
                "cannot identify disk media"
            );
            Ok(id)
        }
        fn is_media(&self) -> bool {
            unsafe { IOObjectConformsTo(self.0, c"IOMedia".as_ptr()) != 0 }
        }
        fn relatives(&self, parents: bool) -> Result<Vec<Self>> {
            let mut iterator = 0;
            // kIORegistryIterateRecursively | optional kIORegistryIterateParents.
            ensure!(
                unsafe {
                    IORegistryEntryCreateIterator(
                        self.0,
                        c"IOService".as_ptr(),
                        if parents { 3 } else { 1 },
                        &mut iterator,
                    )
                } == 0,
                "cannot enumerate disk media"
            );
            let iterator = Self::new(iterator)?;
            let mut entries = Vec::new();
            loop {
                let entry = unsafe { IOIteratorNext(iterator.0) };
                if entry == 0 {
                    break;
                }
                entries.push(Self(entry));
            }
            ensure!(
                unsafe { IOIteratorIsValid(iterator.0) } != 0,
                "disk topology changed during discovery; retry"
            );
            Ok(entries)
        }
    }
    impl Drop for Io {
        fn drop(&mut self) {
            unsafe { IOObjectRelease(self.0) };
        }
    }

    fn string(value: Ref) -> Result<Option<String>> {
        if value.is_null() {
            return Ok(None);
        }
        ensure!(
            unsafe { CFGetTypeID(value) == CFStringGetTypeID() },
            "invalid disk description string"
        );
        let size =
            unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(value), 0x08000100) };
        ensure!(size >= 0, "invalid disk description length");
        let mut bytes = vec![0u8; size as usize + 1];
        ensure!(
            unsafe {
                CFStringGetCString(
                    value,
                    bytes.as_mut_ptr().cast(),
                    bytes.len() as isize,
                    0x08000100,
                )
            } != 0,
            "invalid disk description encoding"
        );
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .context("disk description terminator")?;
        Ok(Some(String::from_utf8(bytes[..end].to_vec())?))
    }

    fn description_volume(description: Ref) -> Result<Option<Value>> {
        // Values and their types are defined by Disk Arbitration's public SDK.
        let get = |key| unsafe { CFDictionaryGetValue(description, key) };
        let Some(filesystem) = string(get(unsafe { kDADiskDescriptionVolumeKindKey }))? else {
            return Ok(None);
        };
        let id = string(get(unsafe { kDADiskDescriptionMediaBSDNameKey }))?
            .context("volume BSD name")?;
        let uuid = get(unsafe { kDADiskDescriptionVolumeUUIDKey });
        let uuid = if uuid.is_null() {
            None
        } else {
            string(Owned::new(unsafe { CFUUIDCreateString(null(), uuid) })?.get())?
        };
        let path = get(unsafe { kDADiskDescriptionVolumePathKey });
        let mountpoint = if path.is_null() {
            None
        } else {
            string(Owned::new(unsafe { CFURLCopyFileSystemPath(path, 0) })?.get())?
        };
        let mut content = string(get(unsafe { kDADiskDescriptionMediaContentKey }))?;
        if content.as_deref() == Some("C12A7328-F81F-11D2-BA4B-00A0C93EC93B") {
            content = Some("EFI".to_owned());
        }
        let writable = get(unsafe { kDADiskDescriptionMediaWritableKey });
        let mut read_only = if writable.is_null() {
            None
        } else {
            Some(unsafe { CFBooleanGetValue(writable) } == 0)
        };
        if let Some(point) = &mountpoint {
            let path = CString::new(point.as_bytes())?;
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            ensure!(
                unsafe { libc::statfs(path.as_ptr(), &mut stat) } == 0,
                "mounted volume disappeared"
            );
            let source =
                unsafe { std::ffi::CStr::from_ptr(stat.f_mntfromname.as_ptr()) }.to_str()?;
            ensure!(
                source == format!("/dev/{id}"),
                "volume mountpoint identity changed"
            );
            read_only = Some(stat.f_flags & libc::MNT_RDONLY as u32 != 0);
        }
        Ok(Some(
            json!({"device_path":format!("/dev/{id}"), "uuid":uuid,
            "name":string(get(unsafe { kDADiskDescriptionVolumeNameKey }))?,
            "filesystem":filesystem,"partition_content":content,"mountpoint":mountpoint,"read_only":read_only}),
        ))
    }

    pub fn volumes(device: &str) -> Result<Vec<Value>> {
        let name = CString::new(device)?;
        let session = Owned::new(unsafe { DASessionCreate(null()) })?;
        let disk =
            Owned::new(unsafe { DADiskCreateFromBSDName(null(), session.get(), name.as_ptr()) })?;
        let description = Owned::new(unsafe { DADiskCopyDescription(disk.get()) })?;
        let whole =
            unsafe { CFDictionaryGetValue(description.get(), kDADiskDescriptionMediaWholeKey) };
        ensure!(
            !whole.is_null() && unsafe { CFBooleanGetValue(whole) } != 0,
            "volume discovery requires a whole disk"
        );
        let root = Io::new(unsafe { DADiskCopyIOMedia(disk.get()) })?;
        let root_id = root.id()?;
        let mut entries = root.relatives(false)?;
        entries.push(root);
        let mut volumes = Vec::new();
        for entry in entries.iter().filter(|e| e.is_media()) {
            let disk =
                Owned::new(unsafe { DADiskCreateFromIOMedia(null(), session.get(), entry.0) })?;
            let description = Owned::new(unsafe { DADiskCopyDescription(disk.get()) })?;
            if let Some(volume) = description_volume(description.get())? {
                // A shared/multi-store APFS container can appear beneath more
                // than one physical disk. Every IOMedia ancestor must belong
                // below this exact RAW root, not just one of its parent paths.
                for parent in entry.relatives(true)?.iter().filter(|e| e.is_media()) {
                    if parent.id()? != root_id {
                        ensure!(
                            parent
                                .relatives(true)?
                                .iter()
                                .any(|p| p.id().ok() == Some(root_id)),
                            "volume spans storage outside the managed RAW attachment"
                        );
                    }
                }
                volumes.push(volume);
            }
        }
        // Check the BSD name still resolves to the same registry object.
        let current =
            Owned::new(unsafe { DADiskCreateFromBSDName(null(), session.get(), name.as_ptr()) })?;
        ensure!(
            Io::new(unsafe { DADiskCopyIOMedia(current.get()) })?.id()? == root_id,
            "disk identity changed during discovery"
        );
        volumes.sort_by(|a, b| a["device_path"].as_str().cmp(&b["device_path"].as_str()));
        volumes.dedup_by(|a, b| a["device_path"] == b["device_path"]);
        Ok(volumes)
    }

    struct Owned(NonNull<c_void>);
    impl Owned {
        fn new(value: Ref) -> Result<Self> {
            NonNull::new(value.cast_mut())
                .map(Self)
                .context("Disk Arbitration allocation failed")
        }
        fn get(&self) -> Ref {
            self.0.as_ptr()
        }
        fn string(text: &str) -> Result<Self> {
            let text = CString::new(text)?;
            // kCFStringEncodingUTF8, from the platform SDK.
            Self::new(unsafe { CFStringCreateWithCString(null(), text.as_ptr(), 0x08000100) })
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { CFRelease(self.get()) };
        }
    }

    struct Scheduled<'a> {
        session: &'a Owned,
        run_loop: Ref,
        mode: &'a Owned,
    }
    impl Drop for Scheduled<'_> {
        fn drop(&mut self) {
            unsafe {
                DASessionUnscheduleFromRunLoop(self.session.get(), self.run_loop, self.mode.get())
            };
        }
    }

    extern "C" fn completed(_: Ref, dissenter: Ref, context: *mut c_void) {
        // The session delivers callbacks only on this thread's private run-loop
        // mode. State lives until after unscheduling and releasing all DA refs.
        let state = unsafe { &*context.cast::<Cell<Option<i32>>>() };
        let status = if dissenter.is_null() {
            0
        } else {
            unsafe { DADissenterGetStatus(dissenter) }
        };
        state.set(Some(status));
    }

    pub fn mount(device: &str, point: &Path, read_only: bool, nobrowse: bool) -> Result<()> {
        let device = CString::new(device)?;
        let bytes = point.as_os_str().as_bytes();
        // Declared first so it outlives the disk, session, and run-loop source,
        // including on timeout. No callback uses it on a different thread.
        let state = Cell::new(None);
        let mode = Owned::string(&format!("OverlayBDMount-{}", uuid::Uuid::new_v4()))?;
        let session = Owned::new(unsafe { DASessionCreate(null()) })?;
        let disk =
            Owned::new(unsafe { DADiskCreateFromBSDName(null(), session.get(), device.as_ptr()) })?;
        let url = Owned::new(unsafe {
            CFURLCreateFromFileSystemRepresentation(null(), bytes.as_ptr(), bytes.len() as isize, 1)
        })?;
        let mut options = Vec::new();
        if read_only {
            options.push(Owned::string("rdonly")?);
        }
        if nobrowse {
            options.push(Owned::string("nobrowse")?);
        }
        let mut arguments: Vec<Ref> = options.iter().map(Owned::get).collect();
        arguments.push(null());
        let run_loop = unsafe { CFRunLoopGetCurrent() };
        unsafe { DASessionScheduleWithRunLoop(session.get(), run_loop, mode.get()) };
        let _scheduled = Scheduled {
            session: &session,
            run_loop,
            mode: &mode,
        };
        unsafe {
            DADiskMountWithArguments(
                disk.get(),
                url.get(),
                0,
                completed,
                (&state as *const Cell<Option<i32>>).cast_mut().cast(),
                arguments.as_ptr(),
            );
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = state.get() {
                ensure!(
                    status == 0,
                    "Disk Arbitration mount failed (0x{:08X}); inspect device status",
                    status as u32
                );
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "Disk Arbitration mount timed out; inspect device status before retrying"
            );
            // Only this private mode may dispatch the callback. On return or
            // error, the guard removes its source before state can be dropped.
            let result = unsafe { CFRunLoopRunInMode(mode.get(), remaining.as_secs_f64(), 1) };
            ensure!(
                result != 1 || state.get().is_some(),
                "Disk Arbitration session ended before mount completed; inspect device status"
            );
        }
    }
}
