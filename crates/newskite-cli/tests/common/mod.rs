//! Shared helpers for the headless CLI tests: yEnc test articles, NZB files and an
//! authenticating mock NNTP server.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use newskite_nntp::testkit::{MockConfig, MockServer};
use newskite_nzb::yenc;
use sha2::{Digest, Sha256};

pub const USER: &str = "cli-user";
pub const PASS: &str = "cli-s3cret-pass";

/// Deterministic xorshift64* bytes.
pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.max(1);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

/// One source file split into yEnc articles.
pub struct TestFile {
    pub name: String,
    pub data: Vec<u8>,
    /// (message id, article body) in part order.
    pub articles: Vec<(String, Vec<u8>)>,
}

impl TestFile {
    pub fn new(tag: &str, name: &str, data: Vec<u8>, parts: usize) -> Self {
        let size = data.len() as u64;
        let chunk = data.len().div_ceil(parts.max(1)).max(1);
        let total = data.chunks(chunk).count() as u32;
        let articles = data
            .chunks(chunk)
            .enumerate()
            .map(|(i, c)| {
                let begin = (i * chunk) as u64 + 1;
                let body = yenc::encode_article(name, size, i as u32 + 1, total, begin, c, 128);
                (format!("{tag}.part{}@test.newskite", i + 1), body)
            })
            .collect();
        Self {
            name: name.to_string(),
            data,
            articles,
        }
    }

    /// `n` random bytes split into `parts` articles.
    pub fn random(tag: &str, name: &str, len: usize, parts: usize, seed: u64) -> Self {
        Self::new(tag, name, random_bytes(len, seed), parts)
    }

    pub fn ids(&self) -> Vec<String> {
        self.articles.iter().map(|(id, _)| id.clone()).collect()
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// NZB XML for `files`, with an optional `name` meta entry.
pub fn nzb_xml(job_name: Option<&str>, files: &[&TestFile]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    if let Some(n) = job_name {
        s.push_str(&format!(
            "<head><meta type=\"name\">{}</meta></head>\n",
            xml_escape(n)
        ));
    }
    for f in files {
        let subject = format!("test [1/1] - \"{}\" yEnc (1/{})", f.name, f.articles.len());
        s.push_str(&format!(
            "<file poster=\"poster@test\" date=\"0\" subject=\"{}\">\n<groups><group>alt.binaries.test</group></groups>\n<segments>\n",
            xml_escape(&subject)
        ));
        for (i, (id, body)) in f.articles.iter().enumerate() {
            s.push_str(&format!(
                "<segment bytes=\"{}\" number=\"{}\">{}</segment>\n",
                body.len(),
                i + 1,
                xml_escape(id)
            ));
        }
        s.push_str("</segments>\n</file>\n");
    }
    s.push_str("</nzb>\n");
    s
}

/// Writes an NZB for `files` to `dir/file_name` and returns its path.
pub fn write_nzb(
    dir: &Path,
    file_name: &str,
    job_name: Option<&str>,
    files: &[&TestFile],
) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join(file_name);
    std::fs::write(&p, nzb_xml(job_name, files)).unwrap();
    p
}

/// Mock server holding every article of `files` (except ids in `omit`), requiring auth.
pub async fn mock_for(files: &[&TestFile], omit: &[&str], delay_ms: u64) -> MockServer {
    let mut articles = HashMap::new();
    for f in files {
        for (id, body) in &f.articles {
            if !omit.contains(&id.as_str()) {
                articles.insert(id.clone(), body.clone());
            }
        }
    }
    MockServer::start(MockConfig {
        username: Some(USER.into()),
        password: Some(PASS.into()),
        articles,
        delay_ms,
        ..Default::default()
    })
    .await
}
