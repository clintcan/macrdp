#![no_main]
use libfuzzer_sys::fuzz_target;

// RDPEUDP v1 whole-datagram decoder: parses a raw UDP datagram from anyone who
// can reach the multitransport port, BEFORE any TLS/auth. It must never panic
// (or hang / OOM) on malformed input — a panic here is a pre-auth remote DoS.
fuzz_target!(|data: &[u8]| {
    let _ = ironrdp_rdpeudp::datagram::Datagram::decode(data);
});
