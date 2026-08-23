use std::ffi::OsStr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use fuser::{
    Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, OpenFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite,
    ReplyXattr, Request,
};
use rdpcore_rdpdr::irp::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_BASIC_INFORMATION, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_END_OF_FILE_INFORMATION, FILE_OPEN, FILE_OPEN_IF, FILE_OVERWRITE_IF,
    FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_ATTRIBUTES, GENERIC_READ, GENERIC_WRITE, SYNCHRONIZE,
    basic_information_buffer, end_of_file_information_buffer,
};

use crate::rdpdr_path::{join_win, parent_of};

use super::bridge::{Bridge, CachedMeta, OpenHandle};
use super::{TTL, systemtime_to_filetime, time_or_now_to_filetime, time_or_now_to_systemtime};

/// Swappable RDPDR backend for a shared FUSE mount. Owner handoff updates
/// this without umounting, so disconnect cannot block other RDP sessions.
pub(super) struct ActiveBackend {
    pub(super) bridge: Arc<Bridge>,
    pub(super) device_id: u32,
}

pub(super) struct FuseFs {
    pub(super) active: Arc<Mutex<ActiveBackend>>,
}

impl FuseFs {
    pub(super) fn active(&self) -> (Arc<Bridge>, u32) {
        let g = self.active.lock().unwrap_or_else(|e| e.into_inner());
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
                bridge
                    .opens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
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
        let opens = bridge.opens.lock().unwrap_or_else(|e| e.into_inner());
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
            let Some(child) = join_win(&path, &name) else {
                continue;
            };
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
        if let Some(handle) = bridge
            .opens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&fh.0)
        {
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
                bridge
                    .opens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
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
        let opens = bridge.opens.lock().unwrap_or_else(|e| e.into_inner());
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
        let opens = bridge.opens.lock().unwrap_or_else(|e| e.into_inner());
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
                    let mut meta = bridge.meta.lock().unwrap_or_else(|e| e.into_inner());
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
        if let Some(handle) = bridge
            .opens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&fh.0)
        {
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
        let Some(path) = join_win(&parent_path, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
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
                bridge
                    .meta
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        (device_id, path.clone()),
                        CachedMeta::fresh(false, bridge.uid, bridge.gid, mode),
                    );
                let Some(attr) = bridge.attr_for(device_id, &path) else {
                    reply.error(Errno::EIO);
                    return;
                };
                let fh = bridge.next_fh.fetch_add(1, Ordering::Relaxed);
                bridge
                    .opens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
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
        let Some(path) = join_win(&parent_path, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
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
                bridge
                    .meta
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
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
        let Some(path) = join_win(&parent_path, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
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
        let Some(path) = join_win(&parent_path, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
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
        let (Some(old_path), Some(new_path)) = (
            join_win(&parent_path, name),
            join_win(&new_parent_path, newname),
        ) else {
            reply.error(Errno::EINVAL);
            return;
        };

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
                .unwrap_or_else(|e| e.into_inner())
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
                .unwrap_or_else(|e| e.into_inner())
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
                .unwrap_or_else(|e| e.into_inner())
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
