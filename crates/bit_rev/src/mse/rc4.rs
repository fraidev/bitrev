/// RC4 as mandated by MSE. This is protocol obfuscation, not a modern cipher.
#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    pub fn new(key: &[u8]) -> Self {
        assert!(!key.is_empty(), "RC4 key must not be empty");
        let mut s = [0u8; 256];
        for (i, slot) in s.iter_mut().enumerate() {
            *slot = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        Self { s, i: 0, j: 0 }
    }

    /// MSE discards the first 1024 keystream bytes.
    pub fn for_mse(key: &[u8]) -> Self {
        let mut rc4 = Self::new(key);
        rc4.discard(1024);
        rc4
    }

    pub fn apply(&mut self, buf: &mut [u8]) {
        for byte in buf {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k = self.s[self.s[self.i as usize].wrapping_add(self.s[self.j as usize]) as usize];
            *byte ^= k;
        }
    }

    pub fn discard(&mut self, n: usize) {
        let mut sink = [0u8; 64];
        let mut left = n;
        while left > 0 {
            let chunk = left.min(sink.len());
            self.apply(&mut sink[..chunk]);
            left -= chunk;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Rc4;

    #[test]
    fn wikipedia_vector() {
        let mut rc4 = Rc4::new(b"Key");
        let mut buf = b"Plaintext".to_vec();
        rc4.apply(&mut buf);
        assert_eq!(buf, [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3]);
    }

    #[test]
    fn apply_is_its_own_inverse() {
        let mut enc = Rc4::new(b"mse-key");
        let mut dec = Rc4::new(b"mse-key");
        let mut buf = b"bitrev obfuscation".to_vec();
        let original = buf.clone();
        enc.apply(&mut buf);
        assert_ne!(buf, original);
        dec.apply(&mut buf);
        assert_eq!(buf, original);
    }
}
