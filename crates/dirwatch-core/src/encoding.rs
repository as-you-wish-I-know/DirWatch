//! Encoding detection for log files. Ported from the .NET `EncodingDetector` (behavioral spec,
//! build 2026-07-14.9).
//!
//! Recognises UTF-8, UTF-16 LE, and UTF-16 BE by byte-order mark, with a light no-BOM heuristic
//! for UTF-16 (many interleaved zero bytes). Falls back to UTF-8 when nothing else matches — the
//! common case for plain ASCII/UTF-8 logs. Uses `encoding_rs` for the actual codecs.

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE, UTF_8};

/// Result of encoding detection: the codec, the BOM byte length to skip, and a human label.
/// The labels match the .NET strings exactly so logs read the same across ports.
#[derive(Debug, Clone)]
pub struct Detected {
    pub encoding: &'static Encoding,
    pub bom_length: usize,
    pub label: &'static str,
}

/// Detect the encoding of a file head. `sample` is the first bytes; `count` how many are valid.
pub fn detect(sample: &[u8], count: usize) -> Detected {
    let count = count.min(sample.len());

    if count >= 3 && sample[0] == 0xEF && sample[1] == 0xBB && sample[2] == 0xBF {
        return Detected {
            encoding: UTF_8,
            bom_length: 3,
            label: "UTF-8 (BOM)",
        };
    }
    if count >= 2 && sample[0] == 0xFF && sample[1] == 0xFE {
        return Detected {
            encoding: UTF_16LE,
            bom_length: 2,
            label: "UTF-16 LE (BOM)",
        };
    }
    if count >= 2 && sample[0] == 0xFE && sample[1] == 0xFF {
        return Detected {
            encoding: UTF_16BE,
            bom_length: 2,
            label: "UTF-16 BE (BOM)",
        };
    }

    // No BOM: heuristic for UTF-16 by counting zero bytes at even/odd offsets.
    if count >= 4 {
        let (mut zero_odd, mut zero_even, mut examined) = (0usize, 0usize, 0usize);
        for (i, &b) in sample.iter().enumerate().take(count) {
            if b == 0x00 {
                if i & 1 == 0 {
                    zero_even += 1;
                } else {
                    zero_odd += 1;
                }
            }
            examined += 1;
        }
        // ASCII text in UTF-16 LE has zeros in odd positions; UTF-16 BE in even.
        let half = examined as f64 / 2.0;
        if zero_odd as f64 > half * 0.6 && zero_even == 0 {
            return Detected {
                encoding: UTF_16LE,
                bom_length: 0,
                label: "UTF-16 LE (heuristic)",
            };
        }
        if zero_even as f64 > half * 0.6 && zero_odd == 0 {
            return Detected {
                encoding: UTF_16BE,
                bom_length: 0,
                label: "UTF-16 BE (heuristic)",
            };
        }
    }

    Detected {
        encoding: UTF_8,
        bom_length: 0,
        label: "UTF-8/ASCII",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from EncodingDetectorTests.cs (parity oracle).

    #[test]
    fn utf8_bom() {
        let r = detect(&[0xEF, 0xBB, 0xBF, b'A'], 4);
        assert_eq!(r.bom_length, 3);
        assert!(r.label.contains("UTF-8"));
    }

    #[test]
    fn utf16_le_bom() {
        let r = detect(&[0xFF, 0xFE, b'A', 0x00], 4);
        assert_eq!(r.bom_length, 2);
        assert!(r.label.contains("LE"));
    }

    #[test]
    fn utf16_be_bom() {
        let r = detect(&[0xFE, 0xFF, 0x00, b'A'], 4);
        assert_eq!(r.bom_length, 2);
        assert!(r.label.contains("BE"));
    }

    #[test]
    fn ascii_defaults_utf8_no_bom() {
        let r = detect(b"hello", 5);
        assert_eq!(r.bom_length, 0);
        assert!(r.label.contains("UTF-8"));
    }

    #[test]
    fn utf16_le_no_bom_heuristic() {
        // "ABCD" in UTF-16 LE without BOM: zeros in odd positions.
        let bytes: Vec<u8> = "ABCD"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let r = detect(&bytes, bytes.len());
        assert!(r.label.contains("LE"));
    }

    #[test]
    fn utf16_be_no_bom_heuristic() {
        let bytes: Vec<u8> = "ABCD"
            .encode_utf16()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        let r = detect(&bytes, bytes.len());
        assert!(r.label.contains("BE"));
    }
}
