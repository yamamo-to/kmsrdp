use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{Errno, FileAttr, FileType, INodeNo};
use rdpcore_rdpdr::DriveCommand;
use rdpcore_rdpdr::irp::{
    CreateReply, DELETE, DirectoryEntry, FILE_ATTRIBUTE_DIRECTORY, FILE_DELETE_ON_CLOSE,
    FILE_DIRECTORY_FILE, FILE_DISPOSITION_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_READ_DATA, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, GENERIC_READ,
    SYNCHRONIZE, disposition_information_buffer, rename_information_buffer,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::rdpdr_path::{join_win, parent_of};

use super::{OP_TIMEOUT, ROOT_INO, filetime_to_systemtime, ntstatus_to_errno};

#[derive(Clone)]
pub(super) struct CachedMeta {
    pub(super) size: u64,
    pub(super) is_dir: bool,
    pub(super) mtime: SystemTime,
    pub(super) atime: SystemTime,
    pub(super) ctime: SystemTime,
    pub(super) crtime: SystemTime,
    /// Local FUSE metadata only — not sent to the RDP client.
    pub(super) perm: u16,
    pub(super) uid: u32,
    pub(super) gid: u32,
}

fn default_perm(is_dir: bool) -> u16 {
    if is_dir { 0o755 } else { 0o644 }
}

impl CachedMeta {
    pub(super) fn new(is_dir: bool, uid: u32, gid: u32) -> Self {
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

    pub(super) fn fresh(is_dir: bool, uid: u32, gid: u32, mode: u32) -> Self {
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

pub(super) struct OpenHandle {
    pub(super) device_id: u32,
    pub(super) file_id: u32,
}

pub(super) enum Pending {
    Create(mpsc::Sender<Result<CreateReply, u32>>),
    Close(mpsc::Sender<u32>),
    Read(mpsc::Sender<Result<Vec<u8>, u32>>),
    Write(mpsc::Sender<Result<u32, u32>>),
    QueryDir(mpsc::Sender<Result<Option<DirectoryEntry>, u32>>),
    SetInfo(mpsc::Sender<Result<(), u32>>),
}

pub(super) struct Bridge {
    pub(super) wake: UnboundedSender<()>,
    pub(super) outbound: Mutex<VecDeque<DriveCommand>>,
    pub(super) pending: Mutex<HashMap<u64, Pending>>,
    pub(super) next_tag: AtomicU64,
    pub(super) next_fh: AtomicU64,
    pub(super) next_ino: AtomicU64,
    /// `(device_id, windows_path)` → inode
    pub(super) path_to_ino: Mutex<HashMap<(u32, String), u64>>,
    pub(super) ino_to_path: Mutex<HashMap<(u32, u64), String>>,
    pub(super) meta: Mutex<HashMap<(u32, String), CachedMeta>>,
    pub(super) opens: Mutex<HashMap<u64, OpenHandle>>,
    pub(super) uid: u32,
    pub(super) gid: u32,
}

impl Bridge {
    pub(super) fn new(wake: UnboundedSender<()>, uid: u32, gid: u32) -> Arc<Self> {
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

    pub(super) fn alloc_tag(&self) -> u64 {
        self.next_tag.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn enqueue(&self, command: DriveCommand) {
        self.outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(command);
        if self.wake.send(()).is_err() {
            tracing::warn!("kmsrdp: rdpdr FUSE: wake channel closed; RDP connection may be gone");
        }
    }

    pub(super) fn poll_commands(&self) -> Vec<DriveCommand> {
        self.outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    /// Drop all in-flight waiters so FUSE threads unblock immediately when
    /// the RDP connection is gone (umount must not wait out [`OP_TIMEOUT`]).
    pub(super) fn abort_pending(&self) {
        self.outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub(super) fn submit_create(
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
            .unwrap_or_else(|e| e.into_inner())
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
        match self.recv_pending(tag, &rx) {
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

    pub(super) fn recv_pending<T>(
        &self,
        tag: u64,
        rx: &mpsc::Receiver<T>,
    ) -> Result<T, RecvTimeoutError> {
        self.recv_pending_timeout(tag, rx, OP_TIMEOUT)
    }

    pub(super) fn recv_pending_timeout<T>(
        &self,
        tag: u64,
        rx: &mpsc::Receiver<T>,
        timeout: Duration,
    ) -> Result<T, RecvTimeoutError> {
        match rx.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(e) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&tag);
                Err(e)
            }
        }
    }

    pub(super) fn submit_close(&self, device_id: u32, file_id: u32) -> Result<(), Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, Pending::Close(tx));
        self.enqueue(DriveCommand::Close {
            device_id,
            file_id,
            request_tag: tag,
        });
        match self.recv_pending(tag, &rx) {
            Ok(0) => Ok(()),
            Ok(status) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    pub(super) fn submit_read(
        &self,
        device_id: u32,
        file_id: u32,
        length: u32,
        offset: u64,
    ) -> Result<Vec<u8>, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, Pending::Read(tx));
        self.enqueue(DriveCommand::Read {
            device_id,
            file_id,
            length,
            offset,
            request_tag: tag,
        });
        match self.recv_pending(tag, &rx) {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    pub(super) fn submit_write(
        &self,
        device_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<u32, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, Pending::Write(tx));
        self.enqueue(DriveCommand::Write {
            device_id,
            file_id,
            offset,
            data,
            request_tag: tag,
        });
        match self.recv_pending(tag, &rx) {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    pub(super) fn submit_query_dir(
        &self,
        device_id: u32,
        file_id: u32,
        path: Option<String>,
    ) -> Result<Option<DirectoryEntry>, Errno> {
        let (tx, rx) = mpsc::channel();
        let tag = self.alloc_tag();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, Pending::QueryDir(tx));
        self.enqueue(DriveCommand::QueryDirectory {
            device_id,
            file_id,
            path,
            request_tag: tag,
        });
        match self.recv_pending(tag, &rx) {
            Ok(Ok(entry)) => Ok(entry),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    pub(super) fn submit_set_information(
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
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, Pending::SetInfo(tx));
        self.enqueue(DriveCommand::SetInformation {
            device_id,
            file_id,
            fs_information_class,
            set_buffer,
            request_tag: tag,
        });
        match self.recv_pending(tag, &rx) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(status)) => Err(ntstatus_to_errno(status)),
            Err(_) => Err(Errno::EIO),
        }
    }

    /// FreeRDP-compatible delete with a disposition fallback for stricter
    /// clients: open with `DELETE`, mark delete-on-close and/or send
    /// `FileDispositionInformation`, then CLOSE (actual unlink happens then).
    pub(super) fn delete_path(
        &self,
        device_id: u32,
        path: &str,
        is_dir: bool,
    ) -> Result<(), Errno> {
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

    pub(super) fn rename_path(
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
    pub(super) fn exchange_paths(
        &self,
        device_id: u32,
        path_a: &str,
        path_b: &str,
    ) -> Result<(), Errno> {
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

    pub(super) fn path_exists(&self, device_id: u32, win_path: &str) -> bool {
        self.attr_for(device_id, win_path).is_some()
    }

    /// Fail fast with `ENOTEMPTY` before sending a doomed directory delete IRP.
    pub(super) fn ensure_dir_empty(&self, device_id: u32, dir_path: &str) -> Result<(), Errno> {
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

    pub(super) fn apply_local_attrs(
        &self,
        device_id: u32,
        path: &str,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) {
        let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());
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

    pub(super) fn ensure_root_ino(&self, device_id: u32) {
        let key = (device_id, "\\".to_owned());
        let mut path_to_ino = self.path_to_ino.lock().unwrap_or_else(|e| e.into_inner());
        let mut ino_to_path = self.ino_to_path.lock().unwrap_or_else(|e| e.into_inner());
        if path_to_ino.contains_key(&key) {
            return;
        }
        path_to_ino.insert(key, ROOT_INO);
        ino_to_path.insert((device_id, ROOT_INO), "\\".to_owned());
        self.meta.lock().unwrap_or_else(|e| e.into_inner()).insert(
            (device_id, "\\".to_owned()),
            CachedMeta::new(true, self.uid, self.gid),
        );
    }

    pub(super) fn inode_for(&self, device_id: u32, win_path: &str) -> u64 {
        let key = (device_id, win_path.to_owned());
        let mut path_to_ino = self.path_to_ino.lock().unwrap_or_else(|e| e.into_inner());
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
            .unwrap_or_else(|e| e.into_inner())
            .insert((device_id, ino), win_path.to_owned());
        ino
    }

    pub(super) fn path_for(&self, device_id: u32, ino: u64) -> Option<String> {
        self.ino_to_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(device_id, ino))
            .cloned()
    }

    pub(super) fn cache_entry(&self, device_id: u32, parent: &str, entry: &DirectoryEntry) {
        let name = entry.file_name.trim_end_matches('\0');
        if !crate::rdpdr_path::is_safe_win_component(name) {
            return;
        }
        let Some(path) = join_win(parent, name) else {
            return;
        };
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
        self.meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((device_id, path), meta);
    }

    pub(super) fn attr_for(&self, device_id: u32, win_path: &str) -> Option<FileAttr> {
        let meta = self
            .meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
    pub(super) fn refresh_dir(&self, device_id: u32, parent: &str) -> Result<Vec<String>, Errno> {
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
    pub(super) fn enumerate_directory(
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

    pub(super) fn lookup_child(
        &self,
        device_id: u32,
        parent: &str,
        name: &str,
    ) -> Result<FileAttr, Errno> {
        let path = join_win(parent, name).ok_or(Errno::EINVAL)?;
        if let Some(attr) = self.attr_for(device_id, &path) {
            return Ok(attr);
        }
        let _ = self.refresh_dir(device_id, parent)?;
        self.attr_for(device_id, &path).ok_or(Errno::ENOENT)
    }

    pub(super) fn forget_path(&self, device_id: u32, win_path: &str) {
        self.meta
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(device_id, win_path.to_owned()));
        let ino = self
            .path_to_ino
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(device_id, win_path.to_owned()));
        if let Some(ino) = ino {
            self.ino_to_path
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(device_id, ino));
        }
    }

    /// Remap `old_path` and any cached descendants (`old_path\\…`) to `new_path`.
    pub(super) fn remap_path(&self, device_id: u32, old_path: &str, new_path: &str) {
        let prefix = if old_path == "\\" {
            "\\".to_owned()
        } else {
            format!("{old_path}\\")
        };

        let mut path_to_ino = self.path_to_ino.lock().unwrap_or_else(|e| e.into_inner());
        let mut ino_to_path = self.ino_to_path.lock().unwrap_or_else(|e| e.into_inner());
        let mut meta = self.meta.lock().unwrap_or_else(|e| e.into_inner());

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
