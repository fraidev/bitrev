#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(response) = bit_rev::peer::BencodeResponse::from_bytes(data) {
        let _ = response.get_peers();
    }
});
