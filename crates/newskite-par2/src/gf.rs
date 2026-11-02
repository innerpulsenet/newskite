//! GF(2^16) arithmetic as PAR2 defines it: generator polynomial
//! x^16 + x^12 + x^3 + x + 1 (0x1100B), generator 2, slice data read as little-endian
//! 16-bit words.

use std::sync::OnceLock;

const POLY: u32 = 0x1100B;
/// Order of the multiplicative group.
pub(crate) const LIMIT: u32 = 65535;

struct Tables {
    log: Vec<u16>,
    exp: Vec<u16>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut log = vec![0u16; 65536];
        let mut exp = vec![0u16; 2 * LIMIT as usize];
        let mut x: u32 = 1;
        for i in 0..LIMIT {
            exp[i as usize] = x as u16;
            log[x as usize] = i as u16;
            x <<= 1;
            if x & 0x10000 != 0 {
                x ^= POLY;
            }
        }
        for i in LIMIT..2 * LIMIT {
            exp[i as usize] = exp[(i - LIMIT) as usize];
        }
        Tables { log, exp }
    })
}

pub(crate) fn mul(a: u16, b: u16) -> u16 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

pub(crate) fn inv(a: u16) -> u16 {
    assert!(a != 0, "zero has no inverse");
    let t = tables();
    t.exp[(LIMIT - t.log[a as usize] as u32) as usize % LIMIT as usize]
}

/// `2^n`.
pub(crate) fn exp2(n: u32) -> u16 {
    tables().exp[(n % LIMIT) as usize]
}

/// `a^e`.
pub(crate) fn pow(a: u16, e: u32) -> u16 {
    if e == 0 {
        return 1;
    }
    if a == 0 {
        return 0;
    }
    let t = tables();
    let l = (t.log[a as usize] as u64 * e as u64) % LIMIT as u64;
    t.exp[l as usize]
}

/// The constants PAR2 assigns to input slices: `2^n` for the n-th positive integer that
/// shares no factor with 65535 (= 3 * 5 * 17 * 257).
pub(crate) fn input_constants(count: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(count);
    let mut n: u32 = 0;
    while out.len() < count {
        n += 1;
        if n % 3 != 0 && n % 5 != 0 && n % 17 != 0 && n % 257 != 0 {
            out.push(exp2(n));
        }
    }
    out
}

/// Products of one constant with every low and high byte, so a word multiplies with two
/// lookups.
pub(crate) struct MulTable {
    lo: [u16; 256],
    hi: [u16; 256],
}

impl MulTable {
    pub(crate) fn new(c: u16) -> Self {
        let mut lo = [0u16; 256];
        let mut hi = [0u16; 256];
        for b in 0..256u16 {
            lo[b as usize] = mul(c, b);
            hi[b as usize] = mul(c, b << 8);
        }
        Self { lo, hi }
    }
}

/// `dst ^= c * src` word by word (`t` is `c`'s table). Lengths must match and be even.
pub(crate) fn mul_acc(dst: &mut [u8], src: &[u8], t: &MulTable) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, s) in dst.chunks_exact_mut(2).zip(src.chunks_exact(2)) {
        let p = t.lo[s[0] as usize] ^ t.hi[s[1] as usize];
        d[0] ^= p as u8;
        d[1] ^= (p >> 8) as u8;
    }
}

/// Inverts the `n`x`n` row-major matrix `m` by Gauss-Jordan elimination. `None` when it
/// is singular.
pub(crate) fn invert(m: &[u16], n: usize) -> Option<Vec<u16>> {
    let mut a = m.to_vec();
    let mut r = vec![0u16; n * n];
    for i in 0..n {
        r[i * n + i] = 1;
    }
    for col in 0..n {
        let pivot = (col..n).find(|&row| a[row * n + col] != 0)?;
        if pivot != col {
            for k in 0..n {
                a.swap(pivot * n + k, col * n + k);
                r.swap(pivot * n + k, col * n + k);
            }
        }
        let f = inv(a[col * n + col]);
        for k in 0..n {
            a[col * n + k] = mul(a[col * n + k], f);
            r[col * n + k] = mul(r[col * n + k], f);
        }
        for row in 0..n {
            if row == col {
                continue;
            }
            let g = a[row * n + col];
            if g == 0 {
                continue;
            }
            for k in 0..n {
                a[row * n + k] ^= mul(g, a[col * n + k]);
                r[row * n + k] ^= mul(g, r[col * n + k]);
            }
        }
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_laws() {
        for a in [1u16, 2, 3, 0x1234, 0xffff, 0x8000] {
            assert_eq!(mul(a, inv(a)), 1, "{a:#x}");
            assert_eq!(mul(a, 1), a);
            assert_eq!(pow(a, 3), mul(a, mul(a, a)));
        }
        // 2^16 reduces by the polynomial.
        assert_eq!(mul(0x8000, 2), (0x1100Bu32 ^ 0x10000) as u16);
    }

    #[test]
    fn constants_skip_shared_factors() {
        let c = input_constants(6);
        let want: Vec<u16> = [1, 2, 4, 7, 8, 11].iter().map(|&n| exp2(n)).collect();
        assert_eq!(c, want);
    }

    #[test]
    fn mul_acc_matches_scalar() {
        let src: Vec<u8> = (0..64u8).map(|b| b.wrapping_mul(37)).collect();
        let mut dst = vec![0u8; 64];
        mul_acc(&mut dst, &src, &MulTable::new(0x1d3f));
        for i in (0..64).step_by(2) {
            let w = u16::from_le_bytes([src[i], src[i + 1]]);
            assert_eq!(u16::from_le_bytes([dst[i], dst[i + 1]]), mul(0x1d3f, w));
        }
    }

    #[test]
    fn inverts() {
        let c = input_constants(4);
        let n = 4;
        let m: Vec<u16> = (0..n).flat_map(|e| c.iter().map(move |&x| pow(x, e as u32))).collect();
        let r = invert(&m, n).unwrap();
        for i in 0..n {
            for j in 0..n {
                let mut s = 0u16;
                for k in 0..n {
                    s ^= mul(m[i * n + k], r[k * n + j]);
                }
                assert_eq!(s, u16::from(i == j));
            }
        }
        assert!(invert(&[1, 1, 1, 1], 2).is_none());
    }
}
