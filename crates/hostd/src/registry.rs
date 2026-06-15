//! The per-host registry of running VMs.
//!
//! The daemon is long-lived and owns the VMs on its host: [`VmRegistry`] boots
//! them ([`spawn`](VmRegistry::spawn)), tracks each one's live Firecracker
//! process and control handle ([`RunningVm`]), and reports the host's load
//! ([`status`](VmRegistry::status)) so the rebalancer can see the fleet and pick
//! moves. Memory pressure is the registry's resident footprint over the host's
//! configured capacity — the signal the placement loop balances on.
//!
//! Linux-only: it drives Firecracker. Migration of a registered VM (snapshot the
//! live one, hand it to a peer, deregister) builds on this in a later slice; here
//! a VM is booted, registered, and reaped.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proto::{HostId, VmId};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::firecracker::{
    BootSource, Drive, Firecracker, FirecrackerApi, MachineConfig, VsockConfig,
};
use crate::migrate::{Artifacts, MigrateError};
use crate::process::FcProcess;

/// The guest's vsock context id (host is always CID 2). Each Firecracker instance
/// has its own vsock namespace, so every VM on the host can reuse it.
const GUEST_CID: u32 = 3;
/// Boot args for the guestd rootfs: ext4 root, read-write, guestd as init.
const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init";

/// Per-process sequence so concurrent spawns get distinct work dirs / sockets.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// One running VM the daemon owns.
#[derive(Debug)]
pub struct RunningVm {
    /// The VM's id (its identity across a migration).
    pub id: VmId,
    /// The live Firecracker process; killed when the VM is removed.
    pub proc: FcProcess,
    /// The Firecracker control handle (pause/snapshot for migration).
    pub fc: Firecracker,
    /// The per-VM work dir (sockets, logs).
    pub work: PathBuf,
    /// The host-side vsock unix socket.
    pub vsock_uds: PathBuf,
    /// The VM's memory size, in MiB — its contribution to host pressure.
    pub mib: u32,
}

impl RunningVm {
    /// Kill the Firecracker process and remove the VM's work dir and socket.
    pub fn teardown(mut self) {
        let _ = self.proc.kill();
        let _ = std::fs::remove_dir_all(&self.work);
        let _ = std::fs::remove_file(&self.vsock_uds);
    }
}

/// A host's load, as reported to the rebalancer.
#[derive(Debug, Clone, Serialize)]
pub struct HostStatus {
    /// This host's id.
    pub host: String,
    /// The VMs running here.
    pub vms: Vec<String>,
    /// Memory pressure in `[0, 1]`: resident VM memory over capacity.
    pub pressure: f64,
}

/// The set of VMs running on one host.
#[derive(Debug)]
pub struct VmRegistry {
    vms: Mutex<BTreeMap<VmId, RunningVm>>,
    host: HostId,
    /// The host's usable guest memory, in MiB — the denominator of pressure.
    capacity_mib: u32,
}

impl VmRegistry {
    /// A registry for `host` with `capacity_mib` of usable guest memory.
    #[must_use]
    pub fn new(host: HostId, capacity_mib: u32) -> Self {
        Self {
            vms: Mutex::new(BTreeMap::new()),
            host,
            capacity_mib: capacity_mib.max(1),
        }
    }

    /// Boot a `mib`-sized guestd VM, register it, and return its id.
    ///
    /// # Errors
    /// If spawning Firecracker or any boot-configuration step fails.
    pub async fn spawn(&self, art: &Artifacts, mib: u32) -> Result<VmId, MigrateError> {
        let id = VmId::new();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let work = std::env::temp_dir().join(format!("sleepwalk-vm-{pid}-{seq}"));
        std::fs::create_dir_all(&work).map_err(|e| io("create vm work dir", e))?;
        let vsock_uds = std::env::temp_dir().join(format!("sleepwalk-vm-vsock-{pid}-{seq}.sock"));

        let proc = FcProcess::spawn(
            &art.fc_bin,
            &work.join("fc.sock"),
            &work.join("fc.log"),
            Duration::from_secs(10),
        )
        .map_err(|e| io("spawn firecracker", e))?;
        let fc = Firecracker::new(work.join("fc.sock"));
        fc.configure_machine(MachineConfig {
            vcpu_count: 1,
            mem_size_mib: mib,
        })
        .await?;
        fc.configure_boot_source(BootSource {
            kernel_image: art.kernel.clone(),
            boot_args: BOOT_ARGS.to_owned(),
        })
        .await?;
        fc.configure_drive(Drive {
            drive_id: "rootfs".to_owned(),
            path_on_host: art.rootfs.clone(),
            is_root_device: true,
            is_read_only: false,
        })
        .await?;
        fc.configure_vsock(VsockConfig {
            guest_cid: GUEST_CID,
            uds_path: vsock_uds.clone(),
        })
        .await?;
        fc.boot().await?;

        self.vms.lock().await.insert(
            id,
            RunningVm {
                id,
                proc,
                fc,
                work,
                vsock_uds,
                mib,
            },
        );
        Ok(id)
    }

    /// Deregister and tear down the VM `id`, if present.
    pub async fn remove(&self, id: &VmId) -> bool {
        let vm = self.vms.lock().await.remove(id);
        let found = vm.is_some();
        if let Some(vm) = vm {
            vm.teardown();
        }
        found
    }

    /// This host's current load, for the rebalancer.
    pub async fn status(&self) -> HostStatus {
        let vms = self.vms.lock().await;
        let used: u64 = vms.values().map(|v| u64::from(v.mib)).sum();
        HostStatus {
            host: self.host.to_string(),
            vms: vms.keys().map(ToString::to_string).collect(),
            pressure: used as f64 / f64::from(self.capacity_mib),
        }
    }
}

fn io(context: &str, source: std::io::Error) -> MigrateError {
    MigrateError::Io {
        context: context.to_owned(),
        source,
    }
}
