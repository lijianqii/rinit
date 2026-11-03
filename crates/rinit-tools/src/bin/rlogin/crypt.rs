//! SHA-512 and SHA-512-crypt for `rlogin`.
//!
//! The target images do not ship libcrypt, so password verification cannot
//! depend on crypt(3). This module implements SHA-512 (FIPS 180-4) and the
//! `$6$` crypt construction. The crypt algorithm follows Ulrich Drepper's
//! public-domain reference construction; it is used only to compare a supplied
//! password with the hash in `/etc/shadow`.

const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

const H0: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const B64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

struct Sha512 {
    state: [u64; 8],
    buffer: [u8; 128],
    buffer_len: usize,
    length: u128,
}

impl Sha512 {
    fn new() -> Self {
        Self {
            state: H0,
            buffer: [0; 128],
            buffer_len: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u128;
        if self.buffer_len > 0 {
            let space = 128 - self.buffer_len;
            let take = data.len().min(space);
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 128 {
                let block = self.buffer;
                compress(&mut self.state, &block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 128 {
            let Ok(block): Result<&[u8; 128], _> = data[..128].try_into() else {
                break;
            };
            compress(&mut self.state, block);
            data = &data[128..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 64] {
        let bit_len = self.length * 8;
        let mut padding = vec![0x80_u8];
        let remainder = (self.buffer_len + 1) % 128;
        let zeros = if remainder <= 112 {
            112 - remainder
        } else {
            240 - remainder
        };
        padding.extend(std::iter::repeat_n(0_u8, zeros));
        padding.extend_from_slice(&bit_len.to_be_bytes());
        self.update(&padding);
        let mut out = [0_u8; 64];
        for (index, word) in self.state.iter().enumerate() {
            out[index * 8..index * 8 + 8].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(state: &mut [u64; 8], block: &[u8; 128]) {
    let mut schedule = [0_u64; 80];
    for (index, slot) in schedule.iter_mut().take(16).enumerate() {
        *slot = u64::from_be_bytes(block[index * 8..index * 8 + 8].try_into().unwrap_or([0; 8]));
    }
    for index in 16..80 {
        let s0 = schedule[index - 15].rotate_right(1)
            ^ schedule[index - 15].rotate_right(8)
            ^ (schedule[index - 15] >> 7);
        let s1 = schedule[index - 2].rotate_right(19)
            ^ schedule[index - 2].rotate_right(61)
            ^ (schedule[index - 2] >> 6);
        schedule[index] = schedule[index - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..80 {
        let sigma1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
        let choose = (e & f) ^ (!e & g);
        let temp1 = h
            .wrapping_add(sigma1)
            .wrapping_add(choose)
            .wrapping_add(K[index])
            .wrapping_add(schedule[index]);
        let sigma0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = sigma0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    *state = [
        state[0].wrapping_add(a),
        state[1].wrapping_add(b),
        state[2].wrapping_add(c),
        state[3].wrapping_add(d),
        state[4].wrapping_add(e),
        state[5].wrapping_add(f),
        state[6].wrapping_add(g),
        state[7].wrapping_add(h),
    ];
}

#[cfg(test)]
fn digest(data: &[u8]) -> [u8; 64] {
    let mut context = Sha512::new();
    context.update(data);
    context.finalize()
}

fn repeat_to_len(bytes: &[u8; 64], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    while out.len() < length {
        let take = (length - out.len()).min(64);
        out.extend_from_slice(&bytes[..take]);
    }
    out
}

fn b64_from_24bit(out: &mut String, b2: u8, b1: u8, b0: u8, count: usize) {
    let mut value = ((b2 as u32) << 16) | ((b1 as u32) << 8) | b0 as u32;
    for _ in 0..count {
        out.push(B64[(value & 0x3f) as usize] as char);
        value >>= 6;
    }
}

fn encode_sha512(hash: &[u8; 64]) -> String {
    let mut out = String::new();
    let mut index = 0;
    loop {
        let mut j = index + 21;
        let mut k = index + 42;
        if j >= 63 {
            j -= 63;
        }
        if k >= 63 {
            k -= 63;
        }
        b64_from_24bit(&mut out, hash[index], hash[j], hash[k], 4);
        if j == 20 {
            break;
        }
        index = j + 1;
    }
    b64_from_24bit(&mut out, 0, 0, hash[63], 2);
    out
}

/// Compute the `$6$` crypt representation of `password` for `setting`.
///
/// `setting` is the stored hash prefix, including `$6$` and optionally
/// `rounds=N$`; the salt is taken from it. Returns `None` for a non-SHA-512
/// setting.
pub fn sha512_crypt(password: &str, setting: &str) -> Option<String> {
    let mut rest = setting.strip_prefix("$6$")?;
    let mut rounds = 5000_u32;
    let mut explicit_rounds = false;
    if let Some(value) = rest.strip_prefix("rounds=") {
        let end = value.find('$')?;
        rounds = value.get(..end)?.parse::<u32>().ok()?;
        rounds = rounds.clamp(1000, 999_999_999);
        rest = value.get(end + 1..)?;
        explicit_rounds = true;
    }
    let salt_end = rest.find('$').unwrap_or(rest.len());
    let salt = rest.get(..salt_end)?.as_bytes();
    let salt = salt.get(..salt.len().min(16))?;
    let key = password.as_bytes();

    let mut context = Sha512::new();
    context.update(key);
    context.update(salt);

    let mut alternate = Sha512::new();
    alternate.update(key);
    alternate.update(salt);
    alternate.update(key);
    let mut alt = alternate.finalize();

    let mut count = key.len();
    while count > 64 {
        context.update(&alt);
        count -= 64;
    }
    context.update(&alt[..count]);
    let mut bits = key.len();
    while bits != 0 {
        context.update(if bits & 1 != 0 { &alt } else { key });
        bits >>= 1;
    }
    alt = context.finalize();

    let mut p_context = Sha512::new();
    for _ in 0..key.len() {
        p_context.update(key);
    }
    let p_bytes = repeat_to_len(&p_context.finalize(), key.len());

    let mut s_context = Sha512::new();
    for _ in 0..(16 + alt[0] as usize) {
        s_context.update(salt);
    }
    let s_bytes = repeat_to_len(&s_context.finalize(), salt.len());

    for round in 0..rounds {
        let mut context = Sha512::new();
        context.update(if round & 1 != 0 { &p_bytes } else { &alt });
        if round % 3 != 0 {
            context.update(&s_bytes);
        }
        if round % 7 != 0 {
            context.update(&p_bytes);
        }
        context.update(if round & 1 != 0 { &alt } else { &p_bytes });
        alt = context.finalize();
    }

    let mut out = String::from("$6$");
    if explicit_rounds {
        out.push_str(&format!("rounds={rounds}$"));
    }
    out.push_str(std::str::from_utf8(salt).ok()?);
    out.push('$');
    out.push_str(&encode_sha512(&alt));
    Some(out)
}

/// Compare a password against a `$6$` hash in constant time.
pub fn verify(password: &str, hash: &str) -> bool {
    let Some(computed) = sha512_crypt(password, hash) else {
        return false;
    };
    let left = computed.as_bytes();
    let right = hash.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn sha512_matches_known_vectors() {
        assert_eq!(
            hex(&digest(b"")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
                .replace(char::is_whitespace, "")
        );
        assert_eq!(
            hex(&digest(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
                .replace(char::is_whitespace, "")
        );
    }

    #[test]
    fn verifies_the_image_root_hash() {
        let hash = "$6$rinit$h.sfVK5a75HqcCQ8mjFVlpyRSK5rin/8GpC7okbfq82BH16wN0lJDJUvCd.0bpt2/q/MtfkobUfMxtG72uIuj.";
        assert!(verify("toor", hash));
        assert!(!verify("wrong", hash));
    }

    #[test]
    fn rejects_other_hash_formats() {
        assert!(!verify("x", "$1$salt$hash"));
        assert!(!verify("x", "!"));
        assert!(!verify("x", ""));
    }
}
