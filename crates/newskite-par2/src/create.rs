//! A small PAR2 creator. It keeps every slice in memory, so it is meant for tests and
//! small sets, not for posting.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

use crate::gf::{self, MulTable};
use crate::packet::{Id, MAGIC, T_CREATOR, T_DESC, T_IFSC, T_MAIN, T_RECV};

fn packet(out: &mut Vec<u8>, set_id: &Id, ptype: &[u8; 16], body: &[u8]) {
    let len = 64 + body.len() as u64;
    let mut h = Md5::new();
    h.update(set_id);
    h.update(ptype);
    h.update(body);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&h.finalize());
    out.extend_from_slice(set_id);
    out.extend_from_slice(ptype);
    out.extend_from_slice(body);
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// Writes `<base>.par2` (critical packets) and `<base>.vol00+NN.par2` (critical packets
/// and `recovery` recovery slices) protecting `names` (relative to `dir`). `slice_size`
/// must be a positive multiple of 4. Returns the files written.
pub fn create(dir: &Path, names: &[&str], slice_size: u64, recovery: u32, base: &Path) -> io::Result<Vec<PathBuf>> {
    assert!(slice_size > 0 && slice_size % 4 == 0, "slice size must be a positive multiple of 4");
    struct F {
        id: Id,
        name: String,
        data: Vec<u8>,
    }
    let mut files = Vec::new();
    for &n in names {
        let data = std::fs::read(dir.join(n))?;
        let md5_16k: Id = Md5::digest(&data[..data.len().min(16384)]).into();
        let mut h = Md5::new();
        h.update(md5_16k);
        h.update((data.len() as u64).to_le_bytes());
        h.update(n.as_bytes());
        files.push(F { id: h.finalize().into(), name: n.to_string(), data });
    }
    files.sort_by_key(|f| f.id);

    let mut main = Vec::new();
    main.extend_from_slice(&slice_size.to_le_bytes());
    main.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for f in &files {
        main.extend_from_slice(&f.id);
    }
    let set_id: Id = Md5::digest(&main).into();

    let mut critical = Vec::new();
    packet(&mut critical, &set_id, T_MAIN, &main);
    let mut slices: Vec<Vec<u8>> = Vec::new();
    for f in &files {
        let md5_16k: Id = Md5::digest(&f.data[..f.data.len().min(16384)]).into();
        let mut desc = Vec::new();
        desc.extend_from_slice(&f.id);
        desc.extend_from_slice(&Md5::digest(&f.data));
        desc.extend_from_slice(&md5_16k);
        desc.extend_from_slice(&(f.data.len() as u64).to_le_bytes());
        desc.extend_from_slice(f.name.as_bytes());
        pad4(&mut desc);
        packet(&mut critical, &set_id, T_DESC, &desc);
        let mut ifsc = Vec::new();
        ifsc.extend_from_slice(&f.id);
        for chunk in f.data.chunks(slice_size as usize) {
            let mut s = chunk.to_vec();
            s.resize(slice_size as usize, 0);
            ifsc.extend_from_slice(&Md5::digest(&s));
            ifsc.extend_from_slice(&crc32fast::hash(&s).to_le_bytes());
            slices.push(s);
        }
        packet(&mut critical, &set_id, T_IFSC, &ifsc);
    }
    let mut creator = b"Newskite".to_vec();
    pad4(&mut creator);
    packet(&mut critical, &set_id, T_CREATOR, &creator);

    let main_path = PathBuf::from(format!("{}.par2", base.display()));
    std::fs::File::create(&main_path)?.write_all(&critical)?;
    let mut written = vec![main_path];
    if recovery > 0 {
        let consts = gf::input_constants(slices.len());
        let mut vol = Vec::new();
        for e in 0..recovery {
            let mut r = vec![0u8; slice_size as usize];
            for (i, s) in slices.iter().enumerate() {
                gf::mul_acc(&mut r, s, &MulTable::new(gf::pow(consts[i], e)));
            }
            let mut body = e.to_le_bytes().to_vec();
            body.extend_from_slice(&r);
            packet(&mut vol, &set_id, T_RECV, &body);
        }
        vol.extend_from_slice(&critical);
        let p = PathBuf::from(format!("{}.vol00+{recovery:02}.par2", base.display()));
        std::fs::File::create(&p)?.write_all(&vol)?;
        written.push(p);
    }
    Ok(written)
}
