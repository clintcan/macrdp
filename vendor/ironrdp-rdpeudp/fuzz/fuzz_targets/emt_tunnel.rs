#![no_main]
use libfuzzer_sys::fuzz_target;

// MS-RDPEMT tunnel PDUs. These ride INSIDE the reliable-UDP TLS tunnel (so
// post-handshake, lower exposure than the two raw decoders), but the codec still
// parses attacker-influenced bytes and must not panic.
fuzz_target!(|data: &[u8]| {
    let _ = ironrdp_rdpeudp::emt::TunnelCreateRequest::decode(data);
    let _ = ironrdp_core::decode::<ironrdp_rdpeudp::emt::TunnelHeader>(data);
});
