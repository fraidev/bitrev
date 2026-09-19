pub fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    crate::utils::sha1_parts(parts)
}

pub fn hash_req1(s: &[u8; 96]) -> [u8; 20] {
    sha1(&[b"req1", s.as_slice()])
}

pub fn hash_req2(skey: &[u8; 20]) -> [u8; 20] {
    sha1(&[b"req2", skey.as_slice()])
}

pub fn hash_req3(s: &[u8; 96]) -> [u8; 20] {
    sha1(&[b"req3", s.as_slice()])
}

pub fn key_a(s: &[u8; 96], skey: &[u8; 20]) -> [u8; 20] {
    sha1(&[b"keyA", s.as_slice(), skey.as_slice()])
}

pub fn key_b(s: &[u8; 96], skey: &[u8; 20]) -> [u8; 20] {
    sha1(&[b"keyB", s.as_slice(), skey.as_slice()])
}

pub fn xor20(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for i in 0..20 {
        out[i] = a[i] ^ b[i];
    }
    out
}

pub fn find_slice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}
