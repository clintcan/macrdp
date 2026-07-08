#![no_main]
use libfuzzer_sys::fuzz_target;

// RDPEUDP2 (0x0101) framing: the other raw pre-TLS datagram decoder. Drive both
// the full packet parser and the bare header decode. Must never panic on
// malformed input.
fuzz_target!(|data: &[u8]| {
    let _ = ironrdp_rdpeudp::eudp2::Eudp2Packet::parse(data);
    let _ = ironrdp_rdpeudp::eudp2::Eudp2Header::decode(data);
});
