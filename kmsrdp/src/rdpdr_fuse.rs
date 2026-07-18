//! FUSE mount for RDPDR filesystem devices: client-redirected drives appear
//! under `{xdg_runtime_dir}/kmsrdp/drives/<DosName>` for the active session.
//!
//! Wire IRPs are limited to CREATE/CLOSE/READ/WRITE/QueryDirectory (same as
//! FreeRDP's server). unlink/rmdir/rename/setattr therefore return ENOSYS.

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
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, SessionACL,
};
use rdpcore_rdpdr::irp::{
    CreateReply, DirectoryEntry, FILE_ATTRIBUTE_DIRECTORY, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_OPEN, FILE_OPEN_IF, FILE_OVERWRITE_IF, FILE_SYNCHRONOUS_IO_NONALERT, GENERIC_READ,
    GENERIC_WRITE, SYNCHRONIZE,
};
use rdpcore_rdpdr::pdu::RDPDR_DTYP_FILESYSTEM;
use rdpcore_rdpdr::{DriveCommand, DriveConsumer, DriveConsumerFactory};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::session::Session;

const TTL: Duration = Duration::from_secs(1);
const OP_TIMEOUT: Duration = Duration::from_secs(60);
const ROOT_INO: u64 = 1;

#[derive(Clone)]
struct CachedMeta {
    size: u64,
    is_dir: bool,
    mtime: SystemTime,
    atime: SystemTime,
    ctime: SystemTime,
    crtime: SystemTime,
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
            eprintln!("kmsrdp: rdpdr FUSE: wake channel closed; RDP connection may be gone");
        }
    }

    fn poll_commands(&self) -> Vec<DriveCommand> {
        self.outbound.lock().unwrap().drain(..).collect()
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
                eprintln!(
                    "kmsrdp: rdpdr FUSE: CREATE path={path_for_log:?} device={device_id} → NTSTATUS {status:#010x}"
                );
                Err(ntstatus_to_errno(status))
            }
            Err(RecvTimeoutError::Timeout) => {
                eprintln!(
                    "kmsrdp: rdpdr FUSE: CREATE timed out path={path_for_log:?} device={device_id} (no IoCompletion)"
                );
                Err(Errno::ETIMEDOUT)
            }
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!(
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
            Ok(_) => Ok(()), // treat non-zero close status as soft failure
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
            CachedMeta {
                size: 0,
                is_dir: true,
                mtime: UNIX_EPOCH,
                atime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
            },
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
            perm: if meta.is_dir { 0o755 } else { 0o644 },
            nlink: if meta.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
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
        let pattern = if parent == "\\" {
            "\\*".to_owned()
        } else {
            format!("{parent}\\*")
        };
        let mut names = Vec::new();
        let mut first = Some(pattern);
        loop {
            match self.submit_query_dir(device_id, file_id, first.take()) {
                Ok(Some(entry)) => {
                    let name = entry.file_name.trim_end_matches('\0').to_owned();
                    self.cache_entry(device_id, parent, &entry);
                    if !name.is_empty() && name != "." && name != ".." {
                        names.push(name);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = self.submit_close(device_id, file_id);
                    return Err(e);
                }
            }
        }
        let _ = self.submit_close(device_id, file_id);
        Ok(names)
    }

    fn lookup_child(&self, device_id: u32, parent: &str, name: &str) -> Result<FileAttr, Errno> {
        let path = join_win(parent, name);
        if let Some(attr) = self.attr_for(device_id, &path) {
            return Ok(attr);
        }
        let _ = self.refresh_dir(device_id, parent)?;
        self.attr_for(device_id, &path).ok_or(Errno::ENOENT)
    }
}

struct FuseFs {
    bridge: Arc<Bridge>,
    device_id: u32,
}

impl Filesystem for FuseFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.bridge.path_for(self.device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.bridge.lookup_child(self.device_id, &parent_path, name) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.bridge.path_for(self.device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(attr) = self.bridge.attr_for(self.device_id, &path) {
            reply.attr(&TTL, &attr);
            return;
        }
        if path == "\\" {
            self.bridge.ensure_root_ino(self.device_id);
            if let Some(attr) = self.bridge.attr_for(self.device_id, "\\") {
                reply.attr(&TTL, &attr);
                return;
            }
        }
        // Refresh parent listing to populate cache.
        let parent = parent_of(&path);
        let _ = self.bridge.refresh_dir(self.device_id, &parent);
        match self.bridge.attr_for(self.device_id, &path) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.bridge.path_for(self.device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match self.bridge.submit_create(
            self.device_id,
            path,
            GENERIC_READ,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        ) {
            Ok(create) => {
                let fh = self.bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                self.bridge.opens.lock().unwrap().insert(
                    fh,
                    OpenHandle {
                        device_id: self.device_id,
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
        let Some(path) = self.bridge.path_for(self.device_id, ino.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let opens = self.bridge.opens.lock().unwrap();
        let Some(handle) = opens.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let file_id = handle.file_id;
        drop(opens);

        // Always re-enumerate from the start into a local list; FUSE offset
        // is an opaque cursor we treat as 1-based entry index.
        let mut entries = Vec::new();
        let pattern = if path == "\\" {
            "\\*".to_owned()
        } else {
            format!("{path}\\*")
        };
        let mut first = Some(pattern);
        loop {
            match self
                .bridge
                .submit_query_dir(self.device_id, file_id, first.take())
            {
                Ok(Some(entry)) => {
                    self.bridge.cache_entry(self.device_id, &path, &entry);
                    let name = entry.file_name.trim_end_matches('\0').to_owned();
                    if name.is_empty() || name == "." || name == ".." {
                        continue;
                    }
                    let child = join_win(&path, &name);
                    let ino = self.bridge.inode_for(self.device_id, &child);
                    let kind = if entry.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    };
                    entries.push((ino, kind, name));
                }
                Ok(None) => break,
                Err(e) => {
                    reply.error(e);
                    return;
                }
            }
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
                self.bridge.inode_for(self.device_id, &parent_of(&path))
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
        if let Some(handle) = self.bridge.opens.lock().unwrap().remove(&fh.0) {
            let _ = self.bridge.submit_close(handle.device_id, handle.file_id);
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let Some(path) = self.bridge.path_for(self.device_id, ino.0) else {
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
        match self.bridge.submit_create(
            self.device_id,
            path,
            access,
            disposition,
            FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(create) => {
                let fh = self.bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                self.bridge.opens.lock().unwrap().insert(
                    fh,
                    OpenHandle {
                        device_id: self.device_id,
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
        let opens = self.bridge.opens.lock().unwrap();
        let Some(handle) = opens.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let device_id = handle.device_id;
        let file_id = handle.file_id;
        drop(opens);
        match self.bridge.submit_read(device_id, file_id, size, offset) {
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
        let opens = self.bridge.opens.lock().unwrap();
        let Some(handle) = opens.get(&fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };
        let device_id = handle.device_id;
        let file_id = handle.file_id;
        drop(opens);
        match self
            .bridge
            .submit_write(device_id, file_id, offset, data.to_vec())
        {
            Ok(n) => {
                if let Some(path) = self.bridge.path_for(device_id, ino.0) {
                    let mut meta = self.bridge.meta.lock().unwrap();
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
        if let Some(handle) = self.bridge.opens.lock().unwrap().remove(&fh.0) {
            let _ = self.bridge.submit_close(handle.device_id, handle.file_id);
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
        let Some(parent_path) = self.bridge.path_for(self.device_id, parent.0) else {
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
        match self.bridge.submit_create(
            self.device_id,
            path.clone(),
            access,
            FILE_OPEN_IF,
            FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(create) => {
                self.bridge.meta.lock().unwrap().insert(
                    (self.device_id, path.clone()),
                    CachedMeta {
                        size: 0,
                        is_dir: false,
                        mtime: SystemTime::now(),
                        atime: SystemTime::now(),
                        ctime: SystemTime::now(),
                        crtime: SystemTime::now(),
                    },
                );
                let attr = self
                    .bridge
                    .attr_for(self.device_id, &path)
                    .expect("meta just inserted");
                let fh = self.bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                self.bridge.opens.lock().unwrap().insert(
                    fh,
                    OpenHandle {
                        device_id: self.device_id,
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
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_path) = self.bridge.path_for(self.device_id, parent.0) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        let path = join_win(&parent_path, name);
        match self.bridge.submit_create(
            self.device_id,
            path.clone(),
            GENERIC_READ | SYNCHRONIZE,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
        ) {
            Ok(create) => {
                let _ = self.bridge.submit_close(self.device_id, create.file_id);
                self.bridge.meta.lock().unwrap().insert(
                    (self.device_id, path.clone()),
                    CachedMeta {
                        size: 0,
                        is_dir: true,
                        mtime: SystemTime::now(),
                        atime: SystemTime::now(),
                        ctime: SystemTime::now(),
                        crtime: SystemTime::now(),
                    },
                );
                match self.bridge.attr_for(self.device_id, &path) {
                    Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
                    None => reply.error(Errno::EIO),
                }
            }
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::ENOSYS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::ENOSYS);
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::ENOSYS);
    }
}

struct MountedDrive {
    mount_point: PathBuf,
    _session: BackgroundSession,
}

pub struct FuseDriveFactory {
    session_rx: watch::Receiver<Option<Session>>,
}

impl FuseDriveFactory {
    pub fn new(session_rx: watch::Receiver<Option<Session>>) -> Self {
        Self { session_rx }
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
                eprintln!(
                    "kmsrdp: rdpdr FUSE: no active session; mounts disabled for this connection"
                );
                (0, 0, PathBuf::from("/tmp"), false)
            }
        };
        Box::new(FuseDriveConsumer {
            bridge: Bridge::new(wake, uid, gid),
            runtime_dir: runtime,
            uid,
            mounts: HashMap::new(),
            have_session,
        })
    }
}

struct FuseDriveConsumer {
    bridge: Arc<Bridge>,
    runtime_dir: PathBuf,
    uid: u32,
    mounts: HashMap<u32, MountedDrive>,
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
            eprintln!("kmsrdp: rdpdr FUSE: ignoring device {device_id} with empty DosName");
            return Vec::new();
        }
        let mount_point = self.runtime_dir.join("kmsrdp").join("drives").join(&name);
        if let Err(e) = prepare_mount_point(&mount_point) {
            eprintln!(
                "kmsrdp: rdpdr FUSE: failed to prepare {}: {e}",
                mount_point.display()
            );
            return Vec::new();
        }
        chown_path(&mount_point, self.uid, self.bridge.gid);

        self.bridge.ensure_root_ino(device_id);

        let mut config = Config::default();
        // SessionACL::All → allow_other so the session user can use a
        // root-owned mount. File ownership comes from FileAttr uid/gid,
        // not fusermount uid=/gid= (fusermount3 rejects those options).
        config.acl = SessionACL::All;
        config.mount_options = vec![
            MountOption::FSName(format!("kmsrdp-{name}")),
            MountOption::DefaultPermissions,
            MountOption::AutoUnmount,
        ];
        config.n_threads = Some(1);

        let fs = FuseFs {
            bridge: Arc::clone(&self.bridge),
            device_id,
        };
        match fuser::spawn_mount2(fs, &mount_point, &config) {
            Ok(session) => {
                println!(
                    "kmsrdp: rdpdr FUSE mounted {} at {}",
                    name,
                    mount_point.display()
                );
                self.mounts.insert(
                    device_id,
                    MountedDrive {
                        mount_point,
                        _session: session,
                    },
                );
            }
            Err(e) => {
                eprintln!(
                    "kmsrdp: rdpdr FUSE: mount failed at {}: {e} \
                     (need fuse3, and usually `user_allow_other` in /etc/fuse.conf)",
                    mount_point.display()
                );
            }
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

    fn poll_commands(&mut self) -> Vec<DriveCommand> {
        self.bridge.poll_commands()
    }
}

impl Drop for FuseDriveConsumer {
    fn drop(&mut self) {
        for (device_id, mounted) in self.mounts.drain() {
            println!(
                "kmsrdp: rdpdr FUSE unmounting device {device_id} at {}",
                mounted.mount_point.display()
            );
            // BackgroundSession drop unmounts.
            drop(mounted);
        }
    }
}

fn prepare_mount_point(path: &Path) -> std::io::Result<()> {
    // A previous RDP session may have left a stale FUSE mount. create_dir_all
    // then hits EEXIST and is_dir() on the broken mount returns EIO, which
    // surfaces as "File exists". Lazy-unmount first, then ensure the dir.
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
        _ => {
            eprintln!("kmsrdp: rdpdr FUSE: unmapped NTSTATUS {status:#010x} → EIO");
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
