//! The firmware host suite's vectors (`tests/fixtures/link_vectors.txt`)
//! decoded and re-encoded by this crate: both sides of the LINK must agree
//! byte for byte.

use sctrl_proto::slip::{Decoder, Event, encode_frame};
use sctrl_proto::{crc, payload::Status, rle};

fn unhex(s: &str) -> Vec<u8> {
    if s == "-" {
        return Vec::new();
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn vectors() -> Vec<Vec<String>> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/link_vectors.txt");
    std::fs::read_to_string(path)
        .expect("tests/fixtures/link_vectors.txt")
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| l.split_whitespace().map(str::to_owned).collect())
        .collect()
}

fn decode_one(wire: &[u8]) -> sctrl_proto::Frame {
    let mut d = Decoder::new();
    let mut ev = Vec::new();
    d.feed(wire, &mut ev);
    match ev.as_slice() {
        [Event::Frame(f)] => f.clone(),
        other => panic!("expected one frame, got {other:?}"),
    }
}

#[test]
fn every_vector_agrees() {
    let v = vectors();
    let mut seen = std::collections::BTreeMap::<&str, usize>::new();
    for f in &v {
        *seen.entry(f[0].as_str()).or_default() += 1;
        match f[0].as_str() {
            "crc" => assert_eq!(crc::crc16(&unhex(&f[1])), u16::from_str_radix(&f[2], 16).unwrap()),
            "frame" => {
                let (ty, seq) = (
                    u8::from_str_radix(&f[2], 16).unwrap(),
                    u8::from_str_radix(&f[3], 16).unwrap(),
                );
                let payload = unhex(&f[4]);
                let wire = unhex(&f[5]);
                assert_eq!(encode_frame(ty, seq, &payload), wire, "encode {}", f[1]);
                let got = decode_one(&wire);
                assert_eq!((got.ty, got.seq, got.payload), (ty, seq, payload), "decode {}", f[1]);
            }
            "rle" => {
                let raw = unhex(&f[2]);
                let enc = unhex(&f[3]);
                assert_eq!(
                    rle::decode(&enc, raw.len()).as_deref(),
                    Some(raw.as_slice()),
                    "decode {}",
                    f[1]
                );
                assert_eq!(rle::encode(&raw), enc, "encode {}", f[1]);
            }
            "hostframe" => {
                let got = decode_one(&unhex(&f[2]));
                assert_eq!(encode_frame(got.ty, got.seq, &got.payload), unhex(&f[2]));
            }
            "status" => {
                let s = Status::parse(&unhex(&f[2])).unwrap();
                assert_eq!(s.uptime_ms, 0x0102_0304);
                assert_eq!(s.dial_hz, 14_074_000);
                assert_eq!((s.state, s.mode, s.page), (1, 5, 15));
                assert_eq!(s.faults, 0x40);
                assert_eq!(s.vin_mv, 8800);
                assert_eq!(s.rx_dbfs_x10, -723);
                assert_eq!(s.settings_hash, 0xDEAD_BEEF);
                assert_eq!(s.ref_ppm_x100, -1720);
                assert_eq!((s.band, s.arm_left_s), (6, 42));
                assert_eq!((s.cat0_age_ds, s.event_seq), (0x1234, 0x5A));
            }
            other => panic!("unknown vector kind {other}"),
        }
    }
    for kind in ["crc", "frame", "rle", "hostframe", "status"] {
        assert!(seen.get(kind).copied().unwrap_or(0) > 0, "no {kind} vectors");
    }
}
