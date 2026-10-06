//! CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection, xorout 0.

pub const INIT: u16 = 0xFFFF;

pub fn update(mut crc: u16, bytes: &[u8]) -> u16 {
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn crc16(bytes: &[u8]) -> u16 {
    update(INIT, bytes)
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value() {
        assert_eq!(super::crc16(b"123456789"), 0x29B1);
    }
}
