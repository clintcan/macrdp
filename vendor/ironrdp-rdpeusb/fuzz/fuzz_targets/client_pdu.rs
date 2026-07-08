#![no_main]
use libfuzzer_sys::fuzz_target;

// The URBDRC client→server PDU decoder — what the macrdp SERVER parses off the
// (post-auth, opt-in, entitled) URBDRC dynamic virtual channel: capability
// responses, AddDevice announcements, and URB/IOCTL completions. Untrusted
// client input; must never panic / OOM / hang on malformed bytes.
fuzz_target!(|data: &[u8]| {
    let _ = ironrdp_core::decode::<ironrdp_rdpeusb::pdu::UrbdrcClientPdu>(data);
});
