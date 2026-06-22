//! In-process RDP connect/negotiation integration test (Layer 2 "deep").
//!
//! Drives a real IronRDP *client* (`ironrdp-connector` + `ironrdp-tokio`)
//! through the full connect handshake against our own `ironrdp_server::RdpServer`
//! over an in-memory `tokio::io::duplex` pipe — no TCP, no ScreenCaptureKit, no
//! TCC. This is the first protocol-level test that exercises the real acceptor +
//! capability/connection-finalization sequence end to end, rather than a pure
//! decision function in isolation.
//!
//! Security is **TLS-only**: the IronRDP client refuses standard RDP security
//! (`standard RDP security is not supported`), so TLS is mandatory — but CredSSP
//! / NLA is left off (`enable_credssp: false`) so none of the SSPI/NTLM
//! machinery runs and the `NetworkClient` is never called.
//!
//! Display + input are test doubles (not macrdp's `CaptureDisplay`/
//! `MacInputHandler`), so the test is fully cross-platform — it runs on Linux CI
//! AND locally on macOS without touching the screen-capture backend.
//!
//! What it asserts: with `set_honor_client_desktop_size(true)`, a client that
//! requests 1920×1080 gets a session negotiated at 1920×1080 even though the
//! server's display starts at 1024×768 — i.e. the vendored acceptor's
//! client-resolution auto-adopt works across a real handshake. With it off, the
//! client gets the server's own size. (Pure-fn coverage of the adopt decision
//! lives in `capture.rs::adopt_client_size`; this proves the wire path.)

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Result;
use ironrdp_connector::sspi::generator::NetworkRequest;
use ironrdp_connector::{ClientConnector, Config, ConnectorResult, Credentials, DesktopSize};
use ironrdp_server::{
    DesktopSize as ServerDesktopSize, DisplayUpdate, KeyboardEvent, MouseEvent, RdpServer,
    RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler,
};
use ironrdp_tokio::TokioFramed;
use tokio_rustls::{rustls, TlsConnector};

// ---- server-side test doubles ---------------------------------------------

struct TestInput;
impl RdpServerInputHandler for TestInput {
    fn keyboard(&mut self, _e: KeyboardEvent) {}
    fn mouse(&mut self, _e: MouseEvent) {}
}

/// Yields nothing — the test only needs the connect/negotiation phase, which
/// completes before any frame is required. The server task is cancelled when the
/// test returns, so parking here forever is fine.
struct TestUpdates;
#[async_trait::async_trait]
impl RdpServerDisplayUpdates for TestUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        std::future::pending::<()>().await;
        Ok(None)
    }
}

struct TestDisplay {
    size: ServerDesktopSize,
}
#[async_trait::async_trait]
impl RdpServerDisplay for TestDisplay {
    async fn size(&mut self) -> ServerDesktopSize {
        self.size
    }
    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(TestUpdates))
    }
}

// ---- client-side plumbing --------------------------------------------------

/// CredSSP is disabled, so the connector never performs a network request and
/// this is never called.
struct NoNetwork;
impl ironrdp_tokio::NetworkClient for NoNetwork {
    async fn send(&mut self, _req: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        unreachable!("CredSSP disabled — NetworkClient must not be called")
    }
}

/// Accept any server certificate: both ends are in-process and the cert is a
/// throwaway self-signed one generated per test. (Standard test-only verifier.)
#[derive(Debug)]
struct NoCertVerify;
impl rustls::client::danger::ServerCertVerifier for NoCertVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ED25519,
        ]
    }
}

// ---- harness ---------------------------------------------------------------

/// Build a server-side `TlsAcceptor` from a fresh throwaway self-signed cert
/// (same `rcgen` path as `main.rs::make_tls_acceptor`, minus the on-disk
/// persistence we don't want in a test).
fn server_tls_acceptor() -> tokio_rustls::TlsAcceptor {
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let CertifiedKey { cert, key_pair } =
        generate_simple_self_signed(vec!["localhost".to_owned()]).expect("gen self-signed cert");
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(key_pair.serialize_der()).expect("key der");
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("server config");
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

/// A minimal client `Config` requesting a `width`×`height` desktop, TLS-only
/// (no CredSSP). Field set mirrors IronRDP's own `examples/screenshot.rs` for
/// this pinned rev.
fn client_config(width: u16, height: u16) -> Config {
    use ironrdp_pdu::gcc::KeyboardType;
    use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
    use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};

    Config {
        credentials: Credentials::UsernamePassword {
            username: "test".to_owned(),
            password: "test".to_owned(),
        },
        domain: None,
        enable_tls: true,
        enable_credssp: false,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize { width, height },
        bitmap: None,
        client_build: 0,
        client_name: "macrdp-conn-test".to_owned(),
        client_dir: String::new(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: None,
        pointer_software_rendering: false,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

/// Run one full client→server connect over an in-memory duplex with the given
/// `honor_client_desktop_size`, returning the negotiated desktop size the client
/// sees. The server's own display is fixed at `server_w`×`server_h`.
async fn negotiate(
    server_w: u16,
    server_h: u16,
    client_w: u16,
    client_h: u16,
    honor: bool,
) -> anyhow::Result<(u16, u16)> {
    // Best-effort: surface server/connector tracing when RUST_LOG is set.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);

    // ---- server task ----
    let acceptor = server_tls_acceptor();
    let display = TestDisplay {
        size: ServerDesktopSize {
            width: server_w,
            height: server_h,
        },
    };
    let mut server = RdpServer::builder()
        .with_addr((Ipv4Addr::LOCALHOST, 3389))
        .with_tls(acceptor)
        .with_input_handler(TestInput)
        .with_display_handler(display)
        .build();
    server.set_honor_client_desktop_size(honor);

    // The server's `run_connection` future is `!Send` (the vendored server uses
    // `Rc` internally), so it can't be `tokio::spawn`ed. Run it as a background
    // task on a `LocalSet` (the `#[tokio::test]` current-thread runtime) and
    // await ONLY the client. The two interleave over the duplex while the client
    // awaits; once the client has its result we abort the server task. (Awaiting
    // the client directly — rather than racing it against the server in
    // `select!` — avoids a false failure when the client finishing drops the
    // duplex and the server then ends with `Ok(Disconnect)`.)
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let server_task = tokio::task::spawn_local(async move {
                let _ = server.run_connection(server_io).await;
            });

            // pre-TLS negotiation
            let mut connector = ClientConnector::new(
                client_config(client_w, client_h),
                SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            );
            let mut framed = TokioFramed::new(client_io);
            let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector).await?;
            let initial = framed.into_inner_no_leftover();

            // TLS upgrade over the same duplex
            let mut tls_cfg = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoCertVerify))
                .with_no_client_auth();
            // CredSSP would forbid resumption; harmless to disable here regardless.
            tls_cfg.resumption = rustls::client::Resumption::disabled();
            let tls_connector = TlsConnector::from(Arc::new(tls_cfg));
            let server_name = rustls::pki_types::ServerName::try_from("localhost")?.to_owned();
            let tls_stream = tls_connector.connect(server_name, initial).await?;

            // finalize over TLS
            let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
            let mut tls_framed = TokioFramed::new(tls_stream);
            let mut net = NoNetwork;
            let result = ironrdp_tokio::connect_finalize(
                upgraded,
                connector,
                &mut tls_framed,
                &mut net,
                "localhost".into(),
                // server_public_key: only used for CredSSP channel binding (off).
                Vec::new(),
                None,
            )
            .await?;

            server_task.abort();
            Ok::<(u16, u16), anyhow::Error>((result.desktop_size.width, result.desktop_size.height))
        })
        .await
}

#[tokio::test]
async fn client_resolution_adopted_when_honored() -> anyhow::Result<()> {
    // Server display is 1024×768; client asks for 1920×1080. With honoring on,
    // the vendored acceptor negotiates the client's size in Demand Active.
    let (w, h) = negotiate(1024, 768, 1920, 1080, true).await?;
    assert_eq!(
        (w, h),
        (1920, 1080),
        "client-requested size should be adopted"
    );
    Ok(())
}

#[tokio::test]
async fn server_size_kept_when_not_honored() -> anyhow::Result<()> {
    // Same request, honoring off → the client gets the server's own size,
    // proving the adopt is gated by the flag (not incidental).
    let (w, h) = negotiate(1024, 768, 1920, 1080, false).await?;
    assert_eq!(
        (w, h),
        (1024, 768),
        "without honoring, server size is served"
    );
    Ok(())
}
