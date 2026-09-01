#[derive(Debug, Clone, PartialEq)]
pub struct Bitfield {
    bytes: Vec<u8>,
}

impl Bitfield {
    pub fn new(bytes: Vec<u8>) -> Bitfield {
        Bitfield { bytes }
    }

    pub fn with_piece_count(count: usize) -> Bitfield {
        let nbytes = count.div_ceil(8);
        Bitfield {
            bytes: vec![0u8; nbytes],
        }
    }

    pub fn filled(piece_count: usize) -> Bitfield {
        let mut bitfield = Self::with_piece_count(piece_count);
        for index in 0..piece_count {
            bitfield.set_piece(index);
        }
        bitfield
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn has_piece(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let offset = index % 8;
        if byte_index >= self.bytes.len() {
            return false;
        }
        (self.bytes[byte_index] >> (7 - offset)) & 1 != 0
    }

    pub fn set_piece(&mut self, index: usize) {
        let byte_index = index / 8;
        let offset = index % 8;
        if byte_index >= self.bytes.len() {
            return;
        }
        let new_char = self.bytes[byte_index] | (1 << (7 - offset));
        self.bytes[byte_index] = new_char;
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.iter().all(|&x| x == 0)
    }
}

#[test]
fn has_piece_test() {
    let bitfield = Bitfield::new(vec![0b01010100, 0b01010100]);
    let outputs = [
        false, true, false, true, false, true, false, false, false, true, false, true, false, true,
        false, false, false, false, false, false,
    ];
    for (index, expected) in outputs.iter().enumerate() {
        assert_eq!(bitfield.has_piece(index), *expected);
    }
}

#[test]
fn set_piece_test() {
    let tests = [
        (
            // Set
            vec![0b01010100, 0b01010100],
            vec![0b01011100, 0b01010100],
            4,
        ),
        (
            // Not Set
            vec![0b01010100, 0b01010100],
            vec![0b01010100, 0b01010100],
            9,
        ),
        (
            // Set
            vec![0b01010100, 0b01010100],
            vec![0b01010100, 0b01010101],
            15,
        ),
        (
            //Not Set
            vec![0b01010100, 0b01010100],
            vec![0b01010100, 0b01010100],
            19,
        ),
    ];

    for (actual, expected, index) in tests.iter() {
        let mut bitfield = Bitfield::new(actual.clone());
        bitfield.set_piece(*index);
        assert_eq!(bitfield.bytes, *expected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_and_set_across_byte_boundaries() {
        let mut bitfield = Bitfield::new(vec![0, 0]);
        let last = 15;
        for index in [0, 7, 8, last] {
            bitfield.set_piece(index);
            assert!(bitfield.has_piece(index));
        }
        for index in [1, 6, 9, 14] {
            assert!(!bitfield.has_piece(index));
        }
    }

    #[test]
    fn out_of_range_has_and_set_are_safe() {
        let mut bitfield = Bitfield::new(vec![0, 0]);
        assert!(!bitfield.has_piece(100));
        bitfield.set_piece(100);
        assert!(!bitfield.has_piece(100));
        assert!(bitfield.is_empty());
    }

    #[test]
    fn is_empty_for_zero_set_and_empty_vec() {
        let mut bitfield = Bitfield::new(vec![0, 0]);
        assert!(bitfield.is_empty());
        bitfield.set_piece(0);
        assert!(!bitfield.is_empty());
        assert!(Bitfield::new(vec![]).is_empty());
    }

    #[test]
    fn with_piece_count_sizes_and_set() {
        let mut bitfield = Bitfield::with_piece_count(10);
        assert_eq!(bitfield.as_bytes().len(), 2);
        assert!(bitfield.is_empty());
        bitfield.set_piece(9);
        assert!(bitfield.has_piece(9));
        assert!(!bitfield.has_piece(0));
    }

    #[test]
    fn filled_sets_only_piece_count_bits() {
        let bitfield = Bitfield::filled(10);
        for index in 0..10 {
            assert!(bitfield.has_piece(index));
        }
        assert!(!bitfield.has_piece(10));
        assert_eq!(bitfield.as_bytes(), &[0b1111_1111, 0b1100_0000]);
    }
}
