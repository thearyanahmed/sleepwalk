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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use proto::{HostId, VmId};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::firecracker::{
    BootSource, Drive, Firecracker, FirecrackerApi, MachineConfig, NetworkConfig, VsockConfig,
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
/// Per-process tap index, giving each networked VM a distinct tap / MAC / IP.
static NET_SEQ: AtomicU64 = AtomicU64::new(0);

/// Wrap any error as an `io::Error` (for folding into [`MigrateError::Io`]).
fn io_other<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> std::io::Error {
    std::io::Error::other(e)
}

/// The UFFD page server that backs a VM restored on this host. The serving
/// thread must outlive the restore: pages are faulted from the snapshot file
/// lazily for the VM's whole life, so the registry owns it until teardown.
#[derive(Debug)]
pub struct UffdState {
    /// Signals the serving thread to stop.
    pub stop: Arc<AtomicBool>,
    /// The serving thread, joined at teardown after the VM is killed.
    pub thread: JoinHandle<()>,
}

/// One running VM the daemon owns.
#[derive(Debug)]
pub struct RunningVm {
    /// The VM's id (its identity on this host).
    pub id: VmId,
    /// The live Firecracker process; killed when the VM is removed.
    pub proc: FcProcess,
    /// The Firecracker control handle (pause/snapshot for migration).
    pub fc: Firecracker,
    /// The per-VM work dir (sockets, logs).
    pub work: PathBuf,
    /// The host-side vsock unix socket. Empty for a VM restored from a migration
    /// (its socket path lives in the snapshot, not chosen here), which is why such
    /// a VM is terminal for now — it is counted but not re-migrated.
    pub vsock_uds: PathBuf,
    /// The VM's memory size, in MiB — its contribution to host pressure.
    pub mib: u32,
    /// The UFFD page server, present only for a VM restored from a migration.
    pub uffd: Option<UffdState>,
    /// The VM's network identity (tap/MAC/IP), present only for a networked VM.
    pub net: Option<crate::net::NetId>,
}

impl RunningVm {
    /// Kill the Firecracker process, stop any UFFD server, drop the VM's tap, and
    /// remove its work dir and socket. Order matters: the VM is killed first so it
    /// stops faulting, then the page-server thread is joined, then its files go.
    pub fn teardown(mut self) {
        let _ = self.proc.kill();
        if let Some(uffd) = self.uffd {
            uffd.stop.store(true, Ordering::Release);
            let _ = uffd.thread.join();
        }
        if let Some(net) = &self.net {
            crate::net::destroy(&net.tap);
        }
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
    /// Live host memory pressure in `[0, 1]`, sampled from `/proc/meminfo`.
    pub pressure: f64,
}

/// The set of VMs running on one host.
#[derive(Debug)]
pub struct VmRegistry {
    vms: Mutex<BTreeMap<VmId, RunningVm>>,
    host: HostId,
}

impl VmRegistry {
    /// A registry for `host`.
    #[must_use]
    pub fn new(host: HostId) -> Self {
        Self {
            vms: Mutex::new(BTreeMap::new()),
            host,
        }
    }

    /// Boot a `mib`-sized guestd VM, register it, and return its id. With
    /// `networked`, attach a tap on the shared bridge and give the guest a stable
    /// MAC/IP (it can then reach the network and be reached).
    ///
    /// # Errors
    /// If spawning Firecracker, plumbing the network, or any boot step fails.
    pub async fn spawn(
        &self,
        art: &Artifacts,
        mib: u32,
        networked: bool,
    ) -> Result<VmId, MigrateError> {
        let id = VmId::new();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let work = std::env::temp_dir().join(format!("sleepwalk-vm-{pid}-{seq}"));
        std::fs::create_dir_all(&work).map_err(|e| io("create vm work dir", e))?;
        let vsock_uds = std::env::temp_dir().join(format!("sleepwalk-vm-vsock-{pid}-{seq}.sock"));

        // Plumb the network first so its address goes into the boot args.
        let net = if networked {
            let idx = NET_SEQ.fetch_add(1, Ordering::Relaxed) as u32;
            Some(crate::net::create(idx).map_err(|e| io("plumb vm network", io_other(e)))?)
        } else {
            None
        };
        let mut boot_args = BOOT_ARGS.to_owned();
        if let Some(n) = &net {
            boot_args.push(' ');
            boot_args.push_str(&crate::net::boot_arg(n));
        }

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
            boot_args,
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
        if let Some(n) = &net {
            fc.configure_network(NetworkConfig {
                iface_id: "eth0".to_owned(),
                host_dev_name: n.tap.clone(),
                guest_mac: n.mac.clone(),
            })
            .await?;
        }
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
                uffd: None,
                net,
            },
        );
        Ok(id)
    }

    /// Register an already-running VM (e.g. one just restored from a migration).
    pub async fn insert(&self, vm: RunningVm) {
        self.vms.lock().await.insert(vm.id, vm);
    }

    /// Deregister the VM `id` and hand it back to the caller, *without* tearing it
    /// down — for migrating it out, where the caller drives its snapshot.
    pub async fn take(&self, id: &VmId) -> Option<RunningVm> {
        self.vms.lock().await.remove(id)
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

    /// This host's current load, for the rebalancer — the live VM set plus the
    /// host's real memory pressure from `/proc/meminfo`.
    pub async fn status(&self) -> HostStatus {
        let vms = self.vms.lock().await;
        HostStatus {
            host: self.host.to_string(),
            vms: vms.keys().map(ToString::to_string).collect(),
            pressure: crate::sysmem::memory_pressure(),
        }
    }
}

fn io(context: &str, source: std::io::Error) -> MigrateError {
    MigrateError::Io {
        context: context.to_owned(),
        source,
    }
}
