//! GF(2^16) arithmetic as PAR2 defines it: generator polynomial
//! x^16 + x^12 + x^3 + x + 1 (0x1100B), generator 2, slice data read as little-endian
//! 16-bit words.

use std::cell::OnceCell;
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

#[cfg(test)]
fn mul(a: u16, b: u16) -> u16 {
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
        if !n.is_multiple_of(3) && !n.is_multiple_of(5) && !n.is_multiple_of(17) && !n.is_multiple_of(257) {
            out.push(exp2(n));
        }
    }
    out
}

/// Products of one constant `c` with every nibble of a word: `nib[i][x]` is the low byte
/// of `c * (x << 4i)` and `nib[4 + i][x]` its high byte, for nibble position `i` in `0..4`.
/// Each 16-entry table fits one SIMD shuffle register; the scalar path widens them to
/// byte tables once, on its first long run with this table.
#[repr(align(64))]
pub(crate) struct MulTable {
    nib: [[u8; 16]; 8],
    wide: OnceCell<Box<ByteTables>>,
}

/// Products with every low byte and every high byte of a word.
struct ByteTables {
    lo: [u16; 256],
    hi: [u16; 256],
}

impl MulTable {
    pub(crate) fn new(c: u16) -> Self {
        // Multiplication by `c` is linear over GF(2): the product of a nibble is the XOR
        // of the products of its set bits, so 16 doublings and one XOR per entry suffice.
        let mut nib = [[0u8; 16]; 8];
        let mut p = c as u32;
        for i in 0..4 {
            let mut bit = [0u16; 4];
            for b in &mut bit {
                *b = p as u16;
                p <<= 1;
                if p & 0x10000 != 0 {
                    p ^= POLY;
                }
            }
            let mut v = [0u16; 16];
            for x in 1..16usize {
                v[x] = v[x & (x - 1)] ^ bit[x.trailing_zeros() as usize];
                nib[i][x] = v[x] as u8;
                nib[4 + i][x] = (v[x] >> 8) as u8;
            }
        }
        Self { nib, wide: OnceCell::new() }
    }

    /// The constant `c` this table multiplies by (`c * 1`).
    #[inline]
    fn constant(&self) -> u16 {
        u16::from_le_bytes([self.nib[0][1], self.nib[4][1]])
    }

    /// `c * w` from the nibble tables (eight lookups).
    #[inline]
    fn word(&self, w: u16) -> u16 {
        let w = w as usize;
        let (n0, n1, n2, n3) = (w & 15, (w >> 4) & 15, (w >> 8) & 15, w >> 12);
        let lo = self.nib[0][n0] ^ self.nib[1][n1] ^ self.nib[2][n2] ^ self.nib[3][n3];
        let hi = self.nib[4][n0] ^ self.nib[5][n1] ^ self.nib[6][n2] ^ self.nib[7][n3];
        u16::from_le_bytes([lo, hi])
    }

    /// Products with every low byte and every high byte of a word (two lookups per word),
    /// built on first use.
    fn bytes(&self) -> &ByteTables {
        self.wide.get_or_init(|| {
            let part = |i: usize, x: usize| u16::from_le_bytes([self.nib[i][x], self.nib[4 + i][x]]);
            let mut t = Box::new(ByteTables { lo: [0; 256], hi: [0; 256] });
            for b in 0..256usize {
                t.lo[b] = part(0, b & 15) ^ part(1, b >> 4);
                t.hi[b] = part(2, b & 15) ^ part(3, b >> 4);
            }
            t
        })
    }
}

/// A `mul_acc` implementation the running CPU supports. Only [`Kernel::best`] and
/// [`Kernel::available`] create values, after checking the CPU features, which is what
/// makes calling the SIMD paths sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Kernel(KernelKind);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KernelKind {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Ssse3,
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

impl Kernel {
    /// The fastest kernel for this CPU (detected once).
    pub(crate) fn best() -> Kernel {
        static BEST: OnceLock<Kernel> = OnceLock::new();
        *BEST.get_or_init(|| *Kernel::available().last().expect("scalar is always available"))
    }

    /// Every kernel this CPU can run, slowest (the portable scalar one) first.
    pub(crate) fn available() -> Vec<Kernel> {
        #[allow(unused_mut)]
        let mut out = vec![Kernel(KernelKind::Scalar)];
        #[cfg(target_arch = "x86_64")]
        {
            if std::arch::is_x86_feature_detected!("ssse3") {
                out.push(Kernel(KernelKind::Ssse3));
            }
            if std::arch::is_x86_feature_detected!("avx2") {
                out.push(Kernel(KernelKind::Avx2));
            }
        }
        out
    }

    /// Short name for benchmarks and logs.
    #[allow(dead_code)]
    pub(crate) fn name(self) -> &'static str {
        match self.0 {
            KernelKind::Scalar => "scalar",
            #[cfg(target_arch = "x86_64")]
            KernelKind::Ssse3 => "ssse3",
            #[cfg(target_arch = "x86_64")]
            KernelKind::Avx2 => "avx2",
        }
    }

    /// Bytes per [`mul_acc`](Self::mul_acc) call that make its fixed cost negligible
    /// while staying small enough for the first-level cache.
    pub(crate) fn block(self) -> usize {
        4 << 10
    }

    /// `dst ^= c * src` word by word (`t` is `c`'s table). Lengths must match and be even.
    /// Multiplying by 0 changes nothing and by 1 is a plain XOR, so neither goes through
    /// the tables.
    pub(crate) fn mul_acc(self, dst: &mut [u8], src: &[u8], t: &MulTable) {
        assert_eq!(dst.len(), src.len(), "mul_acc needs equal lengths");
        debug_assert!(dst.len().is_multiple_of(2), "mul_acc works on whole words");
        match t.constant() {
            0 => {}
            1 => match self.0 {
                // SAFETY: as for `mul_acc_tables`.
                #[cfg(target_arch = "x86_64")]
                KernelKind::Avx2 => unsafe { x86::xor_avx2(dst, src) },
                _ => xor_into(dst, src),
            },
            _ => self.mul_acc_tables(dst, src, t),
        }
    }

    /// [`mul_acc`](Self::mul_acc) through the tables whatever the constant.
    fn mul_acc_tables(self, dst: &mut [u8], src: &[u8], t: &MulTable) {
        match self.0 {
            KernelKind::Scalar => mul_acc_scalar(dst, src, t),
            // SAFETY: a `Kernel` with this kind exists only when `available` saw the CPU
            // report SSSE3 (and AVX2 below).
            #[cfg(target_arch = "x86_64")]
            KernelKind::Ssse3 => unsafe { x86::mul_acc_ssse3(dst, src, t) },
            #[cfg(target_arch = "x86_64")]
            KernelKind::Avx2 => unsafe { x86::mul_acc_avx2(dst, src, t) },
        }
    }
}

/// `dst ^= c * src` word by word (`t` is `c`'s table) with the fastest kernel. Lengths
/// must match and be even.
pub(crate) fn mul_acc(dst: &mut [u8], src: &[u8], t: &MulTable) {
    Kernel::best().mul_acc(dst, src, t);
}

/// `dst ^= src`.
#[inline(always)]
fn xor_into(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= s;
    }
}

/// The portable path. Long runs use the byte tables (two lookups per word, four words per
/// step); short ones (SIMD tails) use the nibble tables.
fn mul_acc_scalar(dst: &mut [u8], src: &[u8], t: &MulTable) {
    if dst.len() < 1024 && t.wide.get().is_none() {
        for (d, s) in dst.chunks_exact_mut(2).zip(src.chunks_exact(2)) {
            let p = t.word(u16::from_le_bytes([s[0], s[1]]));
            d[0] ^= p as u8;
            d[1] ^= (p >> 8) as u8;
        }
        return;
    }
    let ByteTables { lo, hi } = t.bytes();
    let mut d8 = dst.chunks_exact_mut(8);
    let mut s8 = src.chunks_exact(8);
    for (d, s) in (&mut d8).zip(&mut s8) {
        let w = u64::from_le_bytes(s.try_into().expect("8 bytes"));
        let word = |k: u32| {
            let p = lo[(w >> (16 * k)) as u8 as usize] ^ hi[(w >> (16 * k + 8)) as u8 as usize];
            (p as u64) << (16 * k)
        };
        let p = word(0) | word(1) | word(2) | word(3);
        let v = u64::from_le_bytes((&*d).try_into().expect("8 bytes")) ^ p;
        d.copy_from_slice(&v.to_le_bytes());
    }
    for (d, s) in d8.into_remainder().chunks_exact_mut(2).zip(s8.remainder().chunks_exact(2)) {
        let p = lo[s[0] as usize] ^ hi[s[1] as usize];
        d[0] ^= p as u8;
        d[1] ^= (p >> 8) as u8;
    }
}

/// x86-64 SIMD kernels: the split-nibble technique. The low and high bytes of 16 (SSSE3)
/// or 32 (AVX2) words are gathered into one register each, each nibble indexes a 16-entry
/// table of partial products with one `pshufb`, the partial products are XORed, and the
/// low and high result bytes are interleaved back into words.
#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    use super::{mul_acc_scalar, xor_into, MulTable};

    /// `dst ^= src` with 32-byte vectors.
    ///
    /// Only call this when the CPU supports AVX2 (`Kernel` guarantees it).
    #[target_feature(enable = "avx2")]
    pub(super) fn xor_avx2(dst: &mut [u8], src: &[u8]) {
        xor_into(dst, src);
    }

    /// AVX2: 64 bytes (32 words) per step; the rest goes through the scalar path.
    ///
    /// Only call this when the CPU supports AVX2 (`Kernel` guarantees it).
    #[target_feature(enable = "avx2")]
    pub(super) fn mul_acc_avx2(dst: &mut [u8], src: &[u8], t: &MulTable) {
        // Each 16-entry table repeated in both 128-bit lanes (pshufb looks up per lane).
        let tab = |i: usize| {
            // SAFETY: `t.nib[i]` is 16 readable bytes; unaligned loads are allowed.
            let v = unsafe { _mm_loadu_si128(t.nib[i].as_ptr().cast()) };
            _mm256_broadcastsi128_si256(v)
        };
        let (l0, l1, l2, l3) = (tab(0), tab(1), tab(2), tab(3));
        let (h0, h1, h2, h3) = (tab(4), tab(5), tab(6), tab(7));
        let nibble = _mm256_set1_epi8(0x0f);
        let low_byte = _mm256_set1_epi16(0x00ff);
        let len = dst.len().min(src.len());
        let whole = len - len % 64;
        let mut i = 0;
        while i < whole {
            // SAFETY: `i + 64 <= whole <= len`, so both 32-byte loads from `src` and the
            // loads/stores at `dst` stay inside the slices; unaligned access is allowed.
            let (a, b, da, db) = unsafe {
                (
                    _mm256_loadu_si256(src.as_ptr().add(i).cast()),
                    _mm256_loadu_si256(src.as_ptr().add(i + 32).cast()),
                    _mm256_loadu_si256(dst.as_ptr().add(i).cast()),
                    _mm256_loadu_si256(dst.as_ptr().add(i + 32).cast()),
                )
            };
            // Low and high bytes of the 32 words. packus works per 128-bit lane, so the
            // word order is a0 b0 a1 b1 (lane halves); the unpacks below undo exactly that.
            let lo = _mm256_packus_epi16(_mm256_and_si256(a, low_byte), _mm256_and_si256(b, low_byte));
            let hi = _mm256_packus_epi16(_mm256_srli_epi16::<8>(a), _mm256_srli_epi16::<8>(b));
            let n0 = _mm256_and_si256(lo, nibble);
            let n1 = _mm256_and_si256(_mm256_srli_epi16::<4>(lo), nibble);
            let n2 = _mm256_and_si256(hi, nibble);
            let n3 = _mm256_and_si256(_mm256_srli_epi16::<4>(hi), nibble);
            let plo = _mm256_xor_si256(
                _mm256_xor_si256(_mm256_shuffle_epi8(l0, n0), _mm256_shuffle_epi8(l1, n1)),
                _mm256_xor_si256(_mm256_shuffle_epi8(l2, n2), _mm256_shuffle_epi8(l3, n3)),
            );
            let phi = _mm256_xor_si256(
                _mm256_xor_si256(_mm256_shuffle_epi8(h0, n0), _mm256_shuffle_epi8(h1, n1)),
                _mm256_xor_si256(_mm256_shuffle_epi8(h2, n2), _mm256_shuffle_epi8(h3, n3)),
            );
            // Interleaving low/high bytes per lane yields a's 16 words, then b's.
            let pa = _mm256_unpacklo_epi8(plo, phi);
            let pb = _mm256_unpackhi_epi8(plo, phi);
            // SAFETY: as for the loads above.
            unsafe {
                _mm256_storeu_si256(dst.as_mut_ptr().add(i).cast(), _mm256_xor_si256(da, pa));
                _mm256_storeu_si256(dst.as_mut_ptr().add(i + 32).cast(), _mm256_xor_si256(db, pb));
            }
            i += 64;
        }
        mul_acc_scalar(&mut dst[whole..len], &src[whole..len], t);
    }

    /// SSSE3: 32 bytes (16 words) per step; the rest goes through the scalar path.
    ///
    /// Only call this when the CPU supports SSSE3 (`Kernel` guarantees it).
    #[target_feature(enable = "ssse3")]
    pub(super) fn mul_acc_ssse3(dst: &mut [u8], src: &[u8], t: &MulTable) {
        // SAFETY: `t.nib[i]` is 16 readable bytes; unaligned loads are allowed.
        let tab = |i: usize| unsafe { _mm_loadu_si128(t.nib[i].as_ptr().cast()) };
        let (l0, l1, l2, l3) = (tab(0), tab(1), tab(2), tab(3));
        let (h0, h1, h2, h3) = (tab(4), tab(5), tab(6), tab(7));
        let nibble = _mm_set1_epi8(0x0f);
        let low_byte = _mm_set1_epi16(0x00ff);
        let len = dst.len().min(src.len());
        let whole = len - len % 32;
        let mut i = 0;
        while i < whole {
            // SAFETY: `i + 32 <= whole <= len`, so both 16-byte loads from `src` and the
            // loads/stores at `dst` stay inside the slices; unaligned access is allowed.
            let (a, b, da, db) = unsafe {
                (
                    _mm_loadu_si128(src.as_ptr().add(i).cast()),
                    _mm_loadu_si128(src.as_ptr().add(i + 16).cast()),
                    _mm_loadu_si128(dst.as_ptr().add(i).cast()),
                    _mm_loadu_si128(dst.as_ptr().add(i + 16).cast()),
                )
            };
            // Low bytes of a then b, and high bytes of a then b.
            let lo = _mm_packus_epi16(_mm_and_si128(a, low_byte), _mm_and_si128(b, low_byte));
            let hi = _mm_packus_epi16(_mm_srli_epi16::<8>(a), _mm_srli_epi16::<8>(b));
            let n0 = _mm_and_si128(lo, nibble);
            let n1 = _mm_and_si128(_mm_srli_epi16::<4>(lo), nibble);
            let n2 = _mm_and_si128(hi, nibble);
            let n3 = _mm_and_si128(_mm_srli_epi16::<4>(hi), nibble);
            let plo = _mm_xor_si128(
                _mm_xor_si128(_mm_shuffle_epi8(l0, n0), _mm_shuffle_epi8(l1, n1)),
                _mm_xor_si128(_mm_shuffle_epi8(l2, n2), _mm_shuffle_epi8(l3, n3)),
            );
            let phi = _mm_xor_si128(
                _mm_xor_si128(_mm_shuffle_epi8(h0, n0), _mm_shuffle_epi8(h1, n1)),
                _mm_xor_si128(_mm_shuffle_epi8(h2, n2), _mm_shuffle_epi8(h3, n3)),
            );
            let pa = _mm_unpacklo_epi8(plo, phi);
            let pb = _mm_unpackhi_epi8(plo, phi);
            // SAFETY: as for the loads above.
            unsafe {
                _mm_storeu_si128(dst.as_mut_ptr().add(i).cast(), _mm_xor_si128(da, pa));
                _mm_storeu_si128(dst.as_mut_ptr().add(i + 16).cast(), _mm_xor_si128(db, pb));
            }
            i += 32;
        }
        mul_acc_scalar(&mut dst[whole..len], &src[whole..len], t);
    }
}

/// Inverts the `n`x`n` row-major matrix `m` by Gauss-Jordan elimination. `None` when it
/// is singular.
pub(crate) fn invert(m: &[u16], n: usize) -> Option<Vec<u16>> {
    invert_with(Kernel::best(), m, n)
}

/// [`invert`] with a given kernel. The rows of `[m | I]` are kept as little-endian words,
/// so every row operation is one [`Kernel::mul_acc`]. The pivot row of column `col` is
/// zero left of it (earlier columns were cleared in every row but their own), so the row
/// operations for `col` start there.
pub(crate) fn invert_with(kernel: Kernel, m: &[u16], n: usize) -> Option<Vec<u16>> {
    let row_bytes = 4 * n;
    let mut rows = vec![0u8; n * row_bytes];
    for (i, row) in rows.chunks_exact_mut(row_bytes.max(1)).enumerate() {
        for (j, w) in row[..2 * n].chunks_exact_mut(2).enumerate() {
            w.copy_from_slice(&m[i * n + j].to_le_bytes());
        }
        row[2 * (n + i)] = 1;
    }
    let at = |rows: &[u8], r: usize, c: usize| u16::from_le_bytes([rows[r * row_bytes + 2 * c], rows[r * row_bytes + 2 * c + 1]]);
    let mut pivot_row = vec![0u8; row_bytes];
    for col in 0..n {
        let pivot = (col..n).find(|&r| at(&rows, r, col) != 0)?;
        if pivot != col {
            let (head, tail) = rows.split_at_mut(pivot * row_bytes);
            head[col * row_bytes..(col + 1) * row_bytes].swap_with_slice(&mut tail[..row_bytes]);
        }
        let from = 2 * col;
        let p = &mut pivot_row[from..];
        p.fill(0);
        kernel.mul_acc(p, &rows[col * row_bytes + from..(col + 1) * row_bytes], &MulTable::new(inv(at(&rows, col, col))));
        rows[col * row_bytes + from..(col + 1) * row_bytes].copy_from_slice(p);
        for (r, row) in rows.chunks_exact_mut(row_bytes).enumerate() {
            let g = u16::from_le_bytes([row[from], row[from + 1]]);
            if r != col && g != 0 {
                kernel.mul_acc(&mut row[from..], p, &MulTable::new(g));
            }
        }
    }
    Some(rows.chunks_exact(row_bytes.max(1)).flat_map(|row| row[2 * n..].chunks_exact(2).map(|w| u16::from_le_bytes([w[0], w[1]]))).collect())
}

/// The plain Gauss-Jordan elimination [`invert_with`] must agree with.
#[cfg(test)]
fn invert_reference(m: &[u16], n: usize) -> Option<Vec<u16>> {
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

    /// xorshift64, so the tests need no extra dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    /// `dst ^ c * src` computed one word at a time with `mul`.
    fn reference(dst: &[u8], src: &[u8], c: u16) -> Vec<u8> {
        dst.chunks_exact(2)
            .zip(src.chunks_exact(2))
            .flat_map(|(d, s)| {
                let p = mul(c, u16::from_le_bytes([s[0], s[1]]));
                (u16::from_le_bytes([d[0], d[1]]) ^ p).to_le_bytes()
            })
            .collect()
    }

    #[test]
    fn tables_match_mul() {
        for c in [0u16, 1, 2, 0x1d3f, 0x8000, 0xffff] {
            let t = MulTable::new(c);
            for i in 0..4 {
                for x in 0..16u16 {
                    let p = mul(c, x << (4 * i));
                    assert_eq!(u16::from_le_bytes([t.nib[i][x as usize], t.nib[4 + i][x as usize]]), p);
                }
            }
            assert_eq!(t.constant(), c);
            let ByteTables { lo, hi } = t.bytes();
            for b in 0..256u16 {
                assert_eq!(lo[b as usize], mul(c, b));
                assert_eq!(hi[b as usize], mul(c, b << 8));
            }
        }
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

    /// Every kernel the CPU supports gives the reference result for random constants,
    /// contents, lengths (odd word counts and tails shorter than one SIMD step included)
    /// and misaligned starts.
    #[test]
    fn every_kernel_matches_reference() {
        let kernels = Kernel::available();
        println!("kernels: {:?}", kernels.iter().map(|k| k.name()).collect::<Vec<_>>());
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut lens: Vec<usize> = (0..=200).step_by(2).collect();
        lens.extend([254, 256, 258, 1022, 1024, 1026, 4094, 65536 + 62]);
        for &len in &lens {
            for _ in 0..8 {
                let c = match rng.next() % 8 {
                    0 => 0,
                    1 => 1,
                    2 => 0xffff,
                    _ => rng.next() as u16,
                };
                let off_s = (rng.next() % 32) as usize;
                let off_d = (rng.next() % 32) as usize;
                let src_buf = rng.bytes(len + off_s);
                let dst_buf = rng.bytes(len + off_d);
                let src = &src_buf[off_s..];
                let want = reference(&dst_buf[off_d..], src, c);
                for k in &kernels {
                    // The dispatching entry point, the table path whatever the constant
                    // (also for 0 and 1), and the table path with the byte tables built.
                    for path in ["mul_acc", "tables", "wide tables"] {
                        let t = MulTable::new(c);
                        let mut d = dst_buf.clone();
                        match path {
                            "mul_acc" => k.mul_acc(&mut d[off_d..], src, &t),
                            "tables" => k.mul_acc_tables(&mut d[off_d..], src, &t),
                            _ => {
                                t.bytes();
                                k.mul_acc_tables(&mut d[off_d..], src, &t);
                            }
                        }
                        assert_eq!(&d[off_d..], &want[..], "{} {path} c={c:#x} len={len}", k.name());
                        assert_eq!(&d[..off_d], &dst_buf[..off_d], "{} {path} wrote before the slice", k.name());
                    }
                }
            }
        }
    }

    /// All 65536 words through every kernel for a spread of constants, and every
    /// constant for a fixed random block.
    #[test]
    fn every_kernel_is_exhaustively_exact() {
        let kernels = Kernel::available();
        let words: Vec<u8> = (0..=u16::MAX).flat_map(u16::to_le_bytes).collect();
        let zero = vec![0u8; words.len()];
        let mut rng = Rng(42);
        let consts: Vec<u16> = [0u16, 1, 2, 3, 0x100b, 0x8000, 0xffff].into_iter().chain((0..25).map(|_| rng.next() as u16)).collect();
        for &c in &consts {
            let want = reference(&zero, &words, c);
            let t = MulTable::new(c);
            for k in &kernels {
                let mut d = zero.clone();
                k.mul_acc(&mut d, &words, &t);
                assert!(d == want, "{} c={c:#x}", k.name());
                let mut d = zero.clone();
                k.mul_acc_tables(&mut d, &words, &t);
                assert!(d == want, "{} tables c={c:#x}", k.name());
            }
        }
        let src = rng.bytes(192);
        let dst = rng.bytes(192);
        for c in 0..=u16::MAX {
            let want = reference(&dst, &src, c);
            let t = MulTable::new(c);
            for k in &kernels {
                let mut d = dst.clone();
                k.mul_acc(&mut d, &src, &t);
                assert!(d == want, "{} c={c:#x}", k.name());
            }
        }
    }

    /// Kernel throughput (not run by default):
    /// `cargo test --release -p newskite-par2 gf::tests::kernel_throughput -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn kernel_throughput() {
        use std::time::Instant;
        let mut rng = Rng(7);
        let tables: Vec<MulTable> = (0..64u16).map(|c| MulTable::new(0x1234 ^ c)).collect();
        for size in [2 << 10, 4 << 10, 32 << 10, 768 << 10, 16 << 20] {
            let src = rng.bytes(size);
            let mut dst = rng.bytes(size);
            let reps = ((2usize << 30) / size).max(1);
            for k in Kernel::available() {
                let mut best = f64::MAX;
                for _ in 0..3 {
                    let t0 = Instant::now();
                    for r in 0..reps {
                        k.mul_acc(&mut dst, &src, &tables[r % 64]);
                    }
                    best = best.min(t0.elapsed().as_secs_f64());
                }
                println!("{:>6} {:>8} KiB: {:6.2} GB/s", k.name(), size >> 10, (size * reps) as f64 / best / 1e9);
            }
            let one = MulTable::new(1);
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                for _ in 0..reps {
                    Kernel::best().mul_acc(&mut dst, &src, &one);
                }
                best = best.min(t0.elapsed().as_secs_f64());
            }
            println!("{:>6} {:>8} KiB: {:6.2} GB/s", "c=1", size >> 10, (size * reps) as f64 / best / 1e9);
        }
        let t0 = Instant::now();
        let mut x = 0u16;
        for c in 0..100_000u32 {
            x ^= u16::from(std::hint::black_box(MulTable::new(c as u16)).nib[1][3]);
        }
        println!("MulTable::new: {:.0} ns ({x})", t0.elapsed().as_secs_f64() / 1e5 * 1e9);
        for n in [100, 300, 600] {
            let m = par2_matrix(n, 0);
            let t0 = Instant::now();
            let a = invert_reference(&m, n);
            let old = t0.elapsed().as_secs_f64();
            let t0 = Instant::now();
            let b = invert(&m, n);
            let new = t0.elapsed().as_secs_f64();
            assert_eq!(a, b);
            println!("invert {n}x{n}: reference {:.1} ms, row operations {:.1} ms", old * 1e3, new * 1e3);
        }
    }

    /// The matrix a repair with `n` missing slices inverts: rows are recovery exponents
    /// `first..first + n`, columns the constants of `n` input slices spread over a set.
    fn par2_matrix(n: usize, first: u32) -> Vec<u16> {
        let c = input_constants(3 * n);
        (0..n).flat_map(|e| (0..n).map(move |i| (e, i))).map(|(e, i)| pow(c[3 * i + 1], first + e as u32)).collect()
    }

    /// The row-operation inverse equals the plain elimination with every kernel, for
    /// PAR2 matrices and random ones (singular ones included), and inverts.
    #[test]
    fn invert_matches_reference() {
        let mut rng = Rng(0x5eed);
        let mut cases: Vec<(usize, Vec<u16>)> = Vec::new();
        for n in [1, 2, 3, 5, 8, 17, 40] {
            cases.push((n, par2_matrix(n, 0)));
            cases.push((n, par2_matrix(n, 7)));
            cases.push((n, (0..n * n).map(|_| rng.next() as u16).collect()));
            // Sparse: zero pivots force row swaps, and some of these are singular.
            cases.push((n, (0..n * n).map(|_| if rng.next().is_multiple_of(3) { rng.next() as u16 } else { 0 }).collect()));
            // Duplicate rows: always singular from n = 2.
            let mut dup: Vec<u16> = (0..n * n).map(|_| rng.next() as u16).collect();
            if n > 1 {
                let (a, b) = dup.split_at_mut(n);
                b[..n].copy_from_slice(a);
            }
            cases.push((n, dup));
        }
        let (mut singular, mut regular) = (0, 0);
        for (n, m) in &cases {
            let want = invert_reference(m, *n);
            match &want {
                Some(r) => {
                    regular += 1;
                    for i in 0..*n {
                        for j in 0..*n {
                            let s = (0..*n).fold(0u16, |s, k| s ^ mul(m[i * n + k], r[k * n + j]));
                            assert_eq!(s, u16::from(i == j));
                        }
                    }
                }
                None => singular += 1,
            }
            for k in Kernel::available() {
                assert_eq!(invert_with(k, m, *n), want, "{} n={n}", k.name());
            }
        }
        assert!(singular >= 6 && regular >= 20, "{singular} singular, {regular} regular");
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
