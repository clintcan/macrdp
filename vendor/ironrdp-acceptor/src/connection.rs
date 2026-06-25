use core::mem;

use ironrdp_connector::{
    ConnectorError, ConnectorErrorExt as _, ConnectorResult, DesktopSize, Sequence, State, Written, encode_x224_packet,
    general_err, reason_err,
};
use ironrdp_core::{WriteBuf, decode};
use ironrdp_pdu as pdu;
use ironrdp_pdu::nego::SecurityProtocol;
use ironrdp_pdu::x224::X224;
use ironrdp_svc::{StaticChannelSet, SvcServerProcessor};
use pdu::rdp::capability_sets::CapabilitySet;
use pdu::rdp::client_info::Credentials;
use pdu::rdp::headers::ShareControlPdu;
use pdu::rdp::server_error_info::{ErrorInfo, ProtocolIndependentCode, ServerSetErrorInfoPdu};
use pdu::rdp::server_license::{LicensePdu, LicensingErrorMessage};
use pdu::{gcc, mcs, nego, rdp};
use tracing::{debug, warn};

use super::channel_connection::ChannelConnectionSequence;
use super::finalization::FinalizationSequence;
use crate::util::{self, wrap_share_data};

const IO_CHANNEL_ID: u16 = 1003;
const USER_CHANNEL_ID: u16 = 1002;

pub struct Acceptor {
    pub(crate) state: AcceptorState,
    security: SecurityProtocol,
    io_channel_id: u16,
    user_channel_id: u16,
    desktop_size: DesktopSize,
    server_capabilities: Vec<CapabilitySet>,
    static_channels: StaticChannelSet,
    saved_for_reactivation: AcceptorState,
    pub(crate) creds: Option<Credentials>,
    received_credentials: Option<Credentials>,
    reactivation: bool,
    /// (vendored) When set, the desktop size requested by the client in its
    /// Client Core Data (the resolution the user asked for, e.g. an mstsc
    /// full-screen monitor size) replaces the `desktop_size` this acceptor
    /// was built with, BEFORE the server's Demand Active is sent — so the
    /// session is negotiated at the client's resolution from the start, with
    /// no deactivation-reactivation resize. The Confirm Active bitmap capset
    /// is no help here: conformant clients (FreeRDP, mstsc) overwrite their
    /// desktop size with the server's Demand Active values and echo those
    /// back, so the GCC core data is the only place the client's own request
    /// is visible. Server implementations observe the adopted size via the
    /// usual `RdpServerDisplay::request_initial_size` call (the echoed
    /// Confirm Active size now equals the adopted size).
    honor_client_desktop_size: bool,
    /// (vendored) The Windows keyboard-layout identifier (KLID) the client
    /// announced in its GCC Client Core Data, captured in
    /// `BasicSettingsWaitInitial` and surfaced on `AcceptorResult` so the
    /// server can serve the client's keyboard layout. 0 = unknown / not sent.
    client_keyboard_layout: u32,
    /// (vendored) The client's announced multitransport support flags
    /// (MS-RDPEMT) from its GCC MultiTransportChannelData block, captured in
    /// `BasicSettingsWaitInitial` and surfaced on `AcceptorResult` so the
    /// server can decide whether to offer a UDP transport. Empty if the client
    /// sent no multitransport block.
    client_multitransport: gcc::MultiTransportFlags,
    /// (vendored) When true, set `EXTENDED_CLIENT_DATA_SUPPORTED` in the X.224
    /// RDP Negotiation Response. mstsc only sends the OPTIONAL GCC client data
    /// blocks — `CS_MULTITRANSPORT` (UDP support), `CS_MONITOR`,
    /// `CS_MONITOR_EX`, `CS_MESSAGE_CHANNEL` — when the server advertises this
    /// flag; without it, mstsc omits them all (so multitransport can never be
    /// negotiated). Default false to keep the standard handshake byte-identical;
    /// the server turns it on only when a multitransport provider is installed,
    /// since enabling extended data can change the core-data desktop size a
    /// multi-monitor client reports.
    advertise_extended_client_data: bool,
    /// (vendored) The UDP multitransport (MS-RDPEMT) offer to send, if any. When
    /// set (and the client advertised matching support), the acceptor emits a
    /// Server Initiate Multitransport Request **right after the licensing PDU and
    /// before Demand Active** — the only window in which clients accept it
    /// (FreeRDP's `MULTITRANSPORT_BOOTSTRAPPING_REQUEST` state sits between
    /// `LICENSING` and `CAPABILITIES_EXCHANGE_DEMAND_ACTIVE`; mstsc is the same).
    /// Sending it after finalization — once the client is ACTIVE — makes the
    /// client misparse it as a share-control PDU and tear the session down.
    multitransport_offer: Option<MultitransportOffer>,
    /// (vendored) Set to the offer actually emitted (offer present AND the client
    /// advertised the requested transport), so the server can bind the UDP flow /
    /// match the client's Multitransport Response. `None` if nothing was sent.
    multitransport_offered: Option<MultitransportOffer>,
    /// (vendored) The MCS message-channel id allocated when offering UDP
    /// multitransport, announced to the client in `SC_MCS_MSGCHANNEL`. The Server
    /// Initiate Multitransport Request MUST ride this channel (not the I/O
    /// channel): clients route the autodetect/multitransport bootstrap PDUs by
    /// channel id and silently ignore a request on the I/O channel when a message
    /// channel exists (verified — FreeRDP logs `expected messageChannelId=0`).
    /// `None` when not offering multitransport (default path unchanged).
    message_channel_id: Option<u16>,
}

/// (vendored) A UDP multitransport (MS-RDPEMT) offer the server asks the acceptor
/// to send during the connection sequence. See [`Acceptor::set_multitransport_offer`].
#[derive(Debug, Clone, Copy)]
pub struct MultitransportOffer {
    /// Correlates the request with the client's Multitransport Response and the
    /// tunnel-creation request over the new transport.
    pub request_id: u32,
    /// Which UDP transport the server is requesting (reliable / lossy).
    pub protocol: rdp::multitransport::RequestedProtocol,
    /// 16-byte security cookie echoed by the client to bind the UDP flow.
    pub cookie: [u8; 16],
}

#[derive(Debug)]
pub struct AcceptorResult {
    pub static_channels: StaticChannelSet,
    pub capabilities: Vec<CapabilitySet>,
    pub input_events: Vec<Vec<u8>>,
    pub user_channel_id: u16,
    pub io_channel_id: u16,
    pub reactivation: bool,
    /// (vendored) The client's announced keyboard-layout identifier (KLID)
    /// from its GCC Client Core Data; 0 if the client didn't send one.
    pub keyboard_layout: u32,
    /// (vendored) The client's announced multitransport (MS-RDPEMT) support
    /// flags from its GCC MultiTransportChannelData block; empty if the client
    /// sent no multitransport block.
    pub multitransport_flags: gcc::MultiTransportFlags,
    /// (vendored) The UDP multitransport offer the acceptor actually sent during
    /// the connection sequence (after licensing, before Demand Active), or `None`
    /// if none was sent. The server uses it to match the client's Multitransport
    /// Response and bind the inbound UDP flow.
    pub multitransport_offered: Option<MultitransportOffer>,
    /// Credentials received from the client during SecureSettingsExchange.
    ///
    /// Present for TLS-mode connections where the client sends credentials
    /// in the ClientInfoPdu. `None` for CredSSP/Hybrid connections (where
    /// authentication happens during the CredSSP exchange instead).
    ///
    /// Servers that need to validate credentials (e.g., via PAM or LDAP)
    /// can use this field for post-handshake validation.
    pub credentials: Option<Credentials>,
}

impl Acceptor {
    pub fn new(
        security: SecurityProtocol,
        desktop_size: DesktopSize,
        capabilities: Vec<CapabilitySet>,
        creds: Option<Credentials>,
    ) -> Self {
        Self {
            security,
            state: AcceptorState::InitiationWaitRequest,
            user_channel_id: USER_CHANNEL_ID,
            io_channel_id: IO_CHANNEL_ID,
            desktop_size,
            server_capabilities: capabilities,
            static_channels: StaticChannelSet::new(),
            saved_for_reactivation: Default::default(),
            creds,
            received_credentials: None,
            reactivation: false,
            honor_client_desktop_size: false,
            client_keyboard_layout: 0,
            client_multitransport: gcc::MultiTransportFlags::empty(),
            advertise_extended_client_data: false,
            multitransport_offer: None,
            multitransport_offered: None,
            message_channel_id: None,
        }
    }

    /// (vendored) Serve the desktop size the client requests in its Client
    /// Core Data instead of the size this acceptor was built with. See the
    /// `honor_client_desktop_size` field for the rationale.
    pub fn set_honor_client_desktop_size(&mut self, honor: bool) {
        self.honor_client_desktop_size = honor;
    }

    /// (vendored) Advertise `EXTENDED_CLIENT_DATA_SUPPORTED` so the client sends
    /// its optional GCC blocks (notably `CS_MULTITRANSPORT`). See the field doc.
    pub fn set_advertise_extended_client_data(&mut self, advertise: bool) {
        self.advertise_extended_client_data = advertise;
    }

    /// (vendored) Offer an auxiliary UDP multitransport (MS-RDPEMT) transport.
    /// When set, the acceptor sends a Server Initiate Multitransport Request right
    /// after the licensing PDU (before Demand Active) — the only point in the
    /// sequence at which clients accept it — provided the client advertised the
    /// requested transport in its GCC `MultiTransportChannelData`. The emitted
    /// offer is reported back on [`AcceptorResult::multitransport_offered`].
    pub fn set_multitransport_offer(&mut self, offer: Option<MultitransportOffer>) {
        self.multitransport_offer = offer;
    }

    pub fn new_deactivation_reactivation(
        mut consumed: Acceptor,
        static_channels: StaticChannelSet,
        desktop_size: DesktopSize,
    ) -> ConnectorResult<Self> {
        let AcceptorState::CapabilitiesSendServer {
            early_capability,
            channels,
        } = consumed.saved_for_reactivation
        else {
            return Err(general_err!("invalid acceptor state"));
        };

        for cap in consumed.server_capabilities.iter_mut() {
            if let CapabilitySet::Bitmap(cap) = cap {
                cap.desktop_width = desktop_size.width;
                cap.desktop_height = desktop_size.height;
            }
        }
        let state = AcceptorState::CapabilitiesSendServer {
            early_capability,
            channels: channels.clone(),
        };
        let saved_for_reactivation = AcceptorState::CapabilitiesSendServer {
            early_capability,
            channels,
        };
        Ok(Self {
            security: consumed.security,
            state,
            user_channel_id: consumed.user_channel_id,
            io_channel_id: consumed.io_channel_id,
            desktop_size,
            server_capabilities: consumed.server_capabilities,
            static_channels,
            saved_for_reactivation,
            creds: consumed.creds,
            received_credentials: consumed.received_credentials,
            reactivation: true,
            honor_client_desktop_size: consumed.honor_client_desktop_size,
            client_keyboard_layout: consumed.client_keyboard_layout,
            client_multitransport: consumed.client_multitransport,
            advertise_extended_client_data: consumed.advertise_extended_client_data,
            // Reactivation skips LicensingExchange, so nothing is re-emitted;
            // carry both so the fields stay consistent.
            multitransport_offer: consumed.multitransport_offer,
            multitransport_offered: consumed.multitransport_offered,
            message_channel_id: consumed.message_channel_id,
        })
    }

    pub fn attach_static_channel<T>(&mut self, channel: T)
    where
        T: SvcServerProcessor + 'static,
    {
        self.static_channels.insert(channel);
    }

    pub fn reached_security_upgrade(&self) -> Option<SecurityProtocol> {
        match self.state {
            AcceptorState::SecurityUpgrade { .. } => Some(self.security),
            _ => None,
        }
    }

    /// # Panics
    ///
    /// Panics if state is not [AcceptorState::SecurityUpgrade].
    pub fn mark_security_upgrade_as_done(&mut self) {
        assert!(self.reached_security_upgrade().is_some());
        self.step(&[], &mut WriteBuf::new()).expect("transition to next state");
        debug_assert!(self.reached_security_upgrade().is_none());
    }

    pub fn should_perform_credssp(&self) -> bool {
        matches!(self.state, AcceptorState::Credssp { .. })
    }

    /// # Panics
    ///
    /// Panics if state is not [AcceptorState::Credssp].
    pub fn mark_credssp_as_done(&mut self) {
        assert!(self.should_perform_credssp());
        let res = self.step(&[], &mut WriteBuf::new()).expect("transition to next state");
        debug_assert!(!self.should_perform_credssp());
        assert_eq!(res, Written::Nothing);
    }

    pub fn get_result(&mut self) -> Option<AcceptorResult> {
        match mem::take(&mut self.state) {
            AcceptorState::Accepted {
                channels: _channels, // TODO: what about ChannelDef?
                client_capabilities,
                input_events,
            } => Some(AcceptorResult {
                static_channels: mem::take(&mut self.static_channels),
                capabilities: client_capabilities,
                input_events,
                user_channel_id: self.user_channel_id,
                io_channel_id: self.io_channel_id,
                reactivation: self.reactivation,
                credentials: self.received_credentials.take(),
                keyboard_layout: self.client_keyboard_layout,
                multitransport_flags: self.client_multitransport,
                multitransport_offered: self.multitransport_offered,
            }),
            previous_state => {
                self.state = previous_state;
                None
            }
        }
    }
}

#[derive(Default, Debug)]
pub enum AcceptorState {
    #[default]
    Consumed,

    InitiationWaitRequest,
    InitiationSendConfirm {
        requested_protocol: SecurityProtocol,
    },
    SecurityUpgrade {
        requested_protocol: SecurityProtocol,
        protocol: SecurityProtocol,
    },
    Credssp {
        requested_protocol: SecurityProtocol,
        protocol: SecurityProtocol,
    },
    BasicSettingsWaitInitial {
        requested_protocol: SecurityProtocol,
        protocol: SecurityProtocol,
    },
    BasicSettingsSendResponse {
        requested_protocol: SecurityProtocol,
        protocol: SecurityProtocol,
        early_capability: Option<gcc::ClientEarlyCapabilityFlags>,
        channels: Vec<(u16, Option<gcc::ChannelDef>)>,
    },
    ChannelConnection {
        protocol: SecurityProtocol,
        early_capability: Option<gcc::ClientEarlyCapabilityFlags>,
        channels: Vec<(u16, gcc::ChannelDef)>,
        connection: ChannelConnectionSequence,
    },
    RdpSecurityCommencement {
        protocol: SecurityProtocol,
        early_capability: Option<gcc::ClientEarlyCapabilityFlags>,
        channels: Vec<(u16, gcc::ChannelDef)>,
    },
    SecureSettingsExchange {
        protocol: SecurityProtocol,
        early_capability: Option<gcc::ClientEarlyCapabilityFlags>,
        channels: Vec<(u16, gcc::ChannelDef)>,
    },
    LicensingExchange {
        early_capability: Option<gcc::ClientEarlyCapabilityFlags>,
        channels: Vec<(u16, gcc::ChannelDef)>,
    },
    CapabilitiesSendServer {
        early_capability: Option<gcc::ClientEarlyCapabilityFlags>,
        channels: Vec<(u16, gcc::ChannelDef)>,
    },
    MonitorLayoutSend {
        channels: Vec<(u16, gcc::ChannelDef)>,
    },
    CapabilitiesWaitConfirm {
        channels: Vec<(u16, gcc::ChannelDef)>,
    },
    ConnectionFinalization {
        finalization: FinalizationSequence,
        channels: Vec<(u16, gcc::ChannelDef)>,
        client_capabilities: Vec<CapabilitySet>,
    },
    Accepted {
        channels: Vec<(u16, gcc::ChannelDef)>,
        client_capabilities: Vec<CapabilitySet>,
        input_events: Vec<Vec<u8>>,
    },
}

impl State for AcceptorState {
    fn name(&self) -> &'static str {
        match self {
            Self::Consumed => "Consumed",
            Self::InitiationWaitRequest => "InitiationWaitRequest",
            Self::InitiationSendConfirm { .. } => "InitiationSendConfirm",
            Self::SecurityUpgrade { .. } => "SecurityUpgrade",
            Self::Credssp { .. } => "Credssp",
            Self::BasicSettingsWaitInitial { .. } => "BasicSettingsWaitInitial",
            Self::BasicSettingsSendResponse { .. } => "BasicSettingsSendResponse",
            Self::ChannelConnection { .. } => "ChannelConnection",
            Self::RdpSecurityCommencement { .. } => "RdpSecurityCommencement",
            Self::SecureSettingsExchange { .. } => "SecureSettingsExchange",
            Self::LicensingExchange { .. } => "LicensingExchange",
            Self::CapabilitiesSendServer { .. } => "CapabilitiesSendServer",
            Self::MonitorLayoutSend { .. } => "MonitorLayoutSend",
            Self::CapabilitiesWaitConfirm { .. } => "CapabilitiesWaitConfirm",
            Self::ConnectionFinalization { .. } => "ConnectionFinalization",
            Self::Accepted { .. } => "Connected",
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Accepted { .. })
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Sequence for Acceptor {
    fn next_pdu_hint(&self) -> Option<&dyn pdu::PduHint> {
        match &self.state {
            AcceptorState::Consumed => None,
            AcceptorState::InitiationWaitRequest => Some(&pdu::X224_HINT),
            AcceptorState::InitiationSendConfirm { .. } => None,
            AcceptorState::SecurityUpgrade { .. } => None,
            AcceptorState::Credssp { .. } => None,
            AcceptorState::BasicSettingsWaitInitial { .. } => Some(&pdu::X224_HINT),
            AcceptorState::BasicSettingsSendResponse { .. } => None,
            AcceptorState::ChannelConnection { connection, .. } => connection.next_pdu_hint(),
            AcceptorState::RdpSecurityCommencement { .. } => None,
            AcceptorState::SecureSettingsExchange { .. } => Some(&pdu::X224_HINT),
            AcceptorState::LicensingExchange { .. } => None,
            AcceptorState::CapabilitiesSendServer { .. } => None,
            AcceptorState::MonitorLayoutSend { .. } => None,
            AcceptorState::CapabilitiesWaitConfirm { .. } => Some(&pdu::X224_HINT),
            AcceptorState::ConnectionFinalization { finalization, .. } => finalization.next_pdu_hint(),
            AcceptorState::Accepted { .. } => None,
        }
    }

    fn state(&self) -> &dyn State {
        &self.state
    }

    fn step(&mut self, input: &[u8], output: &mut WriteBuf) -> ConnectorResult<Written> {
        let prev_state = mem::take(&mut self.state);

        let (written, next_state) = match prev_state {
            AcceptorState::InitiationWaitRequest => {
                let connection_request = decode::<X224<nego::ConnectionRequest>>(input)
                    .map_err(ConnectorError::decode)
                    .map(|p| p.0)?;

                debug!(message = ?connection_request, "Received");

                (
                    Written::Nothing,
                    AcceptorState::InitiationSendConfirm {
                        requested_protocol: connection_request.protocol,
                    },
                )
            }

            AcceptorState::InitiationSendConfirm { requested_protocol } => {
                let protocols = requested_protocol & self.security;
                let protocol = if protocols.intersects(SecurityProtocol::HYBRID_EX) {
                    SecurityProtocol::HYBRID_EX
                } else if protocols.intersects(SecurityProtocol::HYBRID) {
                    SecurityProtocol::HYBRID
                } else if protocols.intersects(SecurityProtocol::SSL) {
                    SecurityProtocol::SSL
                } else if self.security.is_empty() {
                    SecurityProtocol::empty()
                } else {
                    // No common security protocol. Send RDP_NEG_FAILURE so the client
                    // gets a well-formed response instead of a TCP reset (MS-RDPBCGR 2.2.1.2.2).
                    let failure_code = if self.security.intersects(SecurityProtocol::SSL) {
                        nego::FailureCode::SSL_REQUIRED_BY_SERVER
                    } else if self
                        .security
                        .intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX)
                    {
                        nego::FailureCode::HYBRID_REQUIRED_BY_SERVER
                    } else {
                        nego::FailureCode::SSL_REQUIRED_BY_SERVER
                    };

                    let failure = nego::ConnectionConfirm::Failure { code: failure_code };

                    debug!(message = ?failure, "Send");

                    ironrdp_core::encode_buf(&X224(failure), output).map_err(ConnectorError::encode)?;

                    return Err(reason_err!(
                        "security protocol mismatch",
                        "server requires {:?} but client only offered {:?}",
                        self.security,
                        requested_protocol,
                    ));
                };
                // (vendored) Advertise EXTENDED_CLIENT_DATA_SUPPORTED when asked,
                // so the client sends its optional GCC blocks (CS_MULTITRANSPORT
                // et al.). mstsc omits ALL of them without this flag.
                let mut flags = nego::ResponseFlags::empty();
                if self.advertise_extended_client_data {
                    flags |= nego::ResponseFlags::EXTENDED_CLIENT_DATA_SUPPORTED;
                }
                let connection_confirm = nego::ConnectionConfirm::Response { flags, protocol };

                debug!(message = ?connection_confirm, "Send");

                let written =
                    ironrdp_core::encode_buf(&X224(connection_confirm), output).map_err(ConnectorError::encode)?;

                (
                    Written::from_size(written)?,
                    AcceptorState::SecurityUpgrade {
                        requested_protocol,
                        protocol,
                    },
                )
            }

            AcceptorState::SecurityUpgrade {
                requested_protocol,
                protocol,
            } => {
                debug!(?requested_protocol);
                let next_state = if protocol.intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) {
                    AcceptorState::Credssp {
                        requested_protocol,
                        protocol,
                    }
                } else {
                    AcceptorState::BasicSettingsWaitInitial {
                        requested_protocol,
                        protocol,
                    }
                };
                (Written::Nothing, next_state)
            }

            AcceptorState::Credssp {
                requested_protocol,
                protocol,
            } => (
                Written::Nothing,
                AcceptorState::BasicSettingsWaitInitial {
                    requested_protocol,
                    protocol,
                },
            ),

            AcceptorState::BasicSettingsWaitInitial {
                requested_protocol,
                protocol,
            } => {
                let x224_payload = decode::<X224<pdu::x224::X224Data<'_>>>(input)
                    .map_err(ConnectorError::decode)
                    .map(|p| p.0)?;
                let settings_initial =
                    decode::<mcs::ConnectInitial>(x224_payload.data.as_ref()).map_err(ConnectorError::decode)?;

                debug!(message = ?settings_initial, "Received");

                let gcc_blocks = settings_initial.conference_create_request.into_gcc_blocks();
                let early_capability = gcc_blocks.core.optional_data.early_capability_flags;

                // (vendored) Capture the client's keyboard-layout identifier so
                // the server can serve a non-US layout. Always recorded (it's
                // free); whether to act on it is the server's choice.
                self.client_keyboard_layout = gcc_blocks.core.keyboard_layout;

                // (vendored) Capture the client's multitransport (MS-RDPEMT)
                // support flags so the server can decide whether to offer a UDP
                // transport. Always recorded (it's free); whether to act on it
                // is the server's choice. Empty if the client sent no block.
                self.client_multitransport = gcc_blocks
                    .multi_transport_channel
                    .as_ref()
                    .map(|m| m.flags)
                    .unwrap_or_else(gcc::MultiTransportFlags::empty);

                debug!(
                    multitransport_flags = ?self.client_multitransport,
                    "client multitransport support captured from GCC"
                );

                // (vendored) Adopt the client's requested desktop size from
                // its Client Core Data before the server's Demand Active is
                // built, so the session is negotiated at the client's
                // resolution from the start. 200×200 is the protocol minimum
                // and 8192 the maximum desktop dimension (MS-RDPBCGR);
                // anything outside that band is garbage we refuse to honor.
                if self.honor_client_desktop_size {
                    let width = gcc_blocks.core.desktop_width;
                    let height = gcc_blocks.core.desktop_height;
                    if (200..=8192).contains(&width)
                        && (200..=8192).contains(&height)
                        && (width != self.desktop_size.width || height != self.desktop_size.height)
                    {
                        debug!(
                            client_width = width,
                            client_height = height,
                            server_width = self.desktop_size.width,
                            server_height = self.desktop_size.height,
                            "Honoring client-requested desktop size from Client Core Data"
                        );
                        self.desktop_size = DesktopSize { width, height };
                        for cap in self.server_capabilities.iter_mut() {
                            if let CapabilitySet::Bitmap(cap) = cap {
                                cap.desktop_width = width;
                                cap.desktop_height = height;
                            }
                        }
                    }
                }

                let joined: Vec<_> = gcc_blocks
                    .network
                    .map(|network| {
                        network
                            .channels
                            .into_iter()
                            .map(|c| {
                                self.static_channels
                                    .get_by_channel_name(&c.name)
                                    .map(|(type_id, _)| (type_id, c))
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                #[expect(clippy::arithmetic_side_effects)] // IO channel ID is not big enough for overflowing.
                let channels = joined
                    .into_iter()
                    .enumerate()
                    .map(|(i, channel)| {
                        let channel_id = u16::try_from(i).expect("always in the range") + self.io_channel_id + 1;
                        if let Some((type_id, c)) = channel {
                            self.static_channels.attach_channel_id(type_id, channel_id);
                            (channel_id, Some(c))
                        } else {
                            (channel_id, None)
                        }
                    })
                    .collect();

                (
                    Written::Nothing,
                    AcceptorState::BasicSettingsSendResponse {
                        requested_protocol,
                        protocol,
                        early_capability,
                        channels,
                    },
                )
            }

            AcceptorState::BasicSettingsSendResponse {
                requested_protocol,
                protocol,
                early_capability,
                channels,
            } => {
                let channel_ids: Vec<u16> = channels.iter().map(|&(i, _)| i).collect();

                let skip_channel_join = early_capability
                    .is_some_and(|client| client.contains(gcc::ClientEarlyCapabilityFlags::SUPPORT_SKIP_CHANNELJOIN));

                // (vendored) Advertise reliable UDP multitransport + soft-sync
                // in `SC_MULTITRANSPORT` when the server enabled the extended
                // client data path (which it does only when a multitransport
                // provider is installed). Lossy (`UDP_FECL`) is a later phase.
                let multitransport = self.advertise_extended_client_data.then_some(
                    gcc::MultiTransportFlags::TRANSPORT_TYPE_UDP_FECR
                        | gcc::MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP,
                );

                // (vendored) Allocate a message channel (the next id after the
                // virtual channels) when offering multitransport — the Initiate
                // Request rides it. Same gate as `SC_MULTITRANSPORT`: the client
                // sends `CS_MCS_MSGCHANNEL` only because we set
                // EXTENDED_CLIENT_DATA_SUPPORTED, so granting one back is in-contract.
                let message_channel_id = if self.advertise_extended_client_data {
                    let count = u16::try_from(channel_ids.len()).expect("channel count fits u16");
                    let id = self.io_channel_id.saturating_add(1).saturating_add(count);
                    self.message_channel_id = Some(id);
                    Some(id)
                } else {
                    None
                };

                let server_blocks = create_gcc_blocks(
                    self.io_channel_id,
                    channel_ids.clone(),
                    requested_protocol,
                    skip_channel_join,
                    multitransport,
                    message_channel_id,
                );

                let settings_response = mcs::ConnectResponse {
                    conference_create_response: gcc::ConferenceCreateResponse::new(self.user_channel_id, server_blocks)
                        .map_err(ConnectorError::decode)?,
                    called_connect_id: 1,
                    domain_parameters: mcs::DomainParameters::target(),
                };

                debug!(message = ?settings_response, "Send");

                let written = encode_x224_packet(&settings_response, output)?;
                let channels = channels.into_iter().filter_map(|(i, c)| c.map(|c| (i, c))).collect();

                (
                    Written::from_size(written)?,
                    AcceptorState::ChannelConnection {
                        protocol,
                        early_capability,
                        channels,
                        connection: if skip_channel_join {
                            ChannelConnectionSequence::skip_channel_join(self.user_channel_id)
                        } else {
                            // The message channel (if any) is joined like the
                            // others when the client doesn't skip channel joins.
                            let mut join_ids = channel_ids;
                            if let Some(id) = message_channel_id {
                                join_ids.push(id);
                            }
                            ChannelConnectionSequence::new(self.user_channel_id, self.io_channel_id, join_ids)
                        },
                    },
                )
            }

            AcceptorState::ChannelConnection {
                protocol,
                early_capability,
                channels,
                mut connection,
            } => {
                let written = connection.step(input, output)?;
                let state = if connection.is_done() {
                    AcceptorState::RdpSecurityCommencement {
                        protocol,
                        early_capability,
                        channels,
                    }
                } else {
                    AcceptorState::ChannelConnection {
                        protocol,
                        early_capability,
                        channels,
                        connection,
                    }
                };

                (written, state)
            }

            AcceptorState::RdpSecurityCommencement {
                protocol,
                early_capability,
                channels,
                ..
            } => (
                Written::Nothing,
                AcceptorState::SecureSettingsExchange {
                    protocol,
                    early_capability,
                    channels,
                },
            ),

            AcceptorState::SecureSettingsExchange {
                protocol,
                early_capability,
                channels,
            } => {
                let data: X224<mcs::SendDataRequest<'_>> = decode(input).map_err(ConnectorError::decode)?;
                let data = data.0;
                let client_info: rdp::ClientInfoPdu =
                    decode(data.user_data.as_ref()).map_err(ConnectorError::decode)?;

                debug!(message = ?client_info, "Received");

                if !protocol.intersects(SecurityProtocol::HYBRID | SecurityProtocol::HYBRID_EX) {
                    let creds = client_info.client_info.credentials;

                    if let Some(expected) = &self.creds {
                        if expected != &creds {
                            // FIXME: How authorization should be denied with standard RDP security?
                            // Since standard RDP security is not a priority, we just send a ServerDeniedConnection ServerSetErrorInfo PDU.
                            let info = ServerSetErrorInfoPdu(ErrorInfo::ProtocolIndependentCode(
                                ProtocolIndependentCode::ServerDeniedConnection,
                            ));

                            debug!(message = ?info, "Send");

                            util::encode_send_data_indication(self.user_channel_id, self.io_channel_id, &info, output)?;

                            return Err(ConnectorError::general("invalid credentials"));
                        }
                    }

                    // Store credentials for later retrieval via AcceptorResult.
                    self.received_credentials = Some(creds);
                }

                (
                    Written::Nothing,
                    AcceptorState::LicensingExchange {
                        early_capability,
                        channels,
                    },
                )
            }

            AcceptorState::LicensingExchange {
                early_capability,
                channels,
            } => {
                let license: LicensePdu = LicensingErrorMessage::new_valid_client()
                    .map_err(ConnectorError::encode)?
                    .into();

                debug!(message = ?license, "Send");

                let mut written =
                    util::encode_send_data_indication(self.user_channel_id, self.io_channel_id, &license, output)?;

                // (vendored) Emit the Server Initiate Multitransport Request HERE
                // — after licensing, before Demand Active. This is the only window
                // clients honor it in (FreeRDP's MULTITRANSPORT_BOOTSTRAPPING_REQUEST
                // state; mstsc the same). Sent later (post-finalization) the client
                // is ACTIVE and misreads it as a share-control PDU, tearing down.
                if let Some(offer) = self.multitransport_offer {
                    let needed = match offer.protocol {
                        rdp::multitransport::RequestedProtocol::UdpFecR => {
                            gcc::MultiTransportFlags::TRANSPORT_TYPE_UDP_FECR
                        }
                        rdp::multitransport::RequestedProtocol::UdpFecL => {
                            gcc::MultiTransportFlags::TRANSPORT_TYPE_UDP_FECL
                        }
                    };
                    if self.client_multitransport.contains(needed) {
                        let request = rdp::multitransport::MultitransportRequestPdu {
                            security_header: rdp::headers::BasicSecurityHeader {
                                flags: rdp::headers::BasicSecurityHeaderFlags::TRANSPORT_REQ,
                            },
                            request_id: offer.request_id,
                            requested_protocol: offer.protocol,
                            security_cookie: offer.cookie,
                        };
                        // Ride the message channel if one was granted (clients
                        // route the bootstrap PDU by channel id); fall back to the
                        // I/O channel only if somehow no message channel exists.
                        let mt_channel = self.message_channel_id.unwrap_or(self.io_channel_id);
                        debug!(channel_id = mt_channel, message = ?request, "Send Server Initiate Multitransport Request");
                        written = written.saturating_add(util::encode_send_data_indication(
                            self.user_channel_id,
                            mt_channel,
                            &request,
                            output,
                        )?);
                        self.multitransport_offered = Some(offer);
                    } else {
                        debug!(
                            client_flags = ?self.client_multitransport,
                            ?needed,
                            "not sending multitransport request: client didn't advertise the requested transport"
                        );
                    }
                }

                self.saved_for_reactivation = AcceptorState::CapabilitiesSendServer {
                    early_capability,
                    channels: channels.clone(),
                };

                (
                    Written::from_size(written)?,
                    AcceptorState::CapabilitiesSendServer {
                        early_capability,
                        channels,
                    },
                )
            }

            AcceptorState::CapabilitiesSendServer {
                early_capability,
                channels,
            } => {
                let demand_active = rdp::headers::ShareControlHeader {
                    share_id: 0,
                    pdu_source: self.io_channel_id,
                    share_control_pdu: ShareControlPdu::ServerDemandActive(rdp::capability_sets::ServerDemandActive {
                        pdu: rdp::capability_sets::DemandActive {
                            source_descriptor: "".into(),
                            capability_sets: self.server_capabilities.clone(),
                        },
                    }),
                };

                debug!(message = ?demand_active, "Send");

                let written = util::encode_send_data_indication(
                    self.user_channel_id,
                    self.io_channel_id,
                    &demand_active,
                    output,
                )?;

                let layout_flag = gcc::ClientEarlyCapabilityFlags::SUPPORT_MONITOR_LAYOUT_PDU;
                let next_state = if early_capability.is_some_and(|c| c.contains(layout_flag)) {
                    AcceptorState::MonitorLayoutSend { channels }
                } else {
                    AcceptorState::CapabilitiesWaitConfirm { channels }
                };

                (Written::from_size(written)?, next_state)
            }

            AcceptorState::MonitorLayoutSend { channels } => {
                let monitor_layout =
                    rdp::headers::ShareDataPdu::MonitorLayout(rdp::finalization_messages::MonitorLayoutPdu {
                        monitors: vec![gcc::Monitor {
                            left: 0,
                            top: 0,
                            right: i32::from(self.desktop_size.width),
                            bottom: i32::from(self.desktop_size.height),
                            flags: gcc::MonitorFlags::PRIMARY,
                        }],
                    });

                debug!(message = ?monitor_layout, "Send");

                let share_data = wrap_share_data(monitor_layout, self.io_channel_id);

                let written =
                    util::encode_send_data_indication(self.user_channel_id, self.io_channel_id, &share_data, output)?;

                (
                    Written::from_size(written)?,
                    AcceptorState::CapabilitiesWaitConfirm { channels },
                )
            }

            AcceptorState::CapabilitiesWaitConfirm { ref channels } => {
                let message = decode::<X224<mcs::McsMessage<'_>>>(input)
                    .map_err(ConnectorError::decode)
                    .map(|p| p.0);
                let message = match message {
                    Ok(msg) => msg,
                    Err(e) => {
                        if self.reactivation {
                            debug!("Dropping unexpected PDU during reactivation");
                            self.state = prev_state;
                            return Ok(Written::Nothing);
                        } else {
                            return Err(e);
                        }
                    }
                };
                match message {
                    mcs::McsMessage::SendDataRequest(data) => {
                        // (vendored) Skip message-channel PDUs (autodetect /
                        // multitransport response) that ride a channel != io once a
                        // message channel is granted; only io-channel ShareControl
                        // PDUs (Confirm Active) belong here. Stay put for the next.
                        if data.channel_id != self.io_channel_id {
                            debug!(
                                channel_id = data.channel_id,
                                "Skipping non-io-channel PDU during capabilities-wait"
                            );
                            self.state = prev_state;
                            return Ok(Written::Nothing);
                        }
                        let capabilities_confirm = decode::<rdp::headers::ShareControlHeader>(data.user_data.as_ref())
                            .map_err(ConnectorError::decode);
                        let capabilities_confirm = match capabilities_confirm {
                            Ok(capabilities_confirm) => capabilities_confirm,
                            Err(e) => {
                                if self.reactivation {
                                    debug!("Dropping unexpected PDU during reactivation");
                                    self.state = prev_state;
                                    return Ok(Written::Nothing);
                                } else {
                                    return Err(e);
                                }
                            }
                        };

                        debug!(message = ?capabilities_confirm, "Received");

                        let ShareControlPdu::ClientConfirmActive(confirm) = capabilities_confirm.share_control_pdu
                        else {
                            return Err(ConnectorError::general("expected client confirm active"));
                        };

                        (
                            Written::Nothing,
                            AcceptorState::ConnectionFinalization {
                                channels: channels.clone(),
                                finalization: FinalizationSequence::new(self.user_channel_id, self.io_channel_id),
                                client_capabilities: confirm.pdu.capability_sets,
                            },
                        )
                    }

                    mcs::McsMessage::DisconnectProviderUltimatum(ultimatum) => {
                        return Err(reason_err!("received disconnect ultimatum", "{:?}", ultimatum.reason));
                    }

                    _ => {
                        warn!(?message, "Unexpected MCS message received");

                        (Written::Nothing, prev_state)
                    }
                }
            }

            AcceptorState::ConnectionFinalization {
                mut finalization,
                channels,
                client_capabilities,
            } => {
                let written = finalization.step(input, output)?;

                let state = if finalization.is_done() {
                    AcceptorState::Accepted {
                        channels,
                        client_capabilities,
                        input_events: finalization.into_input_events(),
                    }
                } else {
                    AcceptorState::ConnectionFinalization {
                        finalization,
                        channels,
                        client_capabilities,
                    }
                };

                (written, state)
            }

            _ => unreachable!(),
        };

        self.state = next_state;
        Ok(written)
    }
}

fn create_gcc_blocks(
    io_channel: u16,
    channel_ids: Vec<u16>,
    requested: SecurityProtocol,
    skip_channel_join: bool,
    multitransport: Option<gcc::MultiTransportFlags>,
    message_channel_id: Option<u16>,
) -> gcc::ServerGccBlocks {
    gcc::ServerGccBlocks {
        core: gcc::ServerCoreData {
            version: gcc::RdpVersion::V5_PLUS,
            optional_data: gcc::ServerCoreOptionalData {
                client_requested_protocols: Some(requested),
                early_capability_flags: skip_channel_join
                    .then_some(gcc::ServerEarlyCapabilityFlags::SKIP_CHANNELJOIN_SUPPORTED),
            },
        },
        security: gcc::ServerSecurityData::no_security(),
        network: gcc::ServerNetworkData {
            channel_ids,
            io_channel,
        },
        // (vendored) Grant a message channel (`SC_MCS_MSGCHANNEL`) when offering
        // UDP multitransport: clients route the Server Initiate Multitransport
        // Request (and connect-time autodetect) by channel id and ignore it on
        // the I/O channel once they've sent `CS_MCS_MSGCHANNEL`. `None` on the
        // default (non-multitransport) path keeps the handshake byte-identical.
        message_channel: message_channel_id.map(|id| gcc::ServerMessageChannelData {
            mcs_message_channel_id: id,
        }),
        // (vendored) Echo `SC_MULTITRANSPORT` advertising the server's supported
        // UDP transports when a multitransport provider is installed. mstsc
        // validates an incoming Initiate Multitransport Request against this
        // advertisement; without it, the otherwise well-formed request is
        // out-of-contract and mstsc raises a protocol error + disconnects.
        multi_transport_channel: multitransport.map(|flags| gcc::MultiTransportChannelData { flags }),
    }
}
