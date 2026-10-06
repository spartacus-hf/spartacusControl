//! PackBits as the firmware codes screen pages: control byte `c < 0x80` is
//! `c + 1` literal bytes; `c >= 0x80` repeats the next byte `c - 0x80 + 2`
//! times.

/// Decode exactly `out_len` bytes, or None.
pub fn decode(input: &[u8], out_len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(out_len);
    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        i += 1;
        if c < 0x80 {
            let n = c as usize + 1;
            if i + n > input.len() || out.len() + n > out_len {
                return None;
            }
            out.extend_from_slice(&input[i..i + n]);
            i += n;
        } else {
            let n = (c - 0x80) as usize + 2;
            if i >= input.len() || out.len() + n > out_len {
                return None;
            }
            out.extend(std::iter::repeat_n(input[i], n));
            i += 1;
        }
    }
    (out.len() == out_len).then_some(out)
}

/// The firmware's coder, for the simulator and tests: runs of three or
/// more as runs, literals up to 128 bytes.
pub fn encode(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() + src.len() / 128 + 1);
    let mut pos = 0;
    let total = src.len();
    while pos < total {
        let mut run = 1;
        while run < 129 && pos + run < total && src[pos + run] == src[pos] {
            run += 1;
        }
        if run >= 3 {
            out.push((0x80 + run - 2) as u8);
            out.push(src[pos]);
            pos += run;
            continue;
        }
        let mut len = 0;
        while len < 128 && pos + len < total {
            let i = pos + len;
            if i + 2 < total && src[i] == src[i + 1] && src[i] == src[i + 2] {
                break;
            }
            len += 1;
        }
        let len = len.max(1);
        out.push((len - 1) as u8);
        out.extend_from_slice(&src[pos..pos + len]);
        pos += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut page = vec![0u8; 360];
        page[10] = 5;
        page[11] = 5;
        for (i, b) in page[100..200].iter_mut().enumerate() {
            *b = (i * 37) as u8;
        }
        let enc = encode(&page);
        assert_eq!(decode(&enc, 360).unwrap(), page);
        assert!(decode(&enc, 359).is_none());
        assert!(decode(&[0x80], 2).is_none());
    }
}
