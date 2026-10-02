//! What the operator sees of the network health (M9, threat model C1): the status line, the age format, and the alarm log that says a
//! change once. The engine's alarms themselves are tested in `tenero-net/tests/network_health.rs`.

use std::collections::BTreeSet;

use tenero_app::daemon::{alarm_changes, format_age, health_summary};
use tenero_net::engine::{Alarm, NetHealth};

#[test]
fn ages_are_short_and_do_not_round_up() {
    assert_eq!(format_age(0), "0s");
    assert_eq!(format_age(999), "0s");
    assert_eq!(format_age(42_000), "42s");
    assert_eq!(format_age(119_999), "119s");
    assert_eq!(format_age(120_000), "2m");
    assert_eq!(format_age(7_199_999), "119m");
    assert_eq!(format_age(7_200_000), "2h00m");
    assert_eq!(format_age(2 * 3_600_000 + 5 * 60_000), "2h05m");
    assert_eq!(format_age(172_799_000), "47h59m");
    assert_eq!(format_age(172_800_000), "2d00h");
    assert_eq!(format_age(3 * 86_400_000 + 4 * 3_600_000), "3d04h");
}

#[test]
fn an_alarm_is_logged_when_it_begins_and_when_it_ends_and_not_while_it_lasts() {
    let mut known = BTreeSet::new();
    let behind = |ms| Alarm::Behind { for_ms: ms };
    let first = alarm_changes(&mut known, &[behind(300_000)]);
    assert_eq!(first.len(), 1);
    assert!(first[0].0 && first[0].1.contains("more work"), "{first:?}");
    // the numbers change, the situation does not: silence
    assert!(alarm_changes(&mut known, &[behind(360_000)]).is_empty());
    assert!(alarm_changes(&mut known, &[behind(3_600_000)]).is_empty());
    // a second kind begins
    let two = alarm_changes(
        &mut known,
        &[behind(3_700_000), Alarm::FewOutbound { count: 1 }],
    );
    assert_eq!(two.len(), 1);
    assert!(two[0].0 && two[0].1.contains("outbound"));
    // one ends, one stays
    let end = alarm_changes(&mut known, &[Alarm::FewOutbound { count: 1 }]);
    assert_eq!(end, vec![(false, "behind-peers".to_string())]);
    // both end; and one that comes back is a new episode
    assert_eq!(alarm_changes(&mut known, &[]).len(), 1);
    assert!(known.is_empty());
    assert_eq!(alarm_changes(&mut known, &[behind(1)]).len(), 1);
}

fn health(alarms: Vec<Alarm>) -> NetHealth {
    NetHealth {
        peers: 5,
        inbound: 2,
        outbound: 4,
        outbound_groups: 3,
        tip_age_ms: 95_000,
        peers_ahead: 0,
        behind_for_ms: 0,
        samples: 4,
        samples_ahead: 0,
        bootstrapping: false,
        alarms,
    }
}

#[test]
fn the_status_line_shows_groups_block_age_samples_and_the_alarms() {
    assert_eq!(
        health_summary(&health(vec![])),
        "out groups 3 | last block 95s ago | samples 4 | alarms none"
    );
    assert_eq!(
        health_summary(&health(vec![
            Alarm::StaleTip { age_ms: 700_000 },
            Alarm::FewGroups { groups: 1 }
        ])),
        "out groups 3 | last block 95s ago | samples 4 | alarms stale-tip,few-groups"
    );
}

#[test]
fn every_alarm_says_what_it_is_in_words() {
    let all = [
        Alarm::StaleTip { age_ms: 600_000 },
        Alarm::Behind { for_ms: 300_000 },
        Alarm::SamplesAhead { count: 2 },
        Alarm::FewOutbound { count: 1 },
        Alarm::FewGroups { groups: 1 },
    ];
    let kinds: BTreeSet<&str> = all.iter().map(|a| a.kind()).collect();
    assert_eq!(kinds.len(), 5, "two alarms share a name");
    for a in &all {
        let d = a.describe();
        assert!(d.len() > 30, "{d}");
        // they say what they are not sure of
        assert!(
            d.contains("may")
                || d.contains("possible")
                || d.contains("seems")
                || d.contains("could")
                || d.contains("not choosing"),
            "{d}"
        );
    }
    assert!(Alarm::StaleTip { age_ms: 600_000 }
        .describe()
        .contains("10 minutes"));
    assert!(Alarm::Behind { for_ms: 420_000 }
        .describe()
        .contains("7 minutes"));
}
