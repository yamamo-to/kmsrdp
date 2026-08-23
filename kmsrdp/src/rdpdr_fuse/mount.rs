use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fuser::{BackgroundSession, Config, MountOption, SessionACL};

use super::bridge::Bridge;
use super::fs::{ActiveBackend, FuseFs};
use super::{chown_path, prepare_mount_point, try_unmount};

pub(super) struct MountMember {
    pub(super) bridge: Arc<Bridge>,
    pub(super) device_id: u32,
}

pub(super) struct SharedMount {
    pub(super) mount_point: PathBuf,
    pub(super) owner_conn: u64,
    pub(super) session: BackgroundSession,
    /// Shared with [`FuseFs`]; owner handoff swaps the backend in place.
    pub(super) active: Arc<Mutex<ActiveBackend>>,
    pub(super) members: HashMap<u64, MountMember>,
}

/// One shared FUSE mount per DosName: refcounted across RDP connections,
/// released only when the last member leaves. Owner changes swap the
/// RDPDR backend without umounting.
pub(super) struct MountRegistry {
    pub(super) next_conn: AtomicU64,
    pub(super) slots: Mutex<HashMap<String, SharedMount>>,
}

pub(super) struct JoinRequest {
    pub(super) dos_name: String,
    pub(super) conn_id: u64,
    pub(super) bridge: Arc<Bridge>,
    pub(super) device_id: u32,
    pub(super) mount_point: PathBuf,
    pub(super) uid: u32,
    pub(super) gid: u32,
}

impl MountRegistry {
    pub(super) fn new() -> Self {
        Self {
            next_conn: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn alloc_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Join an existing shared mount, or create it if this is the first member.
    pub(super) fn join(&self, req: JoinRequest) -> bool {
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
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
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

        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
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

    pub(super) fn leave(&self, dos_name: &str, conn_id: u64) {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = slots.get_mut(dos_name) else {
            return;
        };
        slot.members.remove(&conn_id);
        if slot.members.is_empty() {
            let Some(SharedMount {
                mount_point,
                session,
                ..
            }) = slots.remove(dos_name)
            else {
                // Unreachable: `slots.get_mut` succeeded on the line above.
                return;
            };
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
            let Some(&new_owner) = slot.members.keys().next() else {
                // Unreachable: `members.is_empty()` was checked above.
                return;
            };
            let member = &slot.members[&new_owner];
            member.bridge.ensure_root_ino(member.device_id);
            // Clear stale opens from the departing owner's bridge; swap the
            // live backend so FUSE keeps serving without umount/remount.
            {
                let mut active = slot.active.lock().unwrap_or_else(|e| e.into_inner());
                active
                    .bridge
                    .opens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
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

    pub(super) fn unmount_all(&self) {
        let drained: Vec<SharedMount> = {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            slots.drain().map(|(_, slot)| slot).collect()
        };
        for slot in drained {
            for member in slot.members.values() {
                member.bridge.abort_pending();
            }
            tracing::info!(
                "kmsrdp: rdpdr FUSE shutdown unmount {}",
                slot.mount_point.display()
            );
            detach_umount(slot.session, slot.mount_point);
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
    // One mount is shared (refcounted) across every RDP connection that
    // redirects this DosName, and each FUSE op blocks its dispatch thread
    // for up to OP_TIMEOUT waiting on the client to complete the
    // corresponding DriveCommand round trip. With a single thread, one
    // slow/unresponsive client stalls every other op on this mount -
    // including from other connections - for up to that long. Bridge's
    // shared state (`outbound`/`pending`/`path_to_ino`/`ino_to_path`/
    // `meta`/`opens`) is all Mutex- or Atomic-guarded specifically so
    // multiple dispatch threads can run concurrently here.
    config.n_threads = Some(4);
    let fs = FuseFs {
        active: Arc::clone(&active),
    };
    let session = fuser::spawn_mount(fs, mount_point, &config)?;
    Ok((session, active))
}

fn detach_umount(session: BackgroundSession, mount_point: PathBuf) {
    let mp = mount_point.clone();
    let res = std::thread::Builder::new()
        .name("kmsrdp-fuse-umount".into())
        .spawn(move || {
            if let Err(e) = session.umount_and_join() {
                tracing::warn!(
                    "kmsrdp: rdpdr FUSE umount/join failed for {} ({e}); trying lazy unmount",
                    mp.display()
                );
                try_unmount(&mp);
            }
        });
    if let Err(e) = res {
        tracing::warn!(
            "kmsrdp: failed to spawn fuse umount thread ({e}); trying synchronous lazy unmount for {}",
            mount_point.display()
        );
        try_unmount(&mount_point);
    }
}
