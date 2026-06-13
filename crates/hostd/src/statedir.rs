//! Per-VM on-disk layout.
//!
//! Each VM hostd manages owns one directory, `<base>/vms/<vm-id>/`, holding its
//! Firecracker API socket, log fifo, and (in a later slice) its snapshot files.
//! The jailer chroots into this directory, so the layout is also the security
//! boundary. Paths are derived, never passed around as strings — callers ask
//! [`VmDir`] for the path they want.

use std::path::{Path, PathBuf};

use proto::VmId;

/// The directory tree for a single VM, rooted at `<base>/vms/<vm-id>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmDir {
    root: PathBuf,
}

impl VmDir {
    /// The state directory for `vm` under a host's `base` state root.
    #[must_use]
    pub fn new(base: &Path, vm: VmId) -> Self {
        Self {
            root: base.join("vms").join(vm.to_string()),
        }
    }

    /// The VM's directory root. The jailer chroot target.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The Firecracker API unix socket hostd connects to for this VM.
    #[must_use]
    pub fn api_socket(&self) -> PathBuf {
        self.root.join("api.sock")
    }

    /// The VM's log fifo (Firecracker writes structured logs here).
    #[must_use]
    pub fn log_fifo(&self) -> PathBuf {
        self.root.join("fc.log")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_namespaced_under_vm_id() {
        let vm = VmId::from_uuid(uuid::Uuid::nil());
        let dir = VmDir::new(Path::new("/srv/state"), vm);
        assert_eq!(
            dir.root(),
            Path::new("/srv/state/vms/00000000-0000-0000-0000-000000000000")
        );
        assert!(dir.api_socket().ends_with("api.sock"));
        assert!(dir.api_socket().starts_with(dir.root()));
        assert!(dir.log_fifo().starts_with(dir.root()));
    }

    #[test]
    fn distinct_vms_get_distinct_dirs() {
        let base = Path::new("/srv/state");
        let a = VmDir::new(base, VmId::new());
        let b = VmDir::new(base, VmId::new());
        assert_ne!(a.root(), b.root());
    }
}
