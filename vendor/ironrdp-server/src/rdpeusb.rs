//! Server-direction MS-RDPEUSB (`URBDRC` DVC) — USB device redirection.
//!
//! macrdp is the RDP **server**: the RDP client owns a physical USB device and
//! redirects it over the `URBDRC` dynamic virtual channel; this server drives it
//! and presents it locally (on macOS, via a user-space USB host controller — see
//! macrdp's `src/usb_redirect`). This module is the server-side DVC processor,
//! written against the vendored `ironrdp-rdpeusb` **PDU layer** (the pinned rev is
//! PDU-only; the upstream `UrbdrcControlServer`/`UrbdrcDeviceServer` processors
//! live 3 PRs ahead and would force a foundation-wide pin bump, so we drive the
//! wire ourselves here — the same pattern as the server-direction RDPDR in
//! `src/rdpdr.rs`). Divergence 16.
//!
//! **Init handshake (Phase 3.0).** On the main `URBDRC` channel the server drives
//! the MS-RDPEUSB initialization sequence — RIM capability exchange →
//! `CHANNEL_CREATED` → `RIMCALL_RELEASE` — which makes the client announce each
//! redirected device with an `ADD_VIRTUAL_CHANNEL` on the device-sink interface.
//!
//! **Per-device channel (Phase 3.1a).** Each `ADD_VIRTUAL_CHANNEL` asks the server
//! to open a *new* `URBDRC` DVC for that device. The main [`UrbdrcServer`] can't
//! reach [`DrdynvcServer`](ironrdp_dvc::DrdynvcServer) from inside `process()`, so
//! it signals the server event loop via [`ServerEvent::Urbdrc`]; the loop calls
//! `DrdynvcServer::create_channel` with a [`UrbdrcDeviceProcessor`]. That device
//! processor sends its own `RIMCALL_RELEASE` on open (FreeRDP's `INIT_CHANNEL_OUT`
//! barrier), which makes the client send `ADD_DEVICE` — the real device
//! descriptors — on the per-device channel. Transfers (`UsbHandle`/router + the
//! IOUSBHost bridge) are Phase 3.1b.

use core::fmt;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ironrdp_core::{Encode, EncodeResult, ReadCursor, WriteCursor, decode, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcProcessor, DvcServerProcessor};
use ironrdp_pdu::PduResult;
use ironrdp_rdpeusb::pdu::caps::{Capability, RimExchangeCapabilityRequest};
use ironrdp_rdpeusb::pdu::completion::ts_urb_result::{TsUrbResultPayload, TsUrbSelectConfigResult, UsbdPipeType};
use ironrdp_rdpeusb::pdu::header::{FunctionId, InterfaceId, SharedMsgHeader};
use ironrdp_rdpeusb::pdu::iface_manipulation::InterfaceRelease;
use ironrdp_rdpeusb::pdu::notify::{ChannelCreated, Direction};
use ironrdp_rdpeusb::pdu::usb_dev::ts_urb::utils::{
    SetupPacket, TsUrbHeader, TsUsbdInterfaceInfo, TsUsbdPipeInfo, UrbFunction, UsbConfigDesc,
};
use ironrdp_rdpeusb::pdu::usb_dev::ts_urb::{
    TsUrbBulkOrInterruptTransfer, TsUrbControlDescRequest, TsUrbControlFeatRequest, TsUrbControlGetConfigRequest,
    TsUrbControlGetInterfaceRequest, TsUrbControlGetStatusRequest, TsUrbControlTransferEx,
    TsUrbControlVendorClassRequest, TsUrbIn, TsUrbInKind, TsUrbOut, TsUrbOutKind, TsUrbSelectConfig,
};
use ironrdp_rdpeusb::pdu::usb_dev::{RegisterRequestCallback, TransferInRequest, TransferOutRequest};
use ironrdp_rdpeusb::pdu::utils::RequestIdTransferInOut;
use ironrdp_rdpeusb::pdu::{
    UrbdrcClientControlPdu, UrbdrcClientDevicePdu, UrbdrcServerControlPdu, UrbdrcServerDevicePdu,
};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::{ServerEvent, ServerEventSender};

/// USB standard `bDescriptorType` values.
const USB_DESCRIPTOR_TYPE_DEVICE: u8 = 1;
const USB_DESCRIPTOR_TYPE_CONFIGURATION: u8 = 2;
const USB_DESCRIPTOR_TYPE_INTERFACE: u8 = 4;
const USB_DESCRIPTOR_TYPE_ENDPOINT: u8 = 5;
/// A USB device descriptor is 18 bytes.
const USB_DEVICE_DESCRIPTOR_LEN: u32 = 18;
/// `USBD_TRANSFER_DIRECTION_IN` transfer flag (the vendored crate's copy is
/// `pub(crate)`; MS-RDPEUSB fixes it at bit 0).
const USBD_TRANSFER_DIRECTION_IN: u32 = 0x1;
/// `USBD_SHORT_TRANSFER_OK` (bit 1) — a bulk/interrupt **IN** that returns fewer
/// bytes than the requested buffer is normal (a UVC video payload, a short HID
/// report, the tail of any stream), NOT an error. Without it real Windows / mstsc
/// completes a short read as `USBD_STATUS_ERROR_SHORT_TRANSFER`, surfaced over
/// URBDRC as `0x8007001f` (ERROR_GEN_FAILURE) — so a camera's first video payload
/// fails and streaming never starts. Mass storage never tripped it because SCSI
/// bulk reads are exact-length (which is why FreeRDP mass storage worked without it).
const USBD_SHORT_TRANSFER_OK: u32 = 0x2;

/// The MS-RDPEUSB dynamic virtual channel name (this rev's `ironrdp-rdpeusb`
/// predates the upstream `CHANNEL_NAME` const, so it's spelled out here).
pub const URBDRC_CHANNEL_NAME: &str = "URBDRC";

/// Server-loop actions requested by the `URBDRC` processor / [`UsbHandle`] that
/// they can't perform themselves (they need `&mut DrdynvcServer`, which only the
/// event loop holds). Delivered via [`ServerEvent::Urbdrc`].
pub enum UrbdrcServerMessage {
    /// The client announced a device (`ADD_VIRTUAL_CHANNEL`) and wants a fresh
    /// per-device `URBDRC` DVC. The loop opens one with a [`UrbdrcDeviceProcessor`].
    OpenDeviceChannel,
    /// Ship DVC messages (a transfer request originated by a [`UsbHandle`]) on an
    /// already-open per-device channel. The loop DVC-frames them and writes them.
    SendMessages { channel_id: u32, messages: Vec<DvcMessage> },
}

impl fmt::Debug for UrbdrcServerMessage {
    // Manual: `DvcMessage` (a boxed encoder) isn't `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDeviceChannel => write!(f, "OpenDeviceChannel"),
            Self::SendMessages { channel_id, messages } => f
                .debug_struct("SendMessages")
                .field("channel_id", channel_id)
                .field("messages", &messages.len())
                .finish(),
        }
    }
}

/// Adapt any server→client `Encode` PDU to a [`DvcMessage`]. The individual URBDRC
/// PDU types (`TransferInRequest`, `RegisterRequestCallback`, `RimExchangeCapabilityRequest`,
/// `ChannelCreated`, `InterfaceRelease`, …) and the per-channel envelope enums
/// (`UrbdrcServerControlPdu` / `UrbdrcServerDevicePdu`) all implement `Encode`; the
/// DVC layer only adds its own framing around the encoded bytes, so one generic
/// wrapper serves them all. (Post pin bump the pinned crate impls `DvcEncode` on the
/// wire types directly — this wrapper just uniformly covers those AND the envelope
/// enums.) Mirrors `OwnedAudioPdu` in `multitransport/audio_dvc.rs`.
struct UsbDvc<T: Encode>(T);

impl<T: Encode> Encode for UsbDvc<T> {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.0.encode(dst)
    }

    fn name(&self) -> &'static str {
        self.0.name()
    }

    fn size(&self) -> usize {
        self.0.size()
    }
}

impl<T: Encode + Send> DvcEncode for UsbDvc<T> {}

fn dvc_msg<T: Encode + Send + 'static>(pdu: T) -> DvcMessage {
    Box::new(UsbDvc(pdu))
}

/// Build a `RIMCALL_RELEASE` message as a first-class [`InterfaceRelease`] PDU.
/// FreeRDP's `urbdrc_device_control_channel` uses it as a ready barrier: on the
/// main channel it triggers `ADD_VIRTUAL_CHANNEL`; on a per-device channel it
/// triggers `ADD_DEVICE`. `iface_id` is the notify-client interface (`0x2`) OR-ed
/// with the stream-id-proxy mask (`0x1` in the top two bits) — the exact value the
/// old `SharedMsgHeader { interface_id: NOTIFY_CLIENT, mask: StreamIdProxy }`
/// encoded, now that the pinned crate folds the mask into a pre-combined
/// `SharedMsgHeader.iface_id: u32` and keeps `Mask` `pub(crate)`.
const RIMCALL_RELEASE_IFACE_ID: u32 = 0x4000_0002;

fn rimcall_release(msg_id: u32) -> DvcMessage {
    dvc_msg(InterfaceRelease {
        iface_id: RIMCALL_RELEASE_IFACE_ID,
        msg_id,
    })
}

/// Best-effort identify a client PDU by decoding just its shared header (used to
/// log meaningfully when the full body decode fails). Uses the pinned header
/// decoder only — no parallel wire parsing.
fn peek_function_id(payload: &[u8]) -> Option<FunctionId> {
    decode::<SharedMsgHeader>(payload).ok().and_then(|h| h.function_id)
}

/// The fields of a standard 18-byte USB device descriptor we surface. Keeps the
/// descriptor byte-layout knowledge in one typed place instead of inline offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb_version: u16, // bcdUSB
    pub device_class: u8,
    pub vendor_id: u16,      // idVendor
    pub product_id: u16,     // idProduct
    pub device_release: u16, // bcdDevice
}

impl DeviceDescriptor {
    /// Parse a device descriptor (USB spec 9.6.1). Returns `None` if `buf` is too
    /// short or isn't a device descriptor.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        // bLength @0, bDescriptorType @1, then the fixed 18-byte layout.
        if buf.len() < 18 || buf[1] != USB_DESCRIPTOR_TYPE_DEVICE {
            return None;
        }
        let u16le = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
        Some(Self {
            usb_version: u16le(2),
            device_class: buf[4],
            vendor_id: u16le(8),
            product_id: u16le(10),
            device_release: u16le(12),
        })
    }
}

/// The parts of a `URB_COMPLETION` a waiter needs: `output_buffer` (the
/// transferred bytes — a descriptor, or bulk-IN data) and `urb_result` (the raw
/// TS_URB result payload, which a typed request like SelectConfiguration
/// re-decodes for its pipe handles). `hresult` is the client's NTSTATUS-ish code.
#[derive(Default)]
pub struct UrbReply {
    pub output_buffer: Vec<u8>,
    pub urb_result: Vec<u8>,
    pub hresult: u32,
}

/// Correlates outstanding transfer requests with their `URB_COMPLETION`s by the
/// request id echoed in the completion. Mirrors `rdpdr::IoRouter`. Cheaply
/// clonable (shared inner); the request id doubles as the TS_URB `RequestId`
/// (31-bit — the counter is masked to fit).
#[derive(Clone, Default)]
pub struct UsbRouter {
    inner: Arc<UsbRouterInner>,
}

#[derive(Default)]
struct UsbRouterInner {
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<UrbReply>>>,
}

impl UsbRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a 31-bit request id + a receiver for its completion.
    fn register(&self) -> (u32, oneshot::Receiver<UrbReply>) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed) & 0x7FFF_FFFF;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Deliver a completion to the matching waiter (dropped if none).
    fn deliver(&self, req_id: u32, reply: UrbReply) {
        if let Some(tx) = self.inner.pending.lock().unwrap().remove(&req_id) {
            let _ = tx.send(reply);
        } else {
            debug!(req_id, "URBDRC: URB completion with no waiter (dropped)");
        }
    }
}

/// Async handle onto a single redirected USB device's per-device DVC. Clone it to
/// drive transfers from anywhere (the future macOS UserHCI side); each method
/// registers a completion waiter, ships the request through the server event loop
/// (`ServerEvent::Urbdrc(SendMessages)`) onto the device channel, and awaits the
/// matching `URB_COMPLETION`. Mirrors `rdpdr::RdpdrHandle`.
#[derive(Clone)]
pub struct UsbHandle {
    sender: mpsc::UnboundedSender<ServerEvent>,
    router: UsbRouter,
    /// The device's per-device DVC channel id (transfers ride this channel).
    channel_id: u32,
    /// The device's interface id (addresses the device in each request header).
    device_iface: InterfaceId,
    /// The client's device-instance id (the Windows device-instance path). Stable
    /// per physical device, so the presenting side dedups on it — a client that
    /// announces one device on two channels yields two handles with the same id,
    /// and only the first should get a controller. Empty if the client omitted it.
    device_instance_id: Arc<str>,
    /// Resolves when the owning device processor is dropped (the DVC channel /
    /// connection went away), so the presenting side can tear its controller down.
    closed: watch::Receiver<bool>,
}

impl UsbHandle {
    fn new(
        sender: mpsc::UnboundedSender<ServerEvent>,
        router: UsbRouter,
        channel_id: u32,
        device_iface: InterfaceId,
        device_instance_id: Arc<str>,
        closed: watch::Receiver<bool>,
    ) -> Self {
        Self {
            sender,
            router,
            channel_id,
            device_iface,
            device_instance_id,
            closed,
        }
    }

    /// The client's device-instance id — a per-physical-device identity the
    /// presenting side dedups on (empty if the client didn't send one).
    pub fn device_instance_id(&self) -> &str {
        &self.device_instance_id
    }

    /// Await the device going away, whichever way it goes: the client closing the
    /// per-device channel (unplug/reset — the processor's `close()` flips the
    /// `watch` value to `true`) or the whole connection dropping (the processor is
    /// dropped, closing the `watch` sender). Used by the presenting side to stop
    /// driving + destroy its controller. `wait_for` (not `changed`) is load-bearing:
    /// a receiver cloned AFTER the flip considers the current value seen, so
    /// `changed()` would hang for transfers raised after the close.
    pub async fn closed(&self) {
        let mut rx = self.closed.clone();
        // Ok(_) = value flipped to true; Err = sender dropped. Either way: gone.
        let _ = rx.wait_for(|closed| *closed).await;
    }

    /// Await a transfer completion, racing it against the device going away. The
    /// race is LOAD-BEARING: the pending oneshot sender lives in the router map,
    /// which this handle's own `Arc` keeps alive — so on disconnect the completion
    /// never arrives AND the sender is never dropped, and a bare `rx.await` would
    /// pend forever (wedging the presenting driver mid-transfer, leaking the
    /// controller + the dedup slot until process restart). `biased` so a completion
    /// that raced the close is still delivered.
    async fn await_reply(&self, rx: oneshot::Receiver<UrbReply>, what: &'static str) -> Result<UrbReply> {
        tokio::select! {
            biased;
            reply = rx => reply.with_context(|| format!("URBDRC: connection closed before {what} completion")),
            () = self.closed() => anyhow::bail!("URBDRC: device channel closed while awaiting {what} completion"),
        }
    }

    /// Fetch a descriptor via a `GET_DESCRIPTOR` control transfer (IN) and return
    /// its raw bytes. `desc_type`/`index`/`lang_id` are the standard
    /// `SETUP.wValue`/`wIndex`; `max_len` bounds the reply.
    pub async fn get_descriptor(&self, desc_type: u8, index: u8, lang_id: u16, max_len: u32) -> Result<Vec<u8>> {
        let (req_id, rx) = self.router.register();
        let messages = get_descriptor_request(self.device_iface, req_id, desc_type, index, lang_id, max_len);
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        let reply = self.await_reply(rx, "GET_DESCRIPTOR").await?;
        if reply.hresult != 0 {
            // A device may legitimately stall an unsupported descriptor request; the
            // caller turns this into a stall (NOT a success-with-0-bytes, which makes
            // the local kernel retry the same request instead of moving on).
            anyhow::bail!("URBDRC: GET_DESCRIPTOR failed (hresult {:#010x})", reply.hresult);
        }
        Ok(reply.output_buffer)
    }

    /// Convenience: fetch the 18-byte device descriptor and parse it.
    pub async fn device_descriptor(&self) -> Result<DeviceDescriptor> {
        let bytes = self
            .get_descriptor(USB_DESCRIPTOR_TYPE_DEVICE, 0, 0, USB_DEVICE_DESCRIPTOR_LEN)
            .await?;
        DeviceDescriptor::parse(&bytes).context("URBDRC: malformed device descriptor")
    }

    /// Select the device's configuration on the client, opening its endpoints'
    /// pipes, and return them ([`UsbPipe`] per non-default endpoint). This is the
    /// prerequisite for any bulk/interrupt transfer — those address the endpoint by
    /// the client `pipe_handle` returned here, which only `SelectConfiguration`
    /// yields. `config_bytes` is the full configuration descriptor (the config
    /// header plus its interface + endpoint descriptors).
    pub async fn select_configuration(&self, config_bytes: &[u8]) -> Result<Vec<UsbPipe>> {
        let (config_desc, ifaces) = parse_configuration(config_bytes)?;
        debug!(
            device_iface = %self.device_iface,
            total_len = config_desc.total_length,
            interfaces = ifaces.len(),
            "URBDRC SelectConfiguration"
        );
        let (req_id, rx) = self.router.register();
        let messages = select_config_request(self.device_iface, req_id, config_desc, ifaces);
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        let reply = self.await_reply(rx, "SelectConfiguration").await?;
        if reply.hresult != 0 {
            anyhow::bail!("URBDRC: SelectConfiguration failed (hresult {:#010x})", reply.hresult);
        }
        let result = TsUrbSelectConfigResult::decode(&mut ReadCursor::new(&reply.urb_result))
            .map_err(|e| anyhow::anyhow!("URBDRC: malformed SelectConfiguration result: {e}"))?;
        Ok(result
            .interface
            .iter()
            .flat_map(|iface| iface.pipes.iter())
            .map(|pipe| UsbPipe {
                endpoint_address: pipe.endpoint_address,
                pipe_handle: pipe.pipe_handle,
                is_bulk: matches!(pipe.pipe_type, UsbdPipeType::Bulk),
            })
            .collect())
    }

    /// Bulk/interrupt **IN** transfer on `pipe_handle`: read up to `length` bytes
    /// from the device and return what arrived (may be short). `pipe_handle` comes
    /// from [`select_configuration`](Self::select_configuration).
    pub async fn bulk_transfer_in(&self, pipe_handle: u32, length: u32) -> Result<Vec<u8>> {
        let (req_id, rx) = self.router.register();
        let messages = bulk_transfer_request(self.device_iface, req_id, pipe_handle, Dir::In, length, Vec::new());
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        let reply = self.await_reply(rx, "bulk-IN").await?;
        if reply.hresult != 0 {
            anyhow::bail!("URBDRC: bulk IN failed (hresult {:#010x})", reply.hresult);
        }
        Ok(reply.output_buffer)
    }

    /// Bulk/interrupt **OUT** transfer on `pipe_handle`: write `data` to the device.
    /// Returns the number of bytes submitted.
    pub async fn bulk_transfer_out(&self, pipe_handle: u32, data: Vec<u8>) -> Result<usize> {
        let n = data.len();
        let (req_id, rx) = self.router.register();
        let messages = bulk_transfer_request(self.device_iface, req_id, pipe_handle, Dir::Out, 0, data);
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        let reply = self.await_reply(rx, "bulk-OUT").await?;
        if reply.hresult != 0 {
            anyhow::bail!("URBDRC: bulk OUT failed (hresult {:#010x})", reply.hresult);
        }
        Ok(n)
    }

    /// Forward an EP0 **control-IN** request (device→host) that isn't a standard
    /// device-recipient `GET_DESCRIPTOR` — e.g. a class request like mass-storage
    /// `Get Max LUN` (`0xa1/0xfe`) or a HID report-descriptor read (`0x81/0x06`) —
    /// to the client's device via a generic `URB_FUNCTION_CONTROL_TRANSFER_EX` on
    /// the default control pipe, returning the data-stage bytes. `setup` is the raw
    /// 8-byte SETUP (recipient/type bits preserved, which the dedicated descriptor
    /// URB can't carry); `max_len` bounds the reply (the data-stage buffer size,
    /// i.e. `wLength`).
    pub async fn control_transfer_in(&self, setup: [u8; 8], max_len: u32) -> Result<Vec<u8>> {
        let (req_id, rx) = self.router.register();
        let messages = control_transfer_request(self.device_iface, req_id, setup, Dir::In, max_len, Vec::new());
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        let reply = self.await_reply(rx, "control-IN").await?;
        if reply.hresult != 0 {
            anyhow::bail!("URBDRC: control IN failed (hresult {:#010x})", reply.hresult);
        }
        Ok(reply.output_buffer)
    }

    /// Forward an EP0 **control-OUT** request (host→device) to the client's device
    /// via a generic `URB_FUNCTION_CONTROL_TRANSFER_EX` on the default control pipe —
    /// e.g. a mass-storage Bulk-Only Reset or `Clear-Feature(ENDPOINT_HALT)` the local
    /// kernel issued, which must reach the real device to keep its state in sync.
    /// `setup` is the raw 8-byte USB SETUP packet; `data` is any host→device payload
    /// (empty for the no-data requests, which is the common case).
    pub async fn control_transfer_out(&self, setup: [u8; 8], data: Vec<u8>) -> Result<()> {
        let (req_id, rx) = self.router.register();
        let messages = control_transfer_request(self.device_iface, req_id, setup, Dir::Out, 0, data);
        self.sender
            .send(ServerEvent::Urbdrc(UrbdrcServerMessage::SendMessages {
                channel_id: self.channel_id,
                messages,
            }))
            .ok()
            .context("URBDRC: server event loop gone")?;
        let reply = self.await_reply(rx, "control-OUT").await?;
        if reply.hresult != 0 {
            anyhow::bail!("URBDRC: control OUT failed (hresult {:#010x})", reply.hresult);
        }
        Ok(())
    }
}

/// Direction of a forwarded transfer: which request PDU carries it
/// (IN → `TransferInRequest`, OUT → `TransferOutRequest`) and which
/// `USBD_TRANSFER_DIRECTION` flag the URB must carry (the codec enforces the match).
#[derive(Clone, Copy)]
enum Dir {
    In,
    Out,
}

/// Build a bulk/interrupt transfer request: a `RegisterRequestCallback` + a
/// `TransferInRequest` (IN, `output_buffer_size = length`) or `TransferOutRequest`
/// (OUT, `output_buffer = data`) carrying a `TS_URB_BULK_OR_INTERRUPT_TRANSFER` on
/// `pipe_handle`. The transfer-flag direction bit MUST match the request direction
/// (the codec enforces it). Mirrors [`get_descriptor_request`].
fn bulk_transfer_request(
    device_iface: InterfaceId,
    req_id: u32,
    pipe_handle: u32,
    dir: Dir,
    in_length: u32,
    out_data: Vec<u8>,
) -> Vec<DvcMessage> {
    let reg = RegisterRequestCallback {
        msg_id: 0,
        udev_iface: device_iface,
        request_completion: Some(device_iface),
    };
    let ts_req_id = RequestIdTransferInOut::try_from(req_id).expect("router ids are masked to 31 bits");
    let transfer_flags = match dir {
        // Bulk/interrupt IN: accept a short packet (see USBD_SHORT_TRANSFER_OK) — video
        // payloads and HID reports are routinely shorter than the read buffer.
        Dir::In => USBD_TRANSFER_DIRECTION_IN | USBD_SHORT_TRANSFER_OK,
        Dir::Out => 0,
    };
    let urb = TsUrbBulkOrInterruptTransfer {
        pipe_handle,
        transfer_flags,
    };
    // The TS_URB header is now hoisted out of the payload onto TsUrbIn/TsUrbOut.
    // `ts_urb_size` is recomputed from the payload size at encode time
    // (`encode_with_size`), so the constructed value is a placeholder.
    let header = TsUrbHeader {
        ts_urb_size: 0,
        func: UrbFunction::URB_FUNCTION_BULK_OR_INTERRUPT_TRANSFER,
        req_id: ts_req_id,
        no_ack: false,
    };
    let transfer = match dir {
        Dir::In => dvc_msg(UrbdrcServerDevicePdu::TransferIn(TransferInRequest {
            msg_id: 0,
            udev_iface: device_iface,
            ts_urb: TsUrbIn {
                header,
                kind: TsUrbInKind::BulkInterruptTransfer(urb),
            },
            output_buffer_size: in_length,
        })),
        Dir::Out => dvc_msg(UrbdrcServerDevicePdu::TransferOut(TransferOutRequest {
            msg_id: 0,
            udev_iface: device_iface,
            ts_urb: TsUrbOut {
                header,
                kind: TsUrbOutKind::BulkInterruptTransfer(urb),
            },
            output_buffer: out_data,
        })),
    };
    vec![dvc_msg(UrbdrcServerDevicePdu::RegReqCb(reg)), transfer]
}

/// Build a generic EP0 control transfer request: a `RegisterRequestCallback` + a
/// `TransferInRequest` (IN, `output_buffer_size = in_length`) or
/// `TransferOutRequest` (OUT, `output_buffer = out_data`) carrying a
/// `TS_URB_CONTROL_TRANSFER_EX` on the default control pipe (pipe handle 0 → the
/// client maps it to endpoint 0 — per MS-RDPEUSB `EndpointAddress = PipeHandle &
/// 0xff`). The raw SETUP rides verbatim, so recipient/type bits (class/vendor,
/// interface/endpoint-directed) are preserved — which the dedicated descriptor URB
/// can't do. The direction flag must match the request PDU (the codec enforces it).
fn control_transfer_request(
    device_iface: InterfaceId,
    req_id: u32,
    setup: [u8; 8],
    dir: Dir,
    in_length: u32,
    out_data: Vec<u8>,
) -> Vec<DvcMessage> {
    let reg = RegisterRequestCallback {
        msg_id: 0,
        udev_iface: device_iface,
        request_completion: Some(device_iface),
    };
    let ts_req_id = RequestIdTransferInOut::try_from(req_id).expect("router ids are masked to 31 bits");
    let transfer_flags = match dir {
        Dir::In => USBD_TRANSFER_DIRECTION_IN,
        Dir::Out => 0,
    };
    // Map the standard 8-byte SETUP packet to the matching **typed** URB function
    // (URB_FUNCTION_CLASS_INTERFACE, GET_DESCRIPTOR_FROM_INTERFACE, …). Real Windows
    // USB drivers issue these typed URBs, and mstsc's URBDRC only accepts them —
    // the generic URB_FUNCTION_CONTROL_TRANSFER_EX is rejected with 0x80070057
    // (E_INVALIDARG). FreeRDP accepted CONTROL_TRANSFER_EX, which is why it "worked"
    // there; kept as the fallback for any request we don't map. The header is built
    // here from the returned func (it's hoisted onto TsUrbIn/TsUrbOut now).
    let (func, in_kind) = setup_to_typed_urb(setup, transfer_flags);
    let header = TsUrbHeader {
        ts_urb_size: 0, // recomputed from payload size on encode
        func,
        req_id: ts_req_id,
        no_ack: false,
    };
    // A few typed control URBs are no-data-stage requests the MS-RDPEUSB codec only
    // accepts inside a TRANSFER_IN_REQUEST (with OutputBufferSize 0), even though the
    // underlying USB transfer is host->device (Dir::Out) — the URB *function* carries the
    // direction, not the envelope. TS_URB_CONTROL_FEATURE_REQUEST (SET/CLEAR_FEATURE) is
    // the one that reaches here as a control-OUT: mstsc issues
    // SET_FEATURE(DEVICE_REMOTE_WAKEUP) when the Xbox controller's Guide button is pressed.
    // The new codec enforces this at the type level — TsUrbOutKind has no CtlFeatReq — so
    // a feature request MUST ride TRANSFER_IN, exactly as the old force_transfer_in did.
    let force_transfer_in = matches!(in_kind, TsUrbInKind::CtlFeatReq(_));
    let transfer = if force_transfer_in || matches!(dir, Dir::In) {
        dvc_msg(UrbdrcServerDevicePdu::TransferIn(TransferInRequest {
            msg_id: 0,
            udev_iface: device_iface,
            ts_urb: TsUrbIn { header, kind: in_kind },
            output_buffer_size: if force_transfer_in { 0 } else { in_length },
        }))
    } else {
        // Dir::Out: the typed kinds a control-OUT produces all have a TRANSFER_OUT form;
        // anything without one falls back to TRANSFER_IN so the request is never dropped.
        match in_kind_to_out_kind(in_kind) {
            Ok(out_kind) => dvc_msg(UrbdrcServerDevicePdu::TransferOut(TransferOutRequest {
                msg_id: 0,
                udev_iface: device_iface,
                ts_urb: TsUrbOut { header, kind: out_kind },
                output_buffer: out_data,
            })),
            Err(in_kind) => dvc_msg(UrbdrcServerDevicePdu::TransferIn(TransferInRequest {
                msg_id: 0,
                udev_iface: device_iface,
                ts_urb: TsUrbIn { header, kind: in_kind },
                output_buffer_size: in_length,
            })),
        }
    };
    vec![dvc_msg(UrbdrcServerDevicePdu::RegReqCb(reg)), transfer]
}

/// Translate a standard 8-byte USB SETUP packet into the specific **typed** URB
/// function real Windows uses for that request (e.g. a class request to an
/// interface → `URB_FUNCTION_CLASS_INTERFACE`), because mstsc's URBDRC rejects the
/// generic `URB_FUNCTION_CONTROL_TRANSFER_EX` with `0x80070057`. Anything we don't
/// have a typed mapping for falls back to `CONTROL_TRANSFER_EX` (FreeRDP-friendly,
/// and better than dropping the request). `setup` = `[bmRequestType, bRequest,
/// wValueLo, wValueHi, wIndexLo, wIndexHi, wLengthLo, wLengthHi]`.
fn setup_to_typed_urb(setup: [u8; 8], transfer_flags: u32) -> (UrbFunction, TsUrbInKind) {
    let bm_request_type = setup[0];
    let b_request = setup[1];
    let w_value = u16::from_le_bytes([setup[2], setup[3]]);
    let w_index = u16::from_le_bytes([setup[4], setup[5]]);
    let w_length = u16::from_le_bytes([setup[6], setup[7]]);

    // bmRequestType: bits 6:5 = type (0 std / 1 class / 2 vendor), bits 4:0 = recipient
    // (0 device / 1 interface / 2 endpoint / 3 other).
    let req_type = (bm_request_type >> 5) & 0x03;
    let recipient = bm_request_type & 0x1f;

    // The TS_URB header (which carries the URB function) is now hoisted onto TsUrbIn/
    // TsUrbOut, so this returns (func, kind) and the caller builds the header.
    let control_transfer_ex = || {
        (
            UrbFunction::URB_FUNCTION_CONTROL_TRANSFER_EX,
            TsUrbInKind::CtlTransferEx(TsUrbControlTransferEx {
                pipe: 0, // default control endpoint (EP0)
                transfer_flags,
                timeout: 0,
                setup_packet: SetupPacket {
                    request_type: bm_request_type,
                    request: b_request,
                    value: w_value,
                    index: w_index,
                    length: w_length,
                },
            }),
        )
    };

    match req_type {
        // Class (1) / vendor (2) request → URB_FUNCTION_{CLASS,VENDOR}_{DEVICE,INTERFACE,ENDPOINT,OTHER}.
        1 | 2 => {
            use UrbFunction as F;
            let func = match (req_type, recipient) {
                (1, 0) => F::URB_FUNCTION_CLASS_DEVICE,
                (1, 1) => F::URB_FUNCTION_CLASS_INTERFACE,
                (1, 2) => F::URB_FUNCTION_CLASS_ENDPOINT,
                (1, _) => F::URB_FUNCTION_CLASS_OTHER,
                (2, 0) => F::URB_FUNCTION_VENDOR_DEVICE,
                (2, 1) => F::URB_FUNCTION_VENDOR_INTERFACE,
                (2, 2) => F::URB_FUNCTION_VENDOR_ENDPOINT,
                (_, _) => F::URB_FUNCTION_VENDOR_OTHER,
            };
            (
                func,
                TsUrbInKind::VendorClassReq(TsUrbControlVendorClassRequest {
                    transfer_flags,
                    request: b_request,
                    value: w_value,
                    index: w_index,
                }),
            )
        }
        // Standard request (0): pick the typed function by bRequest.
        0 => {
            use UrbFunction as F;
            match b_request {
                // GET_DESCRIPTOR (0x06) — wValue = (descType << 8) | index, wIndex = langid.
                0x06 => {
                    let func = match recipient {
                        1 => F::URB_FUNCTION_GET_DESCRIPTOR_FROM_INTERFACE,
                        2 => F::URB_FUNCTION_GET_DESCRIPTOR_FROM_ENDPOINT,
                        0 => F::URB_FUNCTION_GET_DESCRIPTOR_FROM_DEVICE,
                        _ => return control_transfer_ex(),
                    };
                    (
                        func,
                        TsUrbInKind::CtlDescReq(TsUrbControlDescRequest {
                            index: setup[2],
                            desc_type: setup[3],
                            lang_id: w_index,
                        }),
                    )
                }
                // SET_DESCRIPTOR (0x07).
                0x07 => {
                    let func = match recipient {
                        1 => F::URB_FUNCTION_SET_DESCRIPTOR_TO_INTERFACE,
                        2 => F::URB_FUNCTION_SET_DESCRIPTOR_TO_ENDPOINT,
                        0 => F::URB_FUNCTION_SET_DESCRIPTOR_TO_DEVICE,
                        _ => return control_transfer_ex(),
                    };
                    (
                        func,
                        TsUrbInKind::CtlDescReq(TsUrbControlDescRequest {
                            index: setup[2],
                            desc_type: setup[3],
                            lang_id: w_index,
                        }),
                    )
                }
                // CLEAR_FEATURE (0x01) / SET_FEATURE (0x03) — wValue = feature selector.
                0x01 | 0x03 => {
                    let func = match (b_request, recipient) {
                        (0x03, 0) => F::URB_FUNCTION_SET_FEATURE_TO_DEVICE,
                        (0x03, 1) => F::URB_FUNCTION_SET_FEATURE_TO_INTERFACE,
                        (0x03, 2) => F::URB_FUNCTION_SET_FEATURE_TO_ENDPOINT,
                        (0x01, 0) => F::URB_FUNCTION_CLEAR_FEATURE_TO_DEVICE,
                        (0x01, 1) => F::URB_FUNCTION_CLEAR_FEATURE_TO_INTERFACE,
                        (0x01, 2) => F::URB_FUNCTION_CLEAR_FEATURE_TO_ENDPOINT,
                        _ => return control_transfer_ex(),
                    };
                    (
                        func,
                        TsUrbInKind::CtlFeatReq(TsUrbControlFeatRequest {
                            feat_selector: w_value,
                            index: w_index,
                        }),
                    )
                }
                // GET_STATUS (0x00) — wIndex = interface / endpoint.
                0x00 => {
                    let func = match recipient {
                        0 => F::URB_FUNCTION_GET_STATUS_FROM_DEVICE,
                        1 => F::URB_FUNCTION_GET_STATUS_FROM_INTERFACE,
                        2 => F::URB_FUNCTION_GET_STATUS_FROM_ENDPOINT,
                        _ => return control_transfer_ex(),
                    };
                    (
                        func,
                        TsUrbInKind::CtlGetStatus(TsUrbControlGetStatusRequest { index: w_index }),
                    )
                }
                // GET_CONFIGURATION (0x08).
                0x08 => (
                    F::URB_FUNCTION_GET_CONFIGURATION,
                    TsUrbInKind::CtlGetConfig(TsUrbControlGetConfigRequest),
                ),
                // GET_INTERFACE (0x0a) — wIndex = interface.
                0x0a => (
                    F::URB_FUNCTION_GET_INTERFACE,
                    TsUrbInKind::CtlGetIface(TsUrbControlGetInterfaceRequest { interface: w_index }),
                ),
                // Everything else standard (SET_CONFIGURATION/SET_INTERFACE/SET_ADDRESS are
                // handled elsewhere and shouldn't reach here) → generic fallback.
                _ => control_transfer_ex(),
            }
        }
        // Unreachable (req_type is 2 bits and 3 is caught by the vendor arm), but be safe.
        _ => control_transfer_ex(),
    }
}

/// Convert a TRANSFER_IN URB kind to its TRANSFER_OUT counterpart for a host→device
/// control transfer. Only the kinds a `Dir::Out` control transfer actually produces
/// (`VendorClassReq` / `CtlDescReq` / `CtlTransferEx` — plus the transfer kinds for
/// completeness) exist in [`TsUrbOutKind`]; a feature request is never `Dir::Out`
/// (the caller's `force_transfer_in` handles it, which is also why `TsUrbOutKind` has
/// no `CtlFeatReq`). Any kind without an OUT form is returned as `Err` so the caller
/// can fall back to TRANSFER_IN rather than dropping the request.
fn in_kind_to_out_kind(kind: TsUrbInKind) -> Result<TsUrbOutKind, TsUrbInKind> {
    Ok(match kind {
        TsUrbInKind::VendorClassReq(x) => TsUrbOutKind::VendorClassReq(x),
        TsUrbInKind::CtlDescReq(x) => TsUrbOutKind::CtlDescReq(x),
        TsUrbInKind::CtlTransferEx(x) => TsUrbOutKind::CtlTransferEx(x),
        TsUrbInKind::CtlTransfer(x) => TsUrbOutKind::CtlTransfer(x),
        TsUrbInKind::BulkInterruptTransfer(x) => TsUrbOutKind::BulkInterruptTransfer(x),
        TsUrbInKind::IsochTransfer(x) => TsUrbOutKind::IsochTransfer(x),
        other => return Err(other),
    })
}

/// One endpoint pipe opened by [`UsbHandle::select_configuration`]: its USB
/// address (bit 7 = IN) and the client `pipe_handle` that addresses transfers on
/// it.
#[derive(Debug, Clone, Copy)]
pub struct UsbPipe {
    pub endpoint_address: u8,
    pub pipe_handle: u32,
    pub is_bulk: bool,
}

/// Parse a full configuration descriptor into the [`UsbConfigDesc`] header + one
/// [`TsUsbdInterfaceInfo`] per interface (each carrying a [`TsUsbdPipeInfo`] per
/// endpoint, in descriptor order — the client returns each pipe's handle in the
/// same order). Walks the standard TLV descriptor chain (config 9.6.3).
fn parse_configuration(buf: &[u8]) -> Result<(UsbConfigDesc, Vec<TsUsbdInterfaceInfo>)> {
    if buf.len() < 9 || buf[1] != USB_DESCRIPTOR_TYPE_CONFIGURATION {
        anyhow::bail!("URBDRC: not a configuration descriptor ({} bytes)", buf.len());
    }
    let u16le = |i: usize| u16::from_le_bytes([buf[i], buf[i + 1]]);
    let total_length = u16le(2);
    // Carry the FULL configuration descriptor (header + all interface/endpoint/CS
    // descriptors) — real Windows/mstsc rejects a header-only descriptor with
    // 0x80070057 when ConfigurationDescriptorIsValid is set. `buf` is the descriptor
    // fetched to `wTotalLength`; keep bytes 9..total_length as the trailing part.
    let desc_end = usize::from(total_length).min(buf.len()).max(9);
    let config = UsbConfigDesc {
        length: buf[0],
        descriptor_type: buf[1],
        total_length,
        num_interfaces: buf[4],
        configuration_value: buf[5],
        configuration: buf[6],
        attributes: buf[7],
        max_power: buf[8],
        trailing: buf[9..desc_end].to_vec(),
    };
    // SELECT_CONFIGURATION needs exactly ONE interface-information entry per
    // interface NUMBER, at its **default alternate setting (0)** — the state a
    // freshly-configured device is in (drivers later switch to a streaming/bandwidth
    // alt via SELECT_INTERFACE). A config descriptor enumerates every
    // (interface, alt-setting) pair, so we keep only each interface's alt-0
    // descriptor and the endpoints belonging to that alt setting. Emitting one entry
    // per descriptor — including alt settings — produces DUPLICATE interface numbers,
    // which real Windows / mstsc rejects with 0x80070057 (E_INVALIDARG); a
    // single-interface, no-alt-setting device (a mass-storage drive) never tripped it,
    // which is why it only showed up on a multi-alt device (UVC camera / UAC audio).
    let mut ifaces: Vec<TsUsbdInterfaceInfo> = Vec::new();
    // Whether the interface descriptor we're currently walking under is alt-0 (so its
    // endpoints count) or a non-default alt setting (skip it and its endpoints).
    let mut in_alt0 = false;
    let mut i = 9usize;
    while i + 2 <= buf.len() {
        let len = buf[i] as usize;
        let dtype = buf[i + 1];
        if len == 0 || i + len > buf.len() {
            break;
        }
        match dtype {
            USB_DESCRIPTOR_TYPE_INTERFACE if len >= 9 => {
                let interface_number = buf[i + 2];
                let alternate_setting = buf[i + 3];
                if alternate_setting == 0 {
                    ifaces.push(TsUsbdInterfaceInfo {
                        interface_number,
                        alternate_setting: 0,
                        ts_usbd_pipe_info: Vec::new(),
                    });
                    in_alt0 = true;
                } else {
                    in_alt0 = false;
                }
            }
            USB_DESCRIPTOR_TYPE_ENDPOINT if len >= 7 && in_alt0 => {
                if let Some(iface) = ifaces.last_mut() {
                    iface.ts_usbd_pipe_info.push(TsUsbdPipeInfo {
                        max_packet_size: u16le(i + 4),
                        max_transfer_size: 64 * 1024,
                        pipe_flags: 0,
                    });
                }
            }
            _ => {}
        }
        i += len;
    }
    Ok((config, ifaces))
}

/// Build a `SELECT_CONFIGURATION` request (a `RegisterRequestCallback` naming the
/// completion interface + a `TransferInRequest` carrying the SelectConfig URB with
/// `output_buffer_size = 0`). Mirrors [`get_descriptor_request`].
fn select_config_request(
    device_iface: InterfaceId,
    req_id: u32,
    desc: UsbConfigDesc,
    ifaces: Vec<TsUsbdInterfaceInfo>,
) -> Vec<DvcMessage> {
    let reg = RegisterRequestCallback {
        msg_id: 0,
        udev_iface: device_iface,
        request_completion: Some(device_iface),
    };
    let ts_req_id = RequestIdTransferInOut::try_from(req_id).expect("router ids are masked to 31 bits");
    let select = TransferInRequest {
        msg_id: 0,
        udev_iface: device_iface,
        ts_urb: TsUrbIn {
            header: TsUrbHeader {
                ts_urb_size: 0, // recomputed from payload size on encode
                func: UrbFunction::URB_FUNCTION_SELECT_CONFIGURATION,
                req_id: ts_req_id,
                no_ack: false,
            },
            kind: TsUrbInKind::SelectConfig(TsUrbSelectConfig {
                usbd_ifaces: ifaces,
                desc: Some(desc),
            }),
        },
        output_buffer_size: 0,
    };
    vec![
        dvc_msg(UrbdrcServerDevicePdu::RegReqCb(reg)),
        dvc_msg(UrbdrcServerDevicePdu::TransferIn(select)),
    ]
}

/// Build a `GET_DESCRIPTOR` control-transfer request (IN): a
/// `RegisterRequestCallback` (naming the completion interface) followed by a
/// `TransferInRequest` whose TS_URB `RequestId` is `req_id` (echoed in the
/// completion, so the [`UsbRouter`] can correlate it).
fn get_descriptor_request(
    device_iface: InterfaceId,
    req_id: u32,
    desc_type: u8,
    index: u8,
    lang_id: u16,
    max_len: u32,
) -> Vec<DvcMessage> {
    // Any unique id works for the completion interface; the completion decode is
    // interface-agnostic, so reuse the device's own interface id.
    let reg = RegisterRequestCallback {
        msg_id: 0,
        udev_iface: device_iface,
        request_completion: Some(device_iface),
    };
    let ts_req_id = RequestIdTransferInOut::try_from(req_id).expect("router ids are masked to 31 bits");
    let get_desc = TransferInRequest {
        msg_id: 0,
        udev_iface: device_iface,
        ts_urb: TsUrbIn {
            header: TsUrbHeader {
                ts_urb_size: 0, // recomputed from payload size on encode
                func: UrbFunction::URB_FUNCTION_GET_DESCRIPTOR_FROM_DEVICE,
                req_id: ts_req_id,
                no_ack: false,
            },
            kind: TsUrbInKind::CtlDescReq(TsUrbControlDescRequest {
                index,
                desc_type,
                lang_id,
            }),
        },
        output_buffer_size: max_len,
    };
    vec![
        dvc_msg(UrbdrcServerDevicePdu::RegReqCb(reg)),
        dvc_msg(UrbdrcServerDevicePdu::TransferIn(get_desc)),
    ]
}

/// Invoked once per redirected device, with a [`UsbHandle`] onto it, when the
/// client announces it (`ADD_DEVICE`). This is the seam to the presenting side:
/// the vendored server exposes the handle, and macrdp's `MacUsb` decides what to
/// do with it (fetch descriptors, drive the macOS UserHCI controller). `Send +
/// Sync` so a device processor on any connection can call it.
pub type UsbDeviceCallback = Arc<dyn Fn(UsbHandle) + Send + Sync>;

/// Per-connection ceiling on server-opened per-device `URBDRC` DVCs. Each client
/// `ADD_VIRTUAL_CHANNEL` opens one channel (never pruned within a connection), so
/// this bounds a hostile/buggy client that spams announcements from growing the
/// DRDYNVC slab without limit. Far above any real device count.
const MAX_DEVICE_CHANNELS: u32 = 32;

/// Main server-side `URBDRC` DVC processor: drives the init handshake and, on each
/// device announcement, asks the event loop to open a per-device channel.
pub struct UrbdrcServer {
    /// Monotonic message id for the top-level request/response pairs we originate.
    next_msg_id: u32,
    /// Event-loop channel, used to request per-device DVC creation. `None` leaves
    /// the processor observe-only (the handshake still runs; no device channels).
    sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    /// Per-connection count of device channels we've asked the loop to open, so a
    /// client that spams `ADD_VIRTUAL_CHANNEL` can't grow the DVC slab unbounded.
    device_channels_opened: u32,
}

impl UrbdrcServer {
    pub fn new() -> Self {
        Self {
            next_msg_id: 1,
            sender: None,
            device_channels_opened: 0,
        }
    }

    /// Build a processor wired to the connection's server-event sender so it can
    /// request per-device channel creation.
    pub fn with_sender(sender: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self {
            next_msg_id: 1,
            sender,
            device_channels_opened: 0,
        }
    }

    fn take_msg_id(&mut self) -> u32 {
        let id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);
        id
    }
}

impl Default for UrbdrcServer {
    fn default() -> Self {
        Self::new()
    }
}

impl_as_any!(UrbdrcServer);

impl DvcProcessor for UrbdrcServer {
    fn channel_name(&self) -> &str {
        URBDRC_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // Kick off the capability exchange (MS-RDPEUSB §3.3.5.1): the server sends
        // RIM_EXCHANGE_CAPABILITY_REQUEST first; the client replies, then (after the
        // CHANNEL_CREATED + RIMCALL_RELEASE barrier) announces its devices.
        info!(channel_id, "URBDRC DVC opened — sending capability request");
        let req = RimExchangeCapabilityRequest {
            msg_id: self.take_msg_id(),
            capability: Capability::RimCapabilityVersion01,
        };
        Ok(vec![dvc_msg(UrbdrcServerControlPdu::Caps(req))])
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Never tear down the session on a URBDRC decode error (opt-in feature). The
        // pinned crate splits the client PDUs by channel role; the main channel speaks
        // the CONTROL set (caps / channel-created / add-virtual-channel).
        let pdu = match decode::<UrbdrcClientControlPdu>(payload) {
            Ok(pdu) => pdu,
            Err(e) => {
                warn!(channel_id, error = %e, "URBDRC main-channel PDU decode failed (tolerated)");
                return Ok(Vec::new());
            }
        };
        match pdu {
            UrbdrcClientControlPdu::Caps(resp) => {
                info!(
                    channel_id,
                    result = format_args!("{:#010x}", resp.result),
                    "URBDRC capability response received — client accepted the exchange"
                );
                // MS-RDPEUSB 3.3.5.1: after the capability exchange the server sends
                // CHANNEL_CREATED. This is the message that makes the client ANNOUNCE
                // its redirected devices (ADD_DEVICE / ADD_VIRTUAL_CHANNEL) — without
                // it the client registers the device locally but never tells us.
                info!(
                    channel_id,
                    "URBDRC sending CHANNEL_CREATED (triggers device announcement)"
                );
                let created = ChannelCreated {
                    msg_id: self.take_msg_id(),
                    direction: Direction::ToClient,
                };
                return Ok(vec![dvc_msg(UrbdrcServerControlPdu::ChanCreated(created))]);
            }
            UrbdrcClientControlPdu::ChanCreated(cc) => {
                debug!(channel_id, direction = ?cc.direction, "URBDRC CHANNEL_CREATED");
                // With the channel-created handshake done, send RIMCALL_RELEASE — the
                // ready barrier (FreeRDP: urbdrc_device_control_channel, INIT_CHANNEL_IN)
                // that makes the client announce its devices via ADD_VIRTUAL_CHANNEL.
                info!(channel_id, "URBDRC sending RIMCALL_RELEASE (device-announce barrier)");
                return Ok(vec![rimcall_release(self.take_msg_id())]);
            }
            UrbdrcClientControlPdu::AddChan(add) => {
                // The client announced a device and wants a per-device channel. We
                // can't open a DVC from here (no DrdynvcServer handle), so ask the
                // event loop to (Phase 3.1a). ADD_DEVICE with the descriptors follows
                // on that new channel.
                info!(
                    channel_id,
                    msg_id = add.msg_id,
                    "URBDRC ADD_VIRTUAL_CHANNEL — requesting a per-device channel"
                );
                if self.device_channels_opened >= MAX_DEVICE_CHANNELS {
                    warn!(
                        channel_id,
                        opened = self.device_channels_opened,
                        "URBDRC: per-connection device-channel cap reached, ignoring ADD_VIRTUAL_CHANNEL"
                    );
                } else if let Some(sender) = self.sender.clone() {
                    if sender
                        .send(ServerEvent::Urbdrc(UrbdrcServerMessage::OpenDeviceChannel))
                        .is_err()
                    {
                        warn!(channel_id, "URBDRC: server event loop gone, cannot open device channel");
                    } else {
                        self.device_channels_opened = self.device_channels_opened.saturating_add(1);
                    }
                } else {
                    debug!(
                        channel_id,
                        "URBDRC observe-only (no sender) — not opening a device channel"
                    );
                }
            }
            // ADD_DEVICE arrives on the per-device channel (UrbdrcDeviceProcessor); the
            // main channel decodes the CONTROL set, which can't represent it — an
            // AddDevice mistakenly on the main channel would decode-fail and be tolerated.
            _ => {
                debug!(channel_id, "URBDRC client PDU (unhandled)");
            }
        }
        Ok(Vec::new())
    }
}

impl DvcServerProcessor for UrbdrcServer {}

/// Per-device `URBDRC` DVC processor. Opened by the event loop in response to
/// `ADD_VIRTUAL_CHANNEL`; on open it sends `RIMCALL_RELEASE` (the
/// `INIT_CHANNEL_OUT` barrier) so the client sends `ADD_DEVICE`.
///
/// Its `process()` is intentionally thin — decode and route: it logs
/// `ADD_DEVICE` and hands the device's [`UsbHandle`] to the presenting side (the
/// [`UsbDeviceCallback`]), routes `URB_COMPLETION`s back through the
/// [`UsbRouter`], and tolerates anything else. It does NOT itself decide what to
/// do with the device — that (fetch descriptors, drive the macOS UserHCI
/// controller) lives in macrdp's `MacUsb`, on the other side of the callback.
pub struct UrbdrcDeviceProcessor {
    /// Connection event sender, used to build [`UsbHandle`]s that ship transfers.
    sender: mpsc::UnboundedSender<ServerEvent>,
    /// Shared with every [`UsbHandle`] we build — completions route back through it.
    router: UsbRouter,
    /// The presenting side; called once with the handle when the device announces.
    device_cb: Option<UsbDeviceCallback>,
    /// One-shot guard so the presenting side is notified at most once per device.
    announced: bool,
    /// Liveness signal handed (as a subscriber) to each [`UsbHandle`]. Dropping
    /// this processor — which the server does on disconnect, when it resets
    /// `static_channels` right after the connection loop returns — drops the
    /// sender, resolving every handle's `closed()`. That's how the presenting
    /// side learns the device went away and tears its controller down.
    alive: watch::Sender<bool>,
    /// Monotonic MS-RDPEUSB `MessageId` for the notification messages this
    /// processor emits on its channel (caps request, CHANNEL_CREATED, RIMCALL_RELEASE).
    next_msg_id: u32,
}

impl UrbdrcDeviceProcessor {
    pub fn new(sender: mpsc::UnboundedSender<ServerEvent>, device_cb: Option<UsbDeviceCallback>) -> Self {
        Self {
            sender,
            router: UsbRouter::new(),
            device_cb,
            announced: false,
            alive: watch::channel(false).0,
            next_msg_id: 1,
        }
    }

    fn take_msg_id(&mut self) -> u32 {
        let id = self.next_msg_id;
        self.next_msg_id += 1;
        id
    }

    /// Hand the presenting side a [`UsbHandle`] onto the newly-announced device.
    fn notify_device(&self, channel_id: u32, device_iface: InterfaceId, device_instance_id: Arc<str>) {
        let handle = UsbHandle::new(
            self.sender.clone(),
            self.router.clone(),
            channel_id,
            device_iface,
            device_instance_id,
            self.alive.subscribe(),
        );
        match &self.device_cb {
            Some(cb) => cb(handle),
            None => debug!(channel_id, "URBDRC device announced but no presenting side is wired"),
        }
    }
}

impl_as_any!(UrbdrcDeviceProcessor);

impl DvcProcessor for UrbdrcDeviceProcessor {
    fn channel_name(&self) -> &str {
        URBDRC_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        // The per-device channel needs the SAME per-channel handshake as the main
        // URBDRC channel: capability exchange → CHANNEL_CREATED → RIMCALL_RELEASE.
        // mstsc REQUIRES it — it keeps the channel open+silent when the server sends
        // nothing, but CLOSES it the instant it receives a RIMCALL_RELEASE (or
        // CHANNEL_CREATED) with no preceding capability exchange (verified live
        // 2026-07-06: with the caps exchange, mstsc completes the handshake and sends
        // ADD_DEVICE; without it, mstsc sends a DVC Close and no device ever arrives).
        // FreeRDP tolerates skipping it (its readiness state is global to the main
        // channel's RIMCALL_RELEASE). So kick off the caps exchange here; process()
        // drives CHANNEL_CREATED then RIMCALL_RELEASE, after which the client sends
        // ADD_DEVICE (the real descriptors).
        info!(
            channel_id,
            "URBDRC device channel opened — sending capability request (per-channel handshake)"
        );
        let req = RimExchangeCapabilityRequest {
            msg_id: self.take_msg_id(),
            capability: Capability::RimCapabilityVersion01,
        };
        // The capability request rides the CAPABILITIES interface (the CONTROL set has
        // no device-scoped counterpart), even though it's sent on the per-device DVC.
        Ok(vec![dvc_msg(UrbdrcServerControlPdu::Caps(req))])
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // A decode error on this opt-in, non-critical channel must NEVER tear down
        // the RDP session (same lesson as the ironrdp-dvc Soft-Sync divergence).
        // Tolerate it, and still recognize ADD_DEVICE from its header. The pinned crate
        // splits the client PDUs by channel role: the per-device channel speaks the
        // DEVICE set (CHANNEL_CREATED / ADD_DEVICE / URB completions / the per-request
        // RIMCALL_RELEASE), EXCEPT the per-channel capability RESPONSE, which rides the
        // CAPABILITIES interface and only decodes as the CONTROL set — so decode DEVICE
        // first, then fall back to CONTROL to catch the caps response (the per-device
        // handshake mstsc REQUIRES — see divergence 16).
        match decode::<UrbdrcClientDevicePdu>(payload) {
            Ok(UrbdrcClientDevicePdu::ChanCreated(cc)) => {
                // Channel-created handshake done — send RIMCALL_RELEASE, the barrier
                // that makes the client send ADD_DEVICE (the descriptors) on THIS channel.
                debug!(channel_id, direction = ?cc.direction, "URBDRC per-device CHANNEL_CREATED reply");
                info!(
                    channel_id,
                    "URBDRC per-device handshake complete — sending RIMCALL_RELEASE for ADD_DEVICE"
                );
                return Ok(vec![rimcall_release(self.take_msg_id())]);
            }
            Ok(UrbdrcClientDevicePdu::AddDev(dev)) => {
                info!(
                    channel_id,
                    usb_device = %dev.usb_device,
                    device_instance_id = %dev.device_instance_id,
                    usb_version = ?dev.usb_device_caps.supported_usb_ver,
                    speed = ?dev.usb_device_caps.device_speed,
                    "URBDRC ADD_DEVICE — real device descriptors received (GO)"
                );
                if !self.announced {
                    self.announced = true;
                    let instance_id: Arc<str> = Arc::from(dev.device_instance_id.to_native_lossy().as_ref());
                    self.notify_device(channel_id, dev.usb_device, instance_id);
                }
            }
            Ok(UrbdrcClientDevicePdu::UrbComp(comp)) => {
                let urb_result = match comp.ts_urb_result.payload {
                    TsUrbResultPayload::Raw(bytes) => bytes,
                    _ => Vec::new(),
                };
                self.router.deliver(
                    comp.req_id.into(),
                    UrbReply {
                        output_buffer: comp.output_buffer,
                        urb_result,
                        hresult: comp.hresult,
                    },
                );
            }
            Ok(UrbdrcClientDevicePdu::UrbCompNoData(comp)) => {
                // No data buffer; wake the waiter so it doesn't hang. The URB result
                // (e.g. SelectConfiguration's pipe handles) still rides here.
                debug!(
                    channel_id,
                    hresult = format_args!("{:#010x}", comp.hresult),
                    "URBDRC URB_COMPLETION_NO_DATA"
                );
                let urb_result = match comp.ts_urb_result.payload {
                    TsUrbResultPayload::Raw(bytes) => bytes,
                    _ => Vec::new(),
                };
                self.router.deliver(
                    comp.req_id.into(),
                    UrbReply {
                        output_buffer: Vec::new(),
                        urb_result,
                        hresult: comp.hresult,
                    },
                );
            }
            Ok(UrbdrcClientDevicePdu::IfaceRelease(_)) => {
                // mstsc sends a RIMCALL_RELEASE (RPC-interface release) on the device
                // interface after each request completes, to release the completion
                // callback we registered with RegisterRequestCallback. It carries no
                // data and needs no action — we don't hold per-request callback state —
                // so recognize and ignore it quietly (real mstsc emits one per transfer,
                // which would otherwise flood the log). FreeRDP doesn't send these. (In
                // the pinned crate this now decodes cleanly as IfaceRelease instead of a
                // header-peeked decode error.)
                debug!(
                    channel_id,
                    "URBDRC RIMCALL_RELEASE (per-request callback release) — ignored"
                );
            }
            Ok(_) => {
                debug!(channel_id, "URBDRC device-channel PDU (unhandled)");
            }
            Err(dev_err) => {
                // Not a device PDU. The per-channel capability RESPONSE rides the
                // CAPABILITIES interface (CONTROL set) — try that before treating it as
                // an error, so the per-device handshake completes on mstsc.
                match decode::<UrbdrcClientControlPdu>(payload) {
                    Ok(UrbdrcClientControlPdu::Caps(resp)) => {
                        // Per-channel capability exchange done — advance to CHANNEL_CREATED,
                        // exactly as the main channel does (see `start`).
                        info!(
                            channel_id,
                            result = format_args!("{:#010x}", resp.result),
                            "URBDRC per-device capability response — sending CHANNEL_CREATED"
                        );
                        let created = ChannelCreated {
                            msg_id: self.take_msg_id(),
                            direction: Direction::ToClient,
                        };
                        return Ok(vec![dvc_msg(UrbdrcServerDevicePdu::ChanCreated(created))]);
                    }
                    _ if peek_function_id(payload) == Some(FunctionId::ADD_DEVICE) => {
                        // The device IS being announced — decode only stumbled on the body
                        // (e.g. an even newer caps value the decoder doesn't name). The
                        // forward works; log it as a GO from the header.
                        warn!(
                            channel_id,
                            error = %dev_err,
                            "URBDRC ADD_DEVICE received — real device announced (GO); caps body not fully parsed"
                        );
                    }
                    _ => {
                        warn!(channel_id, error = %dev_err, "URBDRC device-channel PDU decode failed (tolerated)");
                    }
                }
            }
        }
        Ok(Vec::new())
    }

    fn close(&mut self, channel_id: u32) {
        // The client closed this per-device channel — the redirected device was
        // unplugged or reset on the client. Mark the liveness watch so every
        // UsbHandle's `closed()` resolves: the presenting side tears its controller
        // down (the local device disappears) and releases its dedup slot, so a
        // reset's re-announce presents fresh instead of being skipped as a
        // duplicate of a corpse. (Without this, the stale presentation drove a
        // dead channel forever — observed live as a drive that never came back
        // after resetting behind a flaky hub.)
        info!(
            channel_id,
            "URBDRC per-device channel closed by the client (device unplugged/reset) — releasing the presenting side"
        );
        self.alive.send_replace(true);
    }
}

impl DvcServerProcessor for UrbdrcDeviceProcessor {}

/// Factory installed on [`RdpServer`](crate::RdpServer) to enable server-direction
/// USB redirection. Mirrors the other channel factories; ships inert (the server
/// only advertises `URBDRC` when the factory is `Some`). `ServerEventSender` is a
/// supertrait so the factory captures the connection's event sender and hands it
/// to each built processor (for per-device channel requests).
pub trait UrbdrcServerFactory: ServerEventSender + Send {
    /// Build the per-connection main `URBDRC` DVC processor.
    fn build_processor(&self) -> UrbdrcServer;

    /// The presenting side, called once per redirected device with a [`UsbHandle`]
    /// onto it (`ADD_DEVICE`). `None` (the default) drops the handle — the device
    /// is announced but nothing drives it. macrdp's `MacUsb` returns `Some`.
    fn device_callback(&self) -> Option<UsbDeviceCallback> {
        None
    }
}
