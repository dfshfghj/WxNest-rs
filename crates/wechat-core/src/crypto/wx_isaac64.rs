const SIZE: usize = 256;

const INITIAL: [u64; 8] = [
    0x647c4677a2884b7c,
    0xb9f8b322c73ac862,
    0x8c0ea5053d4712a0,
    0xb29b2e824a595524,
    0x82f053db8355e0ce,
    0x48fe4a0fa5a09315,
    0xae985bf2cbfc89ed,
    0x98f5704f6c44c0ab,
];

#[derive(Debug, Clone)]
pub struct WxIsaac64 {
    mm: [u64; SIZE],
    rs: [u64; SIZE],
    aa: u64,
    bb: u64,
    cc: u64,
    count: usize,
}

impl WxIsaac64 {
    pub fn new(seed: u64) -> Self {
        let mut state = Self {
            mm: [0; SIZE],
            rs: [0; SIZE],
            aa: 0,
            bb: 0,
            cc: 0,
            count: 0,
        };
        state.rs[0] = seed;
        let mut v = INITIAL;
        for pass in 0..2 {
            for index in (0..SIZE).step_by(8) {
                for (offset, value) in v.iter_mut().enumerate() {
                    let add = if pass == 0 {
                        state.rs[index + offset]
                    } else {
                        state.mm[index + offset]
                    };
                    *value = value.wrapping_add(add);
                }
                mix(&mut v);
                state.mm[index..index + 8].copy_from_slice(&v);
            }
        }
        state.refill();
        state
    }

    pub fn from_key(key: &str) -> Self {
        let digits: String = key
            .trim()
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        Self::new(digits.parse::<u64>().unwrap_or(0))
    }

    fn refill(&mut self) {
        self.cc = self.cc.wrapping_add(1);
        self.bb = self.bb.wrapping_add(self.cc);
        for index in 0..SIZE {
            let x = self.mm[index];
            self.aa = match index & 3 {
                0 => self.aa ^ !(self.aa << 21),
                1 => self.aa ^ (self.aa >> 5),
                2 => self.aa ^ (self.aa << 12),
                _ => self.aa ^ (self.aa >> 33),
            };
            self.aa = self.mm[(index + SIZE / 2) & (SIZE - 1)].wrapping_add(self.aa);
            let y = self.mm[((x >> 3) & 255) as usize]
                .wrapping_add(self.aa)
                .wrapping_add(self.bb);
            self.mm[index] = y;
            self.bb = self.mm[((y >> 11) & 255) as usize].wrapping_add(x);
            self.rs[index] = self.bb;
        }
        self.count = SIZE;
    }

    pub fn next_u64(&mut self) -> u64 {
        if self.count == 0 {
            self.refill();
        }
        self.count -= 1;
        self.rs[self.count]
    }

    pub fn keystream(&mut self, size: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(size + 8);
        while output.len() < size {
            output.extend_from_slice(&self.next_u64().to_be_bytes());
        }
        output.truncate(size);
        output
    }
}

pub fn xor_in_place(data: &mut [u8], key: &str, limit: Option<usize>) {
    let size = limit.map_or(data.len(), |limit| limit.min(data.len()));
    let keystream = WxIsaac64::from_key(key).keystream(size);
    for (byte, mask) in data[..size].iter_mut().zip(keystream) {
        *byte ^= mask;
    }
}

fn mix(v: &mut [u64; 8]) {
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *v;
    a = a.wrapping_sub(e);
    f ^= h >> 9;
    h = h.wrapping_add(a);
    b = b.wrapping_sub(f);
    g ^= a << 9;
    a = a.wrapping_add(b);
    c = c.wrapping_sub(g);
    h ^= b >> 23;
    b = b.wrapping_add(c);
    d = d.wrapping_sub(h);
    a ^= c << 15;
    c = c.wrapping_add(d);
    e = e.wrapping_sub(a);
    b ^= d >> 14;
    d = d.wrapping_add(e);
    f = f.wrapping_sub(b);
    c ^= e << 20;
    e = e.wrapping_add(f);
    g = g.wrapping_sub(c);
    d ^= f >> 17;
    f = f.wrapping_add(g);
    h = h.wrapping_sub(d);
    e ^= g << 14;
    g = g.wrapping_add(h);
    *v = [a, b, c, d, e, f, g, h];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn matches_vendor_wasm_vectors() {
        assert_eq!(
            hex(&WxIsaac64::from_key("1").keystream(13)),
            "e19ed5d2ca98af2da7a18d07ca"
        );
        assert_eq!(
            hex(&WxIsaac64::from_key("2026090201020304").keystream(16)),
            "cc8ab6592528fbaa2238f7fad7c416ee"
        );
    }

    #[test]
    fn xor_is_involutive_and_respects_limit() {
        let original: Vec<u8> = (0..300u32).map(|value| value as u8).collect();
        let mut data = original.clone();
        xor_in_place(&mut data, "42", Some(100));
        assert_ne!(data[..100], original[..100]);
        assert_eq!(data[100..], original[100..]);
        xor_in_place(&mut data, "42", Some(100));
        assert_eq!(data, original);
    }
}
