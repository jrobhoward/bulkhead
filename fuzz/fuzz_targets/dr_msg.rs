//! Arbitrary bytes from a cluster member as the body of a DR frame
//! (`Frame::Dr`, ROADMAP DR2): a message that decodes encodes and decodes
//! back to itself.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    bulkhead_host::dr_body_for_fuzzing(bytes);
});
