#![cfg_attr(doc, doc = include_str!("../README.md"))]
#![doc(html_logo_url = "https://cdnweb.devolutions.net/images/projects/devolutions/logos/devolutions-icon-shadow.svg")]
#![allow(clippy::arithmetic_side_effects)] // TODO: should we enable this lint back?

pub use {tokio, tokio_rustls};

mod macros;

pub mod autodetect;
mod builder;
mod capabilities;
mod clipboard;
mod display;
mod echo;
mod encoder;
#[cfg(feature = "egfx")]
mod gfx;
mod handler;
#[cfg(feature = "helper")]
mod helper;
#[cfg(feature = "multitransport")]
mod multitransport;
mod rdcamera;
mod rdpdr;
mod rdpeusb;
mod server;
mod sound;

pub use clipboard::CliprdrServerFactory;
pub use display::{
    BitmapUpdate, ColorPointer, DesktopSize, DisplayUpdate, Framebuffer, PixelFormat, RGBAPointer, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
pub use echo::{EchoDvcBridge, EchoRoundTripMeasurement, EchoServerHandle, EchoServerMessage};
#[cfg(feature = "egfx")]
pub use gfx::{EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle};
pub use handler::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
#[cfg(feature = "helper")]
pub use helper::TlsIdentityCtx;
#[cfg(feature = "multitransport")]
pub use multitransport::dtls::DtlsServerContext;
#[cfg(feature = "multitransport")]
pub use multitransport::listener::{ListenerConfig, UdpMultitransportListener};
#[cfg(feature = "multitransport")]
pub use multitransport::{
    CookieRegistry, MultitransportProvider, TunnelSender, encode_initiate_request, tunnel_channel,
};
pub use rdcamera::{
    CameraSampleSink, CameraServerMessage, RDCAMERA_CHANNEL_NAME, RdCameraDeviceProcessor, RdCameraServer,
    RdCameraServerFactory,
};
pub use rdpdr::{
    AnnouncedDevice, DirEntry, RdpdrBackendFactory, RdpdrHandle, RdpdrServer, RdpdrServerFactory, RdpdrServerHandler,
    RdpdrServerMessage, RdpdrStatus, SCARD_EJECT_CARD, SCARD_LEAVE_CARD, SCARD_RESET_CARD, SCARD_SHARE_DIRECT,
    SCARD_SHARE_EXCLUSIVE, SCARD_SHARE_SHARED, SCARD_UNPOWER_CARD,
};
pub use rdpeusb::{
    DeviceDescriptor, URBDRC_CHANNEL_NAME, UrbdrcServer, UrbdrcServerFactory, UrbdrcServerMessage, UsbDeviceCallback,
    UsbHandle, UsbPipe,
};
pub use server::{
    ConnectionHandler, Credentials, PostConnectionAction, RdpServer, RdpServerOptions, RdpServerSecurity, ServerEvent,
    ServerEventSender, tcp_srtt_ms,
};
pub use sound::{AudioWave, RdpsndServerHandler, RdpsndServerMessage, SoundServerFactory};

#[cfg(feature = "__bench")]
pub mod bench {
    pub mod encoder {
        pub mod rfx {
            pub use crate::encoder::rfx::bench::{rfx_enc, rfx_enc_tile};
        }

        pub use crate::encoder::{UpdateEncoder, UpdateEncoderCodecs};
    }
}
