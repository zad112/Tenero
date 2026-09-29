//! Coins as whole numbers of the smallest unit (`CONSENSUS.md` section 1). 4 decimals: one coin is
//! 10,000 units. Amounts are `i64`; parsing text is a user interface matter, not consensus.

pub const DECIMALS: usize = 4;
pub const UNIT: i64 = 10_000;

/// Coins as text to units: an optional `-`, digits, and optionally `.` and 1 to 4 digits.
///
/// Stricter than the Python reference, which also accepts surrounding spaces, a leading `+`,
/// exponent notation (`1e2`), a bare `1.` or `.5`, and more than 4 decimal places when the extra ones
/// are zeros. `CONSENSUS.md` allows a rewrite to reject those.
pub fn to_units(text: &str) -> Result<i64, String> {
    let bad = || format!("'{text}' is not a valid amount");
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, frac) = match digits.split_once('.') {
        Some((w, f)) => (w, f),
        None => (digits, ""),
    };
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(frac) {
        return Err(bad());
    }
    if digits.contains('.') && frac.is_empty() {
        return Err(bad());
    }
    if frac.len() > DECIMALS {
        return Err(format!(
            "amounts can have at most {DECIMALS} decimal places"
        ));
    }
    let mut value: i128 = 0;
    for b in whole.bytes().chain(frac.bytes()) {
        value = value.checked_mul(10).ok_or_else(bad)? + i128::from(b - b'0');
        if value > i128::from(u64::MAX) * 10_000 {
            return Err(bad());
        }
    }
    for _ in frac.len()..DECIMALS {
        value *= 10;
    }
    let value = if negative { -value } else { value };
    i64::try_from(value).map_err(|_| format!("'{text}' is out of range"))
}

/// Units as coins, always with 4 decimal places (`-` for negative values).
pub fn fmt(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let u = units.unsigned_abs();
    let unit = UNIT.unsigned_abs();
    format!("{sign}{}.{:04}", u / unit, u % unit)
}
