//! NZB parsing and filename sanitization tests.

use std::path::Path;

use newskite_nzb::nzb::{parse, parse_file, sanitize_filename, NzbError, MAX_FILENAME_BYTES};

const REALISTIC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="title">Linux ISO Collection &amp; Extras</meta>
    <meta type="password">s3cret</meta>
    <meta type="category">Software</meta>
  </head>
  <!-- a comment -->
  <file poster="Poster &lt;poster@example.com&gt;" date="1700000000"
        subject="Linux.ISO.Collection [01/03] - &quot;linux.part01.rar&quot; yEnc (1/3)">
    <groups>
      <group>alt.binaries.test</group>
      <group> alt.binaries.linux </group>
    </groups>
    <segments>
      <segment bytes="739811" number="3">part3of3.abc&amp;def@news.example.com</segment>
      <segment bytes="739812" number="1">
          part1of3.abc@news.example.com
      </segment>
      <segment bytes="739813" number="2">&lt;part2of3.abc@news.example.com&gt;</segment>
      <segment bytes="1" number="2">duplicate-should-be-dropped@news.example.com</segment>
    </segments>
  </file>
  <file poster="Poster" date="1700000001"
        subject="Linux.ISO.Collection [02/03] - &quot;linux.part02.rar&quot; yEnc (1/2)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="2">b2@x</segment>
      <segment bytes="200" number="1">b1@x</segment>
    </segments>
  </file>
  <file poster="Poster" date="not-a-date"
        subject="Linux.ISO.Collection [03/03] - linux.par2 yEnc (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="50" number="1"><![CDATA[c1@x]]></segment>
    </segments>
  </file>
  <file poster="Nobody" date="0" subject="no segments at all">
    <groups><group>alt.binaries.test</group></groups>
    <segments></segments>
  </file>
</nzb>
"#;

#[test]
fn parses_realistic_multi_file_nzb() {
    let nzb = parse(REALISTIC.as_bytes()).unwrap();
    assert_eq!(nzb.files.len(), 3, "file without segments must be dropped");
    assert_eq!(
        nzb.meta.get("title").map(String::as_str),
        Some("Linux ISO Collection & Extras")
    );
    assert_eq!(nzb.meta.get("password").map(String::as_str), Some("s3cret"));

    let f = &nzb.files[0];
    assert_eq!(f.poster, "Poster <poster@example.com>");
    assert_eq!(f.date, 1_700_000_000);
    assert_eq!(f.groups, vec!["alt.binaries.test", "alt.binaries.linux"]);
    assert_eq!(f.filename(), "linux.part01.rar");
    let numbers: Vec<u32> = f.segments.iter().map(|s| s.number).collect();
    assert_eq!(numbers, vec![1, 2, 3]);
    assert_eq!(f.segments[0].message_id, "part1of3.abc@news.example.com");
    assert_eq!(f.segments[1].message_id, "part2of3.abc@news.example.com");
    assert_eq!(f.segments[1].bytes, 739_813, "first duplicate kept");
    assert_eq!(f.segments[2].message_id, "part3of3.abc&def@news.example.com");
    assert_eq!(f.total_bytes(), 739_811 + 739_812 + 739_813);

    assert_eq!(nzb.files[1].filename(), "linux.part02.rar");
    assert_eq!(nzb.files[1].segments[0].message_id, "b1@x");
    assert_eq!(nzb.files[2].filename(), "linux.par2");
    assert_eq!(nzb.files[2].date, 0);
    assert_eq!(nzb.files[2].segments[0].message_id, "c1@x");

    assert_eq!(nzb.segment_count(), 6);
    assert_eq!(nzb.total_bytes(), 739_811 + 739_812 + 739_813 + 300 + 50);
    assert_eq!(
        nzb.job_name(Path::new("whatever.nzb")),
        "Linux ISO Collection & Extras"
    );
}

#[test]
fn prefixed_namespace_and_char_refs() {
    let xml = r#"<n:nzb xmlns:n="http://www.newzbin.com/DTD/2003/nzb">
      <n:file poster="p" date="1" subject="a &#34;q&#x2E;bin&#34; yEnc">
        <n:groups><n:group>g</n:group></n:groups>
        <n:segments><n:segment bytes="5" number="1">id&#64;host</n:segment></n:segments>
      </n:file></n:nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    assert_eq!(nzb.files[0].subject, r#"a "q.bin" yEnc"#);
    assert_eq!(nzb.files[0].filename(), "q.bin");
    assert_eq!(nzb.files[0].segments[0].message_id, "id@host");
}

#[test]
fn job_name_falls_back_to_file_stem() {
    let xml = r#"<nzb><file subject="x" poster="p" date="1"><groups/><segments>
        <segment number="1" bytes="1">a@b</segment></segments></file></nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    assert_eq!(nzb.job_name(Path::new("/tmp/My Show S01E01.nzb")), "My Show S01E01");
    assert_eq!(nzb.job_name(Path::new("")), "unnamed");
    assert_eq!(nzb.files[0].filename(), "x");
}

#[test]
fn malformed_xml_is_an_error() {
    let cases = [
        "<nzb><file subject=\"x\"><segments></file></nzb>",
        "<nzb><file subject=\"x\">",
        "<html><body/></html>",
        "not xml at all",
        "<nzb></nzb><nzb></nzb>",
    ];
    for c in cases {
        assert!(matches!(parse(c.as_bytes()), Err(NzbError::Xml(_))), "{c}");
    }
}

#[test]
fn empty_nzb_is_empty() {
    for c in [
        "",
        "   \n",
        r#"<?xml version="1.0"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"></nzb>"#,
        "<nzb><head><meta type=\"name\">x</meta></head></nzb>",
        "<nzb><file subject=\"x\"><segments><segment number=\"1\">  </segment></segments></file></nzb>",
        "<nzb><file subject=\"x\"><segments><segment number=\"0\">a@b</segment></segments></file></nzb>",
    ] {
        assert!(matches!(parse(c.as_bytes()), Err(NzbError::Empty)), "{c:?}");
    }
}

#[test]
fn parse_file_reports_io_errors() {
    let err = parse_file(Path::new("/definitely/not/here/x.nzb")).unwrap_err();
    assert!(matches!(err, NzbError::Io(_)));

    let dir = std::env::temp_dir().join(format!("newskite-nzb-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("real.nzb");
    std::fs::write(&path, REALISTIC).unwrap();
    let nzb = parse_file(&path).unwrap();
    assert_eq!(nzb.files.len(), 3);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn utf8_bom_is_accepted() {
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(REALISTIC.as_bytes());
    assert_eq!(parse(&bytes).unwrap().files.len(), 3);
}

#[test]
fn sanitize_path_traversal_and_absolute() {
    assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_filename("/etc/shadow"), "shadow");
    assert_eq!(sanitize_filename(r"..\..\Windows\System32\evil.dll"), "evil.dll");
    assert_eq!(sanitize_filename(r"C:\Users\x\file.txt"), "file.txt");
    assert_eq!(sanitize_filename("C:file.txt"), "file.txt");
    assert_eq!(sanitize_filename("dir/"), "dir");
    assert_eq!(sanitize_filename("a/./.."), "a");
}

#[test]
fn sanitize_never_dot_or_empty() {
    for raw in ["", ".", "..", "...", "/", "../", " . ", "\0\0", "\u{7f}", "  "] {
        let s = sanitize_filename(raw);
        assert_eq!(s, "unnamed", "{raw:?}");
    }
}

#[test]
fn sanitize_reserved_names() {
    assert_eq!(sanitize_filename("CON"), "_CON");
    assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
    assert_eq!(sanitize_filename("com1.tar.gz"), "_com1.tar.gz");
    assert_eq!(sanitize_filename("LPT9"), "_LPT9");
    assert_eq!(sanitize_filename("aux "), "_aux");
    assert_eq!(sanitize_filename("console.log"), "console.log");
    assert_eq!(sanitize_filename("COM10"), "COM10");
}

#[test]
fn sanitize_control_and_special_chars() {
    assert_eq!(sanitize_filename("a\0b\tc\nd\u{1b}e.bin"), "abcde.bin");
    assert_eq!(sanitize_filename(r#"what?<is>"this"|*:.mkv"#), "what__is__this____.mkv");
    assert_eq!(sanitize_filename("name. . ."), "name");
    assert_eq!(sanitize_filename("  leading.txt"), "leading.txt");
    assert_eq!(sanitize_filename(".hidden"), ".hidden");
    assert_eq!(sanitize_filename("Ünïcödé 名前.rar"), "Ünïcödé 名前.rar");
}

#[test]
fn sanitize_length_cap_on_char_boundary() {
    let long_ascii = format!("{}.part01.rar", "a".repeat(300));
    let s = sanitize_filename(&long_ascii);
    assert!(s.len() <= MAX_FILENAME_BYTES);
    assert!(s.ends_with(".rar"));

    // Multi-byte characters straddling the cap must not be split.
    let long_multi = format!("{}.mkv", "名".repeat(120)); // 360 bytes + 4
    let s = sanitize_filename(&long_multi);
    assert!(s.len() <= MAX_FILENAME_BYTES);
    assert!(s.ends_with(".mkv"));
    assert!(s.trim_end_matches(".mkv").chars().all(|c| c == '名'));

    let no_ext = "é".repeat(150);
    let s = sanitize_filename(&no_ext);
    assert!(s.len() <= MAX_FILENAME_BYTES && s.len() >= MAX_FILENAME_BYTES - 1);
}

#[test]
fn filenames_from_subjects_are_sanitized() {
    let xml = r#"<nzb><file subject="evil - &quot;../../etc/passwd&quot; yEnc (1/1)" poster="p" date="1">
        <segments><segment number="1" bytes="1">a@b</segment></segments></file>
        <file subject="[1/1] - &quot;CON.rar&quot; yEnc" poster="p" date="1">
        <segments><segment number="1" bytes="1">c@d</segment></segments></file></nzb>"#;
    let nzb = parse(xml.as_bytes()).unwrap();
    assert_eq!(nzb.files[0].filename(), "passwd");
    assert_eq!(nzb.files[1].filename(), "_CON.rar");
}
