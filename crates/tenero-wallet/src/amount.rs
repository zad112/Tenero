//! Amounts as people write them: coins with up to 8 decimals (`CONSENSUS_V2.md` section 3), held as whole units.
//!
//! Parsing is strict: digits, at most one `.`, at most 8 decimals, no sign, no exponent, no spaces, nothing that
//! does not fit in a `u64` of units. A typing mistake in an amount must be an error, never a different amount.

pub const DECIMALS: usize = 8;
pub const UNITS_PER_COIN: u64 = 100_000_000;

/// `"1.5"` is 150,000,000 units. `None` for anything that is not exactly an amount.
pub fn parse_coins(text: &str) -> Option<u64> {
    let (whole, frac) = match text.split_once('.') {
        Some((w, f)) => (w, f),
        None => (text, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac.len() > DECIMALS || (text.contains('.') && frac.is_empty()) {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut frac_units = 0u64;
    for (i, b) in frac.bytes().enumerate() {
        frac_units += u64::from(b - b'0') * 10u64.pow((DECIMALS - 1 - i) as u32);
    }
    whole.checked_mul(UNITS_PER_COIN)?.checked_add(frac_units)
}

/// `150000000` is `"1.5"`: trailing zeros of the fraction dropped, never an exponent.
pub fn format_coins(units: u64) -> String {
    let (w, f) = (units / UNITS_PER_COIN, units % UNITS_PER_COIN);
    if f == 0 {
        return w.to_string();
    }
    let frac = format!("{f:08}");
    format!("{w}.{}", frac.trim_end_matches('0'))
}
