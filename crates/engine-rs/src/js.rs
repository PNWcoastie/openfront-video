//! JavaScript number semantics the engine relies on.

/// ECMAScript ToInt32: what `x | 0` does.
pub fn to_int32(x: f64) -> i32 {
    if !x.is_finite() {
        return 0;
    }
    let t = x.trunc();
    let m = t.rem_euclid(4294967296.0);
    m as u32 as i32
}

/// engine-lib Util.simpleHash: a 31-bit string hash over UTF-16 code units.
pub fn simple_hash(s: &str) -> f64 {
    let mut hash: i32 = 0;
    for c in s.encode_utf16() {
        // (hash << 5) - hash + char, then `hash & hash` truncates to int32.
        let v = hash.wrapping_shl(5) as f64 - hash as f64 + c as f64;
        hash = to_int32(v);
    }
    (hash as f64).abs()
}

/// Number.isInteger for a JSON number.
pub fn is_integer(x: f64) -> bool {
    x.is_finite() && x.trunc() == x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int32() {
        assert_eq!(to_int32(4294967296.0 + 5.0), 5);
        assert_eq!(to_int32(2147483648.0), -2147483648);
        assert_eq!(to_int32(-1.5), -1);
    }
}
