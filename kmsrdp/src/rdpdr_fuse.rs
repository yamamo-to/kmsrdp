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

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, MountOption, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request, SessionACL,
};
use rdpcore_rdpdr::irp::{
    CreateReply, DELETE, DirectoryEntry, FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFORMATION,
    FILE_CREATE, FILE_DELETE_ON_CLOSE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_INFORMATION,
    FILE_END_OF_FILE_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OVERWRITE_IF, FILE_READ_DATA, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FILE_WRITE_ATTRIBUTES, GENERIC_READ, GENERIC_WRITE, SYNCHRONIZE, basic_information_buffer,
    disposition_information_buffer, end_of_file_information_buffer, rename_information_buffer,
};
use rdpcore_rdpdr::pdu::RDPDR_DTYP_FILESYSTEM;
use rdpcore_rdpdr::{DriveCommand, DriveConsumer, DriveConsumerFactory};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::session::Session;

const TTL: Duration = Duration::from_secs(1);
const OP_TIMEOUT: Duration = Duration::from_secs(5);
const ROOT_INO: u64 = 1;

#[derive(Clone)]
struct CachedMeta {
    size: u64,
    is_dir: bool,
    mtime: SystemTime,
    atime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
    /// Local FUSE metadata only — not sent to the RDP client.
    perm: u16,
    uid: u32,
    gid: u32,
}

fn default_perm(is_dir: bool) -> u16 {
    if is_dir { 0o755 } else { 0o644 }
}

impl CachedMeta {
    fn new(is_dir: bool, uid: u32, gid: u32) -> Self {
        Self {
            size: 0,
            is_dir,
            mtime: UNIX_EPOCH,
            atime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            perm: default_perm(is_dir),
            uid,
            gid,
        }
    }

    fn fresh(is_dir: bool, uid: u32, gid: u32, mode: u32) -> Self {
        let now = SystemTime::now();
        Self {
            size: 0,
            is_dir,
            mtime: now,
            atime: now,
            ctime: now,
            crtime: now,
            perm: (mode & 0o7777) as u16,
            uid,
            gid,
        }
    }
}

struct OpenHandle {
    device_id: u32,
    file_id: u32,
}

enum Pending {
    Create(mpsc::Sender<Result<CreateReply, u32>>),
    Close(mpsc::Sender<u32>),
    Read(mpsc::Sender<Result<Vec<u8>, u32>>),
    Write(mpsc::Sender<Result<u32, u32>>),
    QueryDir(mpsc::Sender<Result<Option<DirectoryEntry>, u32>>),
    SetInfo(mpsc::Sender<Result<(), u32>>),
}

struct Bridge {
    wake: UnboundedSender<()>,
    outbound: Mutex<VecDeque<DriveCommand>>,
    pending: Mutex<HashMap<u64, Pending>>,
    next_tag: AtomicU64,
    next_fh: AtomicU64,
    next_ino: AtomicU64,
    /// `(device_id, windows_path)` → inode
    path_to_ino: Mutex<HashMap<(u32, String), u64>>,
    ino_to_path: Mutex<HashMap<(u32, u64), String>>,
    meta: Mutex<HashMap<(u32, String), CachedMeta>>,
    opens: Mutex<HashMap<u64, OpenHandle>>,
    uid: u32,
    gid: u32,
}

impl Bridge {
    fn new(wake: UnboundedSender<()>, uid: u32, gid: u32) -> Arc<Self> {
        Arc::new(Self {
            wake,
            outbound: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            next_tag: AtomicU64::new(1),
            next_fh: AtomicU64::new(1),
            next_ino: AtomicU64::new(2),
            path_to_ino: Mutex::new(HashMap::new()),
            ino_to_path: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
            opens: Mutex::new(HashMap::new()),
            uid,
            gid,
        })
    }

    fn alloc_tag(&self) -> u64 {
        self.next_tag.fetch_add(1, Ordering::Relaxed)
    }

    fn enqueue(&self, command: DriveCommand) {
        self.outbound.lock().unwrap().push_back(command);
        if self.wake.send(()).is_err() {
            tracing::warn!("kmsrdp: rdpdr FUSE: wake channel closed; RDP connection may be gone");
        }
    }

    fn poll_commands(&self) -> Vec<DriveCommand> {
        self.outbound.lock().unwrap().drain(..).collect()
    }

    /// Drop all in-flight waiters so FUSE threads unblock immediately when
    /// the RDP connection is gone (umount must not wait out [`OP_TIMEOUT`]).
    fn abort_pending(&self) {
        self.outbound.lock().unwrap().clear();
        self.pending.lock().unwrap().clear();
    }

    fn submit_create(
        &self,
        device_id: u32,
        path: String,
        desired_access: u32,
        create_disposition: u32,
        create_options: u32,
    ) -> Result<CreateReply, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap()
            .insert(tag, Pending::Create(tx));
        let path_for_log = path.clone();
        self.enqueue(DriveCommand::Create {
            device_id,
            path,
            desired_access,
            create_disposition,
            create_options,
            request_tag: tag,
        });
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(status)) => {
                tracing::warn!(
                    "kmsrdp: rdpdr FUSE: CREATE path={path_for_log:?} device={device_id} → NTSTATUS {status:#010x}"
                );
                Err(ntstatus_to_errno(status))
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    "kmsrdp: rdpdr FUSE: CREATE timed out path={path_for_log:?} device={device_id} (no IoCompletion);"
                );
                Err(Errno::ETIMEDOUT)
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!(
                    "kmsrdp: rdpdr FUSE: CREATE disconnected path={path_for_log:?} device={device_id}"
                );
                Err(Errno::EIO)
            }
        }
    }

    fn submit_close(&self, device_id: u32, file_id: u32) -> Result<(), Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending.lock().unwrap().insert(tag, Pending::Close(tx));
        self.enqueue(DriveCommand::Close {
            device_id,
            file_id,
            request_tag: tag,
        });
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(0) => Ok(()),
            Ok(status) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    fn submit_read(
        &self,
        device_id: u32,
        file_id: u32,
        length: u32,
        offset: u64,
    ) -> Result<Vec<u8>, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending.lock().unwrap().insert(tag, Pending::Read(tx));
        self.enqueue(DriveCommand::Read {
            device_id,
            file_id,
            length,
            offset,
            request_tag: tag,
        });
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    fn submit_write(
        &self,
        device_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<u32, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending.lock().unwrap().insert(tag, Pending::Write(tx));
        self.enqueue(DriveCommand::Write {
            device_id,
            file_id,
            offset,
            data,
            request_tag: tag,
        });
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    fn submit_query_dir(
        &self,
        device_id: u32,
        file_id: u32,
        path: Option<String>,
    ) -> Result<Option<DirectoryEntry>, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap()
            .insert(tag, Pending::QueryDir(tx));
        self.enqueue(DriveCommand::QueryDirectory {
            device_id,
            file_id,
            path,
            request_tag: tag,
        });
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(Ok(entry)) => Ok(entry),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    fn submit_set_information(
        &self,
        device_id: u32,
        file_id: u32,
        fs_information_class: u32,
        set_buffer: Vec<u8>,
    ) -> Result<(), Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap()
            .insert(tag, Pending::SetInfo(tx));
        self.enqueue(DriveCommand::SetInformation {
            device_id,
            file_id,
            fs_information_class,
            set_buffer,
            request_tag: tag,
        });
        match rx.recv_timeout(OP_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    /// FreeRDP-compatible delete with a disposition fallback for stricter
    /// clients: open with `DELETE`, mark delete-on-close and/or send
    /// `FileDispositionInformation`, then CLOSE (actual unlink happens then).
    fn delete_path(&self, device_id: u32, path: &str, is_dir: bool) -> Result<(), Errno> {
        let create_options = if is_dir {
            FILE_DIRECTORY_FILE | FILE_DELETE_ON_CLOSE | FILE_SYNCHRONOUS_IO_NONALERT
        } else {
            FILE_NON_DIRECTORY_FILE | FILE_DELETE_ON_CLOSE | FILE_SYNCHRONOUS_IO_NONALERT
        };
        // Windows redirectors typically require DELETE in DesiredAccess when
        // FILE_DELETE_ON_CLOSE is set; FreeRDP's own server used FILE_READ_DATA
        // for files, which fails against some clients (Guacamole / mstsc).
        let create = self.submit_create(
            device_id,
            path.to_owned(),
            DELETE | FILE_READ_DATA | SYNCHRONIZE,
            FILE_OPEN,
            create_options,
        );
        let create = match create {
            Ok(c) => c,
            Err(first) => {
                // Retry without FILE_DELETE_ON_CLOSE; disposition IRP alone.
                tracing::debug!(
                    "kmsrdp: rdpdr FUSE: delete CREATE(delete-on-close) failed for {path:?} ({first:?}); retrying with disposition"
                );
                let options = if is_dir {
                    FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT
                } else {
                    FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT
                };
                self.submit_create(
                    device_id,
                    path.to_owned(),
                    DELETE | SYNCHRONIZE,
                    FILE_OPEN,
                    options,
                )
                .inspect_err(|e| {
                    tracing::warn!(
                        "kmsrdp: rdpdr FUSE: delete CREATE failed path={path:?} device={device_id} first={first:?} retry={e:?}"
                    );
                })?
            }
        };

        // Explicit disposition so clients that ignore CreateOptions still delete.
        if let Err(e) = self.submit_set_information(
            device_id,
            create.file_id,
            FILE_DISPOSITION_INFORMATION,
            disposition_information_buffer(true),
        ) {
            tracing::debug!(
                "kmsrdp: rdpdr FUSE: FileDispositionInformation failed for {path:?} ({e:?}); relying on DELETE_ON_CLOSE if set"
            );
        }

        match self.submit_close(device_id, create.file_id) {
            Ok(()) => {
                self.forget_path(device_id, path);
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "kmsrdp: rdpdr FUSE: delete CLOSE failed path={path:?} device={device_id} ({e:?})"
                );
                Err(e)
            }
        }
    }

    fn rename_path(
        &self,
        device_id: u32,
        old_path: &str,
        new_path: &str,
        replace_if_exists: bool,
    ) -> Result<(), Errno> {
        let create = self.submit_create(
            device_id,
            old_path.to_owned(),
            FILE_READ_DATA | SYNCHRONIZE,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        let result = self.submit_set_information(
            device_id,
            create.file_id,
            FILE_RENAME_INFORMATION,
            rename_information_buffer(new_path, replace_if_exists),
        );
        let _ = self.submit_close(device_id, create.file_id);
        result?;
        self.remap_path(device_id, old_path, new_path);
        Ok(())
    }

    /// Best-effort atomic swap via a temporary name (RDPDR has no exchange IRP).
    fn exchange_paths(&self, device_id: u32, path_a: &str, path_b: &str) -> Result<(), Errno> {
        let tag = self.alloc_tag();
        let temp = format!("{}\\.__kmsrdp_xchg_{tag}", parent_of(path_a));
        self.rename_path(device_id, path_a, &temp, false)?;
        match self.rename_path(device_id, path_b, path_a, false) {
            Ok(()) => {}
            Err(e) => {
                let _ = self.rename_path(device_id, &temp, path_a, false);
                return Err(e);
            }
        }
        self.rename_path(device_id, &temp, path_b, false)
    }

    fn path_exists(&self, device_id: u32, win_path: &str) -> bool {
        self.attr_for(device_id, win_path).is_some()
    }

    /// Fail fast with `ENOTEMPTY` before sending a doomed directory delete IRP.
    fn ensure_dir_empty(&self, device_id: u32, dir_path: &str) -> Result<(), Errno> {
        let children = self.refresh_dir(device_id, dir_path)?;
        if children.is_empty() {
            Ok(())
        } else {
            tracing::debug!(
                "kmsrdp: rdpdr FUSE: rmdir {dir_path:?} refused: {} entr(y/ies)",
                children.len()
            );
            Err(Errno::ENOTEMPTY)
        }
    }

    fn apply_local_attrs(
        &self,
        device_id: u32,
        path: &str,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) {
        let mut meta = self.meta.lock().unwrap();
        let Some(meta) = meta.get_mut(&(device_id, path.to_owned())) else {
            return;
        };
        if let Some(m) = mode {
            meta.perm = (m & 0o7777) as u16;
        }
        if let Some(u) = uid {
            meta.uid = u;
        }
        if let Some(g) = gid {
            meta.gid = g;
        }
    }

    fn ensure_root_ino(&self, device_id: u32) {
        let key = (device_id, "\\".to_owned());
        let mut path_to_ino = self.path_to_ino.lock().unwrap();
        let mut ino_to_path = self.ino_to_path.lock().unwrap();
        if path_to_ino.contains_key(&key) {
            return;
        }
        path_to_ino.insert(key, ROOT_INO);
        ino_to_path.insert((device_id, ROOT_INO), "\\".to_owned());
        self.meta.lock().unwrap().insert(
            (device_id, "\\".to_owned()),
            CachedMeta::new(true, self.uid, self.gid),
        );
    }

    fn inode_for(&self, device_id: u32, win_path: &str) -> u64 {
        let key = (device_id, win_path.to_owned());
        let mut path_to_ino = self.path_to_ino.lock().unwrap();
        if let Some(ino) = path_to_ino.get(&key) {
            return *ino;
        }
        let ino = if win_path == "\\" {
            ROOT_INO
        } else {
            self.next_ino.fetch_add(1, Ordering::Relaxed)
        };
        path_to_ino.insert(key, ino);
        self.ino_to_path
            .lock()
            .unwrap()
            .insert((device_id, ino), win_path.to_owned());
        ino
    }

    fn path_for(&self, device_id: u32, ino: u64) -> Option<String> {
        self.ino_to_path
            .lock()
            .unwrap()
            .get(&(device_id, ino))
            .cloned()
    }

    fn cache_entry(&self, device_id: u32, parent: &str, entry: &DirectoryEntry) {
        let name = entry.file_name.trim_end_matches('\0');
        if name.is_empty() || name == "." || name == ".." {
            return;
        }
        let path = join_win(parent, name);
        let is_dir = entry.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let meta = CachedMeta {
            size: entry.end_of_file.max(0) as u64,
            is_dir,
            mtime: filetime_to_systemtime(entry.last_write_time),
            atime: filetime_to_systemtime(entry.last_access_time),
            ctime: filetime_to_systemtime(entry.change_time),
            crtime: filetime_to_systemtime(entry.creation_time),
            perm: default_perm(is_dir),
            uid: self.uid,
            gid: self.gid,
        };
        let _ = self.inode_for(device_id, &path);
        self.meta.lock().unwrap().insert((device_id, path), meta);
    }

    fn attr_for(&self, device_id: u32, win_path: &str) -> Option<FileAttr> {
        let meta = self
            .meta
            .lock()
            .unwrap()
            .get(&(device_id, win_path.to_owned()))?
            .clone();
        let ino = self.inode_for(device_id, win_path);
        Some(FileAttr {
            ino: INodeNo(ino),
            size: meta.size,
            blocks: meta.size.div_ceil(512),
            atime: meta.atime,
            mtime: meta.mtime,
            ctime: meta.ctime,
            crtime: meta.crtime,
            kind: if meta.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: meta.perm,
            nlink: if meta.is_dir { 2 } else { 1 },
            uid: meta.uid,
            gid: meta.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    /// Open `parent`, enumerate with `\*`, cache entries, close.
    fn refresh_dir(&self, device_id: u32, parent: &str) -> Result<Vec<String>, Errno> {
        let reply = self.submit_create(
            device_id,
            parent.to_owned(),
            GENERIC_READ,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?;
        let file_id = reply.file_id;
        let result = self.enumerate_directory(device_id, parent, file_id);
        let _ = self.submit_close(device_id, file_id);
        Ok(result?
            .into_iter()
            .map(|e| e.file_name.trim_end_matches('\0').to_owned())
            .collect())
    }

    /// QueryDirectory loop shared by [`Self::refresh_dir`] (lookup/getattr
    /// cache fill) and FUSE `readdir` (already holds an open `file_id`).
    fn enumerate_directory(
        &self,
        device_id: u32,
        parent: &str,
        file_id: u32,
    ) -> Result<Vec<DirectoryEntry>, Errno> {
        let pattern = if parent == "\\" {
            "\\*".to_owned()
        } else {
            format!("{parent}\\*")
        };
        let mut entries = Vec::new();
        let mut first = Some(pattern);
        loop {
            match self.submit_query_dir(device_id, file_id, first.take()) {
                Ok(Some(entry)) => {
                    self.cache_entry(device_id, parent, &entry);
                    let name = entry.file_name.trim_end_matches('\0');
                    if !name.is_empty() && name != "." && name != ".." {
                        entries.push(entry);
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(entries)
    }

    fn lookup_child(&self, device_id: u32, parent: &str, name: &str) -> Result<FileAttr, Errno> {
        let path = join_win(parent, name);
        if let Some(attr) = self.attr_for(device_id, &path) {
            return Ok(attr);
        }
        let _ = self.refresh_dir(device_id, parent)?;
        self.attr_for(device_id, &path).ok_or(Errno::ENOENT)
    }

    fn forget_path(&self, device_id: u32, win_path: &str) {
        self.meta
            .lock()
            .unwrap()
            .remove(&(device_id, win_path.to_owned()));
        let ino = self
            .path_to_ino
            .lock()
            .unwrap()
            .remove(&(device_id, win_path.to_owned()));
        if let Some(ino) = ino {
            self.ino_to_path.lock().unwrap().remove(&(device_id, ino));
        }
    }

    /// Remap `old_path` and any cached descendants (`old_path\\…`) to `new_path`.
    fn remap_path(&self, device_id: u32, old_path: &str, new_path: &str) {
        let prefix = if old_path == "\\" {
            "\\".to_owned()
        } else {
            format!("{old_path}\\")
        };

        let mut path_to_ino = self.path_to_ino.lock().unwrap();
        let mut ino_to_path = self.ino_to_path.lock().unwrap();
        let mut meta = self.meta.lock().unwrap();

        let mut remaps: Vec<(String, String, u64)> = Vec::new();
        for ((did, path), ino) in path_to_ino.iter() {
            if *did != device_id {
                continue;
            }
            let new = if path == old_path {
                new_path.to_owned()
            } else if path.starts_with(&prefix) {
                format!("{new_path}\\{}", &path[prefix.len()..])
            } else {
                continue;
            };
            remaps.push((path.clone(), new, *ino));
        }

        for (old, new, ino) in remaps {
            path_to_ino.remove(&(device_id, old.clone()));
            path_to_ino.insert((device_id, new.clone()), ino);
            ino_to_path.insert((device_id, ino), new.clone());
            if let Some(m) = meta.remove(&(device_id, old)) {
                meta.insert((device_id, new), m);
            }
        }
    }
}

/// Swappable RDPDR backend for a shared FUSE mount. Owner handoff updates
/// this without umounting, so disconnect cannot block other RDP sessions.
struct ActiveBackend {
    bridge: Arc<Bridge>,
    device_id: u32,
}

struct FuseFs {
    active: Arc<Mutex<ActiveBackend>>,
}

impl FuseFs {
    fn active(&self) -> (Arc<Bridge>, u32) {
        let g = self.active.lock().unwrap();
        (Arc::clone(&g.bridge), g.device_id)
    }
}

impl Filesystem for FuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let (bridge, device_id) = self.active();
        let Some(parent_path) = bridge.path_for(device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match bridge.lookup_child(device_id, &parent_path, name) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let (bridge, device_id) = self.active();
        let Some(path) = bridge.path_for(device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(attr) = bridge.attr_for(device_id, &path) {
            reply.attr(&TTL, &attr);
            return;
        }
        if path == "\\" {
            bridge.ensure_root_ino(device_id);
            if let Some(attr) = bridge.attr_for(device_id, "\\") {
                reply.attr(&TTL, &attr);
                return;
            }
        }
        // Refresh parent listing to populate cache.
        let parent = parent_of(&path);
        let _ = bridge.refresh_dir(device_id, &parent);
        match bridge.attr_for(device_id, &path) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let (bridge, device_id) = self.active();
        let Some(path) = bridge.path_for(device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match bridge.submit_create(
            device_id,
            path,
            GENERIC_READ,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        ) {
            Ok(create) => {
                let fh = bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                bridge.opens.lock().unwrap().insert(
                    fh,
                    OpenHandle {
                        device_id,
                        file_id: create.file_id,
                    },
                );
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => reply.error(e),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let (bridge, device_id) = self.active();
        let Some(path) = bridge.path_for(device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let opens = bridge.opens.lock().unwrap();
        let Some(handle) = opens.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let file_id = handle.file_id;
        drop(opens);

        // Always re-enumerate from the start into a local list; FUSE offset
        // is an opaque cursor we treat as 1-based entry index.
        let listed = match bridge.enumerate_directory(device_id, &path, file_id) {
            Ok(entries) => entries,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let mut entries = Vec::with_capacity(listed.len());
        for entry in listed {
            let name = entry.file_name.trim_end_matches('\0').to_owned();
            let child = join_win(&path, &name);
            let child_ino = bridge.inode_for(device_id, &child);
            let kind = if entry.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child_ino, kind, name));
        }

        let mut next = offset;
        if next == 0 {
            if reply.add(INodeNo(ino.0), 1, FileType::Directory, ".") {
                reply.ok();
                return;
            }
            next = 1;
        }
        if next == 1 {
            let parent_ino = if path == "\\" {
                ino.0
            } else {
                bridge.inode_for(device_id, &parent_of(&path))
            };
            if reply.add(INodeNo(parent_ino), 2, FileType::Directory, "..") {
                reply.ok();
                return;
            }
            next = 2;
        }
        let start = (next as usize).saturating_sub(2);
        for (i, (child_ino, kind, name)) in entries.into_iter().enumerate().skip(start) {
            let off = (i + 3) as u64;
            if reply.add(INodeNo(child_ino), off, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        let (bridge, _device_id) = self.active();
        if let Some(handle) = bridge.opens.lock().unwrap().remove(&fh.0) {
            let _ = bridge.submit_close(handle.device_id, handle.file_id);
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (bridge, device_id) = self.active();
        let Some(path) = bridge.path_for(device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let write = matches!(
            flags.acc_mode(),
            fuser::OpenAccMode::O_WRONLY | fuser::OpenAccMode::O_RDWR
        );
        let access = if write {
            GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE
        } else {
            GENERIC_READ | SYNCHRONIZE
        };
        let disposition = if flags.0 & libc::O_TRUNC != 0 {
            FILE_OVERWRITE_IF
        } else {
            FILE_OPEN
        };
        match bridge.submit_create(
            device_id,
            path,
            access,
            disposition,
            FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(create) => {
                let fh = bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                bridge.opens.lock().unwrap().insert(
                    fh,
                    OpenHandle {
                        device_id,
                        file_id: create.file_id,
                    },
                );
                reply.opened(FileHandle(fh), FopenFlags::empty());
            }
            Err(e) => reply.error(e),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let (bridge, _device_id) = self.active();
        let opens = bridge.opens.lock().unwrap();
        let Some(handle) = opens.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let device_id = handle.device_id;
        let file_id = handle.file_id;
        drop(opens);
        match bridge.submit_read(device_id, file_id, size, offset) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let (bridge, _device_id) = self.active();
        let opens = bridge.opens.lock().unwrap();
        let Some(handle) = opens.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let device_id = handle.device_id;
        let file_id = handle.file_id;
        drop(opens);
        match bridge.submit_write(device_id, file_id, offset, data.to_vec()) {
            Ok(n) => {
                if let Some(path) = bridge.path_for(device_id, ino.0) {
                    let mut meta = bridge.meta.lock().unwrap();
                    if let Some(m) = meta.get_mut(&(device_id, path)) {
                        let end = offset.saturating_add(n as u64);
                        if end > m.size {
                            m.size = end;
                        }
                        m.mtime = SystemTime::now();
                    }
                }
                reply.written(n);
            }
            Err(e) => reply.error(e),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let (bridge, _device_id) = self.active();
        if let Some(handle) = bridge.opens.lock().unwrap().remove(&fh.0) {
            let _ = bridge.submit_close(handle.device_id, handle.file_id);
        }
        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let (bridge, device_id) = self.active();
        let Some(parent_path) = bridge.path_for(device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_win(&parent_path, name);
        let write = flags & (libc::O_WRONLY | libc::O_RDWR) != 0;
        let access = if write {
            GENERIC_READ | GENERIC_WRITE | SYNCHRONIZE
        } else {
            GENERIC_READ | SYNCHRONIZE
        };
        match bridge.submit_create(
            device_id,
            path.clone(),
            access,
            FILE_OPEN_IF,
            FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(create) => {
                let mode = (_mode & !_umask) & 0o7777;
                bridge.meta.lock().unwrap().insert(
                    (device_id, path.clone()),
                    CachedMeta::fresh(false, bridge.uid, bridge.gid, mode),
                );
                let attr = bridge
                    .attr_for(device_id, &path)
                    .expect("meta just inserted");
                let fh = bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                bridge.opens.lock().unwrap().insert(
                    fh,
                    OpenHandle {
                        device_id,
                        file_id: create.file_id,
                    },
                );
                reply.created(
                    &TTL,
                    &attr,
                    Generation(0),
                    FileHandle(fh),
                    FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let (bridge, device_id) = self.active();
        let Some(parent_path) = bridge.path_for(device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_win(&parent_path, name);
        match bridge.submit_create(
            device_id,
            path.clone(),
            GENERIC_READ | SYNCHRONIZE,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(create) => {
                let _ = bridge.submit_close(device_id, create.file_id);
                let dir_mode = (mode & !umask) & 0o7777;
                bridge.meta.lock().unwrap().insert(
                    (device_id, path.clone()),
                    CachedMeta::fresh(true, bridge.uid, bridge.gid, dir_mode),
                );
                match bridge.attr_for(device_id, &path) {
                    Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
                    None => reply.error(Errno::EIO),
                }
            }
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (bridge, device_id) = self.active();
        let Some(parent_path) = bridge.path_for(device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_win(&parent_path, name);
        match bridge.delete_path(device_id, &path, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let (bridge, device_id) = self.active();
        let Some(parent_path) = bridge.path_for(device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_win(&parent_path, name);
        if let Err(e) = bridge.ensure_dir_empty(device_id, &path) {
            reply.error(e);
            return;
        }
        match bridge.delete_path(device_id, &path, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (bridge, device_id) = self.active();
        let Some(parent_path) = bridge.path_for(device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(new_parent_path) = bridge.path_for(device_id, newparent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let old_path = join_win(&parent_path, name);
        let new_path = join_win(&new_parent_path, newname);

        #[cfg(target_os = "linux")]
        if flags.contains(fuser::RenameFlags::RENAME_WHITEOUT) {
            reply.error(Errno::ENOTSUP);
            return;
        }

        #[cfg(target_os = "linux")]
        if flags.contains(fuser::RenameFlags::RENAME_EXCHANGE) {
            match bridge.exchange_paths(device_id, &old_path, &new_path) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
            return;
        }

        let dest_exists = bridge.path_exists(device_id, &new_path);
        #[cfg(target_os = "linux")]
        if flags.contains(fuser::RenameFlags::RENAME_NOREPLACE) && dest_exists {
            reply.error(Errno::EEXIST);
            return;
        }

        let replace = dest_exists;
        match bridge.rename_path(device_id, &old_path, &new_path, replace) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let (bridge, device_id) = self.active();
        let Some(path) = bridge.path_for(device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let need_mode = mode.is_some();
        let need_uid = uid.is_some();
        let need_gid = gid.is_some();
        let need_size = size.is_some();
        let need_times = atime.is_some()
            || mtime.is_some()
            || ctime.is_some()
            || crtime.is_some()
            || chgtime.is_some();

        if need_mode || need_uid || need_gid {
            // Local FUSE view only — RDPDR has no Unix owner/mode IRP.
            bridge.apply_local_attrs(device_id, &path, mode, uid, gid);
        }

        if !need_size && !need_times {
            match bridge.attr_for(device_id, &path) {
                Some(attr) => reply.attr(&TTL, &attr),
                None => reply.error(Errno::ENOENT),
            }
            return;
        }

        let open_fh = fh.map(|h| h.0);
        let opened = open_fh.and_then(|h| {
            bridge
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .map(|o| (o.device_id, o.file_id))
        });

        let (file_device, file_id, close_after) = if let Some((d, f)) = opened {
            (d, f, false)
        } else {
            let access = if need_size {
                GENERIC_WRITE | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE
            } else {
                FILE_WRITE_ATTRIBUTES | SYNCHRONIZE
            };
            match bridge.submit_create(
                device_id,
                path.clone(),
                access,
                FILE_OPEN,
                FILE_SYNCHRONOUS_IO_NONALERT,
            ) {
                Ok(create) => (device_id, create.file_id, true),
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
        };

        if let Some(size) = size {
            if let Err(e) = bridge.submit_set_information(
                file_device,
                file_id,
                FILE_END_OF_FILE_INFORMATION,
                end_of_file_information_buffer(size as i64),
            ) {
                if close_after {
                    let _ = bridge.submit_close(file_device, file_id);
                }
                reply.error(e);
                return;
            }
            if let Some(meta) = bridge
                .meta
                .lock()
                .unwrap()
                .get_mut(&(device_id, path.clone()))
            {
                meta.size = size;
                meta.mtime = SystemTime::now();
                meta.ctime = SystemTime::now();
            }
        }

        if need_times {
            let creation = crtime.map(systemtime_to_filetime).unwrap_or(0);
            let last_access = atime.map(time_or_now_to_filetime).unwrap_or(0);
            let last_write = mtime.map(time_or_now_to_filetime).unwrap_or(0);
            let change = chgtime.or(ctime).map(systemtime_to_filetime).unwrap_or(0);
            if let Err(e) = bridge.submit_set_information(
                file_device,
                file_id,
                FILE_BASIC_INFORMATION,
                basic_information_buffer(creation, last_access, last_write, change, 0),
            ) {
                if close_after {
                    let _ = bridge.submit_close(file_device, file_id);
                }
                reply.error(e);
                return;
            }
            if let Some(meta) = bridge
                .meta
                .lock()
                .unwrap()
                .get_mut(&(device_id, path.clone()))
            {
                if let Some(t) = atime {
                    meta.atime = time_or_now_to_systemtime(t);
                }
                if let Some(t) = mtime {
                    meta.mtime = time_or_now_to_systemtime(t);
                }
                if let Some(t) = ctime.or(chgtime) {
                    meta.ctime = t;
                }
                if let Some(t) = crtime {
                    meta.crtime = t;
                }
            }
        }

        if close_after {
            let _ = bridge.submit_close(file_device, file_id);
        }

        match bridge.attr_for(device_id, &path) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    // Writes go over RDP immediately; nothing buffered server-side to flush.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    // RDPDR has no xattr surface; answer like a filesystem without them.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::ENODATA);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOTSUP);
    }

    fn removexattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOTSUP);
    }
}

struct MountMember {
    bridge: Arc<Bridge>,
    device_id: u32,
}

struct SharedMount {
    mount_point: PathBuf,
    owner_conn: u64,
    session: BackgroundSession,
    /// Shared with [`FuseFs`]; owner handoff swaps the backend in place.
    active: Arc<Mutex<ActiveBackend>>,
    members: HashMap<u64, MountMember>,
}

/// One shared FUSE mount per DosName: refcounted across RDP connections,
/// released only when the last member leaves. Owner changes swap the
/// RDPDR backend without umounting.
struct MountRegistry {
    next_conn: AtomicU64,
    slots: Mutex<HashMap<String, SharedMount>>,
}

struct JoinRequest {
    dos_name: String,
    conn_id: u64,
    bridge: Arc<Bridge>,
    device_id: u32,
    mount_point: PathBuf,
    uid: u32,
    gid: u32,
}

impl MountRegistry {
    fn new() -> Self {
        Self {
            next_conn: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn alloc_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Join an existing shared mount, or create it if this is the first member.
    fn join(&self, req: JoinRequest) -> bool {
        let JoinRequest {
            dos_name,
            conn_id,
            bridge,
            device_id,
            mount_point,
            uid,
            gid,
        } = req;
        bridge.ensure_root_ino(device_id);
        let member = MountMember {
            bridge: Arc::clone(&bridge),
            device_id,
        };

        {
            let mut slots = self.slots.lock().unwrap();
            if let Some(slot) = slots.get_mut(&dos_name) {
                slot.members.insert(conn_id, member);
                tracing::info!(
                    "kmsrdp: rdpdr FUSE joined {} at {} ({} member(s), owner={})",
                    dos_name,
                    slot.mount_point.display(),
                    slot.members.len(),
                    slot.owner_conn
                );
                return true;
            }
        }

        if let Err(e) = prepare_mount_point(&mount_point) {
            tracing::warn!(
                "kmsrdp: rdpdr FUSE: failed to prepare {}: {e}",
                mount_point.display()
            );
            return false;
        }
        chown_path(&mount_point, uid, gid);
        if let Some(parent) = mount_point.parent() {
            chown_path(parent, uid, gid);
        }

        let (session, active) =
            match spawn_shared_mount(&dos_name, &mount_point, &bridge, device_id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "kmsrdp: rdpdr FUSE: mount failed at {}: {e} \
                     (need fuse3, and usually `user_allow_other` in /etc/fuse.conf)",
                        mount_point.display()
                    );
                    return false;
                }
            };

        let mut slots = self.slots.lock().unwrap();
        if let Some(slot) = slots.get_mut(&dos_name) {
            // Another connection won the race; discard our mount asynchronously
            // so we do not block this connection's RDP loop.
            detach_umount(session, mount_point.clone());
            slot.members.insert(conn_id, member);
            tracing::info!(
                "kmsrdp: rdpdr FUSE joined {} at {} ({} member(s), owner={})",
                dos_name,
                slot.mount_point.display(),
                slot.members.len(),
                slot.owner_conn
            );
            return true;
        }

        tracing::info!(
            "kmsrdp: rdpdr FUSE mounted {} at {} (shared)",
            dos_name,
            mount_point.display()
        );
        let mut members = HashMap::new();
        members.insert(conn_id, member);
        slots.insert(
            dos_name,
            SharedMount {
                mount_point,
                owner_conn: conn_id,
                session,
                active,
                members,
            },
        );
        true
    }

    fn leave(&self, dos_name: &str, conn_id: u64) {
        let mut slots = self.slots.lock().unwrap();
        let Some(slot) = slots.get_mut(dos_name) else {
            return;
        };
        slot.members.remove(&conn_id);
        if slot.members.is_empty() {
            let SharedMount {
                mount_point,
                session,
                ..
            } = slots.remove(dos_name).expect("slot just checked");
            drop(slots);
            tracing::info!(
                "kmsrdp: rdpdr FUSE releasing {} at {} (last connection)",
                dos_name,
                mount_point.display()
            );
            // Never block the RDP connection task / tokio worker on umount.
            detach_umount(session, mount_point);
            return;
        }

        if slot.owner_conn == conn_id {
            let new_owner = *slot.members.keys().next().expect("members non-empty");
            let member = &slot.members[&new_owner];
            member.bridge.ensure_root_ino(member.device_id);
            // Clear stale opens from the departing owner's bridge; swap the
            // live backend so FUSE keeps serving without umount/remount.
            {
                let mut active = slot.active.lock().unwrap();
                active.bridge.opens.lock().unwrap().clear();
                active.bridge.abort_pending();
                *active = ActiveBackend {
                    bridge: Arc::clone(&member.bridge),
                    device_id: member.device_id,
                };
            }
            slot.owner_conn = new_owner;
            tracing::info!(
                "kmsrdp: rdpdr FUSE owner handoff {} → {new_owner} (no umount, {} member(s))",
                dos_name,
                slot.members.len()
            );
        } else {
            tracing::info!(
                "kmsrdp: rdpdr FUSE member {conn_id} left {} ({} remaining, owner={})",
                dos_name,
                slot.members.len(),
                slot.owner_conn
            );
        }
    }
}

fn spawn_shared_mount(
    dos_name: &str,
    mount_point: &Path,
    bridge: &Arc<Bridge>,
    device_id: u32,
) -> std::io::Result<(BackgroundSession, Arc<Mutex<ActiveBackend>>)> {
    let active = Arc::new(Mutex::new(ActiveBackend {
        bridge: Arc::clone(bridge),
        device_id,
    }));
    let mut config = Config::default();
    // SessionACL::All → allow_other so the session user can use a
    // root-owned mount. File ownership comes from FileAttr uid/gid,
    // not fusermount uid=/gid= (fusermount3 rejects those options).
    config.acl = SessionACL::All;
    config.mount_options = vec![
        MountOption::FSName(format!("kmsrdp-{dos_name}")),
        MountOption::DefaultPermissions,
        MountOption::AutoUnmount,
    ];
    config.n_threads = Some(1);
    let fs = FuseFs {
        active: Arc::clone(&active),
    };
    let session = fuser::spawn_mount2(fs, mount_point, &config)?;
    Ok((session, active))
}

fn detach_umount(session: BackgroundSession, mount_point: PathBuf) {
    std::thread::Builder::new()
        .name("kmsrdp-fuse-umount".into())
        .spawn(move || {
            if let Err(e) = session.umount_and_join() {
                tracing::warn!(
                    "kmsrdp: rdpdr FUSE umount/join failed for {} ({e}); trying lazy unmount",
                    mount_point.display()
                );
                try_unmount(&mount_point);
            }
        })
        .expect("spawn fuse umount thread");
}

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
        if let Some(Pending::Create(tx)) = self.bridge.pending.lock().unwrap().remove(&request_tag)
        {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_close_reply(&mut self, request_tag: u64, status: u32) -> Vec<DriveCommand> {
        if let Some(Pending::Close(tx)) = self.bridge.pending.lock().unwrap().remove(&request_tag) {
            let _ = tx.send(status);
        }
        Vec::new()
    }

    fn on_read_reply(
        &mut self,
        request_tag: u64,
        result: Result<Vec<u8>, u32>,
    ) -> Vec<DriveCommand> {
        if let Some(Pending::Read(tx)) = self.bridge.pending.lock().unwrap().remove(&request_tag) {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_write_reply(&mut self, request_tag: u64, result: Result<u32, u32>) -> Vec<DriveCommand> {
        if let Some(Pending::Write(tx)) = self.bridge.pending.lock().unwrap().remove(&request_tag) {
            let _ = tx.send(result);
        }
        Vec::new()
    }

    fn on_query_directory_reply(
        &mut self,
        request_tag: u64,
        result: Result<Option<DirectoryEntry>, u32>,
    ) -> Vec<DriveCommand> {
        if let Some(Pending::QueryDir(tx)) =
            self.bridge.pending.lock().unwrap().remove(&request_tag)
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
        if let Some(Pending::SetInfo(tx)) = self.bridge.pending.lock().unwrap().remove(&request_tag)
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

fn sanitize_dos_name(raw: &str) -> String {
    let trimmed = raw.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn join_win(parent: &str, name: &str) -> String {
    if parent == "\\" {
        format!("\\{name}")
    } else {
        format!("{parent}\\{name}")
    }
}

fn parent_of(path: &str) -> String {
    if path == "\\" {
        return "\\".to_owned();
    }
    match path.rsplit_once('\\') {
        Some(("", _)) => "\\".to_owned(),
        Some((parent, _)) => parent.to_owned(),
        None => "\\".to_owned(),
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
    // SAFETY: getpwuid returns a static buffer; we only read pw_gid.
    unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() { uid } else { (*pw).pw_gid }
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
    unsafe {
        let _ = libc::chown(c_path.as_ptr(), uid, gid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdpcore_rdpdr::irp::FILE_OPENED;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    fn test_bridge() -> Arc<Bridge> {
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        Bridge::new(wake, 1000, 1001)
    }

    fn seed_path(bridge: &Bridge, device_id: u32, path: &str, is_dir: bool) {
        bridge.ensure_root_ino(device_id);
        let _ = bridge.inode_for(device_id, path);
        bridge.meta.lock().unwrap().insert(
            (device_id, path.to_owned()),
            CachedMeta::new(is_dir, bridge.uid, bridge.gid),
        );
    }

    fn complete_command(bridge: &Bridge, cmd: DriveCommand) {
        match cmd {
            DriveCommand::Create { request_tag, .. } => {
                if let Some(Pending::Create(tx)) =
                    bridge.pending.lock().unwrap().remove(&request_tag)
                {
                    let _ = tx.send(Ok(CreateReply {
                        file_id: 42,
                        information: FILE_OPENED,
                    }));
                }
            }
            DriveCommand::Close { request_tag, .. } => {
                if let Some(Pending::Close(tx)) =
                    bridge.pending.lock().unwrap().remove(&request_tag)
                {
                    let _ = tx.send(0);
                }
            }
            DriveCommand::SetInformation { request_tag, .. } => {
                if let Some(Pending::SetInfo(tx)) =
                    bridge.pending.lock().unwrap().remove(&request_tag)
                {
                    let _ = tx.send(Ok(()));
                }
            }
            DriveCommand::QueryDirectory {
                request_tag, path, ..
            } => {
                if let Some(Pending::QueryDir(tx)) =
                    bridge.pending.lock().unwrap().remove(&request_tag)
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
                if let Some(Pending::Read(tx)) = bridge.pending.lock().unwrap().remove(&request_tag)
                {
                    let _ = tx.send(Ok(Vec::new()));
                }
            }
            DriveCommand::Write { request_tag, .. } => {
                if let Some(Pending::Write(tx)) =
                    bridge.pending.lock().unwrap().remove(&request_tag)
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
        assert_eq!(join_win("\\", "foo"), "\\foo");
        assert_eq!(join_win("\\dir", "bar"), "\\dir\\bar");
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
