//! (macrdp divergence 2) Server-direction MS-RDPESC halves.
//!
//! Upstream's smart card PDUs are written for the client side: it decodes each `*Call` and
//! encodes each `*Return`. macrdp is the server, so it needs the mirror halves: encode the
//! `*Call`s it sends to the client's real reader, and decode the `*Return`s that come back. Wire
//! layouts mirror upstream's opposite halves byte for byte, which the tests prove by round-tripping
//! through them. macrdp uses the W (Unicode) IOCTL variants, so strings are UTF-16.
//!
//! Everything here is additive; no upstream item is modified.

use ironrdp_core::{
    DecodeError, DecodeResult, Encode as _, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_size,
    invalid_field_err, other_err,
};
use ironrdp_pdu::utils::{
    self, CharacterSet, encoded_multistring_len, read_multistring_from_cursor, write_multistring_to_cursor,
};

use super::ndr::{self, Decode as _, Encode as _};
use super::{
    CardProtocol, CardState, ConnectCall, ConnectCommon, ConnectReturn, ContextCall, EstablishContextCall,
    EstablishContextReturn, GetStatusChangeCall, GetStatusChangeReturn, HCardAndDispositionCall, ListReadersCall,
    ListReadersReturn, LongReturn, ReaderState, ReaderStateCommonCall, ReturnCode, SCardIORequest, ScardCall,
    ScardContext, ScardHandle, Scope, StatusCall, StatusReturn, TransmitCall, TransmitReturn, expect_charset,
    expect_no_charset, rpce,
};

// ---- Envelope and conversions ----

impl ScardCall {
    /// Marshals this call as the RPCE input buffer of a `DR_CONTROL_REQ`. Only the calls the server
    /// issues are encodable.
    pub fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        match self {
            Self::EstablishContextCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::ContextCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::ListReadersCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::GetStatusChangeCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::ConnectCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::HCardAndDispositionCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::StatusCall(c) => rpce::Pdu(c.clone()).encode(dst),
            Self::TransmitCall(c) => rpce::Pdu(c.clone()).encode(dst),
            _ => Err(other_err!("ScardCall::encode: unsupported call variant")),
        }
    }

    /// The marshaled size of [`Self::encode`] (the `DR_CONTROL_REQ` input buffer length).
    pub fn size(&self) -> usize {
        match self {
            Self::EstablishContextCall(c) => rpce::Pdu(c.clone()).size(),
            Self::ContextCall(c) => rpce::Pdu(c.clone()).size(),
            Self::ListReadersCall(c) => rpce::Pdu(c.clone()).size(),
            Self::GetStatusChangeCall(c) => rpce::Pdu(c.clone()).size(),
            Self::ConnectCall(c) => rpce::Pdu(c.clone()).size(),
            Self::HCardAndDispositionCall(c) => rpce::Pdu(c.clone()).size(),
            Self::StatusCall(c) => rpce::Pdu(c.clone()).size(),
            Self::TransmitCall(c) => rpce::Pdu(c.clone()).size(),
            _ => 0,
        }
    }
}

impl From<Scope> for u32 {
    #[expect(
        clippy::as_conversions,
        reason = "guarantees discriminant layout, and as is the only way to cast enum -> primitive"
    )]
    fn from(val: Scope) -> Self {
        val as u32
    }
}

impl TryFrom<u32> for CardState {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0x0000_0000 => Ok(CardState::Unknown),
            0x0000_0001 => Ok(CardState::Absent),
            0x0000_0002 => Ok(CardState::Present),
            0x0000_0003 => Ok(CardState::Swallowed),
            0x0000_0004 => Ok(CardState::Powered),
            0x0000_0005 => Ok(CardState::Negotiable),
            0x0000_0006 => Ok(CardState::SpecificMode),
            _ => Err(invalid_field_err!("try_from", "CardState", "unsupported value")),
        }
    }
}

impl TryFrom<u32> for ReturnCode {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        let code = match value {
            0x0000_0000 => ReturnCode::Success,
            0x8010_0001 => ReturnCode::InternalError,
            0x8010_0002 => ReturnCode::Cancelled,
            0x8010_0003 => ReturnCode::InvalidHandle,
            0x8010_0004 => ReturnCode::InvalidParameter,
            0x8010_0005 => ReturnCode::InvalidTarget,
            0x8010_0006 => ReturnCode::NoMemory,
            0x8010_0007 => ReturnCode::WaitedTooLong,
            0x8010_0008 => ReturnCode::InsufficientBuffer,
            0x8010_0009 => ReturnCode::UnknownReader,
            0x8010_000A => ReturnCode::Timeout,
            0x8010_000B => ReturnCode::SharingViolation,
            0x8010_000C => ReturnCode::NoSmartcard,
            0x8010_000D => ReturnCode::UnknownCard,
            0x8010_000E => ReturnCode::CantDispose,
            0x8010_000F => ReturnCode::ProtoMismatch,
            0x8010_0010 => ReturnCode::NotReady,
            0x8010_0011 => ReturnCode::InvalidValue,
            0x8010_0012 => ReturnCode::SystemCancelled,
            0x8010_0013 => ReturnCode::CommError,
            0x8010_0014 => ReturnCode::UnknownError,
            0x8010_0015 => ReturnCode::InvalidAtr,
            0x8010_0016 => ReturnCode::NotTransacted,
            0x8010_0017 => ReturnCode::ReaderUnavailable,
            0x8010_0018 => ReturnCode::Shutdown,
            0x8010_0019 => ReturnCode::PciTooSmall,
            0x8010_001A => ReturnCode::ReaderUnsupported,
            0x8010_001B => ReturnCode::DuplicateReader,
            0x8010_001C => ReturnCode::CardUnsupported,
            0x8010_001D => ReturnCode::NoService,
            0x8010_001E => ReturnCode::ServiceStopped,
            0x8010_001F => ReturnCode::Unexpected,
            0x8010_0020 => ReturnCode::IccInstallation,
            0x8010_0021 => ReturnCode::IccCreateorder,
            0x8010_0022 => ReturnCode::UnsupportedFeature,
            0x8010_0023 => ReturnCode::DirNotFound,
            0x8010_0024 => ReturnCode::FileNotFound,
            0x8010_0025 => ReturnCode::NoDir,
            0x8010_0026 => ReturnCode::NoFile,
            0x8010_0027 => ReturnCode::NoAccess,
            0x8010_0028 => ReturnCode::WriteTooMany,
            0x8010_0029 => ReturnCode::BadSeek,
            0x8010_002A => ReturnCode::InvalidChv,
            0x8010_002B => ReturnCode::UnknownResMsg,
            0x8010_002C => ReturnCode::NoSuchCertificate,
            0x8010_002D => ReturnCode::CertificateUnavailable,
            0x8010_002E => ReturnCode::NoReadersAvailable,
            0x8010_002F => ReturnCode::CommDataLost,
            0x8010_0030 => ReturnCode::NoKeyContainer,
            0x8010_0031 => ReturnCode::ServerTooBusy,
            0x8010_0032 => ReturnCode::PinCacheExpired,
            0x8010_0033 => ReturnCode::NoPinCache,
            0x8010_0034 => ReturnCode::ReadOnlyCard,
            0x8010_0065 => ReturnCode::UnsupportedCard,
            0x8010_0066 => ReturnCode::UnresponsiveCard,
            0x8010_0067 => ReturnCode::UnpoweredCard,
            0x8010_0068 => ReturnCode::ResetCard,
            0x8010_0069 => ReturnCode::RemovedCard,
            0x8010_006A => ReturnCode::SecurityViolation,
            0x8010_006B => ReturnCode::WrongChv,
            0x8010_006C => ReturnCode::ChvBlocked,
            0x8010_006D => ReturnCode::Eof,
            0x8010_006E => ReturnCode::CancelledByUser,
            0x8010_006F => ReturnCode::CardNotAuthenticated,
            0x8010_0070 => ReturnCode::CacheItemNotFound,
            0x8010_0071 => ReturnCode::CacheItemStale,
            0x8010_0072 => ReturnCode::CacheItemTooBig,
            _ => return Err(invalid_field_err!("try_from", "ReturnCode", "unsupported value")),
        };
        Ok(code)
    }
}

// ---- NDR helpers ----

/// Writes an NDR conformant+varying string: `MaximumCount`, `Offset` (0), `ActualCount`, then the
/// NUL-terminated string, zero-padded to 4 bytes. The mirror of [`ndr::read_string_from_cursor`].
///
/// The counts are in characters including the NUL (UTF-16 code units for Unicode). The read side
/// aligns on the absolute cursor position; every MS-RDPESC string field starts 4-byte aligned, so
/// padding the region to a multiple of 4 is equivalent, which lets [`ndr_string_size`] predict it.
fn write_ndr_string(dst: &mut WriteCursor<'_>, value: &str, charset: CharacterSet) -> EncodeResult<()> {
    let encoded_len = utils::encoded_str_len(value, charset, true);
    let char_size = match charset {
        CharacterSet::Unicode => 2,
        CharacterSet::Ansi => 1,
    };
    let char_count: u32 = cast_length!("write_ndr_string", "char_count", encoded_len / char_size)?;

    ensure_size!(ctx: "write_ndr_string", in: dst, size: ndr_string_size(value, charset));
    dst.write_u32(char_count); // MaximumCount
    dst.write_u32(0); // Offset
    dst.write_u32(char_count); // ActualCount
    utils::write_string_to_cursor(dst, value, charset, true)?;
    let pad = (4 - dst.pos() % 4) % 4;
    dst.write_slice(&[0u8; 4][..pad]);
    Ok(())
}

/// The number of bytes [`write_ndr_string`] writes for `value`.
fn ndr_string_size(value: &str, charset: CharacterSet) -> usize {
    (size_of::<u32>() * 3 + utils::encoded_str_len(value, charset, true)).next_multiple_of(4)
}

/// Writes an `SCardIO_Request`'s pointer part. Unlike upstream's [`ndr::Encode`] impl, no extra
/// bytes means a NULL `pbExtraBytes` referent. A non-NULL referent tells the peer a conformant
/// array (`MaximumCount` + bytes) follows; since none does, everything after it is misread, and
/// real Windows rejects the whole `Transmit_Call` with `STATUS_UNSUCCESSFUL` (found live on mstsc).
/// A NULL referent consumes no referent id, so `index` doesn't advance.
fn encode_io_request_ptr(req: &SCardIORequest, index: &mut u32, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
    if !req.extra_bytes.is_empty() {
        return req.encode_ptr(index, dst);
    }
    ensure_size!(in: dst, size: req.size_ptr());
    dst.write_u32(req.protocol.bits());
    dst.write_u32(0); // cbExtraBytes
    dst.write_u32(0); // NULL pbExtraBytes referent
    Ok(())
}

impl ndr::Encode for ConnectCommon {
    fn encode_ptr(&self, index: &mut u32, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.context.encode_ptr(index, dst)?;
        ensure_size!(in: dst, size: size_of::<u32>() * 2);
        dst.write_u32(self.share_mode);
        dst.write_u32(self.preferred_protocols.bits());
        Ok(())
    }

    fn encode_value(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.context.encode_value(dst)
    }

    fn size_ptr(&self) -> usize {
        self.context.size_ptr() + size_of::<u32>() * 2
    }

    fn size_value(&self) -> usize {
        self.context.size_value()
    }
}

impl ndr::Encode for ReaderState {
    fn encode_ptr(&self, index: &mut u32, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ndr::encode_ptr(None, index, dst)?; // szReader referent
        ensure_size!(in: dst, size: ReaderStateCommonCall::size());
        self.common.encode(dst)
    }

    fn encode_value(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        write_ndr_string(dst, &self.reader, CharacterSet::Unicode)
    }

    fn size_ptr(&self) -> usize {
        ndr::ptr_size(false) + ReaderStateCommonCall::size()
    }

    fn size_value(&self) -> usize {
        ndr_string_size(&self.reader, CharacterSet::Unicode)
    }
}

// ---- `*Call` encoders (what the server sends) ----

impl rpce::HeaderlessEncode for EstablishContextCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());
        dst.write_u32(self.scope.into());
        Ok(())
    }

    fn name(&self) -> &'static str {
        "EstablishContext_Call"
    }

    fn size(&self) -> usize {
        self.scope.size()
    }
}

impl rpce::HeaderlessEncode for ContextCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        self.context.encode_ptr(&mut index, dst)?;
        self.context.encode_value(dst)
    }

    fn name(&self) -> &'static str {
        "Context_Call"
    }

    fn size(&self) -> usize {
        self.context.size()
    }
}

impl rpce::HeaderlessEncode for ListReadersCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        self.context.encode_ptr(&mut index, dst)?;
        ensure_size!(in: dst, size: size_of::<u32>() * 4);
        dst.write_u32(self.groups_ptr_length);
        if self.groups.is_empty() {
            dst.write_u32(0); // NULL mszGroups referent
        } else {
            ndr::encode_ptr(None, &mut index, dst)?;
        }
        dst.write_u32(u32::from(self.readers_is_null));
        dst.write_u32(self.readers_size);
        self.context.encode_value(dst)?;
        if !self.groups.is_empty() {
            ensure_size!(in: dst, size: size_of::<u32>());
            dst.write_u32(self.groups_length);
            write_multistring_to_cursor(dst, &self.groups, CharacterSet::Unicode)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "ListReaders_Call"
    }

    fn size(&self) -> usize {
        self.context.size_ptr()
            + size_of::<u32>() * 4 // groups_ptr_length, mszGroups referent, readers_is_null, readers_size
            + self.context.size_value()
            + if self.groups.is_empty() {
                0
            } else {
                size_of::<u32>() + encoded_multistring_len(&self.groups, CharacterSet::Unicode)
            }
    }
}

impl rpce::HeaderlessEncode for GetStatusChangeCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        self.context.encode_ptr(&mut index, dst)?;
        ensure_size!(in: dst, size: size_of::<u32>() * 2);
        dst.write_u32(self.timeout);
        dst.write_u32(self.states_ptr_length);
        ndr::encode_ptr(None, &mut index, dst)?; // rgReaderStates referent
        self.context.encode_value(dst)?;
        ensure_size!(in: dst, size: size_of::<u32>());
        dst.write_u32(self.states_length);
        for state in &self.states {
            state.encode_ptr(&mut index, dst)?;
        }
        for state in &self.states {
            state.encode_value(dst)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "GetStatusChange_Call"
    }

    fn size(&self) -> usize {
        self.context.size_ptr()
            + size_of::<u32>() * 2 // timeout, states_ptr_length
            + ndr::ptr_size(false) // rgReaderStates referent
            + self.context.size_value()
            + size_of::<u32>() // states_length
            + self.states.iter().map(ndr::Encode::size).sum::<usize>()
    }
}

impl rpce::HeaderlessEncode for ConnectCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        ndr::encode_ptr(None, &mut index, dst)?; // szReader referent
        self.common.encode_ptr(&mut index, dst)?;
        write_ndr_string(dst, &self.reader, CharacterSet::Unicode)?;
        self.common.encode_value(dst)
    }

    fn name(&self) -> &'static str {
        "Connect_Call"
    }

    fn size(&self) -> usize {
        ndr::ptr_size(false) // szReader referent
            + self.common.size_ptr()
            + ndr_string_size(&self.reader, CharacterSet::Unicode)
            + self.common.size_value()
    }
}

impl rpce::HeaderlessEncode for HCardAndDispositionCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        self.handle.encode_ptr(&mut index, dst)?;
        ensure_size!(in: dst, size: size_of::<u32>());
        dst.write_u32(self.disposition);
        self.handle.encode_value(dst)
    }

    fn name(&self) -> &'static str {
        "HCardAndDisposition_Call"
    }

    fn size(&self) -> usize {
        self.handle.size_ptr() + size_of::<u32>() + self.handle.size_value()
    }
}

impl rpce::HeaderlessEncode for StatusCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        self.handle.encode_ptr(&mut index, dst)?;
        ensure_size!(in: dst, size: size_of::<u32>() * 3);
        dst.write_u32(u32::from(self.reader_names_is_null));
        dst.write_u32(self.reader_length);
        dst.write_u32(self.atr_length);
        self.handle.encode_value(dst)
    }

    fn name(&self) -> &'static str {
        "Status_Call"
    }

    fn size(&self) -> usize {
        self.handle.size_ptr() + size_of::<u32>() * 3 + self.handle.size_value()
    }
}

impl rpce::HeaderlessEncode for TransmitCall {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        let mut index = 0;
        // Pointer section.
        self.handle.encode_ptr(&mut index, dst)?;
        encode_io_request_ptr(&self.send_pci, &mut index, dst)?;
        ensure_size!(in: dst, size: size_of::<u32>());
        dst.write_u32(self.send_length);
        ndr::encode_ptr(None, &mut index, dst)?; // pbSendBuffer referent
        if self.recv_pci.is_some() {
            ndr::encode_ptr(None, &mut index, dst)?; // pioRecvPci referent
        } else {
            ensure_size!(in: dst, size: size_of::<u32>());
            dst.write_u32(0); // NULL pioRecvPci
        }
        ensure_size!(in: dst, size: size_of::<u32>() * 2);
        dst.write_u32(u32::from(self.recv_buffer_is_null));
        dst.write_u32(self.recv_length);

        // Value section.
        self.handle.encode_value(dst)?;
        self.send_pci.encode_value(dst)?;
        ensure_size!(in: dst, size: size_of::<u32>() + self.send_buffer.len());
        dst.write_u32(self.send_length);
        dst.write_slice(&self.send_buffer);
        // The pioRecvPci struct is written in full only here, after pbSendBuffer; its
        // pointer-section entry above was just the referent.
        if let Some(recv_pci) = &self.recv_pci {
            encode_io_request_ptr(recv_pci, &mut index, dst)?;
            recv_pci.encode_value(dst)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "Transmit_Call"
    }

    fn size(&self) -> usize {
        self.handle.size_ptr()
            + self.send_pci.size_ptr()
            + size_of::<u32>() // send_length (pointer section)
            + ndr::ptr_size(false) // pbSendBuffer referent
            + size_of::<u32>() // pioRecvPci referent (or NULL)
            + size_of::<u32>() * 2 // recv_buffer_is_null, recv_length
            + self.handle.size_value()
            + self.send_pci.size_value()
            + size_of::<u32>() // send_length (value section)
            + self.send_buffer.len()
            + self.recv_pci.as_ref().map_or(0, ndr::Encode::size) // trailing pioRecvPci struct
    }
}

// ---- `*Return` decoders (what the client's reader sends back) ----

impl LongReturn {
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, None)?.into_inner())
    }

    pub fn return_code(&self) -> ReturnCode {
        self.return_code
    }
}

impl rpce::HeaderlessDecode for LongReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        expect_no_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>());
        let return_code = ReturnCode::try_from(src.read_u32())?;
        Ok(Self { return_code })
    }
}

impl EstablishContextReturn {
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, None)?.into_inner())
    }

    pub fn return_code(&self) -> ReturnCode {
        self.return_code
    }

    pub fn context(&self) -> ScardContext {
        self.context
    }
}

impl rpce::HeaderlessDecode for EstablishContextReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        expect_no_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>());
        let return_code = ReturnCode::try_from(src.read_u32())?;
        let mut index = 0;
        let mut context = ScardContext::decode_ptr(src, &mut index)?;
        context.decode_value(src, None)?;
        Ok(Self { return_code, context })
    }
}

impl ListReadersReturn {
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, Some(CharacterSet::Unicode))?.into_inner())
    }
}

impl rpce::HeaderlessDecode for ListReadersReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        let encoding = expect_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>() * 2);
        let return_code = ReturnCode::try_from(src.read_u32())?;
        let c_bytes = src.read_u32();
        let mut index = 0;
        let readers_ptr = ndr::decode_ptr(src, &mut index)?;
        // A NULL msz referent (a length-only probe) has no deferred conformant array.
        let readers = if readers_ptr != 0 {
            ensure_size!(in: src, size: size_of::<u32>());
            let _max_count = src.read_u32();
            Some(read_multistring_from_cursor(src, encoding)?)
        } else {
            None
        };
        Ok(Self {
            return_code,
            encoding,
            c_bytes,
            readers,
        })
    }
}

impl GetStatusChangeReturn {
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, None)?.into_inner())
    }
}

impl rpce::HeaderlessDecode for GetStatusChangeReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        expect_no_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>() * 2);
        let return_code = ReturnCode::try_from(src.read_u32())?;
        let _max_count = src.read_u32();
        let mut index = 0;
        let _states_ptr = ndr::decode_ptr(src, &mut index)?;
        ensure_size!(in: src, size: size_of::<u32>());
        let count = src.read_u32();
        let mut reader_states = Vec::new();
        for _ in 0..count {
            reader_states.push(ReaderStateCommonCall::decode(src)?);
        }
        Ok(Self {
            return_code,
            reader_states,
        })
    }
}

impl ConnectReturn {
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, None)?.into_inner())
    }
}

impl rpce::HeaderlessDecode for ConnectReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        expect_no_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>());
        let return_code = ReturnCode::try_from(src.read_u32())?;
        let mut index = 0;
        let mut handle = ScardHandle::decode_ptr(src, &mut index)?;
        ensure_size!(in: src, size: size_of::<u32>());
        let active_protocol = CardProtocol::from_bits_retain(src.read_u32());
        handle.decode_value(src, None)?;
        Ok(Self {
            return_code,
            handle,
            active_protocol,
        })
    }
}

impl StatusReturn {
    pub fn decode(src: &mut ReadCursor<'_>, charset: CharacterSet) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, Some(charset))?.into_inner())
    }
}

impl rpce::HeaderlessDecode for StatusReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        let encoding = expect_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>() * 2);
        let return_code = ReturnCode::try_from(src.read_u32())?;
        let reader_c_bytes = src.read_u32();
        let mut index = 0;
        let reader_names_ptr = ndr::decode_ptr(src, &mut index)?;
        // Fixed part after the pointer: dwState, dwProtocol, pbAtr[32], cbAtrLen.
        ensure_size!(in: src, size: size_of::<u32>() * 2 + 32 + size_of::<u32>());
        let state = CardState::try_from(src.read_u32())?;
        let protocol = CardProtocol::from_bits_retain(src.read_u32());
        let atr = src.read_array::<32>();
        let atr_length = src.read_u32();
        // Real Windows often returns a NULL mszReaderNames (an ATR-only Status): no deferred array.
        let reader_names = if reader_names_ptr != 0 {
            ensure_size!(in: src, size: size_of::<u32>());
            let _max_count = src.read_u32();
            Some(read_multistring_from_cursor(src, encoding)?)
        } else {
            None
        };
        Ok(Self {
            return_code,
            reader_names,
            reader_c_bytes,
            state,
            protocol,
            atr,
            atr_length,
            encoding,
        })
    }
}

impl TransmitReturn {
    pub fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        Ok(rpce::Pdu::<Self>::decode(src, None)?.into_inner())
    }
}

impl rpce::HeaderlessDecode for TransmitReturn {
    fn headerless_decode(src: &mut ReadCursor<'_>, charset: Option<CharacterSet>) -> DecodeResult<Self> {
        expect_no_charset(charset)?;
        ensure_size!(in: src, size: size_of::<u32>() * 2);
        let return_code = ReturnCode::try_from(src.read_u32())?;
        let mut index = 0;

        // pioRecvPci, in upstream's encoding: a bare u32(0) for none, otherwise the SCardIO_Request
        // pointer part (dwProtocol first, nonzero for a real T=0/T=1 protocol) then its value.
        let protocol_or_null = src.read_u32();
        let recv_pci = if protocol_or_null == 0 {
            None
        } else {
            ensure_size!(in: src, size: size_of::<u32>());
            let extra_bytes_length = cast_length!("TransmitReturn", "extra_bytes_length", src.read_u32())?;
            let _extra_bytes_ptr = ndr::decode_ptr(src, &mut index)?;
            ensure_size!(in: src, size: extra_bytes_length);
            let extra_bytes = src.read_slice(extra_bytes_length).to_vec();
            Some(SCardIORequest {
                protocol: CardProtocol::from_bits_retain(protocol_or_null),
                extra_bytes_length,
                extra_bytes,
            })
        };

        ensure_size!(in: src, size: size_of::<u32>());
        let recv_len = src.read_u32(); // cbRecvLength
        let recv_buffer_ptr = ndr::decode_ptr(src, &mut index)?;
        // When the card returns no data (e.g. a failed transaction), real Windows sends a NULL
        // pbRecvBuffer: no deferred array.
        let recv_buffer = if recv_buffer_ptr != 0 {
            ensure_size!(in: src, size: size_of::<u32>());
            let n = cast_length!("TransmitReturn", "recv_buffer_length", src.read_u32())?;
            ensure_size!(in: src, size: n);
            Some(src.read_slice(n).to_vec())
        } else {
            None
        };

        Ok(Self {
            return_code,
            recv_pci,
            recv_buffer,
            recv_len,
        })
    }
}

#[cfg(test)]
mod tests {
    //! `*Call`: our encode, then upstream's decode. `*Return`: upstream's encode, then our decode.
    //! Equality after the round trip proves the two halves agree byte for byte, without a client.
    //! The hand-built cases are byte layouts real 64-bit Windows sends that our own encoder never
    //! produces.

    use ironrdp_core::encode_vec;

    use super::super::{CardStateFlags, ScardIoCtlCode};
    use super::*;
    use crate::pdu::esc::rpce::{HeaderlessDecode as _, HeaderlessEncode as _};

    fn ctx() -> ScardContext {
        ScardContext::new(0x0102_0304)
    }

    /// Pointer-sized, as 64-bit Windows sends.
    fn ctx8() -> ScardContext {
        ScardContext::from_opaque(&0x1122_3344_5566_7788u64.to_le_bytes()).unwrap()
    }

    fn handle() -> ScardHandle {
        ScardHandle::new(ctx(), 0xABCD_1234)
    }

    fn handle8() -> ScardHandle {
        ScardHandle::from_opaque(ctx8(), &0xAABB_CCDD_EEFF_0011u64.to_le_bytes()).unwrap()
    }

    fn t1_pci() -> SCardIORequest {
        SCardIORequest {
            protocol: CardProtocol::SCARD_PROTOCOL_T1,
            extra_bytes_length: 0,
            extra_bytes: Vec::new(),
        }
    }

    fn common_call() -> ReaderStateCommonCall {
        let mut atr = [0u8; 36];
        atr[..11].copy_from_slice(&[0x3b, 0x95, 0x13, 0x81, 0x01, 0x80, 0x73, 0xff, 0x01, 0x00, 0x0b]);
        ReaderStateCommonCall {
            current_state: CardStateFlags::SCARD_STATE_UNAWARE,
            event_state: CardStateFlags::SCARD_STATE_PRESENT,
            atr_length: 11,
            atr,
        }
    }

    fn transmit_call(handle: ScardHandle, recv_pci: Option<SCardIORequest>) -> TransmitCall {
        let send_buffer = vec![0x00, 0xa4, 0x04, 0x00, 0x00];
        TransmitCall {
            handle,
            send_pci: t1_pci(),
            send_length: u32::try_from(send_buffer.len()).unwrap(),
            send_buffer,
            recv_pci,
            recv_buffer_is_null: false,
            recv_length: 256,
        }
    }

    /// Our encode, then upstream's decode via the `ScardCall` dispatcher the client uses.
    fn call_roundtrip(io_ctl_code: ScardIoCtlCode, call: ScardCall) {
        let mut bytes = vec![0u8; call.size()];
        call.encode(&mut WriteCursor::new(&mut bytes)).unwrap();
        let decoded = ScardCall::decode(io_ctl_code, &mut ReadCursor::new(&bytes)).unwrap();
        assert_eq!(decoded, call);
    }

    #[test]
    fn call_roundtrips() {
        call_roundtrip(
            ScardIoCtlCode::EstablishContext,
            ScardCall::EstablishContextCall(EstablishContextCall { scope: Scope::System }),
        );
        call_roundtrip(
            ScardIoCtlCode::ReleaseContext,
            ScardCall::ContextCall(ContextCall { context: ctx() }),
        );
        call_roundtrip(
            ScardIoCtlCode::ListReadersW,
            ScardCall::ListReadersCall(ListReadersCall {
                context: ctx(),
                groups_ptr_length: 0,
                groups_length: 0,
                groups_ptr: 0,
                groups: Vec::new(),
                readers_is_null: true,
                readers_size: 0,
            }),
        );
        call_roundtrip(
            ScardIoCtlCode::GetStatusChangeW,
            ScardCall::GetStatusChangeCall(GetStatusChangeCall {
                context: ctx(),
                timeout: 1000,
                states_ptr_length: 1,
                states_ptr: 0x0002_0004,
                states_length: 1,
                states: vec![ReaderState {
                    reader: "macrdp".to_owned(),
                    common: common_call(),
                }],
            }),
        );
        for reader in ["macrdp", "reader7"] {
            // "reader7" is odd-length, so its UTF-16 string needs a 2-byte tail pad.
            call_roundtrip(
                ScardIoCtlCode::ConnectW,
                ScardCall::ConnectCall(ConnectCall {
                    reader: reader.to_owned(),
                    common: ConnectCommon {
                        context: ctx(),
                        share_mode: 2,
                        preferred_protocols: CardProtocol::SCARD_PROTOCOL_T0 | CardProtocol::SCARD_PROTOCOL_T1,
                    },
                }),
            );
        }
        call_roundtrip(
            ScardIoCtlCode::Disconnect,
            ScardCall::HCardAndDispositionCall(HCardAndDispositionCall {
                handle: handle(),
                disposition: 1,
            }),
        );
        call_roundtrip(
            ScardIoCtlCode::StatusW,
            ScardCall::StatusCall(StatusCall {
                handle: handle(),
                reader_names_is_null: false,
                reader_length: 0,
                atr_length: 0,
            }),
        );
        for (handle, recv_pci) in [(handle(), None), (handle(), Some(t1_pci())), (handle8(), None)] {
            call_roundtrip(
                ScardIoCtlCode::Transmit,
                ScardCall::TransmitCall(transmit_call(handle, recv_pci)),
            );
        }
    }

    #[test]
    fn unsupported_call_does_not_encode() {
        let call = ScardCall::Unsupported;
        assert_eq!(call.size(), 0);
        assert!(call.encode(&mut WriteCursor::new(&mut [0u8; 16])).is_err());
    }

    /// Regression (live mstsc): with no extra PCI bytes, `pbExtraBytes` must be a NULL referent.
    /// Upstream's own `SCardIORequest` encoder writes a non-NULL one, which real Windows rejects.
    #[test]
    fn transmit_call_empty_pci_has_null_extra_bytes_referent() {
        let call = transmit_call(handle(), None);
        let mut body = vec![0u8; call.size()];
        rpce::HeaderlessEncode::encode(&call, &mut WriteCursor::new(&mut body)).unwrap();
        let send_pci = &body[handle().size_ptr()..][..12];
        assert_eq!(send_pci[..4], CardProtocol::SCARD_PROTOCOL_T1.bits().to_le_bytes());
        assert_eq!(send_pci[4..], [0u8; 8], "cbExtraBytes = 0 and a NULL referent");

        // The upstream encoder this works around: a non-NULL referent for the same input.
        let mut upstream = [0u8; 12];
        t1_pci()
            .encode_ptr(&mut 0, &mut WriteCursor::new(&mut upstream))
            .unwrap();
        assert_ne!(upstream[8..], [0u8; 4]);
    }

    /// Upstream's encode, then our decode.
    fn return_roundtrip<T>(pdu: rpce::Pdu<T>, decode: impl Fn(&mut ReadCursor<'_>) -> DecodeResult<T>)
    where
        T: rpce::HeaderlessEncode + PartialEq + core::fmt::Debug,
    {
        let bytes = encode_vec(&pdu).unwrap();
        assert_eq!(decode(&mut ReadCursor::new(&bytes)).unwrap(), pdu.into_inner());
    }

    #[test]
    fn return_roundtrips() {
        return_roundtrip(LongReturn::new(ReturnCode::Success), LongReturn::decode);
        return_roundtrip(LongReturn::new(ReturnCode::RemovedCard), LongReturn::decode);
        for context in [ScardContext::new(0xDEAD_BEEF), ctx8()] {
            return_roundtrip(
                EstablishContextReturn::new(ReturnCode::Success, context),
                EstablishContextReturn::decode,
            );
        }
        return_roundtrip(
            ListReadersReturn::new(ReturnCode::Success, vec!["macrdp".to_owned()], CharacterSet::Unicode),
            ListReadersReturn::decode,
        );
        return_roundtrip(
            ListReadersReturn::probe(ReturnCode::InsufficientBuffer, 64, CharacterSet::Unicode),
            ListReadersReturn::decode,
        );
        return_roundtrip(
            GetStatusChangeReturn::new(ReturnCode::Success, vec![common_call(), common_call()]),
            GetStatusChangeReturn::decode,
        );
        for handle in [ScardHandle::new(ctx(), 0x9999), handle8()] {
            return_roundtrip(
                ConnectReturn::new(ReturnCode::Success, handle, CardProtocol::SCARD_PROTOCOL_T1),
                ConnectReturn::decode,
            );
        }
        let mut atr = [0u8; 32];
        atr[..11].copy_from_slice(&[0x3b, 0x95, 0x13, 0x81, 0x01, 0x80, 0x73, 0xff, 0x01, 0x00, 0x0b]);
        return_roundtrip(
            StatusReturn::new(
                ReturnCode::Success,
                vec!["macrdp".to_owned()],
                CardState::Present,
                CardProtocol::SCARD_PROTOCOL_T1,
                atr,
                11,
                CharacterSet::Unicode,
            ),
            |src| StatusReturn::decode(src, CharacterSet::Unicode),
        );
        return_roundtrip(
            StatusReturn::names_probe(
                ReturnCode::Success,
                0,
                CardState::Present,
                CardProtocol::SCARD_PROTOCOL_T1,
                atr,
                11,
                CharacterSet::Unicode,
            ),
            |src| StatusReturn::decode(src, CharacterSet::Unicode),
        );
        for recv_pci in [None, Some(t1_pci())] {
            return_roundtrip(
                TransmitReturn::new(ReturnCode::Success, recv_pci, vec![0x90, 0x00]),
                TransmitReturn::decode,
            );
        }
        return_roundtrip(
            TransmitReturn::recv_probe(ReturnCode::InsufficientBuffer, None, 258),
            TransmitReturn::decode,
        );
    }

    #[test]
    fn return_accessors() {
        let ret = EstablishContextReturn::decode(&mut ReadCursor::new(
            &encode_vec(&EstablishContextReturn::new(ReturnCode::Success, ctx8())).unwrap(),
        ))
        .unwrap();
        assert_eq!(ret.return_code(), ReturnCode::Success);
        assert_eq!(ret.context(), ctx8());
        let ret = LongReturn::decode(&mut ReadCursor::new(
            &encode_vec(&LongReturn::new(ReturnCode::NoSmartcard)).unwrap(),
        ))
        .unwrap();
        assert_eq!(ret.return_code(), ReturnCode::NoSmartcard);
    }

    /// Real Windows returns the connect handle with an EMPTY embedded context (`cbContext = 0`,
    /// NULL referent, no deferred value) and an 8-byte handle.
    #[test]
    fn connect_return_decodes_empty_handle_context() {
        let handle_bytes = [0u8, 0, 0, 0, 1, 0, 0, 0xea];
        let mut body = Vec::new();
        body.extend_from_slice(&u32::from(ReturnCode::Success).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // cbContext = 0
        body.extend_from_slice(&0u32.to_le_bytes()); // NULL pbContext referent
        body.extend_from_slice(&8u32.to_le_bytes()); // cbHandle = 8
        body.extend_from_slice(&0x0002_0000u32.to_le_bytes()); // pbHandle referent
        body.extend_from_slice(&CardProtocol::SCARD_PROTOCOL_T1.bits().to_le_bytes());
        body.extend_from_slice(&8u32.to_le_bytes()); // pbHandle MaximumCount
        body.extend_from_slice(&handle_bytes);

        let decoded = ConnectReturn::headerless_decode(&mut ReadCursor::new(&body), None).unwrap();
        assert_eq!(decoded.return_code, ReturnCode::Success);
        assert_eq!(decoded.active_protocol, CardProtocol::SCARD_PROTOCOL_T1);
        assert!(decoded.handle.context().is_empty());
        assert_eq!(decoded.handle.as_bytes(), handle_bytes);

        // Re-sent in a Transmit_Call, the handle keeps its 8 bytes (not clobbered to 0).
        call_roundtrip(
            ScardIoCtlCode::Transmit,
            ScardCall::TransmitCall(transmit_call(decoded.handle, None)),
        );
    }

    /// When the card returns no data, real Windows sends NULL `pioRecvPci` and NULL `pbRecvBuffer`:
    /// 16 bytes, no deferred arrays.
    #[test]
    fn transmit_return_decodes_null_recv_buffer() {
        let mut body = Vec::new();
        body.extend_from_slice(&u32::from(ReturnCode::Success).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // pioRecvPci = NULL
        body.extend_from_slice(&0u32.to_le_bytes()); // cbRecvLength = 0
        body.extend_from_slice(&0u32.to_le_bytes()); // pbRecvBuffer = NULL

        let decoded = TransmitReturn::headerless_decode(&mut ReadCursor::new(&body), None).unwrap();
        assert_eq!(decoded.return_code, ReturnCode::Success);
        assert!(decoded.recv_pci.is_none());
        assert!(decoded.recv_buffer.is_none());
    }

    /// Real Windows often returns `Status_Return` with a NULL `mszReaderNames` (ATR only).
    #[test]
    fn status_return_decodes_null_reader_names() {
        let mut atr = [0u8; 32];
        atr[..11].copy_from_slice(&[0x3b, 0x8d, 0x01, 0x80, 0xfb, 0xa0, 0x00, 0x00, 0x03, 0x97, 0x42]);
        let mut body = Vec::new();
        body.extend_from_slice(&u32::from(ReturnCode::Success).to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes()); // cBytes = 0
        body.extend_from_slice(&0u32.to_le_bytes()); // mszReaderNames = NULL
        body.extend_from_slice(&u32::from(CardState::Present).to_le_bytes());
        body.extend_from_slice(&CardProtocol::SCARD_PROTOCOL_T1.bits().to_le_bytes());
        body.extend_from_slice(&atr);
        body.extend_from_slice(&11u32.to_le_bytes()); // cbAtrLen

        let decoded =
            StatusReturn::headerless_decode(&mut ReadCursor::new(&body), Some(CharacterSet::Unicode)).unwrap();
        assert_eq!(decoded.return_code, ReturnCode::Success);
        assert_eq!(decoded.state, CardState::Present);
        assert_eq!(decoded.atr_length, 11);
        assert_eq!(decoded.atr[..11], atr[..11]);
        assert!(decoded.reader_names.is_none());
    }

    /// A count larger than the input is an error, not a short list.
    #[test]
    fn get_status_change_return_rejects_count_past_input() {
        let mut body = Vec::new();
        body.extend_from_slice(&u32::from(ReturnCode::Success).to_le_bytes());
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // MaximumCount
        body.extend_from_slice(&0x0002_0000u32.to_le_bytes()); // referent
        body.extend_from_slice(&u32::MAX.to_le_bytes()); // count
        assert!(GetStatusChangeReturn::headerless_decode(&mut ReadCursor::new(&body), None).is_err());
    }

    /// `TryFrom<u32> for ReturnCode` is the inverse of upstream's `From<ReturnCode> for u32` for
    /// every code (a typo in either list breaks the round trip).
    #[test]
    fn return_code_conversion_inverts_upstream() {
        for raw in (0x8010_0001..=0x8010_0034).chain(0x8010_0065..=0x8010_0072).chain([0]) {
            let code = ReturnCode::try_from(raw).unwrap();
            assert_eq!(u32::from(code), raw);
        }
        assert!(ReturnCode::try_from(0x8010_0035).is_err());
        for raw in 0..=6 {
            assert_eq!(u32::from(CardState::try_from(raw).unwrap()), raw);
        }
        assert!(CardState::try_from(7).is_err());
    }
}
