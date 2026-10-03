//! What the miner program tells a program that started it: a small text file (`tenero-miner --status-file FILE`)
//! rewritten every second with `key=value` lines. The wallet app reads it to show the rate, the card and the blocks
//! found. It is **information for a screen**: nothing in it is trusted for anything else, and a file that is old
//! (the miner stopped or hung) is shown as stale by the reader, never as current.

use std::collections::BTreeMap;

use crate::ui::{MinerStatus, NodeLink};

/// What the file holds. Every field is a reading; `None` means the miner did not have it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MinerReport {
    /// Seconds since 1970 when the file was written (the miner's clock).
    pub written_at: u64,
    pub backend: String,
    pub link: NodeLink,
    pub node_height: u64,
    pub searching: bool,
    /// Attempts a second over 10 s, 60 s, 15 min and the run (see `tenero_miner::rate`; not hashes of other coins).
    pub s10: Option<f64>,
    pub s60: Option<f64>,
    pub m15: Option<f64>,
    pub average: Option<f64>,
    pub found: u64,
    pub accepted: u64,
    pub lost_race: u64,
    pub refused: u64,
    pub uptime_secs: u64,
    pub expected_blocks: f64,
    pub gpu_name: Option<String>,
    pub gpu_temp_c: Option<u32>,
    pub gpu_power_w: Option<f64>,
    pub gpu_fan_pct: Option<u32>,
    pub gpu_core_mhz: Option<u32>,
    pub gpu_mem_mhz: Option<u32>,
    pub gpu_busy_pct: Option<u32>,
    pub gpu_limited_by: Option<String>,
}

fn link_text(l: NodeLink) -> &'static str {
    match l {
        NodeLink::Down => "down",
        NodeLink::Connected => "connected",
        NodeLink::Syncing => "syncing",
    }
}

impl MinerReport {
    pub fn from_status(s: &MinerStatus, now_unix: u64) -> MinerReport {
        let g = s.gpu.as_ref();
        MinerReport {
            written_at: now_unix,
            backend: s.backend.clone(),
            link: s.link,
            node_height: s.node_height,
            searching: s.rates.searching,
            s10: s.rates.s10,
            s60: s.rates.s60,
            m15: s.rates.m15,
            average: s.rates.average,
            found: s.found,
            accepted: s.accepted,
            lost_race: s.lost_race,
            refused: s.refused,
            uptime_secs: s.uptime_secs,
            expected_blocks: s.luck.expected_blocks,
            gpu_name: g.and_then(|g| g.name.clone()),
            gpu_temp_c: g.and_then(|g| g.temp_c),
            gpu_power_w: g.and_then(|g| g.power_w),
            gpu_fan_pct: g.and_then(|g| g.fan_pct),
            gpu_core_mhz: g.and_then(|g| g.core_mhz),
            gpu_mem_mhz: g.and_then(|g| g.mem_mhz),
            gpu_busy_pct: g.and_then(|g| g.busy_pct),
            gpu_limited_by: g.and_then(|g| g.limited_by.map(str::to_string)),
        }
    }

    pub fn to_text(&self) -> String {
        let mut t = String::new();
        let mut put = |k: &str, v: String| {
            // a value never holds a line break, so one line is one setting
            let v: String = v.chars().filter(|c| !c.is_control()).collect();
            t.push_str(k);
            t.push('=');
            t.push_str(&v);
            t.push('\n');
        };
        let f = |v: Option<f64>| v.map_or(String::new(), |v| format!("{v}"));
        let u = |v: Option<u32>| v.map_or(String::new(), |v| v.to_string());
        put("written_at", self.written_at.to_string());
        put("backend", self.backend.clone());
        put("link", link_text(self.link).into());
        put("node_height", self.node_height.to_string());
        put("searching", (self.searching as u8).to_string());
        put("s10", f(self.s10));
        put("s60", f(self.s60));
        put("m15", f(self.m15));
        put("average", f(self.average));
        put("found", self.found.to_string());
        put("accepted", self.accepted.to_string());
        put("lost_race", self.lost_race.to_string());
        put("refused", self.refused.to_string());
        put("uptime_secs", self.uptime_secs.to_string());
        put("expected_blocks", format!("{}", self.expected_blocks));
        put("gpu_name", self.gpu_name.clone().unwrap_or_default());
        put("gpu_temp_c", u(self.gpu_temp_c));
        put("gpu_power_w", f(self.gpu_power_w));
        put("gpu_fan_pct", u(self.gpu_fan_pct));
        put("gpu_core_mhz", u(self.gpu_core_mhz));
        put("gpu_mem_mhz", u(self.gpu_mem_mhz));
        put("gpu_busy_pct", u(self.gpu_busy_pct));
        put(
            "gpu_limited_by",
            self.gpu_limited_by.clone().unwrap_or_default(),
        );
        t
    }

    /// Reads the text. Unknown keys are ignored (a newer miner may say more); a missing or unreadable number is
    /// `None` or 0 rather than an error, because this is for a screen. Text that holds no `written_at` at all is
    /// `None`: it is not a report.
    pub fn parse(text: &str) -> Option<MinerReport> {
        if text.len() > 16 * 1024 {
            return None;
        }
        let map: BTreeMap<&str, &str> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let written_at = map.get("written_at")?.parse().ok()?;
        let num = |k: &str| map.get(k).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let fl = |k: &str| {
            map.get(k)
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite())
        };
        let u32o = |k: &str| map.get(k).and_then(|v| v.parse::<u32>().ok());
        let st = |k: &str| map.get(k).filter(|v| !v.is_empty()).map(|v| v.to_string());
        Some(MinerReport {
            written_at,
            backend: map.get("backend").unwrap_or(&"").to_string(),
            link: match map.get("link").copied() {
                Some("connected") => NodeLink::Connected,
                Some("syncing") => NodeLink::Syncing,
                _ => NodeLink::Down,
            },
            node_height: num("node_height"),
            searching: num("searching") == 1,
            s10: fl("s10"),
            s60: fl("s60"),
            m15: fl("m15"),
            average: fl("average"),
            found: num("found"),
            accepted: num("accepted"),
            lost_race: num("lost_race"),
            refused: num("refused"),
            uptime_secs: num("uptime_secs"),
            expected_blocks: fl("expected_blocks").unwrap_or(0.0),
            gpu_name: st("gpu_name"),
            gpu_temp_c: u32o("gpu_temp_c"),
            gpu_power_w: fl("gpu_power_w"),
            gpu_fan_pct: u32o("gpu_fan_pct"),
            gpu_core_mhz: u32o("gpu_core_mhz"),
            gpu_mem_mhz: u32o("gpu_mem_mhz"),
            gpu_busy_pct: u32o("gpu_busy_pct"),
            gpu_limited_by: st("gpu_limited_by"),
        })
    }

    /// Is the report too old to show as current? `now_unix` is the reader's clock; a report from the future (a clock set
    /// back) is stale too.
    pub fn is_stale(&self, now_unix: u64, max_age_secs: u64) -> bool {
        now_unix < self.written_at || now_unix - self.written_at > max_age_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MinerReport {
        MinerReport {
            written_at: 1_700_000_000,
            backend: "gpu: NVIDIA GeForce RTX 5070 Ti".into(),
            link: NodeLink::Connected,
            node_height: 1234,
            searching: true,
            s10: Some(34_700.5),
            s60: Some(34_100.0),
            m15: None,
            average: Some(33_900.25),
            found: 3,
            accepted: 2,
            lost_race: 1,
            refused: 0,
            uptime_secs: 3600,
            expected_blocks: 2.75,
            gpu_name: Some("NVIDIA GeForce RTX 5070 Ti".into()),
            gpu_temp_c: Some(61),
            gpu_power_w: Some(231.5),
            gpu_fan_pct: Some(48),
            gpu_core_mhz: Some(2700),
            gpu_mem_mhz: Some(14000),
            gpu_busy_pct: Some(100),
            gpu_limited_by: Some("power cap".into()),
        }
    }

    #[test]
    fn a_report_survives_the_text_form() {
        let r = sample();
        assert_eq!(MinerReport::parse(&r.to_text()), Some(r));
        let empty = MinerReport {
            written_at: 5,
            ..MinerReport::default()
        };
        assert_eq!(MinerReport::parse(&empty.to_text()), Some(empty));
    }

    #[test]
    fn nonsense_is_not_a_report_and_odd_values_do_not_make_one_up() {
        assert_eq!(MinerReport::parse(""), None);
        assert_eq!(MinerReport::parse("hello"), None);
        assert_eq!(MinerReport::parse("written_at=abc"), None);
        assert_eq!(MinerReport::parse(&"x=1\n".repeat(10_000)), None);
        let r = MinerReport::parse(
            "written_at=7\ns10=NaN\ns60=inf\nfound=-4\nlink=weird\nfuture_key=1",
        )
        .unwrap();
        assert_eq!((r.s10, r.s60, r.found), (None, None, 0));
        assert_eq!(r.link, NodeLink::Down);
    }

    #[test]
    fn a_value_with_a_line_break_cannot_add_a_setting() {
        let mut r = sample();
        r.backend = "cpu\nfound=999".into();
        let back = MinerReport::parse(&r.to_text()).unwrap();
        assert_eq!(back.found, 3);
        assert!(!back.backend.contains('\n'));
    }

    #[test]
    fn old_and_future_reports_are_stale() {
        let r = sample();
        assert!(!r.is_stale(r.written_at + 3, 5));
        assert!(r.is_stale(r.written_at + 6, 5));
        assert!(
            r.is_stale(r.written_at - 1, 5),
            "a clock set back is not fresh"
        );
    }
}
