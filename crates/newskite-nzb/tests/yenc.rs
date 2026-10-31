//! yEnc decoder/encoder tests, including the all-256-byte round trip from the acceptance
//! criteria (which catches an escape applied in the wrong direction).

use newskite_nzb::yenc::{decode, decode_data, encode_article, encode_data, YencError};

/// Deterministic pseudo-random bytes (xorshift) so tests need no RNG crate.
fn pseudo_random(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        })
        .collect()
}

fn all_bytes_orders() -> Vec<Vec<u8>> {
    let asc: Vec<u8> = (0..=255u8).collect();
    let desc: Vec<u8> = (0..=255u8).rev().collect();
    // Every byte value at every position of a line by repeating with a stride coprime to 256.
    let strided: Vec<u8> = (0..1024u32).map(|i| ((i * 37 + 11) % 256) as u8).collect();
    let mut mixed = asc.clone();
    mixed.extend(pseudo_random(4096, 0x9E37_79B9_7F4A_7C15));
    mixed.extend(&desc);
    vec![asc, desc, strided, mixed]
}

#[test]
fn known_vectors_fix_escape_direction() {
    // Plain: 0 -> '*' (42).  Must-escape bytes: 214->NUL, 224->LF, 227->CR, 19->'='.
    assert_eq!(encode_data(&[0], 128), b"*\r\n");
    assert_eq!(encode_data(&[214, 224, 227, 19], 128), b"=@=J=M=}\r\n");
    assert_eq!(decode_data(b"=@=J=M=}"), vec![214, 224, 227, 19]);
    assert_eq!(decode_data(b"*"), vec![0]);
    // TAB (byte 223) and space (byte 246) escaped at line edges; '.' (byte 4) at line start.
    assert_eq!(encode_data(&[223], 128), b"=I\r\n");
    assert_eq!(encode_data(&[246], 128), b"=`\r\n");
    assert_eq!(encode_data(&[4, 4], 128), b"=n.\r\n");
    assert_eq!(encode_data(&[0, 223, 0], 128), b"*\t*\r\n");
}

#[test]
fn must_escape_bytes_round_trip() {
    let payload = [214u8, 224, 227, 19, 214, 214, 19, 224];
    for ll in [128, 64, 1] {
        let enc = encode_data(&payload, ll);
        assert!(!enc.contains(&0), "NUL leaked into output");
        // Only CRLF terminators may contain CR/LF.
        for line in enc.split(|&b| b == b'\n') {
            let body = line.strip_suffix(b"\r").unwrap_or(line);
            assert!(!body.contains(&b'\r'));
        }
        assert_eq!(decode_data(&enc), payload);
        let art = encode_article("esc.bin", payload.len() as u64, 1, 1, 1, &payload, ll);
        let part = decode(&art).unwrap();
        assert_eq!(part.data, payload);
        assert_eq!(part.crc_ok, Some(true));
    }
}

#[test]
fn all_256_byte_values_round_trip() {
    for payload in all_bytes_orders() {
        for ll in [128usize, 64, 1, 0, 7] {
            let enc = encode_data(&payload, ll);
            assert_eq!(decode_data(&enc), payload, "line_len {ll}");
            // LF-only variant.
            let lf: Vec<u8> = enc.iter().copied().filter(|&b| b != b'\r').collect();
            assert_eq!(decode_data(&lf), payload, "LF-only, line_len {ll}");

            let art = encode_article("all bytes.bin", payload.len() as u64, 1, 1, 1, &payload, ll);
            let part = decode(&art).unwrap();
            assert_eq!(part.data, payload, "article line_len {ll}");
            assert_eq!(part.crc_ok, Some(true));
            assert_eq!(part.name, "all bytes.bin");
            assert_eq!((part.begin, part.end), (1, payload.len() as u64));
            assert_eq!(part.part, None);

            let art_lf: Vec<u8> = art.iter().copied().filter(|&b| b != b'\r').collect();
            assert_eq!(decode(&art_lf).unwrap().data, payload);
        }
    }
}

#[test]
fn encoded_lines_respect_length_and_edges() {
    let payload = pseudo_random(10_000, 42);
    for ll in [128usize, 64, 1] {
        let enc = encode_data(&payload, ll);
        for line in enc.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let body = line.strip_suffix(b"\r").unwrap();
            assert!(body.len() <= ll + 1, "line too long: {}", body.len());
            assert!(!body.is_empty());
            assert_ne!(body[0], b'.', "leading dot not escaped");
            assert!(!matches!(body[0], b' ' | b'\t'));
            assert!(!matches!(body[body.len() - 1], b' ' | b'\t'));
        }
    }
}

#[test]
fn multipart_begin_end() {
    let file = pseudo_random(3000, 7);
    let chunks = [(0usize, 1000usize), (1000, 2000), (2000, 3000)];
    let mut rebuilt = vec![0u8; file.len()];
    for (i, (a, b)) in chunks.iter().enumerate() {
        let art = encode_article(
            "multi part file.bin",
            file.len() as u64,
            i as u32 + 1,
            3,
            *a as u64 + 1,
            &file[*a..*b],
            128,
        );
        let p = decode(&art).unwrap();
        assert_eq!(p.part, Some(i as u32 + 1));
        assert_eq!(p.total, Some(3));
        assert_eq!(p.size, 3000);
        assert_eq!(p.begin, *a as u64 + 1);
        assert_eq!(p.end, *b as u64);
        assert_eq!(p.crc_ok, Some(true));
        assert_eq!(p.name, "multi part file.bin");
        rebuilt[(p.begin - 1) as usize..p.end as usize].copy_from_slice(&p.data);
    }
    assert_eq!(rebuilt, file);
}

#[test]
fn size_mismatch_is_reported() {
    // Header claims 5 bytes, data holds 3.
    let mut art = b"=ybegin line=128 size=5 name=x.bin\r\n".to_vec();
    art.extend(encode_data(b"abc", 128));
    art.extend(b"=yend size=3\r\n");
    assert_eq!(
        decode(&art),
        Err(YencError::SizeMismatch { expected: 5, got: 3 })
    );

    // Multipart range says 10 bytes, data holds 4.
    let mut art = b"=ybegin part=1 total=2 line=128 size=20 name=x.bin\r\n=ypart begin=1 end=10\r\n"
        .to_vec();
    art.extend(encode_data(b"abcd", 128));
    art.extend(b"=yend size=4 part=1\r\n");
    assert_eq!(
        decode(&art),
        Err(YencError::SizeMismatch { expected: 10, got: 4 })
    );

    // =yend size disagrees with the decoded data.
    let mut art = b"=ybegin line=128 size=3 name=x.bin\r\n".to_vec();
    art.extend(encode_data(b"abc", 128));
    art.extend(b"=yend size=4\r\n");
    assert_eq!(
        decode(&art),
        Err(YencError::SizeMismatch { expected: 4, got: 3 })
    );
}

#[test]
fn crc_mismatch_sets_crc_ok_false() {
    let data = b"hello yEnc world";
    let mut art = encode_article("h.txt", 100, 1, 2, 1, data, 128);
    let key = b"pcrc32=";
    let pos = art.windows(key.len()).position(|w| w == key).unwrap() + key.len();
    // Flip one hex digit of the checksum.
    art[pos] = if art[pos] == b'0' { b'1' } else { b'0' };
    let p = decode(&art).unwrap();
    assert_eq!(p.crc_ok, Some(false));
    assert_eq!(p.data, data);

    // Single-part crc32 mismatch.
    let mut art = b"=ybegin line=128 size=3 name=a\r\n".to_vec();
    art.extend(encode_data(b"abc", 128));
    art.extend(b"=yend size=3 crc32=deadbeef\r\n");
    assert_eq!(decode(&art).unwrap().crc_ok, Some(false));

    // No checksum at all.
    let mut art = b"=ybegin line=128 size=3 name=a\r\n".to_vec();
    art.extend(encode_data(b"abc", 128));
    art.extend(b"=yend size=3\r\n");
    assert_eq!(decode(&art).unwrap().crc_ok, None);

    // A multipart article carries only the whole-file crc32: not applicable to this part.
    let mut art = b"=ybegin part=1 total=2 line=128 size=6 name=a\r\n=ypart begin=1 end=3\r\n".to_vec();
    art.extend(encode_data(b"abc", 128));
    art.extend(b"=yend size=3 part=1 crc32=deadbeef\r\n");
    assert_eq!(decode(&art).unwrap().crc_ok, None);
}

#[test]
fn junk_before_header_is_skipped() {
    let payload: Vec<u8> = (0..=255u8).collect();
    let mut body = b"This is a preamble line\r\n\r\n=y not a header\r\n=ybeginning? no\r\n".to_vec();
    body.extend(encode_article("j.bin", 1024, 2, 4, 257, &payload, 64));
    let p = decode(&body).unwrap();
    assert_eq!(p.data, payload);
    assert_eq!((p.begin, p.end), (257, 512));
    assert_eq!(p.crc_ok, Some(true));
}

#[test]
fn no_header_and_bad_headers() {
    assert_eq!(decode(b""), Err(YencError::NoHeader));
    assert_eq!(decode(b"just text\r\nmore\r\n"), Err(YencError::NoHeader));
    assert!(matches!(
        decode(b"=ybegin line=128 name=x\r\n=yend size=0\r\n"),
        Err(YencError::BadHeader(_))
    ));
    assert!(matches!(
        decode(b"=ybegin size=abc name=x\r\n"),
        Err(YencError::BadHeader(_))
    ));
    // part= without =ypart.
    assert!(matches!(
        decode(b"=ybegin part=1 size=3 name=x\r\nKLM\r\n=yend size=3\r\n"),
        Err(YencError::BadHeader(_))
    ));
    // =ypart missing end.
    assert!(matches!(
        decode(b"=ybegin part=1 size=3 name=x\r\n=ypart begin=1\r\nKLM\r\n"),
        Err(YencError::BadHeader(_))
    ));
}

#[test]
fn bad_ranges() {
    for ypart in ["begin=0 end=3", "begin=5 end=4", "begin=1 end=11"] {
        let art = format!("=ybegin part=1 size=10 name=x\r\n=ypart {ypart}\r\nKLM\r\n=yend size=3\r\n");
        assert_eq!(decode(art.as_bytes()), Err(YencError::BadRange), "{ypart}");
    }
}

#[test]
fn double_dot_lines_are_not_unstuffed_here() {
    // A data line beginning with ".." is genuine data at this layer: '.'-42 twice.
    let body = b"=ybegin line=128 size=3 name=d\r\n..*\r\n=yend size=3\r\n";
    let p = decode(body).unwrap();
    assert_eq!(p.data, vec![4, 4, 0]);
}

#[test]
fn missing_yend_is_tolerated() {
    let mut body = b"=ybegin line=128 size=3 name=n o.bin\r\n".to_vec();
    body.extend(encode_data(b"xyz", 128));
    let p = decode(&body).unwrap();
    assert_eq!(p.data, b"xyz");
    assert_eq!(p.crc_ok, None);
    assert_eq!(p.name, "n o.bin");
}

#[test]
fn empty_single_part_file() {
    let art = encode_article("empty", 0, 1, 1, 1, &[], 128);
    let p = decode(&art).unwrap();
    assert!(p.data.is_empty());
    assert_eq!((p.begin, p.end, p.size), (1, 0, 0));
    assert_eq!(p.crc_ok, Some(true));
}

#[test]
fn control_chars_stripped_from_encoded_name() {
    let art = encode_article("evil\r\nname=x", 1, 1, 1, 1, &[1], 128);
    let p = decode(&art).unwrap();
    assert_eq!(p.name, "evilname=x");
    assert_eq!(p.data, vec![1]);
}

#[test]
fn large_payload_round_trip() {
    let payload = pseudo_random(2 * 1024 * 1024, 0xDEAD_BEEF);
    let art = encode_article("big.bin", payload.len() as u64, 1, 1, 1, &payload, 128);
    let p = decode(&art).unwrap();
    assert_eq!(p.data.len(), payload.len());
    assert!(p.data == payload);
    assert_eq!(p.crc_ok, Some(true));
}
