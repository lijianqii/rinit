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

const K256: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H256: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    buffer_len: usize,
    length: u128,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: H256,
            buffer: [0; 64],
            buffer_len: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u128;
        if self.buffer_len > 0 {
            let space = 64 - self.buffer_len;
            let take = data.len().min(space);
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                compress256(&mut self.state, &block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let Ok(block): Result<&[u8; 64], _> = data[..64].try_into() else {
                break;
            };
            compress256(&mut self.state, block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.length * 8;
        let mut padding = vec![0x80_u8];
        let remainder = (self.buffer_len + 1) % 64;
        let zeros = if remainder <= 56 {
            56 - remainder
        } else {
            120 - remainder
        };
        padding.extend(std::iter::repeat_n(0_u8, zeros));
        padding.extend_from_slice(&(bit_len as u64).to_be_bytes());
        self.update(&padding);
        let mut out = [0_u8; 32];
        for (index, word) in self.state.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress256(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut schedule = [0_u32; 64];
    for (index, slot) in schedule.iter_mut().take(16).enumerate() {
        *slot = u32::from_be_bytes(block[index * 4..index * 4 + 4].try_into().unwrap_or([0; 4]));
    }
    for index in 16..64 {
        let s0 = schedule[index - 15].rotate_right(7)
            ^ schedule[index - 15].rotate_right(18)
            ^ (schedule[index - 15] >> 3);
        let s1 = schedule[index - 2].rotate_right(17)
            ^ schedule[index - 2].rotate_right(19)
            ^ (schedule[index - 2] >> 10);
        schedule[index] = schedule[index - 16]
            .wrapping_add(s0)
            .wrapping_add(schedule[index - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choose = (e & f) ^ (!e & g);
        let temp1 = h
            .wrapping_add(sigma1)
            .wrapping_add(choose)
            .wrapping_add(K256[index])
            .wrapping_add(schedule[index]);
        let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
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

fn encode_sha256(hash: &[u8; 32]) -> String {
    let mut out = String::new();
    let mut index = 0;
    loop {
        let mut j = index + 10;
        let mut k = index + 20;
        if j >= 30 {
            j -= 30;
        }
        if k >= 30 {
            k -= 30;
        }
        b64_from_24bit(&mut out, hash[index], hash[j], hash[k], 4);
        if k == 29 {
            break;
        }
        index = k + 1;
    }
    b64_from_24bit(&mut out, 0, hash[31], hash[30], 3);
    out
}

/// Compute the `$5$` crypt representation of `password` for `setting`.
pub fn sha256_crypt(password: &str, setting: &str) -> Option<String> {
    let mut rest = setting.strip_prefix("$5$")?;
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

    let mut context = Sha256::new();
    context.update(key);
    context.update(salt);
    let mut alternate = Sha256::new();
    alternate.update(key);
    alternate.update(salt);
    alternate.update(key);
    let mut alt = alternate.finalize();

    let mut count = key.len();
    while count > 32 {
        context.update(&alt);
        count -= 32;
    }
    context.update(&alt[..count]);
    let mut bits = key.len();
    while bits != 0 {
        context.update(if bits & 1 != 0 { &alt } else { key });
        bits >>= 1;
    }
    alt = context.finalize();

    let mut p_context = Sha256::new();
    for _ in 0..key.len() {
        p_context.update(key);
    }
    let p_bytes = repeat_to_len(&p_context.finalize(), key.len());

    let mut s_context = Sha256::new();
    for _ in 0..(16 + alt[0] as usize) {
        s_context.update(salt);
    }
    let s_bytes = repeat_to_len(&s_context.finalize(), salt.len());

    for round in 0..rounds {
        let mut context = Sha256::new();
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

    let mut out = String::from("$5$");
    if explicit_rounds {
        out.push_str(&format!("rounds={rounds}$"));
    }
    out.push_str(std::str::from_utf8(salt).ok()?);
    out.push('$');
    out.push_str(&encode_sha256(&alt));
    Some(out)
}

#[cfg(test)]
fn digest(data: &[u8]) -> [u8; 64] {
    let mut context = Sha512::new();
    context.update(data);
    context.finalize()
}

#[cfg(test)]
fn digest256(data: &[u8]) -> [u8; 32] {
    let mut context = Sha256::new();
    context.update(data);
    context.finalize()
}

fn repeat_to_len(bytes: &[u8], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    while out.len() < length {
        let take = (length - out.len()).min(bytes.len());
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
    let computed = if hash.starts_with("$6$") {
        sha512_crypt(password, hash)
    } else if hash.starts_with("$5$") {
        sha256_crypt(password, hash)
    } else if hash.starts_with("$1$") {
        md5_crypt(password, hash)
    } else {
        None
    };
    let Some(computed) = computed else {
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

const MD5_K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

const MD5_S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

struct Md5 {
    state: [u32; 4],
    buffer: [u8; 64],
    buffer_len: usize,
    length: u64,
}

impl Md5 {
    fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            buffer: [0; 64],
            buffer_len: 0,
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        if self.buffer_len > 0 {
            let space = 64 - self.buffer_len;
            let take = data.len().min(space);
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                compress_md5(&mut self.state, &block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let Ok(block): Result<&[u8; 64], _> = data[..64].try_into() else {
                break;
            };
            compress_md5(&mut self.state, block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 16] {
        let bit_len = self.length * 8;
        let mut padding = vec![0x80_u8];
        let remainder = (self.buffer_len + 1) % 64;
        let zeros = if remainder <= 56 {
            56 - remainder
        } else {
            120 - remainder
        };
        padding.extend(std::iter::repeat_n(0_u8, zeros));
        padding.extend_from_slice(&bit_len.to_le_bytes());
        self.update(&padding);
        let mut out = [0_u8; 16];
        for (index, word) in self.state.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

fn compress_md5(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut words = [0_u32; 16];
    for (index, slot) in words.iter_mut().enumerate() {
        *slot = u32::from_le_bytes(block[index * 4..index * 4 + 4].try_into().unwrap_or([0; 4]));
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for index in 0..64 {
        let (function, word) = match index {
            0..=15 => ((b & c) | (!b & d), index),
            16..=31 => ((d & b) | (!d & c), (5 * index + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * index + 5) % 16),
            _ => (c ^ (b | !d), (7 * index) % 16),
        };
        let next = b.wrapping_add(
            a.wrapping_add(function)
                .wrapping_add(MD5_K[index])
                .wrapping_add(words[word])
                .rotate_left(MD5_S[index]),
        );
        a = d;
        d = c;
        c = b;
        b = next;
    }
    *state = [
        state[0].wrapping_add(a),
        state[1].wrapping_add(b),
        state[2].wrapping_add(c),
        state[3].wrapping_add(d),
    ];
}

/// Compute the `$1$` MD5 crypt representation of `password` for `setting`.
pub fn md5_crypt(password: &str, setting: &str) -> Option<String> {
    let rest = setting.strip_prefix("$1$")?;
    let salt_end = rest.find('$').unwrap_or(rest.len());
    let salt = rest.get(..salt_end.min(8))?;
    let key = password.as_bytes();
    let salt_bytes = salt.as_bytes();

    let mut context = Md5::new();
    context.update(key);
    context.update(b"$1$");
    context.update(salt_bytes);

    let mut alternate = Md5::new();
    alternate.update(key);
    alternate.update(salt_bytes);
    alternate.update(key);
    let mut final_result = alternate.finalize();

    let mut remaining = key.len();
    while remaining > 0 {
        let take = remaining.min(16);
        context.update(&final_result[..take]);
        remaining -= take;
    }
    let zero = [0_u8; 1];
    let mut bits = key.len();
    while bits != 0 {
        context.update(if bits & 1 != 0 { &zero } else { &key[..1] });
        bits >>= 1;
    }
    final_result = context.finalize();

    for round in 0..1000 {
        let mut context = Md5::new();
        context.update(if round & 1 != 0 { key } else { &final_result });
        if round % 3 != 0 {
            context.update(salt_bytes);
        }
        if round % 7 != 0 {
            context.update(key);
        }
        context.update(if round & 1 != 0 { &final_result } else { key });
        final_result = context.finalize();
    }

    let mut encoded = [0_u8; 17];
    encoded[..16].copy_from_slice(&final_result);
    encoded[16] = final_result[5];
    let mut out = String::from("$1$");
    out.push_str(salt);
    out.push('$');
    for index in 0..5 {
        b64_from_24bit(
            &mut out,
            encoded[index],
            encoded[index + 6],
            encoded[index + 12],
            4,
        );
    }
    b64_from_24bit(&mut out, 0, 0, final_result[11], 2);
    Some(out)
}

#[cfg(test)]
fn digest_md5(data: &[u8]) -> [u8; 16] {
    let mut context = Md5::new();
    context.update(data);
    context.finalize()
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
    fn sha256_matches_known_vectors() {
        assert_eq!(
            hex(&digest256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&digest256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(hex(&digest_md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&digest_md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn verifies_an_md5_hash() {
        let hash = "$1$rinit$TgEj7YentAlBjR1IVAQLG1";
        assert!(verify("toor", hash));
        assert!(!verify("wrong", hash));
    }

    #[test]
    fn verifies_a_sha256_hash() {
        let hash = "$5$rinit$3J/IImUzg9LF.7ZabNp3wB5/C1QgamiQI92FXaJf7s/";
        assert!(verify("toor", hash));
        assert!(!verify("wrong", hash));
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
