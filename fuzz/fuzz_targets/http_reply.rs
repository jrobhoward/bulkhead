//! Arbitrary reply bodies from an app in the HTTP response format, through
//! the ingress driver's reading of a reply (`bulkhead_sdk::http`'s
//! `Response::from_reply`, then the driver's hyper response). It never
//! panics and answers a valid status code. Needs `--cfg fuzzing`
//! (cargo-fuzz sets it).

#![no_main]

use libfuzzer_sys::fuzz_target;
use bulkhead_ingress::serve::fuzz_reply;

fuzz_target!(|bytes: &[u8]| {
    let code = fuzz_reply(bytes);
    assert!((100..=999).contains(&code));
});
