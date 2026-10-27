use std::str::FromStr;

use crate::error::{CoreError, Result};

const MASK64: u64 = u64::MAX;
const GOLDEN_RATIO: u64 = 0x9E37_79B9_7F4A_7C15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordFormat {
    RawLe,
    RawBe,
    BeSwap32,
    LeSwap32,
}

impl FromStr for WordFormat {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "raw_le" => Ok(Self::RawLe),
            "raw_be" => Ok(Self::RawBe),
            "be_swap32" => Ok(Self::BeSwap32),
            "le_swap32" => Ok(Self::LeSwap32),
            other => Err(CoreError::Crypto(format!(
                "unknown ISAAC-64 word format: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Isaac64 {
    mm: [u64; 256],
    aa: u64,
    bb: u64,
    cc: u64,
    randrsl: [u64; 256],
    randcnt: usize,
}

impl Isaac64 {
    pub fn new(seed: &str) -> Self {
        let seed_value = parse_seed(seed);
        let mut state = Self {
            mm: [0; 256],
            aa: 0,
            bb: 0,
            cc: 0,
            randrsl: [0; 256],
            randcnt: 0,
        };
        state.randrsl[0] = seed_value;
        state.init(true);
        state
    }

    fn init(&mut self, flag: bool) {
        let mut a = GOLDEN_RATIO;
        let mut b = GOLDEN_RATIO;
        let mut c = GOLDEN_RATIO;
        let mut d = GOLDEN_RATIO;
        let mut e = GOLDEN_RATIO;
        let mut f = GOLDEN_RATIO;
        let mut g = GOLDEN_RATIO;
        let mut h = GOLDEN_RATIO;

        for _ in 0..4 {
            mix(
                &mut a, &mut b, &mut c, &mut d, &mut e, &mut f, &mut g, &mut h,
            );
        }

        let mut index = 0;
        while index < 256 {
            if flag {
                a = a.wrapping_add(self.randrsl[index]);
                b = b.wrapping_add(self.randrsl[index + 1]);
                c = c.wrapping_add(self.randrsl[index + 2]);
                d = d.wrapping_add(self.randrsl[index + 3]);
                e = e.wrapping_add(self.randrsl[index + 4]);
                f = f.wrapping_add(self.randrsl[index + 5]);
                g = g.wrapping_add(self.randrsl[index + 6]);
                h = h.wrapping_add(self.randrsl[index + 7]);
            }
            mix(
                &mut a, &mut b, &mut c, &mut d, &mut e, &mut f, &mut g, &mut h,
            );
            self.mm[index] = a;
            self.mm[index + 1] = b;
            self.mm[index + 2] = c;
            self.mm[index + 3] = d;
            self.mm[index + 4] = e;
            self.mm[index + 5] = f;
            self.mm[index + 6] = g;
            self.mm[index + 7] = h;
            index += 8;
        }

        if flag {
            let mut index = 0;
            while index < 256 {
                a = a.wrapping_add(self.mm[index]);
                b = b.wrapping_add(self.mm[index + 1]);
                c = c.wrapping_add(self.mm[index + 2]);
                d = d.wrapping_add(self.mm[index + 3]);
                e = e.wrapping_add(self.mm[index + 4]);
                f = f.wrapping_add(self.mm[index + 5]);
                g = g.wrapping_add(self.mm[index + 6]);
                h = h.wrapping_add(self.mm[index + 7]);
                mix(
                    &mut a, &mut b, &mut c, &mut d, &mut e, &mut f, &mut g, &mut h,
                );
                self.mm[index] = a;
                self.mm[index + 1] = b;
                self.mm[index + 2] = c;
                self.mm[index + 3] = d;
                self.mm[index + 4] = e;
                self.mm[index + 5] = f;
                self.mm[index + 6] = g;
                self.mm[index + 7] = h;
                index += 8;
            }
        }

        self.isaac64();
        self.randcnt = 256;
    }

    fn isaac64(&mut self) {
        self.cc = self.cc.wrapping_add(1);
        self.bb = self.bb.wrapping_add(self.cc);

        for index in 0..256 {
            let x = self.mm[index];
            self.aa = match index & 3 {
                0 => self.aa ^ ((self.aa << 21) ^ MASK64),
                1 => self.aa ^ (self.aa >> 5),
                2 => self.aa ^ (self.aa << 12),
                _ => self.aa ^ (self.aa >> 33),
            };
            self.aa = self.mm[(index + 128) & 255].wrapping_add(self.aa);
            let y = self.mm[((x >> 3) & 255) as usize]
                .wrapping_add(self.aa)
                .wrapping_add(self.bb);
            self.mm[index] = y;
            self.bb = self.mm[((y >> 11) & 255) as usize].wrapping_add(x);
            self.randrsl[index] = self.bb;
        }
    }

    pub fn rand_u64(&mut self) -> u64 {
        if self.randcnt == 0 {
            self.isaac64();
            self.randcnt = 256;
        }
        self.randcnt -= 1;
        self.randrsl[self.randcnt]
    }

    pub fn generate_keystream(&mut self, size: usize, format: WordFormat) -> Vec<u8> {
        if size == 0 {
            return Vec::new();
        }
        let blocks = size.div_ceil(8);
        let mut output = Vec::with_capacity(blocks * 8);
        for _ in 0..blocks {
            output.extend_from_slice(&raw_to_bytes(self.rand_u64(), format));
        }
        output.truncate(size);
        output
    }
}

fn raw_to_bytes(value: u64, format: WordFormat) -> [u8; 8] {
    match format {
        WordFormat::RawLe => value.to_le_bytes(),
        WordFormat::RawBe => value.to_be_bytes(),
        WordFormat::BeSwap32 => {
            let bytes = value.to_be_bytes();
            [
                bytes[4], bytes[5], bytes[6], bytes[7], bytes[0], bytes[1], bytes[2], bytes[3],
            ]
        }
        WordFormat::LeSwap32 => {
            let bytes = value.to_le_bytes();
            [
                bytes[4], bytes[5], bytes[6], bytes[7], bytes[0], bytes[1], bytes[2], bytes[3],
            ]
        }
    }
}

fn parse_seed(seed: &str) -> u64 {
    let text = seed.trim();
    if text.is_empty() {
        return 0;
    }
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let parsed = if let Some(rest) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        u64::from_str_radix(rest, 16).ok()
    } else if let Some(rest) = unsigned
        .strip_prefix("0o")
        .or_else(|| unsigned.strip_prefix("0O"))
    {
        u64::from_str_radix(rest, 8).ok()
    } else if let Some(rest) = unsigned
        .strip_prefix("0b")
        .or_else(|| unsigned.strip_prefix("0B"))
    {
        u64::from_str_radix(rest, 2).ok()
    } else {
        if unsigned.len() > 1
            && unsigned.starts_with('0')
            && unsigned.chars().all(|c| c.is_ascii_digit())
        {
            None
        } else {
            unsigned.parse::<u64>().ok()
        }
    };
    let value = parsed.unwrap_or(0);
    if negative {
        (0u64).wrapping_sub(value)
    } else {
        value
    }
}

#[allow(clippy::too_many_arguments)]
fn mix(
    a: &mut u64,
    b: &mut u64,
    c: &mut u64,
    d: &mut u64,
    e: &mut u64,
    f: &mut u64,
    g: &mut u64,
    h: &mut u64,
) {
    *a = a.wrapping_sub(*e);
    *f ^= *h >> 9;
    *h = h.wrapping_add(*a);

    *b = b.wrapping_sub(*f);
    *g ^= *a << 9;
    *a = a.wrapping_add(*b);

    *c = c.wrapping_sub(*g);
    *h ^= *b >> 23;
    *b = b.wrapping_add(*c);

    *d = d.wrapping_sub(*h);
    *a ^= *c << 15;
    *c = c.wrapping_add(*d);

    *e = e.wrapping_sub(*a);
    *b ^= *d >> 14;
    *d = d.wrapping_add(*e);

    *f = f.wrapping_sub(*b);
    *c ^= *e << 20;
    *e = e.wrapping_add(*f);

    *g = g.wrapping_sub(*c);
    *d ^= *f >> 17;
    *f = f.wrapping_add(*g);

    *h = h.wrapping_sub(*d);
    *e ^= *g << 14;
    *g = g.wrapping_add(*h);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_output() {
        let mut first = Isaac64::new("12345");
        let mut second = Isaac64::new("12345");
        let left = first.generate_keystream(64, WordFormat::BeSwap32);
        let right = second.generate_keystream(64, WordFormat::BeSwap32);
        assert_eq!(left, right);
        assert_eq!(left.len(), 64);
    }

    #[test]
    fn seed_parsing() {
        assert_eq!(parse_seed(""), 0);
        assert_eq!(parse_seed(" 42 "), 42);
        assert_eq!(parse_seed("0x10"), 16);
        assert_eq!(parse_seed("0123"), 0);
        assert_eq!(parse_seed("-1"), u64::MAX);
    }
}
