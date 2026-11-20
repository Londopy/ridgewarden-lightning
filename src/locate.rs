//! The solver: groups arrivals from different sites that can belong to one strike, solves
//! each group, and logs the strikes through the core so every site gets them.
//!
//! Arrivals are taken in time order. The earliest unused one anchors a group; from each
//! other site it may take one arrival no later than the light-time between the two
//! receivers (plus timing slack), since no strike can make the gap between two receivers
//! bigger than the distance between them. Every combination of candidates (and of
//! leaving a site out) that passes that check for every pair is solved; the group with
//! the most sites that passes the solution's checks wins, ties going to the lowest
//! chi-square. Any three arrivals fit exactly, so a three-site group is only taken when
//! no four arrivals line up and it is the only three-site group that fits: otherwise
//! there is no telling which three (if any) were one strike, and guessing puts strikes
//! in the wrong place.
//!
//! A moment is solved once, `settle_s` after it happened, so that every site's reports
//! have had time to sync. Arrivals that turn up later than that are not used.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::app::App;
use crate::config::SolverCfg;
use crate::geo::{LatLon, distance_km};
use crate::solve::{Arrival, Fix, solve};
use crate::store::{self, MAX_STRIKES, Strike};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS solver_state (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    through_us INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS solved (
    site TEXT NOT NULL,
    t_us INTEGER NOT NULL,
    PRIMARY KEY (site, t_us)
) WITHOUT ROWID;";

/// Arrivals already part of a strike, by site and time.
pub type Used = HashSet<(String, i64)>;

#[derive(Clone, Debug, PartialEq)]
pub struct Det {
    pub site: String,
    pub t_us: i64,
    pub pos: LatLon,
    pub sigma_us: f64,
}

pub struct Params {
    pub v_km_s: f64,
    pub min_sites: usize,
    pub max_range_km: f64,
    pub max_axis_km: f64,
    pub max_chi2: f64,
}

impl Params {
    pub fn from_cfg(s: &SolverCfg) -> Self {
        Self {
            v_km_s: s.velocity_factor * crate::solve::C_KM_S,
            min_sites: s.min_sites,
            max_range_km: s.max_range_km,
            max_axis_km: s.max_axis_km,
            max_chi2: s.max_chi2,
        }
    }

    /// Largest possible gap between arrivals at `a` and `b`, in microseconds.
    fn max_gap_us(&self, a: &Det, b: &Det) -> f64 {
        distance_km(a.pos, b.pos) / self.v_km_s * 1e6 + 4.0 * a.sigma_us.hypot(b.sigma_us) + 1.0
    }

    fn accept(&self, fix: &Fix, n: usize) -> bool {
        fix.ellipse.major_km <= self.max_axis_km
            && (n == 3 || fix.chi2 / (n - 3) as f64 <= self.max_chi2)
    }
}

pub struct Located {
    pub fix: Fix,
    pub members: Vec<Det>,
}

/// Most combinations tried for one anchor; beyond that each site offers only its
/// earliest candidate.
const MAX_COMBOS: usize = 1024;
/// Most other sites taken into one group; keeps the combinations at 2^10 at worst.
const MAX_SITES: usize = 10;
const MAX_PER_SITE: usize = 3;

/// The other sites' candidates for a group anchored at `dets[a]`, by site.
fn candidates(dets: &[Det], a: usize, used: &Used, p: &Params, span_us: f64) -> Vec<Vec<usize>> {
    let anchor = &dets[a];
    let mut by_site: Vec<(String, Vec<usize>)> = vec![];
    for (j, d) in dets.iter().enumerate().skip(a + 1) {
        if (d.t_us - anchor.t_us) as f64 > span_us {
            break;
        }
        if d.site == anchor.site
            || used.contains(&(d.site.clone(), d.t_us))
            || (d.t_us - anchor.t_us) as f64 > p.max_gap_us(anchor, d)
        {
            continue;
        }
        let full = by_site.len() >= MAX_SITES;
        match by_site.iter_mut().find(|(s, _)| *s == d.site) {
            Some((_, v)) if v.len() < MAX_PER_SITE => v.push(j),
            None if !full => by_site.push((d.site.clone(), vec![j])),
            _ => {}
        }
    }
    by_site.into_iter().map(|(_, v)| v).collect()
}

/// Every way of taking at most one candidate per site, as lists of indices, with
/// enough sites to solve, the biggest groups first.
fn combos(mut sites: Vec<Vec<usize>>, min_others: usize) -> Vec<Vec<usize>> {
    let total: usize = sites.iter().map(|c| c.len() + 1).product();
    if total > MAX_COMBOS {
        for c in &mut sites {
            c.truncate(1);
        }
    }
    let mut out: Vec<Vec<usize>> = vec![vec![]];
    for c in &sites {
        let mut next = Vec::with_capacity(out.len() * (c.len() + 1));
        for partial in &out {
            next.push(partial.clone());
            for j in c {
                let mut p = partial.clone();
                p.push(*j);
                next.push(p);
            }
        }
        out = next;
    }
    out.retain(|c| c.len() >= min_others);
    out.sort_by_key(|c| std::cmp::Reverse(c.len()));
    out
}

fn consistent(group: &[&Det], p: &Params) -> bool {
    group.iter().enumerate().all(|(i, a)| {
        group[i + 1..]
            .iter()
            .all(|b| ((a.t_us - b.t_us).abs() as f64) <= p.max_gap_us(a, b))
    })
}

fn fit(group: &[&Det], p: &Params) -> Option<Fix> {
    let t_ref = group[0].t_us;
    let arr: Vec<Arrival> = group
        .iter()
        .map(|d| Arrival {
            site: d.pos,
            t: (d.t_us - t_ref) as f64 * 1e-6,
            sigma: d.sigma_us * 1e-6,
        })
        .collect();
    solve(&arr, p.v_km_s, p.max_range_km).filter(|f| p.accept(f, group.len()))
}

/// Group and solve the arrivals anchored in `(from_us, until_us]`. `dets` must be sorted
/// by time and reach at least the longest light-time past `until_us`. Members of each
/// strike found are added to `used`. Stops early at `deadline`; returns the strikes and
/// the time up to which every anchor was dealt with.
pub fn locate(
    dets: &[Det],
    from_us: i64,
    until_us: i64,
    used: &mut Used,
    p: &Params,
    deadline: Option<Instant>,
) -> (Vec<Located>, i64) {
    let span_us = span_us(dets, p);
    let mut out = vec![];
    for a in 0..dets.len() {
        let anchor = &dets[a];
        if anchor.t_us <= from_us || used.contains(&(anchor.site.clone(), anchor.t_us)) {
            continue;
        }
        if anchor.t_us > until_us {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return (out, anchor.t_us - 1);
        }
        let cands = candidates(dets, a, used, p, span_us);
        let mut best: Option<(Fix, Vec<usize>)> = None;
        let mut biggest = 0;
        let mut fits = 0;
        // arrivals that lined up with this anchor, in case the group is refused
        let mut seen: Vec<usize> = vec![];
        for c in combos(cands, p.min_sites - 1) {
            if best.as_ref().is_some_and(|(_, b)| b.len() > c.len()) {
                break;
            }
            let group: Vec<&Det> = std::iter::once(anchor)
                .chain(c.iter().map(|j| &dets[*j]))
                .collect();
            // any three arrivals fit exactly, so once four or more line up but don't fit
            // one strike, a three-site subset of them proves nothing
            if group.len() == 3 && biggest >= 4 {
                break;
            }
            if !consistent(&group, p) {
                continue;
            }
            biggest = biggest.max(group.len());
            seen.extend(&c);
            let Some(fix) = fit(&group, p) else {
                continue;
            };
            fits += 1;
            if best.as_ref().is_none_or(|(b, _)| fix.chi2 < b.chi2) {
                best = Some((fix, c));
            }
        }
        // and two different three-site groups that both fit can't be told apart. Only
        // three-site solving needs the arrivals set aside, so a later anchor can't pick a
        // three-site guess out of them; with four or more required they stay free, in
        // case this anchor was a stray just ahead of a real strike.
        let refused = p.min_sites <= 3
            && biggest >= 3
            && (best.is_none() || best.as_ref().is_some_and(|(_, c)| c.len() == 2 && fits > 1));
        if refused {
            for j in std::iter::once(a).chain(seen) {
                used.insert((dets[j].site.clone(), dets[j].t_us));
            }
            continue;
        }
        if let Some((fix, c)) = best {
            let members: Vec<Det> = std::iter::once(a)
                .chain(c)
                .map(|j| dets[j].clone())
                .collect();
            for m in &members {
                used.insert((m.site.clone(), m.t_us));
            }
            out.push(Located { fix, members });
        }
    }
    (out, until_us)
}

/// The longest light-time between any two receivers seen, plus slack, microseconds.
pub fn span_us(dets: &[Det], p: &Params) -> f64 {
    let mut seen = HashSet::new();
    let sites: Vec<&Det> = dets
        .iter()
        .filter(|d| seen.insert((d.site.as_str(), d.pos.lat.to_bits(), d.pos.lon.to_bits())))
        .collect();
    let mut span: f64 = 0.0;
    for (i, a) in sites.iter().enumerate() {
        for b in &sites[i + 1..] {
            span = span.max(p.max_gap_us(a, b));
        }
    }
    span
}

/// A stable id: the same arrivals give the same strike, whichever site solved it.
pub fn strike_id(members: &[Det]) -> String {
    let mut keys: Vec<String> = members
        .iter()
        .map(|m| format!("{}:{}", m.site, m.t_us))
        .collect();
    keys.sort();
    blake3::hash(keys.join(";").as_bytes()).to_hex()[..16].to_string()
}

pub fn to_strike(l: &Located) -> Strike {
    let t_ref = l.members[0].t_us;
    let n = l.members.len();
    let rms_us = (n > 3)
        .then(|| (l.fix.resid.iter().map(|r| (r * 1e6).powi(2)).sum::<f64>() / n as f64).sqrt());
    let round = |x: f64, k: f64| (x * k).round() / k;
    Strike {
        id: strike_id(&l.members),
        t_us: t_ref + (l.fix.t0 * 1e6).round() as i64,
        pos: LatLon::new(round(l.fix.pos.lat, 1e6), round(l.fix.pos.lon, 1e6)),
        major_km: round(l.fix.ellipse.major_km, 1000.0),
        minor_km: round(l.fix.ellipse.minor_km, 1000.0),
        az_deg: round(l.fix.ellipse.az_deg, 10.0),
        rms_us: rms_us.map(|r| round(r, 100.0)),
        ambiguous: l.fix.ambiguous,
        sites: l
            .members
            .iter()
            .zip(&l.fix.resid)
            .map(|(m, r)| store::Arrival {
                site: m.site.clone(),
                t_us: m.t_us,
                resid_us: r * 1e6,
            })
            .collect(),
    }
}

fn load(conn: &Connection, from_us: i64, to_us: i64) -> Result<(Vec<Det>, Used)> {
    let mut st = conn.prepare(
        "SELECT site, t_us, lat, lon, sigma_us FROM togas WHERE t_us > ?1 AND t_us <= ?2 ORDER BY t_us, site",
    )?;
    let dets = st
        .query_map(params![from_us, to_us], |r| {
            Ok(Det {
                site: r.get(0)?,
                t_us: r.get(1)?,
                pos: LatLon::new(r.get(2)?, r.get(3)?),
                sigma_us: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut st = conn.prepare("SELECT site, t_us FROM solved WHERE t_us > ?1 AND t_us <= ?2")?;
    let used = st
        .query_map(params![from_us, to_us], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok((dets, used))
}

/// One solver round: solve everything that has settled and log the strikes. Returns
/// how many were found.
const HOUR_US: i64 = 3_600_000_000;
/// Longest one round may spend solving; the rest waits for the next round.
const ROUND_BUDGET: Duration = Duration::from_secs(5);

/// Where a round starts: the cursor, but never more than six hours back. On a first run,
/// or when the cursor is ahead of `upto` because the clock was stepped back, ten minutes
/// back; arrivals already used are in `solved`, so nothing is logged twice.
pub fn window_start(through: Option<i64>, upto: i64) -> i64 {
    match through {
        Some(t) if t <= upto => t.max(upto - 6 * HOUR_US),
        _ => upto - 600_000_000,
    }
}

pub async fn round(app: &App) -> Result<usize> {
    let p = Params::from_cfg(&app.cfg.solver);
    let upto = (ridgewarden_client::util::now_ms() - app.cfg.solver.settle_s as i64 * 1000) * 1000;
    let (from, to, dets, mut used) = {
        let db = app.db();
        let through: Option<i64> = db
            .query_row(
                "SELECT through_us FROM solver_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let from = window_start(through, upto);
        // at most ten minutes of arrivals per round, so a storm can't fill memory
        let to = upto.min(from + 600_000_000);
        let (dets, used) = load(&db, from - 10_000, to)?;
        (from, to, dets, used)
    };
    let until = to - span_us(&dets, &p).ceil() as i64;
    if until <= from {
        return Ok(0);
    }
    // solving can take a while in a busy storm; keep it off the runtime and the database
    let (found, reached, used) = tokio::task::spawn_blocking(move || {
        let (found, reached) = locate(
            &dets,
            from,
            until,
            &mut used,
            &p,
            Some(Instant::now() + ROUND_BUDGET),
        );
        (found, reached, used)
    })
    .await?;
    if reached < until {
        tracing::info!("solver is behind; carrying on from {} next round", reached);
    }
    let strikes: Vec<Strike> = found.iter().map(to_strike).collect();
    for chunk in strikes.chunks(MAX_STRIKES) {
        app.core
            .append(
                "lightning.strikes",
                json!({
                    "velocity_factor": app.cfg.solver.velocity_factor,
                    "strikes": chunk.iter().map(store::strike_json).collect::<Vec<_>>(),
                }),
            )
            .await?;
    }
    let db = app.db();
    let tx = db.unchecked_transaction()?;
    // strike members and arrivals of refused groups alike
    for (site, t_us) in used.iter().filter(|u| u.1 > from) {
        tx.execute(
            "INSERT OR IGNORE INTO solved (site, t_us) VALUES (?1, ?2)",
            params![site, t_us],
        )?;
    }
    tx.execute(
        "INSERT INTO solver_state (id, through_us) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET through_us = excluded.through_us",
        [reached],
    )?;
    tx.execute(
        "DELETE FROM solved WHERE t_us < ?1",
        [from - 86_400_000_000],
    )?;
    tx.commit()?;
    Ok(strikes.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::Rng;
    use crate::solve::C_KM_S;
    use crate::solve::tests::ring;

    fn params() -> Params {
        Params {
            v_km_s: 0.9922 * C_KM_S,
            min_sites: 3,
            max_range_km: 300.0,
            max_axis_km: 30.0,
            max_chi2: 9.0,
        }
    }

    const NAMES: [&str; 4] = ["tst-a", "tst-b", "tst-c", "tst-d"];

    fn strike_dets(strike: LatLon, t0_us: i64, sigma_us: f64, rng: &mut Rng) -> Vec<Det> {
        let v = params().v_km_s;
        ring()
            .iter()
            .zip(NAMES)
            .map(|(pos, name)| Det {
                site: name.into(),
                t_us: t0_us
                    + (distance_km(strike, *pos) / v * 1e6 + sigma_us * rng.gauss()).round() as i64,
                pos: *pos,
                sigma_us: sigma_us.max(1.0),
            })
            .collect()
    }

    fn sorted(mut d: Vec<Det>) -> Vec<Det> {
        d.sort_by_key(|x| x.t_us);
        d
    }

    #[test]
    fn finds_two_close_strikes_and_ignores_stray_triggers() {
        let mut rng = Rng::new(5);
        let t = 1_795_000_000_000_000;
        let s1 = LatLon::new(35.30, -120.60);
        let s2 = LatLon::new(35.45, -120.80);
        let mut dets = strike_dets(s1, t, 3.0, &mut rng);
        // a second strike 400 us later: its arrivals interleave with the first's
        dets.extend(strike_dets(s2, t + 400, 3.0, &mut rng));
        // stray triggers at single sites
        for (i, name) in NAMES.iter().enumerate().take(2) {
            dets.push(Det {
                site: name.to_string(),
                t_us: t + 150 + 37 * i as i64,
                pos: ring()[i],
                sigma_us: 3.0,
            });
        }
        let dets = sorted(dets);
        let mut used = HashSet::new();
        let got = locate(&dets, 0, i64::MAX, &mut used, &params(), None).0;
        assert_eq!(
            got.len(),
            2,
            "{:?}",
            got.iter().map(|l| l.fix.pos).collect::<Vec<_>>()
        );
        let near = |s: LatLon| {
            got.iter()
                .any(|l| distance_km(l.fix.pos, s) < 2.0 && l.members.len() == 4)
        };
        assert!(near(s1) && near(s2));
        assert_eq!(used.len(), 8);
    }

    #[test]
    fn a_missing_site_still_gives_a_three_site_fix() {
        let mut rng = Rng::new(9);
        let s = LatLon::new(35.35, -120.62);
        let mut dets = strike_dets(s, 1_000_000, 2.0, &mut rng);
        dets.remove(1);
        let got = locate(
            &sorted(dets),
            0,
            i64::MAX,
            &mut HashSet::new(),
            &params(),
            None,
        )
        .0;
        assert_eq!(got.len(), 1);
        let st = to_strike(&got[0]);
        assert_eq!(st.sites.len(), 3);
        assert_eq!(st.rms_us, None, "exactly determined: no residual to show");
        assert!(distance_km(st.pos, s) < 2.0);
        assert!((st.t_us - 1_000_000).abs() < 20);
    }

    #[test]
    fn arrivals_too_far_apart_for_the_geometry_are_not_grouped() {
        let p = params();
        let dets: Vec<Det> = ring()
            .iter()
            .zip(NAMES)
            .enumerate()
            .map(|(i, (pos, name))| Det {
                site: name.into(),
                // 2 ms apart: further than light crosses this network
                t_us: 2000 * i as i64,
                pos: *pos,
                sigma_us: 5.0,
            })
            .collect();
        assert!(
            locate(&dets, -1, i64::MAX, &mut HashSet::new(), &p, None)
                .0
                .is_empty()
        );
    }

    #[test]
    fn four_arrivals_that_dont_fit_dont_fall_back_to_a_three_site_guess() {
        let mut rng = Rng::new(6);
        let mut dets = strike_dets(LatLon::new(35.3, -120.6), 0, 3.0, &mut rng);
        dets[2].t_us += 60;
        let got = locate(
            &sorted(dets),
            -1,
            i64::MAX,
            &mut HashSet::new(),
            &params(),
            None,
        )
        .0;
        assert!(
            got.is_empty(),
            "{:?}",
            got.iter().map(|l| l.fix.pos).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_stray_just_ahead_of_a_strike_doesnt_cost_the_strike() {
        let mut rng = Rng::new(8);
        let mut dets = strike_dets(LatLon::new(35.3, -120.6), 10_000, 2.0, &mut rng);
        let first = dets.iter().map(|d| d.t_us).min().unwrap();
        dets.push(Det {
            site: "tst-b".into(),
            t_us: first - 20,
            pos: ring()[1],
            sigma_us: 2.0,
        });
        let p = Params {
            min_sites: 4,
            ..params()
        };
        let got = locate(&sorted(dets), 0, i64::MAX, &mut HashSet::new(), &p, None).0;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].members.len(), 4);
    }

    #[test]
    fn two_three_site_groups_that_both_fit_are_refused() {
        let mut rng = Rng::new(10);
        let mut dets = strike_dets(LatLon::new(35.3, -120.6), 10_000, 2.0, &mut rng);
        // only three sites heard it, and the last to hear it has a second trigger close by
        dets.truncate(3);
        let mut dets = sorted(dets);
        let mut extra = dets[2].clone();
        extra.t_us += 15;
        dets.push(extra);
        let got = locate(
            &sorted(dets),
            0,
            i64::MAX,
            &mut HashSet::new(),
            &params(),
            None,
        )
        .0;
        assert!(got.is_empty());
    }

    #[test]
    fn already_solved_arrivals_are_not_reused() {
        let mut rng = Rng::new(2);
        let dets = sorted(strike_dets(LatLon::new(35.3, -120.6), 5_000, 2.0, &mut rng));
        let mut used = HashSet::new();
        assert_eq!(
            locate(&dets, 0, i64::MAX, &mut used, &params(), None)
                .0
                .len(),
            1
        );
        assert!(
            locate(&dets, 0, i64::MAX, &mut used, &params(), None)
                .0
                .is_empty()
        );
    }

    #[test]
    fn cursor_ahead_of_the_clock_restarts_ten_minutes_back() {
        let upto = 100 * HOUR_US;
        assert_eq!(window_start(Some(upto - 1_000), upto), upto - 1_000);
        assert_eq!(
            window_start(Some(upto + 5 * HOUR_US), upto),
            upto - 600_000_000
        );
        assert_eq!(window_start(None, upto), upto - 600_000_000);
        assert_eq!(
            window_start(Some(upto - 9 * HOUR_US), upto),
            upto - 6 * HOUR_US
        );
    }

    #[test]
    fn a_spent_deadline_stops_before_the_next_anchor() {
        let mut rng = Rng::new(3);
        let dets = sorted(strike_dets(LatLon::new(35.3, -120.6), 5_000, 2.0, &mut rng));
        let first = dets[0].t_us;
        let (found, reached) = locate(
            &dets,
            0,
            i64::MAX,
            &mut HashSet::new(),
            &params(),
            Some(Instant::now()),
        );
        assert!(found.is_empty());
        assert_eq!(
            reached,
            first - 1,
            "the unsolved anchor is redone next round"
        );
    }

    #[test]
    fn ids_are_stable_and_order_free() {
        let mut rng = Rng::new(4);
        let d = strike_dets(LatLon::new(35.3, -120.6), 0, 1.0, &mut rng);
        let mut r = d.clone();
        r.reverse();
        assert_eq!(strike_id(&d), strike_id(&r));
        assert_ne!(strike_id(&d), strike_id(&d[..3]));
    }
}
