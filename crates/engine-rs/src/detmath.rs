//! engine-lib DetMath: exp/log/pow built from + - * / and bit views, so every
//! engine produces the same bits. Ported operation for operation (Rust never
//! fuses multiply-adds on its own, so the rounding matches).

const LN2: f64 = 0.6931471805599453;
const LOG2E: f64 = 1.4426950408889634;
const SQRT2: f64 = 1.4142135623730951;

fn hi(x: f64) -> u32 {
    (x.to_bits() >> 32) as u32
}

fn with_hi(x: f64, hi: u32) -> f64 {
    f64::from_bits(((hi as u64) << 32) | (x.to_bits() & 0xffff_ffff))
}

/// 2 ** n for integer n.
pub fn pow2(n: f64) -> f64 {
    if n > 1023.0 {
        return f64::INFINITY;
    }
    if n < -1022.0 {
        return 0.0;
    }
    let hi = ((n as i32 + 1023) << 20) as u32;
    f64::from_bits((hi as u64) << 32)
}

pub fn exp(x: f64) -> f64 {
    if x > 709.0 {
        return f64::INFINITY;
    }
    if x < -708.0 {
        return 0.0;
    }
    let n = (x * LOG2E + 0.5).floor();
    let r = x - n * LN2;
    let p = 1.0
        + r * (1.0
            + r * (1.0 / 2.0
                + r * (1.0 / 6.0
                    + r * (1.0 / 24.0
                        + r * (1.0 / 120.0 + r * (1.0 / 720.0 + r * (1.0 / 5040.0 + r * (1.0 / 40320.0))))))));
    p * pow2(n)
}

pub fn log(mut x: f64) -> f64 {
    if x <= 0.0 {
        return if x == 0.0 { f64::NEG_INFINITY } else { f64::NAN };
    }
    let mut e: f64 = 0.0;
    if x < 2.2250738585072014e-308 {
        x *= 18014398509481984.0; // 2 ** 54
        e = -54.0;
    }
    let h = hi(x);
    e += (((h >> 20) & 0x7ff) as i32 - 1023) as f64;
    let mut m = with_hi(x, (h & 0x000fffff) | 0x3ff00000);
    if m > SQRT2 {
        m *= 0.5;
        e += 1.0;
    }
    let s = (m - 1.0) / (m + 1.0);
    let z = s * s;
    let series = 1.0
        + z * (1.0 / 3.0 + z * (1.0 / 5.0 + z * (1.0 / 7.0 + z * (1.0 / 9.0 + z * (1.0 / 11.0 + z * (1.0 / 13.0))))));
    e * LN2 + 2.0 * s * series
}

/// x ** y for x >= 0.
pub fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f64::INFINITY };
    }
    if x < 0.0 {
        return f64::NAN;
    }
    exp(y * log(x))
}

/// engine-lib Util.sigmoid
pub fn sigmoid(value: f64, decay_rate: f64, midpoint: f64) -> f64 {
    1.0 / (1.0 + exp(-decay_rate * (value - midpoint)))
}

/// engine-lib Util.within: Math.min(Math.max(v, min), max)
pub fn within(v: f64, min: f64, max: f64) -> f64 {
    js_min(js_max(v, min), max)
}

pub fn js_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a < b {
        a
    } else {
        b
    }
}

pub fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else if a > b {
        a
    } else {
        b
    }
}
