//! Words and numbers for the screen, in one place and tested: amounts, addresses, hashes, times.

use tenero_app::ui::{group_digits, TICKER};
use tenero_wallet::amount::format_coins;

/// `1.5 TNR`, with the digits of the whole part grouped (`12,345.5 TNR`).
pub fn coins(units: u64) -> String {
    let plain = format_coins(units);
    let (whole, frac) = match plain.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (plain.as_str(), None),
    };
    let whole = whole.parse::<u64>().map_or(whole.to_string(), group_digits);
    match frac {
        Some(f) => format!("{whole}.{f} {TICKER}"),
        None => format!("{whole} {TICKER}"),
    }
}

/// An address cut for a table: `tni1c31b8473d1172…ef3ffe297bae7`.
pub fn short_address(a: &str) -> String {
    if a.len() <= 28 {
        return a.to_string();
    }
    format!("{}…{}", &a[..16], &a[a.len() - 10..])
}

/// A 32-byte id as 64 hexadecimal digits.
pub fn hex(id: &[u8; 32]) -> String {
    tenero_core::hash::hex_lower(id)
}

/// The first 8 and last 6 digits of an id.
pub fn short_hex(id: &[u8; 32]) -> String {
    let h = hex(id);
    format!("{}…{}", &h[..8], &h[h.len() - 6..])
}

/// `2026-10-03 14:05 UTC` from seconds since 1970 (the computer's clock when the payment was sent).
pub fn when(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// `1 h 05 min`, `3 min 20 s`, `45 s`.
pub fn duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} h {:02} min", secs / 3600, secs % 3600 / 60)
    } else if secs >= 60 {
        format!("{} min {:02} s", secs / 60, secs % 60)
    } else {
        format!("{secs} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_are_grouped_and_never_in_exponent_form() {
        assert_eq!(coins(0), "0 TNR");
        assert_eq!(coins(150_000_000), "1.5 TNR");
        assert_eq!(coins(1), "0.00000001 TNR");
        assert_eq!(coins(1_234_567_800_000_000), "12,345,678 TNR");
        assert_eq!(coins(u64::MAX), "184,467,440,737.09551615 TNR");
    }

    #[test]
    fn dates_are_right_across_the_awkward_days() {
        assert_eq!(when(0), "1970-01-01 00:00 UTC");
        assert_eq!(when(951_782_400), "2000-02-29 00:00 UTC"); // a leap day
        assert_eq!(when(1_709_078_400 + 86_399), "2024-02-28 23:59 UTC");
        assert_eq!(when(1_709_078_400 + 86_400), "2024-02-29 00:00 UTC");
        assert_eq!(when(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(when(4_102_444_800), "2100-01-01 00:00 UTC");
    }

    #[test]
    fn short_forms_keep_both_ends_and_leave_short_text_alone() {
        let a = format!("TENg{}", "0123456789abcdef".repeat(6));
        let s = short_address(&a);
        assert!(s.starts_with("TENg0123456789ab") && s.ends_with("6789abcdef") && s.contains('…'));
        assert_eq!(short_address("TENgabc"), "TENgabc");
        assert_eq!(short_hex(&[0xab; 32]), "abababab…ababab");
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(200), "3 min 20 s");
        assert_eq!(duration(3900), "1 h 05 min");
    }
}
