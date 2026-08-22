//! FUSE mount for RDPDR filesystem devices: client-redirected drives appear
//! under `{xdg_runtime_dir}/kmsrdp/drives/<DosName>` for the active session.
//!
//! Concurrent RDP connections share one mount per DosName (same idea as the
//! shared display). The mount is created by the first connection that
//! announces the device and released only when the last connection leaves.
//! While multiple connections are present, one owner supplies the RDPDR
//! bridge; if that connection disconnects first, ownership is handed off by
//! swapping the backend in place (no umount) so other sessions keep responding.
//!
//! Wire IRPs match FreeRDP's drive server: CREATE/CLOSE/READ/WRITE/
//! QueryDirectory plus SET_INFORMATION (rename / size / times). Deletes use
//! CREATE with `FILE_DELETE_ON_CLOSE` then CLOSE.

mod bridge;
mod fs;
mod mount;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::Errno;
use rdpcore_rdpdr::irp::{CreateReply, DirectoryEntry};
use rdpcore_rdpdr::pdu::RDPDR_DTYP_FILESYSTEM;
use rdpcore_rdpdr::{DriveCommand, DriveConsumer, DriveConsumerFactory};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::rdpdr_path::sanitize_dos_name;
use crate::session::Session;

use bridge::{Bridge, Pending};
use mount::{JoinRequest, MountRegistry};

const TTL: Duration = Duration::from_secs(1);
const OP_TIMEOUT: Duration = Duration::from_secs(5);
const ROOT_INO: u64 = 1;

#[derive(Clone)]
pub struct FuseDriveFactory {
    session_rx: watch::Receiver<Option<Session>>,
    registry: Arc<MountRegistry>,
}

impl FuseDriveFactory {
    pub fn new(session_rx: watch::Receiver<Option<Session>>) -> Self {
        Self {
            session_rx,
            registry: Arc::new(MountRegistry::new()),
        }
    }

    /// Unmount every live FUSE drive. Safe to call from signal handlers'
    /// cleanup path before `process::exit`.
    pub fn unmount_all(&self) {
        self.registry.unmount_all();
    }
}

impl DriveConsumerFactory for FuseDriveFactory {
    fn supported_device_types(&self) -> u32 {
        RDPDR_DTYP_FILESYSTEM
    }

    fn build_drive_consumer(&self, wake: UnboundedSender<()>) -> Box<dyn DriveConsumer> {
        let session = self.session_rx.borrow().clone();
        let (uid, gid, runtime, have_session) = match session {
            Some(ref s) => (s.uid, primary_gid(s.uid), s.xdg_runtime_dir.clone(), true),
            None => {
                tracing::info!(
                    "kmsrdp: rdpdr FUSE: no active session; mounts disabled for this connection"
                );
                (0, 0, PathBuf::from("/tmp"), false)
            }
        };
        Box::new(FuseDriveConsumer {
            bridge: Bridge::new(wake, uid, gid),
            runtime_dir: runtime,
            uid,
            conn_id: self.registry.alloc_conn_id(),
            registry: Arc::clone(&self.registry),
            joined: HashMap::new(),
            have_session,
        })
    }
}

struct FuseDriveConsumer {
    bridge: Arc<Bridge>,
    runtime_dir: PathBuf,
    uid: u32,
    conn_id: u64,
    registry: Arc<MountRegistry>,
    /// device_id → dos_name for shared mounts this connection joined.
    joined: HashMap<u32, String>,
    have_session: bool,
}

impl DriveConsumer for FuseDriveConsumer {
    fn on_device_ready(
        &mut self,
        device_id: u32,
        device_type: u32,
        dos_name: &str,
    ) -> Vec<DriveCommand> {
        if device_type != RDPDR_DTYP_FILESYSTEM {
            return Vec::new();
        }
        if !self.have_session {
            return Vec::new();
        }
        let name = sanitize_dos_name(dos_name);
        if name.is_empty() {
            tracing::debug!("kmsrdp: rdpdr FUSE: ignoring device {device_id} with empty DosName");
            return Vec::new();
        }
        let drives_root = self.runtime_dir.join("kmsrdp").join("drives");
        let mount_point = drives_root.join(&name);
        chown_path(&drives_root, self.uid, self.bridge.gid);

        if self.registry.join(JoinRequest {
            dos_name: name.clone(),
            conn_id: self.conn_id,
            bridge: Arc::clone(&self.bridge),
            device_id,
            mount_point,
            uid: self.uid,
            gid: self.bridge.gid,
        }) {
            self.joined.insert(device_id, name);
        }
        Vec::new()
    }

    fn on_create_reply(
        &mut self,
        request_tag: u64,
        result: Result<CreateReply, u32>,
    ) -> Vec<DriveCommand> {
        if let Some(Pending::Create(tx)) = self
            .bridge
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_tag)
        {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_close_reply(&mut self, request_tag: u64, status: u32) -> Vec<DriveCommand> {
        if let Some(Pending::Close(tx)) = self
            .bridge
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_tag)
        {
            let _ = tx.send(status);
        }
        Vec::new()
    }

    fn on_read_reply(
        &mut self,
        request_tag: u64,
        result: Result<Vec<u8>, u32>,
    ) -> Vec<DriveCommand> {
        if let Some(Pending::Read(tx)) = self
            .bridge
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_tag)
        {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_write_reply(&mut self, request_tag: u64, result: Result<u32, u32>) -> Vec<DriveCommand> {
        if let Some(Pending::Write(tx)) = self
            .bridge
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_tag)
        {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_query_directory_reply(
        &mut self,
        request_tag: u64,
        result: Result<Option<DirectoryEntry>, u32>,
    ) -> Vec<DriveCommand> {
        if let Some(Pending::QueryDir(tx)) = self
            .bridge
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_tag)
        {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_set_information_reply(
        &mut self,
        request_tag: u64,
        result: Result<(), u32>,
    ) -> Vec<DriveCommand> {
        if let Some(Pending::SetInfo(tx)) = self
            .bridge
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_tag)
        {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn poll_commands(&mut self) -> Vec<DriveCommand> {
        self.bridge.poll_commands()
    }
}

impl Drop for FuseDriveConsumer {
    fn drop(&mut self) {
        // Unblock FUSE ops waiting on this connection's bridge before leave
        // may umount (owner) or hand off.
        self.bridge.abort_pending();
        for (_device_id, dos_name) in self.joined.drain() {
            self.registry.leave(&dos_name, self.conn_id);
        }
    }
}

fn prepare_mount_point(path: &Path) -> std::io::Result<()> {
    // A previous RDP session may have left a stale FUSE mount, or a v0.1.9
    // per-connection symlink at this path. Clear those before mounting.
    if path
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(path);
    }
    if path.exists() {
        try_unmount(path);
    }
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            try_unmount(path);
            if path.is_dir() {
                return Ok(());
            }
            let _ = std::fs::remove_dir(path);
            std::fs::create_dir_all(path)
        }
        Err(e) => Err(e),
    }
}

fn try_unmount(path: &Path) {
    let path_str = path.to_string_lossy();
    // Prefer fusermount3; fall back to umount -l (lazy). Ignore failures
    // when nothing is mounted (common on reconnect).
    let ok = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(path_str.as_ref())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::process::Command::new("umount")
            .args(["-l"])
            .arg(path_str.as_ref())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

fn filetime_to_systemtime(ft: i64) -> SystemTime {
    // Windows FILETIME: 100ns since 1601-01-01.
    const EPOCH_DIFF: i64 = 116444736000000000;
    if ft <= EPOCH_DIFF {
        return UNIX_EPOCH;
    }
    let ticks = ft - EPOCH_DIFF;
    let secs = (ticks / 10_000_000) as u64;
    let nanos = ((ticks % 10_000_000) * 100) as u32;
    UNIX_EPOCH + Duration::new(secs, nanos)
}

fn systemtime_to_filetime(t: SystemTime) -> i64 {
    const EPOCH_DIFF: i64 = 116444736000000000;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            EPOCH_DIFF + (d.as_secs() as i64) * 10_000_000 + (i64::from(d.subsec_nanos()) / 100)
        }
        Err(_) => 0,
    }
}

fn time_or_now_to_filetime(t: fuser::TimeOrNow) -> i64 {
    systemtime_to_filetime(time_or_now_to_systemtime(t))
}

fn time_or_now_to_systemtime(t: fuser::TimeOrNow) -> SystemTime {
    match t {
        fuser::TimeOrNow::Now => SystemTime::now(),
        fuser::TimeOrNow::SpecificTime(st) => st,
    }
}

fn ntstatus_to_errno(status: u32) -> Errno {
    match status {
        // STATUS_NO_SUCH_FILE / OBJECT_NAME_* / OBJECT_PATH_NOT_FOUND
        0xC000_000F | 0xC000_0033 | 0xC000_0034 | 0xC000_003A => Errno::ENOENT,
        0xC000_0022 => Errno::EACCES, // STATUS_ACCESS_DENIED
        0xC000_0043 => Errno::ETXTBSY,
        0xC000_0001 => Errno::EIO,                  // STATUS_UNSUCCESSFUL
        0xC000_000D => Errno::EINVAL,               // STATUS_INVALID_PARAMETER
        0xC000_00BB | 0xC000_00A3 => Errno::ENOSYS, // NOT_SUPPORTED / NOT_IMPLEMENTED
        0xC000_0010 => Errno::EIO,
        0xC000_0035 => Errno::EEXIST,    // STATUS_OBJECT_NAME_COLLISION
        0xC000_0101 => Errno::ENOTEMPTY, // STATUS_DIRECTORY_NOT_EMPTY
        0xC000_0121 => Errno::EPERM,     // STATUS_CANNOT_DELETE
        _ => {
            tracing::debug!("kmsrdp: rdpdr FUSE: unmapped NTSTATUS {status:#010x} → EIO");
            Errno::EIO
        }
    }
}

fn primary_gid(uid: u32) -> u32 {
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let mut buf = vec![0; 1024];
    loop {
        // SAFETY: pwd, buf, and result point to valid local memory.
        let ret = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };
        if ret == libc::ERANGE && buf.len() <= 65536 {
            buf.resize(buf.len() * 2, 0);
        } else if ret == 0 && !result.is_null() {
            // SAFETY: getpwuid_r succeeded and initialized pwd.
            let pwd = unsafe { pwd.assume_init() };
            return pwd.pw_gid;
        } else {
            return uid;
        }
    }
}

fn chown_path(path: &Path, uid: u32, gid: u32) {
    let Some(s) = path.to_str() else {
        return;
    };
    let Ok(c_path) = std::ffi::CString::new(s) else {
        return;
    };
    // SAFETY: path is a valid C string we just constructed.
    // Use lchown so symlinks are never followed (preventing symlink attacks).
    unsafe {
        let _ = libc::lchown(c_path.as_ptr(), uid, gid);
    }
}

#[cfg(test)]
mod tests {
    use super::bridge::{CachedMeta, Pending};
    use super::*;
    use crate::rdpdr_path::{join_win, parent_of};
    use rdpcore_rdpdr::irp::{CreateReply, DirectoryEntry, FILE_OPENED};
    use std::sync::Arc;
    use std::sync::mpsc::RecvTimeoutError;
    use std::thread;
    use std::time::Duration;

    fn test_bridge() -> Arc<Bridge> {
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        Bridge::new(wake, 1000, 1001)
    }

    fn seed_path(bridge: &Bridge, device_id: u32, path: &str, is_dir: bool) {
        bridge.ensure_root_ino(device_id);
        let _ = bridge.inode_for(device_id, path);
        bridge
            .meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (device_id, path.to_owned()),
                CachedMeta::new(is_dir, bridge.uid, bridge.gid),
            );
    }

    fn complete_command(bridge: &Bridge, cmd: DriveCommand) {
        match cmd {
            DriveCommand::Create { request_tag, .. } => {
                if let Some(Pending::Create(tx)) = bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_tag)
                {
                    let _ = tx.send(Ok(CreateReply {
                        file_id: 42,
                        information: FILE_OPENED,
                    }));
                }
            }
            DriveCommand::Close { request_tag, .. } => {
                if let Some(Pending::Close(tx)) = bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_tag)
                {
                    let _ = tx.send(0);
                }
            }
            DriveCommand::SetInformation { request_tag, .. } => {
                if let Some(Pending::SetInfo(tx)) = bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_tag)
                {
                    let _ = tx.send(Ok(()));
                }
            }
            DriveCommand::QueryDirectory {
                request_tag, path, ..
            } => {
                if let Some(Pending::QueryDir(tx)) = bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_tag)
                {
                    let _ = tx.send(Ok(if path.is_some() {
                        Some(DirectoryEntry {
                            file_index: 0,
                            file_name: "child.txt".to_owned(),
                            creation_time: 0,
                            last_access_time: 0,
                            last_write_time: 0,
                            change_time: 0,
                            end_of_file: 0,
                            allocation_size: 0,
                            file_attributes: 0,
                        })
                    } else {
                        None
                    }));
                }
            }
            DriveCommand::Read { request_tag, .. } => {
                if let Some(Pending::Read(tx)) = bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_tag)
                {
                    let _ = tx.send(Ok(Vec::new()));
                }
            }
            DriveCommand::Write { request_tag, .. } => {
                if let Some(Pending::Write(tx)) = bridge
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&request_tag)
                {
                    let _ = tx.send(Ok(0));
                }
            }
        }
    }

    fn drain_bridge_until<F>(bridge: &Arc<Bridge>, mut done: F)
    where
        F: FnMut() -> bool,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for bridge operation"
            );
            for cmd in bridge.poll_commands() {
                complete_command(bridge, cmd);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn join_win_and_parent_of() {
        assert_eq!(join_win("\\", "foo").as_deref(), Some("\\foo"));
        assert_eq!(join_win("\\dir", "bar").as_deref(), Some("\\dir\\bar"));
        assert_eq!(parent_of("\\"), "\\");
        assert_eq!(parent_of("\\foo"), "\\");
        assert_eq!(parent_of("\\dir\\file"), "\\dir");
    }

    #[test]
    fn sanitize_dos_name_strips_and_replaces() {
        assert_eq!(sanitize_dos_name("  C  "), "C");
        assert_eq!(sanitize_dos_name(" my-drive "), "my-drive");
        assert_eq!(sanitize_dos_name("foo/bar"), "foo_bar");
    }

    #[test]
    fn pending_timeout_removes_waiter() {
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = Bridge::new(wake, 1000, 1000);
        let (tx, rx) = std::sync::mpsc::channel::<u32>();
        let tag = bridge.alloc_tag();
        bridge
            .pending
            .lock()
            .unwrap()
            .insert(tag, Pending::Close(tx));
        assert!(matches!(
            bridge.recv_pending_timeout(tag, &rx, Duration::from_millis(20)),
            Err(RecvTimeoutError::Timeout)
        ));
        assert!(!bridge.pending.lock().unwrap().contains_key(&tag));
        drop(rx);
    }

    #[test]
    fn filetime_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
        let ft = systemtime_to_filetime(t);
        assert_eq!(ft, 116444736000000000 + 1_704_067_200 * 10_000_000);
        let back = filetime_to_systemtime(ft);
        assert_eq!(
            back.duration_since(UNIX_EPOCH).unwrap().as_secs(),
            1_704_067_200
        );
        assert_eq!(filetime_to_systemtime(0), UNIX_EPOCH);
    }

    #[test]
    fn ntstatus_maps_common_drive_errors() {
        assert_eq!(
            i32::from(ntstatus_to_errno(0xC000_003A)),
            i32::from(Errno::ENOENT)
        );
        assert_eq!(
            i32::from(ntstatus_to_errno(0xC000_0022)),
            i32::from(Errno::EACCES)
        );
        assert_eq!(
            i32::from(ntstatus_to_errno(0xC000_0101)),
            i32::from(Errno::ENOTEMPTY)
        );
        assert_eq!(
            i32::from(ntstatus_to_errno(0xC000_0035)),
            i32::from(Errno::EEXIST)
        );
        assert_eq!(
            i32::from(ntstatus_to_errno(0xC000_0121)),
            i32::from(Errno::EPERM)
        );
        assert_eq!(
            i32::from(ntstatus_to_errno(0xDEAD_BEEF)),
            i32::from(Errno::EIO)
        );
    }

    #[test]
    fn bridge_forget_and_remap_paths() {
        let bridge = test_bridge();
        seed_path(&bridge, 1, "\\a", true);
        seed_path(&bridge, 1, "\\a\\b", false);
        seed_path(&bridge, 1, "\\a\\c", false);

        bridge.remap_path(1, "\\a", "\\x");
        assert!(bridge.path_exists(1, "\\x"));
        assert!(bridge.path_exists(1, "\\x\\b"));
        assert!(!bridge.path_exists(1, "\\a\\c"));

        bridge.forget_path(1, "\\x\\b");
        assert!(!bridge.path_exists(1, "\\x\\b"));
    }

    #[test]
    fn bridge_apply_local_attrs_updates_cached_view() {
        let bridge = test_bridge();
        seed_path(&bridge, 1, "\\f", false);
        bridge.apply_local_attrs(1, "\\f", Some(0o600), Some(2000), Some(2001));
        let attr = bridge.attr_for(1, "\\f").unwrap();
        assert_eq!(attr.perm, 0o600);
        assert_eq!(attr.uid, 2000);
        assert_eq!(attr.gid, 2001);
    }

    #[test]
    fn bridge_delete_path_issues_create_setinfo_close() {
        let bridge = test_bridge();
        seed_path(&bridge, 1, "\\gone.txt", false);
        let bridge2 = Arc::clone(&bridge);
        let handle = thread::spawn(move || bridge2.delete_path(1, "\\gone.txt", false));

        drain_bridge_until(&bridge, || handle.is_finished());
        handle.join().unwrap().unwrap();
        assert!(!bridge.path_exists(1, "\\gone.txt"));
    }

    #[test]
    fn bridge_rename_path_with_replace() {
        let bridge = test_bridge();
        seed_path(&bridge, 1, "\\old.txt", false);
        let bridge2 = Arc::clone(&bridge);
        let handle = thread::spawn(move || bridge2.rename_path(1, "\\old.txt", "\\new.txt", true));

        drain_bridge_until(&bridge, || handle.is_finished());
        handle.join().unwrap().unwrap();
        assert!(!bridge.path_exists(1, "\\old.txt"));
        assert!(bridge.path_exists(1, "\\new.txt"));
    }

    #[test]
    fn bridge_ensure_dir_empty_rejects_nonempty() {
        let bridge = test_bridge();
        seed_path(&bridge, 1, "\\dir", true);
        let bridge2 = Arc::clone(&bridge);
        let handle = thread::spawn(move || bridge2.ensure_dir_empty(1, "\\dir"));

        drain_bridge_until(&bridge, || handle.is_finished());
        let err = handle.join().unwrap().unwrap_err();
        assert_eq!(i32::from(err), i32::from(Errno::ENOTEMPTY));
    }

    #[test]
    fn bridge_exchange_paths_swaps_two_files() {
        let bridge = test_bridge();
        seed_path(&bridge, 1, "\\a.txt", false);
        seed_path(&bridge, 1, "\\b.txt", false);
        let ino_a = bridge.inode_for(1, "\\a.txt");
        let ino_b = bridge.inode_for(1, "\\b.txt");

        let bridge2 = Arc::clone(&bridge);
        let handle = thread::spawn(move || bridge2.exchange_paths(1, "\\a.txt", "\\b.txt"));

        drain_bridge_until(&bridge, || handle.is_finished());
        handle.join().unwrap().unwrap();

        assert_eq!(bridge.path_for(1, ino_a), Some("\\b.txt".to_owned()));
        assert_eq!(bridge.path_for(1, ino_b), Some("\\a.txt".to_owned()));
    }
}
