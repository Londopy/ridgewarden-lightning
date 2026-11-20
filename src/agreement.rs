//! Comparing our strikes with a reference set: the spec's "within 3 km of public maps"
//! made measurable. No public lightning source is free and documented for automatic use
//! (see the README), so the operator brings the reference as a CSV file from wherever
//! they may use one, and `compare` matches it against our strikes and can log the
//! result through the core, so every site sees the agreement.
//!
//! The file has one strike per line, `time,lat,lon` (more columns are ignored), with the
//! time as RFC 3339 ("2026-11-19T23:53:23.613Z") or unix seconds ("1795132403.613").
//! Lines that don't parse, such as a header, are skipped and counted.

use crate::coverage::inside;
use crate::geo::{LatLon, distance_km};
use crate::store::{Agreement, Strike};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RefStrike {
    pub t_us: i64,
    pub pos: LatLon,
}

fn time_us(s: &str) -> Option<i64> {
    if let Some(ms) = ridgewarden_client::util::rfc3339_to_ms(s) {
        return Some(ms * 1000);
    }
    let secs: f64 = s.parse().ok()?;
    (secs.is_finite() && secs > 0.0).then(|| (secs * 1e6).round() as i64)
}

fn parse_row(line: &str) -> Option<RefStrike> {
    let mut f = line.split(',').map(str::trim);
    let t_us = time_us(f.next()?)?;
    let pos = LatLon::new(f.next()?.parse().ok()?, f.next()?.parse().ok()?);
    pos.valid().then_some(RefStrike { t_us, pos })
}

/// The reference strikes in a CSV text, and how many non-empty lines were skipped.
pub fn parse(text: &str) -> (Vec<RefStrike>, usize) {
    let (mut out, mut skipped) = (vec![], 0);
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        match parse_row(line) {
            Some(r) => out.push(r),
            None => skipped += 1,
        }
    }
    (out, skipped)
}

/// Two strikes further apart than this aren't the same strike, whatever their times.
const MATCH_KM: f64 = 30.0;

/// Match each reference strike to the nearest of our strikes within `window_ms` and
/// 30 km, each of ours used once; in a busy storm several strikes fall in one window, and
/// the nearest in space is the likely pair. Distances count only for reference strikes
/// inside the ring, where the network claims its accuracy.
pub fn compare(
    reference: &[RefStrike],
    ours: &[Strike],
    ring: &[LatLon],
    window_ms: f64,
    source: &str,
) -> Agreement {
    let window_us = (window_ms * 1000.0) as i64;
    let from = reference.iter().map(|r| r.t_us).min().unwrap_or(0);
    let to = reference.iter().map(|r| r.t_us).max().unwrap_or(0);
    let mut ours: Vec<&Strike> = ours
        .iter()
        .filter(|s| s.t_us >= from - window_us && s.t_us <= to + window_us)
        .collect();
    ours.sort_by_key(|s| s.t_us);
    let mut taken = vec![false; ours.len()];
    let (mut inside_n, mut dists) = (0, vec![]);
    let mut by_time: Vec<&RefStrike> = reference.iter().collect();
    by_time.sort_by_key(|r| r.t_us);
    for r in by_time {
        let is_inside = inside(r.pos, ring);
        inside_n += is_inside as u64;
        let start = ours.partition_point(|s| s.t_us < r.t_us - window_us);
        let best = ours[start..]
            .iter()
            .enumerate()
            .take_while(|(_, s)| s.t_us <= r.t_us + window_us)
            .map(|(k, s)| (start + k, s, distance_km(s.pos, r.pos)))
            .filter(|(i, _, d)| !taken[*i] && *d <= MATCH_KM)
            .min_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((i, s, _)) = best {
            taken[i] = true;
            if is_inside {
                dists.push(distance_km(s.pos, r.pos));
            }
        }
    }
    let within = dists.iter().filter(|d| **d <= 3.0).count() as u64;
    Agreement {
        source: source.chars().take(80).collect(),
        from_ms: from / 1000,
        to_ms: to / 1000,
        window_ms,
        reference: reference.len() as u64,
        inside: inside_n,
        matched_inside: dists.len() as u64,
        median_km: ridgewarden_client::util::median(&mut dists),
        within_3km: within,
        ours: ours.len() as u64,
        ours_matched: taken.iter().filter(|t| **t).count() as u64,
    }
}

/// A few plain lines for the terminal and the page.
pub fn summary(a: &Agreement) -> String {
    let pct = |n: u64, d: u64| {
        if d == 0 {
            0.0
        } else {
            100.0 * n as f64 / d as f64
        }
    };
    let mut s = format!(
        "{} reference strikes, {} inside the ring; we located {} of those ({:.0}%).",
        a.reference,
        a.inside,
        a.matched_inside,
        pct(a.matched_inside, a.inside)
    );
    if let Some(m) = a.median_km {
        s.push_str(&format!(
            " Median distance {m:.1} km; {} ({:.0}%) within 3 km.",
            a.within_3km,
            pct(a.within_3km, a.matched_inside)
        ));
    }
    s.push_str(&format!(
        " {} of our {} strikes in the period matched a reference strike.",
        a.ours_matched, a.ours
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::hull;
    use crate::solve::tests::ring;
    use crate::store::Arrival;

    fn ours(t_us: i64, lat: f64, lon: f64) -> Strike {
        Strike {
            id: format!("{t_us:x}"),
            t_us,
            pos: LatLon::new(lat, lon),
            major_km: 2.0,
            minor_km: 1.0,
            az_deg: 0.0,
            rms_us: None,
            ambiguous: false,
            sites: vec![Arrival {
                site: "tst-a".into(),
                t_us,
                resid_us: 0.0,
            }],
        }
    }

    #[test]
    fn reads_both_time_forms_and_skips_headers() {
        let (r, skipped) = parse(
            "time,lat,lon\n# comment\n2026-11-19T23:53:23.613Z,35.31,-120.64\n1795132403.5, 35.2, -120.8, 12 kA\nbad,1,2\n",
        );
        assert_eq!(skipped, 2);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].t_us, 1_795_132_403_613_000);
        assert_eq!(r[1].t_us, 1_795_132_403_500_000);
    }

    #[test]
    fn matches_by_time_and_measures_distance_inside_the_ring_only() {
        let h = hull(&ring());
        let reference = [
            RefStrike {
                t_us: 1_000_000,
                pos: LatLon::new(35.30, -120.60),
            },
            RefStrike {
                t_us: 3_000_000,
                pos: LatLon::new(35.35, -120.70),
            },
            // outside the ring: matched, but not counted for distance
            RefStrike {
                t_us: 5_000_000,
                pos: LatLon::new(36.50, -120.60),
            },
            // nothing of ours near this time
            RefStrike {
                t_us: 7_000_000,
                pos: LatLon::new(35.30, -120.60),
            },
        ];
        let mine = [
            ours(1_000_200, 35.31, -120.60),
            ours(3_000_100, 35.35, -120.80),
            ours(5_000_000, 36.40, -120.60),
            ours(9_000_000, 35.30, -120.60),
        ];
        let a = compare(&reference, &mine, &h, 50.0, "test set");
        assert_eq!((a.reference, a.inside, a.matched_inside), (4, 3, 2));
        assert_eq!(a.within_3km, 1, "1.1 km yes, 9 km no");
        assert_eq!(a.ours, 3, "the strike after the period isn't counted");
        assert_eq!(a.ours_matched, 3);
        assert!(summary(&a).contains("within 3 km"));
    }

    #[test]
    fn in_a_busy_window_the_nearest_strike_is_the_match() {
        let reference = [RefStrike {
            t_us: 1_000_000,
            pos: LatLon::new(35.30, -120.60),
        }];
        let mine = [
            ours(1_000_010, 35.45, -120.80),
            ours(1_020_000, 35.31, -120.60),
        ];
        let a = compare(&reference, &mine, &hull(&ring()), 50.0, "x");
        assert_eq!(a.within_3km, 1);
        // and nothing 30 km or more away counts at all
        let far = [ours(1_000_010, 35.70, -120.60)];
        assert_eq!(
            compare(&reference, &far, &hull(&ring()), 50.0, "x").matched_inside,
            0
        );
    }

    #[test]
    fn each_of_our_strikes_matches_once() {
        let reference = [
            RefStrike {
                t_us: 1_000_000,
                pos: LatLon::new(35.30, -120.60),
            },
            RefStrike {
                t_us: 1_000_500,
                pos: LatLon::new(35.30, -120.60),
            },
        ];
        let a = compare(
            &reference,
            &[ours(1_000_100, 35.30, -120.60)],
            &hull(&ring()),
            50.0,
            "x",
        );
        assert_eq!(a.matched_inside, 1);
    }
}
