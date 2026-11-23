//! The solver: groups arrivals from different sites that can belong to one strike, solves
//! each group, and logs the strikes through the core so every site gets them.
//!
//! Arrivals are taken in time order. The earliest unused one anchors a group; from each
//! other site it may take one arrival no later than the light-time between the two
//! receivers (plus timing slack), since no strike can make the gap between two receivers
//! bigger than the distance between them. Every combination of candidates (and of
//! leaving a site out) that passes that check for every pair is solved; the group with
//! the most sites that passes the solution's checks wins, ties going to a group whose
//! fit could be checked (more equations than unknowns; bearings count) over one that
//! fits exactly, then to the lowest chi-square per degree of freedom. Any three arrival
//! times without a bearing fit exactly, so such a group is only taken when no four
//! arrivals line up and it is the only three-site group that fits: otherwise there is no
//! telling which three (if any) were one strike, and guessing puts strikes in the wrong
//! place. With bearings, a three-site group is taken when it fits clearly better than
//! every other three-site group: bearings pick between groups and shrink the ellipse.
//!
//! They can't reject a strike, though, since a loop turned from where vttoga thinks it
//! points would otherwise lose every strike it hears: a group of four or more whose
//! bearings don't fit is solved again from its arrival times alone, and three arrivals
//! whose bearings don't fit are tried again without them, as a network without loops
//! would, and such a strike is marked unchecked. One strike's chi-square only catches a
//! bearing several errors off (about 35 degrees at 5 degrees each), so a site whose
//! bearings have been off steadily (see `store::turned`) has them left out of every fit.
//! Left out or not, each bearing's residual is kept, so the receiver table shows where
//! the loops point.
//!
//! A moment is solved once, `settle_s` after it happened, so that every site's reports
//! have had time to sync. Arrivals that turn up later than that are not used.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::app::App;
use crate::config::SolverCfg;
use crate::geo::{LatLon, distance_km};
use crate::solve::{Arrival, Bearing, Fix, bearing_resid, solve};
use crate::store::{self, Strike};

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
    pub impulse: Option<f64>,
    pub bearing: Option<Bearing>,
    /// its site's bearings have been off for a while, so fits leave them out
    pub turned: bool,
}

impl Det {
    /// Its bearing, if a fit may use it.
    fn usable(&self) -> Option<Bearing> {
        self.bearing.filter(|_| !self.turned)
    }
}

/// Equations beyond the three unknowns: none means any such group fits exactly.
fn dof(group: &[&Det]) -> usize {
    group.len() + group.iter().filter(|d| d.usable().is_some()).count() - 3
}

/// Which of two fits of the same number of sites to prefer, smallest first: one that was
/// checked over one that couldn't be, then the better fit per degree of freedom.
fn rank(f: &Fix) -> (bool, f64) {
    (f.dof == 0, f.chi2 / f.dof.max(1) as f64)
}

/// How much lower a three-site fit's chi-square must be than every other three-site
/// fit's to be told apart from them: three sigma's worth.
const CLEAR: f64 = 9.0;

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

    fn accept(&self, fix: &Fix) -> bool {
        fix.ellipse.major_km <= self.max_axis_km
            && (fix.dof == 0 || fix.chi2 / fix.dof as f64 <= self.max_chi2)
    }
}

pub struct Located {
    pub fix: Fix,
    pub members: Vec<Det>,
    /// three arrival times that fit only once their bearings were left out
    pub unchecked: bool,
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

/// Solve a group, with the bearings it may use or without any. Without, or when they
/// don't fit with four or more arrival times, the times alone fix the strike. Bearings
/// left out still get their residuals.
fn fit(group: &[&Det], p: &Params, bearings: bool) -> Option<Fix> {
    let t_ref = group[0].t_us;
    let arr: Vec<Arrival> = group
        .iter()
        .map(|d| Arrival {
            site: d.pos,
            t: (d.t_us - t_ref) as f64 * 1e-6,
            sigma: d.sigma_us * 1e-6,
            bearing: d.bearing,
        })
        .collect();
    let solved = |with_bearings: bool| {
        let used: Vec<Arrival> = arr
            .iter()
            .zip(group)
            .map(|(a, d)| Arrival {
                bearing: d.usable().filter(|_| with_bearings),
                ..*a
            })
            .collect();
        let mut fix = solve(&used, p.v_km_s, p.max_range_km).filter(|f| p.accept(f))?;
        for (i, a) in arr.iter().enumerate() {
            if !fix.bearing_used[i] {
                fix.bearing_resid[i] = bearing_resid(a, fix.pos);
            }
        }
        Some(fix)
    };
    if bearings && group.iter().any(|d| d.usable().is_some()) {
        let fix = solved(true);
        if fix.is_some() || group.len() < 4 {
            return fix;
        }
    }
    solved(false)
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
        let groups = combos(candidates(dets, a, used, p, span_us), p.min_sites - 1);
        let mut best: Option<(Fix, Vec<usize>)> = None;
        let mut biggest = 0;
        // chi-square of every group that fit, all of the best one's size
        let mut fitted: Vec<f64> = vec![];
        // arrivals that lined up with this anchor, in case the group is refused
        let mut seen: Vec<usize> = vec![];
        let mut retried = false;
        for bearings in [true, false] {
            // when only three line up and their bearings don't fit, they get another go as
            // a network without loops would give them
            if !bearings {
                let loops = seen.iter().chain([&a]).any(|j| dets[*j].usable().is_some());
                if best.is_some() || biggest != 3 || !loops {
                    break;
                }
                retried = true;
            }
            for c in &groups {
                if best.as_ref().is_some_and(|(_, b)| b.len() > c.len()) {
                    break;
                }
                let group: Vec<&Det> = std::iter::once(anchor)
                    .chain(c.iter().map(|j| &dets[*j]))
                    .collect();
                // three arrival times with no bearing fit exactly, so once four or more
                // line up but don't fit one strike, such a subset of them proves nothing
                if dof(&group) == 0 && biggest >= 4 {
                    continue;
                }
                if !consistent(&group, p) {
                    continue;
                }
                biggest = biggest.max(group.len());
                seen.extend(c);
                let Some(fix) = fit(&group, p, bearings) else {
                    continue;
                };
                fitted.push(fix.chi2);
                if best.as_ref().is_none_or(|(b, _)| rank(&fix) < rank(b)) {
                    best = Some((fix, c.clone()));
                }
            }
        }
        // and two different three-site groups that fit about as well can't be told apart
        // (exact fits all have a chi-square of 0). Only three-site solving needs the
        // arrivals set aside, so a later anchor can't pick a three-site guess out of them;
        // with four or more required they stay free, in case this anchor was a stray just
        // ahead of a real strike.
        let unclear = best.as_ref().is_some_and(|(f, c)| {
            c.len() == 2 && fitted.iter().filter(|x| **x <= f.chi2 + CLEAR).count() > 1
        });
        let refused = p.min_sites <= 3 && biggest >= 3 && (best.is_none() || unclear);
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
            out.push(Located {
                fix,
                members,
                unchecked: retried,
            });
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
    let rms_us = (l.fix.dof > 0)
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
        unchecked: l.unchecked,
        sites: l
            .members
            .iter()
            .enumerate()
            .map(|(i, m)| store::Arrival {
                site: m.site.clone(),
                t_us: m.t_us,
                resid_us: l.fix.resid[i] * 1e6,
                impulse: m.impulse,
                // just under 360 rounds up to it
                bearing_deg: m.bearing.map(|b| round(b.az.to_degrees(), 10.0) % 360.0),
                bearing_resid_deg: l.fix.bearing_resid[i].map(f64::to_degrees),
                bearing_left_out: m.bearing.is_some() && !l.fix.bearing_used[i],
            })
            .collect(),
    }
}

fn load(conn: &Connection, from_us: i64, to_us: i64) -> Result<(Vec<Det>, Used)> {
    let mut st = conn.prepare(
        "SELECT site, t_us, lat, lon, sigma_us, impulse, bearing, bearing_sigma, bearing_mod
         FROM togas WHERE t_us > ?1 AND t_us <= ?2 ORDER BY t_us, site",
    )?;
    let dets = st
        .query_map(params![from_us, to_us], |r| {
            let b: (Option<f64>, Option<f64>, Option<f64>) = (r.get(6)?, r.get(7)?, r.get(8)?);
            Ok(Det {
                site: r.get(0)?,
                t_us: r.get(1)?,
                pos: LatLon::new(r.get(2)?, r.get(3)?),
                sigma_us: r.get(4)?,
                impulse: r.get(5)?,
                bearing: match b {
                    (Some(az), Some(sigma), Some(m)) => Some(Bearing {
                        az: az.to_radians(),
                        sigma: sigma.to_radians(),
                        modulus: m.to_radians(),
                    }),
                    _ => None,
                },
                turned: false,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut st = conn.prepare("SELECT site, t_us FROM solved WHERE t_us > ?1 AND t_us <= ?2")?;
    let used = st
        .query_map(params![from_us, to_us], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok((dets, used))
}

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

/// Record these strikes' arrivals as used, so no later round takes them again.
fn mark_solved(conn: &Connection, found: &[Located]) -> Result<()> {
    for m in found.iter().flat_map(|l| &l.members) {
        conn.execute(
            "INSERT OR IGNORE INTO solved (site, t_us) VALUES (?1, ?2)",
            params![m.site, m.t_us],
        )?;
    }
    Ok(())
}

/// The sites whose bearings the solver leaves out (`store::turned`), kept for a minute at
/// a time, since working them out reads the whole week's strikes.
#[derive(Default)]
pub struct Turned {
    at_ms: Option<i64>,
    sites: BTreeMap<String, f64>,
}

impl Turned {
    /// The turned sites as of `now`: worked out again once the list is a minute old, or
    /// when the clock has jumped.
    pub fn get(&mut self, conn: &Connection, now: i64) -> Result<&BTreeMap<String, f64>> {
        if self.at_ms.is_none_or(|t| (now - t).abs() >= 60_000) {
            self.sites = store::turned(conn, now)?;
            self.at_ms = Some(now);
        }
        Ok(&self.sites)
    }
}

/// One solver round: solve everything that has settled and log the strikes. Returns
/// how many were found.
pub async fn round(app: &App, turned: &mut Turned) -> Result<usize> {
    let p = Params::from_cfg(&app.cfg.solver);
    let upto = (ridgewarden_client::util::now_ms() - app.cfg.solver.settle_s as i64 * 1000) * 1000;
    let (from, to, mut dets, mut used) = {
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
    let off = turned.get(&app.db(), ridgewarden_client::util::now_ms())?;
    for d in &mut dets {
        d.turned = off.contains_key(&d.site);
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
    // an event's strikes count as solved as soon as it's logged, so if a later one fails
    // the next round can't find and log them again
    let mut logged = 0;
    for (n, payload) in store::strike_events(&strikes, app.cfg.solver.velocity_factor) {
        app.core.append("lightning.strikes", payload).await?;
        mark_solved(&app.db(), &found[logged..logged + n])?;
        logged += n;
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
    use crate::solve::tests::{ellipse_units, ring};
    use crate::solve::{C_KM_S, Ellipse};
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

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
                impulse: Some(4.0),
                bearing: None,
                turned: false,
            })
            .collect()
    }

    /// Give each of `dets` the bearing from loops alone toward `strike`, off by `off_deg`.
    fn aim(dets: &mut [Det], strike: LatLon, off_deg: f64, sigma_deg: f64) {
        for d in dets {
            let az = crate::geo::azimuth(d.pos, strike) + off_deg.to_radians();
            d.bearing = Some(Bearing {
                az: az.rem_euclid(std::f64::consts::PI),
                sigma: sigma_deg.to_radians(),
                modulus: std::f64::consts::PI,
            });
        }
    }

    fn stray(site: usize, t_us: i64) -> Det {
        Det {
            site: NAMES[site].into(),
            t_us,
            pos: ring()[site],
            sigma_us: 3.0,
            impulse: Some(1.5),
            bearing: None,
            turned: false,
        }
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
        for i in 0..2 {
            dets.push(stray(i, t + 150 + 37 * i as i64));
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
                impulse: None,
                bearing: None,
                turned: false,
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
        dets.push(stray(1, first - 20));
        let p = Params {
            min_sites: 4,
            ..params()
        };
        let got = locate(&sorted(dets), 0, i64::MAX, &mut HashSet::new(), &p, None).0;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].members.len(), 4);
    }

    /// Three sites heard a strike, and the last to hear it has a second trigger close by:
    /// two three-site groups that each fit three arrival times.
    fn two_ways_to_pick_three(bearing_sigma_deg: Option<f64>) -> (Vec<Located>, i64) {
        let mut rng = Rng::new(10);
        let strike = LatLon::new(35.3, -120.6);
        let mut dets = strike_dets(strike, 10_000, 2.0, &mut rng);
        dets.truncate(3);
        if let Some(s) = bearing_sigma_deg {
            aim(&mut dets, strike, 0.0, s);
        }
        let mut dets = sorted(dets);
        let real = dets[2].t_us;
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
        );
        (got.0, real)
    }

    #[test]
    fn two_three_site_groups_that_both_fit_are_refused() {
        assert!(two_ways_to_pick_three(None).0.is_empty());
    }

    #[test]
    fn bearings_tell_which_three_site_group_is_the_strike() {
        // at 0.3 degrees the bearings rule the other group out entirely, and it mustn't
        // come back as an exact fit of its arrival times
        for sigma in [1.0, 0.3] {
            let (got, real) = two_ways_to_pick_three(Some(sigma));
            assert_eq!(got.len(), 1, "{sigma}");
            assert!(got[0].members.iter().any(|m| m.t_us == real));
            assert!(distance_km(got[0].fix.pos, LatLon::new(35.3, -120.6)) < 2.0);
            let st = to_strike(&got[0]);
            assert!(!st.ambiguous);
            assert!(
                st.rms_us.is_some(),
                "bearings make it more than an exact fit"
            );
            assert!(st.sites.iter().all(|a| a.bearing_deg.is_some()
                && !a.bearing_left_out
                && a.bearing_resid_deg.is_some_and(|r| r.abs() < 3.0)
                && a.impulse == Some(4.0)));
        }
    }

    #[test]
    fn a_bearing_far_off_is_left_out_and_shows_in_its_residual() {
        let mut rng = Rng::new(11);
        let strike = LatLon::new(35.3, -120.6);
        let mut dets = strike_dets(strike, 10_000, 2.0, &mut rng);
        aim(&mut dets, strike, 0.0, 5.0);
        let one = |dets: &[Det], min_sites: usize| {
            let p = Params {
                min_sites,
                ..params()
            };
            let got = locate(
                &sorted(dets.to_vec()),
                0,
                i64::MAX,
                &mut HashSet::new(),
                &p,
                None,
            )
            .0;
            assert_eq!(got.len(), 1);
            to_strike(&got[0])
        };
        let st = one(&dets, 4);
        assert!(!st.unchecked);
        assert!(
            st.sites
                .iter()
                .all(|a| a.bearing_deg.is_some() && !a.bearing_left_out)
        );
        // a loop pointed 40 degrees wrong: the arrival times still fix the strike, four of
        // them by least squares and three exactly, and the residual shows the loop
        aim(&mut dets[1..2], strike, 40.0, 5.0);
        dets[3].bearing = None;
        for (n, st) in [(4, one(&dets, 4)), (3, one(&dets[..3], 3))] {
            assert_eq!(st.sites.len(), n);
            assert_eq!(st.rms_us.is_some(), n == 4);
            // three arrival times their bearings spoke against are unchecked
            assert_eq!(st.unchecked, n == 3);
            // within its own ellipse, from the times alone
            let d = distance_km(st.pos, strike);
            assert!(d < st.major_km, "{n}: {d} km off, {} km", st.major_km);
            for a in &st.sites {
                if a.site == "tst-d" {
                    let b = (a.bearing_deg, a.bearing_resid_deg, a.bearing_left_out);
                    assert_eq!(b, (None, None, false), "no bearing to leave out");
                    continue;
                }
                assert!(a.bearing_left_out);
                let want = if a.site == "tst-b" { 40.0 } else { 0.0 };
                let r = a.bearing_resid_deg.unwrap();
                assert!((r - want).abs() < 3.0, "{} of {n}: {r}", a.site);
            }
        }
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

    /// A strike somewhere inside the ring, heard by all four sites with the default 20 us
    /// of timing error and 5 degree bearings from loops alone, tst-b's turned `turn` degrees.
    fn heard(t0_us: i64, turn: f64, rng: &mut Rng) -> (LatLon, Vec<Det>) {
        let strike = LatLon::new(35.1 + 0.4 * rng.unit(), -120.85 + 0.45 * rng.unit());
        let mut dets = strike_dets(strike, t0_us, 20.0, rng);
        for (i, d) in dets.iter_mut().enumerate() {
            let off = if i == 1 { turn } else { 0.0 } + 5.0 * rng.gauss();
            aim(std::slice::from_mut(d), strike, off, 5.0);
        }
        (strike, dets)
    }

    #[test]
    fn a_loop_turned_20_degrees_is_left_out_once_its_residuals_show_it() {
        // one strike's chi-square can't catch 20 degrees at 5 degrees each, and with that
        // loop in the fit the ellipse is too small for where the strikes land
        const NOW: i64 = 1_795_000_000_000;
        let t0 = (NOW - 3_600_000) * 1000;
        let p = Params {
            min_sites: 4,
            ..params()
        };
        let solve_one =
            |dets: Vec<Det>| locate(&sorted(dets), 0, i64::MAX, &mut HashSet::new(), &p, None).0;
        let mut f = Fake::new(vec![Box::new(store::Store)]);
        for (name, pos) in NAMES.iter().zip(ring()) {
            let report = json!({"site": name, "lat": pos.lat, "lon": pos.lon, "sigma_us": 20.0,
                "bearing_sigma_deg": 5.0, "bearing_mod": 180, "gps": "locked",
                "togas": [[t0, 0.01, 4.0, 10.0]]});
            f.add("lightning.toga", report, NOW);
        }
        let mut rng = Rng::new(21);
        let first: Vec<serde_json::Value> = (0..12_i64)
            .map(|i| {
                let (_, dets) = heard(t0 + i * 10_000, 20.0, &mut rng);
                store::strike_json(&to_strike(&solve_one(dets)[0]))
            })
            .collect();
        f.add("lightning.strikes", json!({"strikes": first}), NOW);
        let turned = store::turned(&f.conn, NOW).unwrap();
        // tst-b's own residuals show only about half its turn, since it pulls the fits
        // it's in; the pull can tip a neighbour over the line too until fresh residuals
        // without it outnumber the old
        assert!(turned.contains_key("tst-b"), "{turned:?}");
        // how many of 300 strikes fall inside their 95% ellipse, with the sites found
        // turned left out as a round leaves them, or with every bearing in
        let inside = |turned: &std::collections::BTreeMap<String, f64>| {
            let mut rng = Rng::new(22);
            (0..300_i64)
                .filter(|i| {
                    let (strike, mut dets) = heard(t0 + (100 + i) * 10_000, 20.0, &mut rng);
                    for d in &mut dets {
                        d.turned = turned.contains_key(&d.site);
                    }
                    let got = solve_one(dets);
                    got.first()
                        .is_some_and(|l| ellipse_units(&l.fix, strike) <= 1.0)
                })
                .count()
        };
        let (kept, left) = (inside(&Default::default()), inside(&turned));
        assert!(
            kept < 240,
            "{kept} of 300 inside with the turned loop in the fit"
        );
        assert!(left >= 270, "{left} of 300 inside with it left out");
    }

    #[test]
    fn once_four_line_up_but_dont_fit_only_three_with_a_bearing_count() {
        let mut rng = Rng::new(12);
        let strike = LatLon::new(35.3, -120.6);
        let mut dets = strike_dets(strike, 10_000, 2.0, &mut rng);
        // tst-d's arrival is 25 us late: four line up, but they don't fit one strike
        dets[3].t_us += 25;
        let found = |dets: &[Det]| {
            let got = locate(
                &sorted(dets.to_vec()),
                0,
                i64::MAX,
                &mut HashSet::new(),
                &params(),
                None,
            );
            got.0
        };
        // a, b and c with bearings good enough to rule out every three with tst-d in
        let mut with = dets.clone();
        aim(&mut with[..3], strike, 0.0, 1.0);
        let got = found(&with);
        assert_eq!(got.len(), 1);
        let sites: Vec<&str> = got[0].members.iter().map(|m| m.site.as_str()).collect();
        assert_eq!(sites.len(), 3);
        assert!(!sites.contains(&"tst-d"));
        // without, their exact fit proves nothing, even when tst-d's loop rules out every
        // three with it in
        aim(&mut dets[3..], strike, 60.0, 5.0);
        assert!(found(&dets).is_empty());
    }

    #[test]
    fn a_checked_fit_beats_an_exact_one_then_the_better_fit_per_equation_wins() {
        let fix = |chi2: f64, dof: usize| Fix {
            pos: LatLon::new(35.3, -120.6),
            t0: 0.0,
            ellipse: Ellipse {
                major_km: 1.0,
                minor_km: 1.0,
                az_deg: 0.0,
            },
            chi2,
            dof,
            resid: vec![],
            bearing_resid: vec![],
            bearing_used: vec![],
            ambiguous: false,
        };
        assert!(rank(&fix(5.0, 1)) < rank(&fix(0.0, 0)));
        assert!(rank(&fix(3.0, 2)) < rank(&fix(2.0, 1)));
        assert!(rank(&fix(1.0, 3)) < rank(&fix(2.0, 3)));
    }

    #[test]
    fn strikes_marked_solved_are_not_found_again() {
        let mut rng = Rng::new(5);
        let dets = strike_dets(LatLon::new(35.3, -120.6), 5_000, 2.0, &mut rng);
        let f = Fake::new(vec![Box::new(store::Store)]);
        f.conn.execute_batch(SCHEMA).unwrap();
        for d in &dets {
            f.conn
                .execute(
                    "INSERT INTO togas (site, t_us, rms, lat, lon, sigma_us)
                     VALUES (?1, ?2, 0.01, ?3, ?4, ?5)",
                    params![d.site, d.t_us, d.pos.lat, d.pos.lon, d.sigma_us],
                )
                .unwrap();
        }
        let run = || {
            let (dets, mut used) = load(&f.conn, 0, i64::MAX).unwrap();
            locate(&dets, 0, i64::MAX, &mut used, &params(), None).0
        };
        let found = run();
        assert_eq!(found.len(), 1);
        // as a round does once the event with this strike is logged
        mark_solved(&f.conn, &found).unwrap();
        assert!(run().is_empty());
    }

    #[test]
    fn the_turned_sites_are_worked_out_again_a_minute_later() {
        const NOW: i64 = 1_795_000_000_000;
        let t = (NOW - 60_000) * 1000;
        let mut f = Fake::new(vec![Box::new(store::Store)]);
        let report = json!({"site": "tst-c", "lat": 35.3, "lon": -120.6, "sigma_us": 20.0,
            "bearing_sigma_deg": 5.0, "bearing_mod": 180, "gps": "locked",
            "togas": [[t, 0.01, 4.0, 41.0]]});
        f.add("lightning.toga", report, NOW);
        let mut turned = Turned::default();
        assert!(turned.get(&f.conn, NOW).unwrap().is_empty());
        // twelve strikes where tst-c's bearing is 20 degrees off
        let strikes: Vec<serde_json::Value> = (0..12)
            .map(|i| {
                json!({"id": format!("{i:02x}"), "t_us": t, "lat": 35.3, "lon": -120.6,
                    "major_km": 1.0, "minor_km": 0.5, "az_deg": 0.0, "rms_us": 1.0,
                    "ambiguous": false, "sites": [["tst-a", t, 0.0], ["tst-b", t, 0.0],
                    ["tst-c", t, 0.0, 5.5, 41.0, 20.0], ["tst-d", t, 0.0]]})
            })
            .collect();
        f.add("lightning.strikes", json!({"strikes": strikes}), NOW);
        let sites = |turned: &mut Turned, now: i64| -> Vec<String> {
            turned.get(&f.conn, now).unwrap().keys().cloned().collect()
        };
        assert!(
            sites(&mut turned, NOW + 59_000).is_empty(),
            "kept for a minute"
        );
        assert_eq!(sites(&mut turned, NOW + 60_000), ["tst-c"]);
        // a clock stepped back is a reason to look again too
        f.conn.execute("DELETE FROM strikes", []).unwrap();
        assert_eq!(sites(&mut turned, NOW + 100_000), ["tst-c"]);
        assert!(sites(&mut turned, NOW - 600_000).is_empty());
    }
}
