//! Number formatting and rounding exactly as Python does them, so messages match the reference
//! implementation character for character.
//!
//! Rust's fixed-precision formatting already rounds the exact binary value half to even, like Python's
//! `format(x, ".Nf")` and `round(x, n)`; only `repr` differs (Python writes `1e-05`, Rust `1e-5`).

use serde_json::Value;

/// Python's `round(x)`: half to even, as an integer.
pub fn round(x: f64) -> i64 {
    format!("{x:.0}").parse::<f64>().unwrap_or(x) as i64
}

/// Python's `round(x, n)` for floats.
pub fn round_to(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().unwrap_or(x)
}

/// Python's `repr()` of a float: shortest round-trip form, `.0` on whole numbers, `e+NN`/`e-NN` exponents.
pub fn repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let s = format!("{x:?}");
    match s.find('e') {
        None => s,
        Some(i) => {
            let (mant, exp) = (&s[..i], &s[i + 1..]);
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            let mant = mant.strip_suffix(".0").unwrap_or(mant);
            format!("{mant}e{sign}{digits:0>2}")
        }
    }
}

/// Python's `str()` of a value loaded with `json.loads`: ints as ints, floats as `repr`, strings bare,
/// `None`, `True`/`False`, lists as `[a, b]` with `repr` of their items.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

pub fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) if !n.is_f64() => i.to_string(),
            (_, Some(u)) if !n.is_f64() => u.to_string(),
            _ => repr(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter().map(|(k, v)| format!("'{k}': {}", py_repr(v))).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Python's `sum()` over floats (3.12 and later): Neumaier's compensated summation, as CPython does it.
pub fn sum(xs: impl IntoIterator<Item = f64>) -> f64 {
    let (mut total, mut c) = (0.0f64, 0.0f64);
    for x in xs {
        let t = total + x;
        if total.abs() >= x.abs() {
            c += (total - t) + x;
        } else {
            c += (x - t) + total;
        }
        total = t;
    }
    if c != 0.0 && c.is_finite() {
        total += c;
    }
    total
}

/// The first `n` characters (not bytes), as Python's `s[:n]`.
pub fn prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_is_half_to_even() {
        assert_eq!([0.5, 1.5, 2.5, -0.5, -1.5, 0.49999999999999994].map(round), [0, 2, 2, 0, -2, 0]);
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(round_to(2.675, 2), 2.67); // 2.675 is 2.67499999... in binary
        assert_eq!(round_to(1234.56785, 4), 1234.5678);
    }

    #[test]
    fn repr_like_python() {
        assert_eq!(repr(6.0), "6.0");
        assert_eq!(repr(0.15), "0.15");
        assert_eq!(repr(1e-5), "1e-05");
        assert_eq!(repr(1e16), "1e+16");
        assert_eq!(repr(1.2345678901234568e17), "1.2345678901234568e+17");
        assert_eq!(repr(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(repr(-0.0), "-0.0");
    }

    #[test]
    fn str_of_json_values() {
        let v: Value = serde_json::from_str(r#"[0, 60, 640.0, "x", null, true]"#).unwrap();
        assert_eq!(py_str(&v), "[0, 60, 640.0, 'x', None, True]");
        assert_eq!(py_str(&Value::String("timeout".into())), "timeout");
    }

    #[test]
    fn compensated_sum() {
        assert_eq!(sum([0.1; 10]), 1.0); // plain left-to-right addition gives 0.9999999999999999
        assert_eq!(sum([1e100, 1.0, -1e100]), 1.0);
        assert_eq!(sum([]), 0.0);
    }

    #[test]
    fn prefix_counts_characters() {
        assert_eq!(prefix("héllo", 2), "hé");
        assert_eq!(prefix("ab", 5), "ab");
    }
}
