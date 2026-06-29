//! RDPDR drive redirection — the macrdp side of the server-side RDPDR static
//! channel (the protocol state machine lives in the vendored
//! `ironrdp-server::rdpdr`). The RDP *client* redirects its local drive; the
//! Mac (server) browses/reads the client's files.
//!
//! Done: the MS-RDPEFS init handshake (1a), device I/O — `list_dir` /
//! `read_file` via the [`RdpdrHandle`] (1b) — and the macOS surface: a real
//! NFS mount (Phase 2). An in-process NFSv3 server backed by the `RdpdrHandle`
//! is mounted via the built-in `mount_nfs` (no root, no kext), so the client's
//! drive appears as a proper Finder volume with lazy subdirectory navigation,
//! on-demand reads, and writes (create / write / mkdir / rename / delete /
//! truncate map to RDPDR DeviceWrite / DeviceCreate / SetInformation).
//! Opt-in via `--enable-drive-redirection`. Read-write.

#[cfg(target_os = "macos")]
mod smartcard;
#[cfg(target_os = "macos")]
mod surface;

/// Unmount any RDPDR NFS volumes still mounted at process exit. macrdp's signal
/// handler `std::process::exit`s, which skips `Surface::Drop`, so without this a
/// signal stop (Ctrl-C, `kill`, `launchctl bootout`/`kickstart -k`) would strand
/// the mounts pointing at the now-dead server. Call it from the signal handler
/// next to `file_promise_lazy::shutdown_cleanup()`. No-op when no drive was
/// redirected, and off macOS.
pub fn shutdown_cleanup() {
    #[cfg(target_os = "macos")]
    surface::shutdown_cleanup();
}

/// Reap RDPDR NFS-mount leftovers from a PRIOR macrdp process that died
/// uncleanly (SIGKILL/panic skip both `Surface::Drop` and [`shutdown_cleanup`],
/// stranding a stale mount + its `$TMPDIR/macrdp-rdpdr-<pid>` mountpoint dir).
/// Called once at startup; dead-pid-gated and best-effort. No-op off macOS / when
/// nothing was left behind. See `surface::reap_stale` and `crate::reaper`.
pub fn reap_stale() {
    #[cfg(target_os = "macos")]
    surface::reap_stale();
}

/// Map a client-returned [`NtStatus`] to the closest NFS status.
///
/// Extracted as a pure fn (no platform deps) so it's unit-tested on every
/// target: these are routine client-side outcomes, and getting the translation
/// right is what makes Finder report the correct thing. The headline case is a
/// write to a protected location like the `C:\` root coming back `ACCESS_DENIED`
/// → `NFS3ERR_ACCES` ("you don't have permission") rather than a generic I/O
/// error. `None` (the client gave no concrete status) falls back to I/O.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn ntstatus_to_nfsstat3(status: Option<NtStatus>) -> nfsstat3 {
    match status {
        Some(s) if s == NtStatus::ACCESS_DENIED => nfsstat3::NFS3ERR_ACCES,
        Some(s) if s == NtStatus::OBJECT_NAME_COLLISION => nfsstat3::NFS3ERR_EXIST,
        Some(s) if s == NtStatus::NO_SUCH_FILE => nfsstat3::NFS3ERR_NOENT,
        Some(s) if s == NtStatus::NOT_A_DIRECTORY => nfsstat3::NFS3ERR_NOTDIR,
        Some(s) if s == NtStatus::DIRECTORY_NOT_EMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        Some(s) if s == NtStatus::NOT_SUPPORTED => nfsstat3::NFS3ERR_NOTSUPP,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

#[cfg(target_os = "macos")]
use std::collections::HashMap;

#[cfg(target_os = "macos")]
use ironrdp_rdpdr::pdu::efs::DeviceType;
use ironrdp_rdpdr::pdu::efs::NtStatus;
use ironrdp_server::{
    AnnouncedDevice, RdpdrBackendFactory, RdpdrHandle, RdpdrServerFactory, RdpdrServerHandler,
    ServerEvent, ServerEventSender,
};
use nfsserve::nfs::nfsstat3;
use tokio::sync::mpsc;
use tracing::info;

/// Factory for the RDPDR static channel (mirrors `MacCliprdr` / `MacRdpsnd`).
/// The one channel carries both drive redirection and smart-card redirection;
/// the flags select which the backend acts on.
#[derive(Debug, Default)]
pub struct MacRdpdr {
    enable_drive: bool,
    enable_smartcard: bool,
}

impl MacRdpdr {
    pub fn new(enable_drive: bool, enable_smartcard: bool) -> Self {
        Self {
            enable_drive,
            enable_smartcard,
        }
    }
}

impl ServerEventSender for MacRdpdr {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // No-op: the backend's RdpdrHandle is wired with the connection's event
        // sender by the server's `build_rdpdr`, so the factory needn't retain one.
    }
}

impl RdpdrBackendFactory for MacRdpdr {
    fn build_backend(&self) -> Box<dyn RdpdrServerHandler> {
        #[cfg(not(target_os = "macos"))]
        let _ = (self.enable_drive, self.enable_smartcard);
        Box::new(MacRdpdrHandler {
            handle: None,
            #[cfg(target_os = "macos")]
            enable_drive: self.enable_drive,
            #[cfg(target_os = "macos")]
            enable_smartcard: self.enable_smartcard,
            #[cfg(target_os = "macos")]
            surfaces: HashMap::new(),
            #[cfg(target_os = "macos")]
            smartcard: None,
        })
    }

    fn computer_name(&self) -> String {
        hostname()
    }
}

impl RdpdrServerFactory for MacRdpdr {}

/// Backend for the RDPDR server processor. Logs announced devices and, on
/// macOS, mounts each redirected filesystem as its own real NFS volume
/// (all dropped — and unmounted — when the connection ends).
#[derive(Debug)]
struct MacRdpdrHandler {
    handle: Option<RdpdrHandle>,
    #[cfg(target_os = "macos")]
    enable_drive: bool,
    #[cfg(target_os = "macos")]
    enable_smartcard: bool,
    /// One live NFS mount per redirected filesystem device, keyed by device id
    /// so a re-announce doesn't double-mount an already-mounted drive.
    #[cfg(target_os = "macos")]
    surfaces: HashMap<u32, surface::Surface>,
    /// The smart-card IFD bridge, started once for the first redirected reader.
    #[cfg(target_os = "macos")]
    smartcard: Option<smartcard::SmartcardBridge>,
}

impl RdpdrServerHandler for MacRdpdrHandler {
    fn set_handle(&mut self, handle: RdpdrHandle) {
        self.handle = Some(handle);
    }

    fn on_devices_announced(&mut self, devices: &[AnnouncedDevice]) {
        for d in devices {
            info!(
                device_id = d.device_id,
                device_type = ?d.device_type,
                name = %d.name,
                "drive redirection: client redirected a device"
            );
        }

        #[cfg(target_os = "macos")]
        {
            let Some(handle) = self.handle.clone() else {
                return;
            };

            // Drive redirection: mount every redirected filesystem in Finder, one
            // volume each. The client may re-announce the same list several times
            // during init (and may add drives later), so mount only device ids we
            // aren't already surfacing.
            if self.enable_drive {
                for dev in devices
                    .iter()
                    .filter(|d| d.device_type == DeviceType::Filesystem)
                {
                    if self.surfaces.contains_key(&dev.device_id) {
                        continue;
                    }
                    info!(device_id = dev.device_id, name = %dev.name, "drive redirection: mounting client drive as NFS volume");
                    let surface = surface::Surface::start(handle.clone(), dev.device_id, &dev.name);
                    self.surfaces.insert(dev.device_id, surface);
                }
            }

            // Smart-card redirection: the first redirected reader gets the IFD
            // bridge (one virtual macOS reader on the fixed loopback port, so a
            // single bridge for the session — additional readers map onto it).
            if self.enable_smartcard && self.smartcard.is_none() {
                if let Some(dev) = devices
                    .iter()
                    .find(|d| d.device_type == DeviceType::Smartcard)
                {
                    info!(device_id = dev.device_id, name = %dev.name, "smart card: starting IFD bridge for redirected reader");
                    self.smartcard = Some(smartcard::SmartcardBridge::start(handle, dev.device_id));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = &self.handle;
    }
}

/// The Mac's hostname, shown to the client (Explorer renders a redirected
/// share as "`<dir>` on `<hostname>`"). Falls back to `"macrdp"`.
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes up to buf.len() bytes and null-terminates.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    "macrdp".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `matches!` rather than `assert_eq!` so the test needs no PartialEq/Debug
    // on nfsstat3.
    #[test]
    fn ntstatus_maps_to_expected_nfs_errors() {
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::ACCESS_DENIED)),
            nfsstat3::NFS3ERR_ACCES
        ));
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::OBJECT_NAME_COLLISION)),
            nfsstat3::NFS3ERR_EXIST
        ));
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::NO_SUCH_FILE)),
            nfsstat3::NFS3ERR_NOENT
        ));
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::NOT_A_DIRECTORY)),
            nfsstat3::NFS3ERR_NOTDIR
        ));
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::DIRECTORY_NOT_EMPTY)),
            nfsstat3::NFS3ERR_NOTEMPTY
        ));
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::NOT_SUPPORTED)),
            nfsstat3::NFS3ERR_NOTSUPP
        ));
    }

    #[test]
    fn unknown_or_absent_status_falls_back_to_io() {
        // No concrete status returned by the client → generic I/O.
        assert!(matches!(ntstatus_to_nfsstat3(None), nfsstat3::NFS3ERR_IO));
        // A status we don't specifically translate → generic I/O.
        assert!(matches!(
            ntstatus_to_nfsstat3(Some(NtStatus::UNSUCCESSFUL)),
            nfsstat3::NFS3ERR_IO
        ));
    }
}
