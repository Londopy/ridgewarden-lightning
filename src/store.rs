//! The views built from the feed: every site's receiver reports (`lightning.toga`, in the
//! telemetry log), every solver's located strikes (`lightning.strikes`, main log), and
//! where each node says it is (the core's `node.poll`), for watch areas around a site.
//!
//! A receiver report:
//!   {"site": "tst-cuesta", "lat": 35.35, "lon": -120.65, "sigma_us": 20,
//!    "bearing_sigma_deg": 5, "bearing_mod": 180,
//!    "gps": "locked" | "unlocked" | "unchecked",
//!    "togas": [[t_us, rms, impulse ratio, bearing degrees], ...],
//!    "dropped": 0, "bad": 0, "error": null}
//! The impulse ratio and bearing may be null; the bearing fields are there only from a
//! receiver that uses its bearings, which repeat every `bearing_mod` degrees (180 from
//! loops alone). Reports from 0.1.0 have `[t_us, rms]` only. Reports come every few
//! seconds while sferics arrive and once a minute otherwise, so a silent receiver is
//! visible. Arrivals in a report marked "unlocked" are never used.
//!
//! A strikes event:
//!   {"velocity_factor": 0.9922, "strikes": [{"id", "t_us", "lat", "lon", "major_km",
//!    "minor_km", "az_deg", "rms_us", "ambiguous", "unchecked",
//!    "sites": [[site, t_us, resid_us, impulse, bearing_deg, bearing_resid_deg], ...]}]}
//! with null where an arrival had no impulse ratio or bearing (0.1.0 wrote only the
//! first three), and a seventh element, true, after a bearing that was left out of the
//! fit. "unchecked" marks three arrival times that fit only once their bearings were
//! left out; like an ambiguous strike, it raises no alert. 0.1.0 didn't write it.

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::{f, s};
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::config::{BEARING_FLOOR_DEG, TIMING_FLOOR_US};
use crate::geo::LatLon;

pub struct Store;

/// Most arrivals taken from one report; the core caps an event at 128 KB anyway.
pub const MAX_TOGAS: usize = 1500;
pub const MAX_STRIKES: usize = 200;
/// Largest strikes payload we send; the core refuses app payloads over 128 KiB.
const EVENT_BYTES: usize = 120_000;
/// Most receiver sites in one network; a county needs a dozen at most.
pub const MAX_SITES_TOTAL: i64 = 64;
const MAX_SITES: usize = 32;
const DAY_MS: i64 = 86_400_000;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS togas (
    site     TEXT NOT NULL,
    t_us     INTEGER NOT NULL,
    rms      REAL NOT NULL,
    lat      REAL NOT NULL,
    lon      REAL NOT NULL,
    sigma_us REAL NOT NULL,
    impulse  REAL,
    -- degrees, with its 1-sigma error and the degrees after which it repeats
    bearing       REAL,
    bearing_sigma REAL,
    bearing_mod   REAL,
    PRIMARY KEY (site, t_us)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS togas_t ON togas(t_us);

CREATE TABLE IF NOT EXISTS nodes (
    node TEXT PRIMARY KEY,
    lat  REAL NOT NULL,
    lon  REAL NOT NULL,
    ts   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS reports (
    event_id   TEXT PRIMARY KEY,
    ts         INTEGER NOT NULL,
    author_key TEXT NOT NULL,
    site       TEXT NOT NULL,
    lat        REAL NOT NULL,
    lon        REAL NOT NULL,
    sigma_us   REAL NOT NULL,
    gps        TEXT NOT NULL,
    n          INTEGER NOT NULL,
    dropped    INTEGER NOT NULL,
    bad        INTEGER NOT NULL,
    error      TEXT,
    clock_s    REAL
);
CREATE INDEX IF NOT EXISTS reports_site_ts ON reports(site, ts);
CREATE INDEX IF NOT EXISTS reports_ts ON reports(ts);

CREATE TABLE IF NOT EXISTS agreements (
    event_id TEXT PRIMARY KEY,
    ts       INTEGER NOT NULL,
    author   TEXT NOT NULL,
    json     TEXT NOT NULL
);

-- a site name belongs to the station key that first reported it; bearing_sigma and
-- bearing_mod are from its latest report with a bearing in it, at bearing_ts
CREATE TABLE IF NOT EXISTS sites (
    site          TEXT PRIMARY KEY,
    author_key    TEXT NOT NULL,
    bearing_sigma REAL,
    bearing_mod   REAL,
    bearing_ts    INTEGER
);

CREATE TABLE IF NOT EXISTS strikes (
    id        TEXT PRIMARY KEY,
    ts        INTEGER NOT NULL,
    solver    TEXT NOT NULL,
    t_us      INTEGER NOT NULL,
    lat       REAL NOT NULL,
    lon       REAL NOT NULL,
    major_km  REAL NOT NULL,
    minor_km  REAL NOT NULL,
    az_deg    REAL NOT NULL,
    rms_us    REAL,
    ambiguous INTEGER NOT NULL,
    unchecked INTEGER NOT NULL DEFAULT 0,
    sites     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS strikes_t ON strikes(t_us);
"#;

/// Add the columns a 0.1.0 database lacks.
pub fn migrate(conn: &Connection) -> Result<()> {
    for (table, col, decl) in [
        ("togas", "impulse", "REAL"),
        ("togas", "bearing", "REAL"),
        ("togas", "bearing_sigma", "REAL"),
        ("togas", "bearing_mod", "REAL"),
        ("sites", "bearing_sigma", "REAL"),
        ("sites", "bearing_mod", "REAL"),
        ("sites", "bearing_ts", "INTEGER"),
        ("strikes", "unchecked", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let has: bool = conn.query_row(
            &format!("SELECT count(*) > 0 FROM pragma_table_info('{table}') WHERE name = ?1"),
            [col],
            |r| r.get(0),
        )?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {decl}"))?;
        }
    }
    Ok(())
}

fn clip(v: &Value, k: &str, n: usize) -> Option<String> {
    s(v, k).map(|x| x.chars().take(n).collect())
}

fn num(v: Option<&Value>) -> Option<f64> {
    v?.as_f64().filter(|x| x.is_finite())
}

fn site_name(v: Option<&Value>) -> Option<String> {
    let n = norm_node(v?.as_str()?);
    valid_node(&n).then_some(n)
}

/// An arrival or strike time is believable if it's within a day before the event that
/// carries it, and not more than a few minutes after (clock slop between sites).
fn near(t_us: i64, ts: i64) -> bool {
    let d = t_us / 1000 - ts;
    d > -DAY_MS && d < 600_000
}

impl Projection for Store {
    fn schema(&self) -> &'static str {
        SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute_batch(
            "DELETE FROM togas; DELETE FROM reports; DELETE FROM strikes; DELETE FROM sites;
             DELETE FROM agreements; DELETE FROM nodes;",
        )?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        match e.kind.as_str() {
            "lightning.toga" => apply_report(tx, e, ts),
            "lightning.strikes" => apply_strikes(tx, e, ts),
            "lightning.agreement" => apply_agreement(tx, e, ts),
            "node.poll" => apply_poll(tx, e, ts),
            _ => Ok(()),
        }
    }
}

/// A node's own position, as its owner set it in AREDN, from a core's poll of it.
fn apply_poll(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let (Some(node), Some(lat), Some(lon)) = (site_name(p.get("node")), f(p, "lat"), f(p, "lon"))
    else {
        return Ok(());
    };
    let pos = LatLon::new(lat, lon);
    if pos.valid() {
        tx.execute(
            "INSERT INTO nodes (node, lat, lon, ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(node) DO UPDATE SET lat = excluded.lat, lon = excluded.lon,
                 ts = excluded.ts WHERE excluded.ts >= nodes.ts",
            params![node, pos.lat, pos.lon, ts],
        )?;
    }
    Ok(())
}

/// Where a site is: its receiver's antenna if it has reported one, else its node's own
/// position from the core's polls.
pub fn site_pos(conn: &Connection, site: &str) -> Result<Option<LatLon>> {
    let latlon = |r: &rusqlite::Row| Ok(LatLon::new(r.get(0)?, r.get(1)?));
    let rx = conn
        .query_row(
            "SELECT lat, lon FROM reports WHERE site = ?1 ORDER BY ts DESC LIMIT 1",
            [site],
            latlon,
        )
        .optional()?;
    if rx.is_some() {
        return Ok(rx);
    }
    Ok(conn
        .query_row("SELECT lat, lon FROM nodes WHERE node = ?1", [site], latlon)
        .optional()?)
}

/// A report's bearing settings: (1-sigma error, degrees after which it repeats). An error
/// claimed below the floor is taken as the floor.
fn bearing_cfg(p: &Value) -> Option<(f64, f64)> {
    let sigma = f(p, "bearing_sigma_deg").filter(|s| *s > 0.0 && *s <= 45.0)?;
    let m = f(p, "bearing_mod").filter(|m| *m == 180.0 || *m == 360.0)?;
    Some((sigma.max(BEARING_FLOOR_DEG), m))
}

fn apply_report(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let p = &e.payload;
    let Some(site) = site_name(p.get("site")) else {
        return Ok(());
    };
    let pos = LatLon::new(
        f(p, "lat").unwrap_or(f64::NAN),
        f(p, "lon").unwrap_or(f64::NAN),
    );
    let sigma = f(p, "sigma_us").unwrap_or(0.0);
    if !pos.valid() || !(sigma > 0.0 && sigma <= 1000.0) {
        return Ok(());
    }
    // a claim below the floor would let this site outweigh every other in a fit
    let sigma = sigma.max(TIMING_FLOOR_US);
    let gps = match s(p, "gps") {
        Some(g @ ("locked" | "unlocked" | "unchecked")) => g,
        _ => return Ok(()),
    };
    // another station can't report under a site that isn't its own, or add sites without end
    tx.execute(
        "INSERT OR IGNORE INTO sites (site, author_key)
         SELECT ?1, ?2 WHERE (SELECT count(*) FROM sites) < ?3",
        params![site, e.author.key, MAX_SITES_TOTAL],
    )?;
    let owner: Option<String> = tx
        .query_row(
            "SELECT author_key FROM sites WHERE site = ?1",
            [&site],
            |r| r.get(0),
        )
        .optional()?;
    if owner.as_deref() != Some(e.author.key.as_str()) {
        tracing::debug!(
            "ignoring a report for {site} from {}: the site belongs to another station or the site list is full",
            e.author.call
        );
        return Ok(());
    }
    let bcfg = bearing_cfg(p);
    let (mut n, mut with_bearing) = (0, false);
    if gps != "unlocked" {
        for t in p
            .get("togas")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_TOGAS)
        {
            let (Some(t_us), Some(rms)) = (
                t.get(0).and_then(Value::as_i64),
                t.get(1).and_then(Value::as_f64),
            ) else {
                continue;
            };
            if !near(t_us, ts) || !rms.is_finite() || rms < 0.0 {
                continue;
            }
            let impulse = num(t.get(2)).filter(|i| (0.0..1e6).contains(i));
            let bearing = bcfg.and_then(|(sigma, m)| {
                num(t.get(3))
                    .filter(|b| (0.0..m).contains(b))
                    .map(|b| (b, sigma, m))
            });
            with_bearing |= bearing.is_some();
            n += tx.execute(
                "INSERT OR IGNORE INTO togas (site, t_us, rms, lat, lon, sigma_us, impulse,
                     bearing, bearing_sigma, bearing_mod)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    site,
                    t_us,
                    rms,
                    pos.lat,
                    pos.lon,
                    sigma,
                    impulse,
                    bearing.map(|b| b.0),
                    bearing.map(|b| b.1),
                    bearing.map(|b| b.2),
                ],
            )?;
        }
    }
    if let (true, Some((bsigma, m))) = (with_bearing, bcfg) {
        tx.execute(
            "UPDATE sites SET bearing_sigma = ?2, bearing_mod = ?3, bearing_ts = ?4
             WHERE site = ?1 AND coalesce(bearing_ts, 0) <= ?4",
            params![site, bsigma, m, ts],
        )?;
    }
    let count = |k: &str| p.get(k).and_then(Value::as_u64).unwrap_or(0).min(1 << 40) as i64;
    tx.execute(
        "INSERT OR IGNORE INTO reports (event_id, ts, author_key, site, lat, lon, sigma_us, gps, n,
             dropped, bad, error, clock_s)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            e.id,
            ts,
            e.author.key,
            site,
            pos.lat,
            pos.lon,
            sigma,
            gps,
            n as i64,
            count("dropped"),
            count("bad"),
            clip(p, "error", 200),
            f(p, "clock_s").filter(|c| c.is_finite()),
        ],
    )?;
    Ok(())
}

fn apply_strikes(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    for k in e
        .payload
        .get("strikes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_STRIKES)
    {
        if let Some(st) = parse_strike(k, ts) {
            tx.execute(
                "INSERT OR IGNORE INTO strikes (id, ts, solver, t_us, lat, lon, major_km, minor_km,
                     az_deg, rms_us, ambiguous, unchecked, sites)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    st.id,
                    ts,
                    e.author.call.chars().take(16).collect::<String>(),
                    st.t_us,
                    st.pos.lat,
                    st.pos.lon,
                    st.major_km,
                    st.minor_km,
                    st.az_deg,
                    st.rms_us,
                    st.ambiguous,
                    st.unchecked,
                    sites_json(&st.sites),
                ],
            )?;
        }
    }
    Ok(())
}

/// A comparison of our strikes with a reference set (see `agreement`), as logged.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Agreement {
    /// what the reference was, as the operator described it
    pub source: String,
    pub from_ms: i64,
    pub to_ms: i64,
    /// how close in time a match must be
    pub window_ms: f64,
    /// reference strikes in the period, and how many of them were inside the ring
    pub reference: u64,
    pub inside: u64,
    /// reference strikes inside the ring that we located, and how far off we were
    pub matched_inside: u64,
    pub median_km: Option<f64>,
    pub within_3km: u64,
    /// our strikes in the period, and how many matched a reference strike
    pub ours: u64,
    pub ours_matched: u64,
}

fn apply_agreement(tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
    let Ok(mut a) = serde_json::from_value::<Agreement>(e.payload.clone()) else {
        return Ok(());
    };
    a.source = a.source.chars().take(80).collect();
    let sane = a.inside <= a.reference
        && a.matched_inside <= a.inside
        && a.within_3km <= a.matched_inside
        && a.ours_matched <= a.ours
        && a.from_ms <= a.to_ms
        && a.window_ms.is_finite()
        && a.median_km.is_none_or(|m| m.is_finite() && m >= 0.0);
    if sane {
        tx.execute(
            "INSERT OR IGNORE INTO agreements (event_id, ts, author, json) VALUES (?1, ?2, ?3, ?4)",
            params![
                e.id,
                ts,
                e.author.call.chars().take(16).collect::<String>(),
                serde_json::to_string(&a)?
            ],
        )?;
    }
    Ok(())
}

/// The latest logged comparisons, newest first, with who ran them.
pub fn agreements(conn: &Connection, limit: usize) -> Result<Vec<(i64, String, Agreement)>> {
    let mut st =
        conn.prepare("SELECT ts, author, json FROM agreements ORDER BY ts DESC LIMIT ?1")?;
    let rows = st.query_map([limit as i64], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
    })?;
    let mut out = vec![];
    for r in rows {
        let (ts, author, json) = r?;
        if let Ok(a) = serde_json::from_str(&json) {
            out.push((ts, author, a));
        }
    }
    Ok(out)
}

/// How one receiver has done in located strikes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteFit {
    /// strikes it took part in
    pub strikes: usize,
    /// median residual over strikes with four or more sites (the only ones with
    /// residuals), and how many those were; a large signed median means its timing is off
    pub median_us: Option<f64>,
    pub with_residuals: usize,
    /// median bearing residual over the strikes it gave a bearing to, degrees, taken
    /// around the circle, and how many those were; a large one means its loops aren't
    /// lined up as vttoga was told
    pub bearing_median_deg: Option<f64>,
    pub with_bearings: usize,
}

/// Bearing residuals this far from zero (or the site's own bearing error, if bigger), as
/// a median over ten or more strikes, mean the loops are turned from where vttoga's `-p`
/// says they point. One strike's chi-square only catches a bearing several errors off.
pub const TURNED_DEG: f64 = 3.0;

impl SiteFit {
    /// The median bearing residual, degrees, when it says the site's loops are turned.
    pub fn turned(&self, bearing_sigma_deg: f64) -> Option<f64> {
        self.bearing_median_deg
            .filter(|m| self.with_bearings >= 10 && m.abs() > TURNED_DEG.max(bearing_sigma_deg))
    }
}

pub type Fits = BTreeMap<String, SiteFit>;

/// What the located strikes say about the receivers.
pub struct Residuals {
    pub sites: Fits,
    /// the timing error per arrival the residuals point to, microseconds, and how many
    /// strikes it comes from: sqrt(sum of squared residuals / sum of (n - 3)) over strikes
    /// of four or more sites fixed by their arrival times alone, since bearings in a fit
    /// pull it off the times. It's what timing_sigma_us should be if every site is about
    /// as good.
    pub timing: Option<(f64, usize)>,
    /// RMS of the residuals of bearings in fits of four or more sites, degrees, and how
    /// many bearings; a little low, since each bearing pulls its fit toward itself
    pub bearings: Option<(f64, usize)>,
}

/// Most strikes the statistics look at: the newest this many in the period.
pub const FIT_MAX: usize = 20_000;

/// Per-site residuals and participation in strikes since `from_us`, and the timing and
/// bearing errors they point to.
pub fn fits(conn: &Connection, from_us: i64) -> Result<Residuals> {
    let list = strikes(conn, from_us, i64::MAX, FIT_MAX)?;
    let mut by_site: BTreeMap<String, (usize, Vec<f64>, Vec<f64>)> = BTreeMap::new();
    let (mut sq, mut dof, mut timed) = (0.0, 0usize, 0usize);
    let (mut bsq, mut nb) = (0.0, 0usize);
    for s in &list {
        let n = s.sites.len();
        let by_times = !s.sites.iter().any(Arrival::bearing_used);
        for a in &s.sites {
            let e = by_site.entry(a.site.clone()).or_default();
            e.0 += 1;
            if n > 3 {
                e.1.push(a.resid_us);
                if by_times {
                    sq += a.resid_us * a.resid_us;
                }
                if let (true, Some(r)) = (a.bearing_used(), a.bearing_resid_deg) {
                    bsq += r * r;
                    nb += 1;
                }
            }
            // a bearing is one equation more than the unknowns, or was left out of the fit,
            // so its residual says something even with three sites
            e.2.extend(a.bearing_resid_deg);
        }
        if n > 3 && by_times {
            dof += n - 3;
            timed += 1;
        }
    }
    let mut st =
        conn.prepare("SELECT site, bearing_mod FROM sites WHERE bearing_mod IS NOT NULL")?;
    let periods: BTreeMap<String, f64> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let median = ridgewarden_client::util::median;
    let sites = by_site
        .into_iter()
        .map(|(site, (strikes, mut r, mut b))| {
            let period = periods.get(&site).copied().unwrap_or(360.0);
            let fit = SiteFit {
                strikes,
                with_residuals: r.len(),
                median_us: median(&mut r),
                with_bearings: b.len(),
                bearing_median_deg: circular_median(&mut b, period),
            };
            (site, fit)
        })
        .collect();
    Ok(Residuals {
        sites,
        timing: (dof > 0).then(|| ((sq / dof as f64).sqrt(), timed)),
        bearings: (nb > 0).then(|| ((bsq / nb as f64).sqrt(), nb)),
    })
}

/// The median of angles that repeat every `period` degrees, in [-period/2, period/2):
/// the plain median once the circle is cut at the widest gap between them, so residuals
/// either side of the wrap count as close together.
fn circular_median(v: &mut [f64], period: f64) -> Option<f64> {
    for x in v.iter_mut() {
        *x = x.rem_euclid(period);
    }
    v.sort_by(f64::total_cmp);
    let (first, last) = (*v.first()?, *v.last()?);
    // the gap across the wrap, from the last angle round to the first, then the rest
    let (mut cut, mut widest) = (0, first + period - last);
    for i in 1..v.len() {
        if v[i] - v[i - 1] > widest {
            (cut, widest) = (i, v[i] - v[i - 1]);
        }
    }
    let mut line: Vec<f64> = v[cut..]
        .iter()
        .copied()
        .chain(v[..cut].iter().map(|x| x + period))
        .collect();
    let m = ridgewarden_client::util::median(&mut line)?;
    Some((m + period / 2.0).rem_euclid(period) - period / 2.0)
}

/// Sites whose bearings the solver leaves out, with their median bearing residual: over
/// the past week, ten or more bearings that say the site's loops are turned.
pub fn turned(conn: &Connection, now: i64) -> Result<BTreeMap<String, f64>> {
    let week = now - 7 * DAY_MS;
    let fits = fits(conn, week * 1000)?;
    let mut st = conn.prepare("SELECT site, bearing_sigma FROM sites WHERE bearing_ts >= ?1")?;
    let sigma: BTreeMap<String, f64> = st
        .query_map([week], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(fits
        .sites
        .iter()
        .filter_map(|(site, f)| {
            let m = f.turned(sigma.get(site).copied().unwrap_or(0.0))?;
            Some((site.clone(), m))
        })
        .collect())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arrival {
    pub site: String,
    pub t_us: i64,
    pub resid_us: f64,
    /// vttoga's impulse ratio, where the record had one
    pub impulse: Option<f64>,
    /// the bearing the receiver measured, degrees, and observed minus fitted
    pub bearing_deg: Option<f64>,
    pub bearing_resid_deg: Option<f64>,
    /// the bearing was left out of the fit: it didn't fit with the arrival times, or its
    /// site's bearings have been off for a while
    pub bearing_left_out: bool,
}

impl Arrival {
    /// Whether its bearing went into the fit.
    pub fn bearing_used(&self) -> bool {
        self.bearing_deg.is_some() && !self.bearing_left_out
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Strike {
    pub id: String,
    pub t_us: i64,
    pub pos: LatLon,
    pub major_km: f64,
    pub minor_km: f64,
    pub az_deg: f64,
    /// RMS residual; None when exactly determined (three receivers, no bearing in the fit)
    pub rms_us: Option<f64>,
    pub ambiguous: bool,
    /// three arrival times that fit only once their bearings were left out: nothing
    /// checks them and the bearings speak against them, so like an ambiguous strike it
    /// raises no alert
    pub unchecked: bool,
    pub sites: Vec<Arrival>,
}

pub fn sites_json(sites: &[Arrival]) -> String {
    let r = |x: f64, k: f64| (x * k).round() / k;
    Value::Array(
        sites
            .iter()
            .map(|a| {
                let mut v = vec![
                    json!(a.site),
                    json!(a.t_us),
                    json!(r(a.resid_us, 100.0)),
                    json!(a.impulse.map(|i| r(i, 100.0))),
                    json!(a.bearing_deg),
                    json!(a.bearing_resid_deg.map(|b| r(b, 10.0))),
                ];
                if a.bearing_left_out {
                    v.push(json!(true));
                }
                Value::Array(v)
            })
            .collect(),
    )
    .to_string()
}

/// One arrival of a strike: [site, t_us, resid_us, impulse, bearing_deg,
/// bearing_resid_deg, left out], the last four possibly null or missing.
fn parse_arrival(a: &Value) -> Option<Arrival> {
    let bearing_deg = num(a.get(4)).filter(|b| (0.0..360.0).contains(b));
    Some(Arrival {
        site: site_name(a.get(0))?,
        t_us: a.get(1)?.as_i64()?,
        resid_us: num(a.get(2))?,
        impulse: num(a.get(3)).filter(|i| (0.0..1e6).contains(i)),
        bearing_deg,
        bearing_resid_deg: bearing_deg.and(num(a.get(5)).filter(|r| r.abs() <= 180.0)),
        bearing_left_out: bearing_deg.is_some() && a.get(6) == Some(&Value::Bool(true)),
    })
}

fn axis(v: &Value, k: &str) -> Option<f64> {
    f(v, k).filter(|x| x.is_finite() && *x >= 0.0 && *x < 500.0)
}

/// A strike from an event, or None if anything about it is off.
pub fn parse_strike(v: &Value, ts: i64) -> Option<Strike> {
    let id = s(v, "id")
        .filter(|i| !i.is_empty() && i.len() <= 32 && i.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let t_us = v
        .get("t_us")
        .and_then(Value::as_i64)
        .filter(|t| near(*t, ts))?;
    let pos = LatLon::new(f(v, "lat")?, f(v, "lon")?);
    let az_deg = f(v, "az_deg").filter(|a| a.is_finite())?;
    let sites: Vec<Arrival> = v
        .get("sites")?
        .as_array()?
        .iter()
        .take(MAX_SITES)
        .map(parse_arrival)
        .collect::<Option<_>>()?;
    (pos.valid() && sites.len() >= 3).then_some(())?;
    Some(Strike {
        id: id.to_string(),
        t_us,
        pos,
        major_km: axis(v, "major_km")?,
        minor_km: axis(v, "minor_km")?,
        az_deg,
        rms_us: f(v, "rms_us").filter(|r| r.is_finite()),
        ambiguous: v.get("ambiguous").and_then(Value::as_bool).unwrap_or(false),
        unchecked: v.get("unchecked").and_then(Value::as_bool).unwrap_or(false),
        sites,
    })
}

pub fn strike_json(st: &Strike) -> Value {
    json!({
        "id": st.id, "t_us": st.t_us, "lat": st.pos.lat, "lon": st.pos.lon,
        "major_km": st.major_km, "minor_km": st.minor_km, "az_deg": st.az_deg,
        "rms_us": st.rms_us, "ambiguous": st.ambiguous, "unchecked": st.unchecked,
        "sites": serde_json::from_str::<Value>(&sites_json(&st.sites)).unwrap_or_default(),
    })
}

/// `lightning.strikes` payloads for these strikes, each with how many of them it holds,
/// in order: as many to an event as stay under the core's size limit, and no more than
/// MAX_STRIKES, which is all a reader takes from one.
pub fn strike_events(strikes: &[Strike], velocity_factor: f64) -> Vec<(usize, Value)> {
    let event = |list: Vec<Value>| {
        let n = list.len();
        (
            n,
            json!({"velocity_factor": velocity_factor, "strikes": list}),
        )
    };
    let (mut out, mut list, mut bytes) = (vec![], vec![], 0);
    for st in strikes {
        let v = strike_json(st);
        let n = v.to_string().len() + 1;
        if !list.is_empty() && (bytes + n > EVENT_BYTES || list.len() == MAX_STRIKES) {
            out.push(event(std::mem::take(&mut list)));
            bytes = 0;
        }
        bytes += n;
        list.push(v);
    }
    if !list.is_empty() {
        out.push(event(list));
    }
    out
}

fn row_strike(r: &rusqlite::Row) -> rusqlite::Result<Strike> {
    let sites: String = r.get(10)?;
    let sites = serde_json::from_str::<Value>(&sites)
        .ok()
        .and_then(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(parse_arrival).collect())
        })
        .unwrap_or_default();
    Ok(Strike {
        id: r.get(0)?,
        t_us: r.get(1)?,
        pos: LatLon::new(r.get(2)?, r.get(3)?),
        major_km: r.get(4)?,
        minor_km: r.get(5)?,
        az_deg: r.get(6)?,
        rms_us: r.get(7)?,
        ambiguous: r.get(8)?,
        unchecked: r.get(9)?,
        sites,
    })
}

const STRIKE_COLS: &str =
    "id, t_us, lat, lon, major_km, minor_km, az_deg, rms_us, ambiguous, unchecked, sites";

/// Strikes with `from_us <= t < to_us`, newest first.
pub fn strikes(conn: &Connection, from_us: i64, to_us: i64, limit: usize) -> Result<Vec<Strike>> {
    let mut st = conn.prepare(&format!(
        "SELECT {STRIKE_COLS} FROM strikes WHERE t_us >= ?1 AND t_us < ?2 ORDER BY t_us DESC LIMIT ?3"
    ))?;
    let rows = st.query_map(params![from_us, to_us, limit as i64], row_strike)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Strikes with `from_us <= t < to_us` inside a lat/lon box (south, north, west, east),
/// newest first.
pub fn strikes_in_box(
    conn: &Connection,
    from_us: i64,
    to_us: i64,
    (s, n, w, e): (f64, f64, f64, f64),
    limit: usize,
) -> Result<Vec<Strike>> {
    let mut st = conn.prepare(&format!(
        "SELECT {STRIKE_COLS} FROM strikes WHERE t_us >= ?1 AND t_us < ?2
         AND lat BETWEEN ?3 AND ?4 AND lon BETWEEN ?5 AND ?6 ORDER BY t_us DESC LIMIT ?7"
    ))?;
    let rows = st.query_map(
        params![from_us, to_us, s, n, w, e, limit as i64],
        row_strike,
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn strike(conn: &Connection, id: &str) -> Result<Option<Strike>> {
    Ok(conn
        .query_row(
            &format!("SELECT {STRIKE_COLS} FROM strikes WHERE id = ?1"),
            [id],
            row_strike,
        )
        .optional()?)
}

#[derive(Clone, Debug)]
pub struct Receiver {
    pub site: String,
    pub pos: LatLon,
    pub sigma_us: f64,
    pub gps: String,
    /// when its latest report arrived
    pub last_ts: i64,
    pub last_toga_us: Option<i64>,
    pub hour_n: i64,
    pub hour_dropped: i64,
    pub error: Option<String>,
    /// newest arrival time minus the reporting computer's clock, seconds
    pub clock_s: Option<f64>,
    /// one bearing's error, degrees, if it has sent bearings this week
    pub bearing_sigma_deg: Option<f64>,
}

/// Every site that has reported in the last week, from its latest report.
pub fn receivers(conn: &Connection, now: i64) -> Result<Vec<Receiver>> {
    let mut st = conn.prepare(
        "SELECT r.site, r.lat, r.lon, r.sigma_us, r.gps, r.ts, r.error,
             (SELECT max(t_us) FROM togas t WHERE t.site = r.site),
             (SELECT coalesce(sum(n), 0) FROM reports h WHERE h.site = r.site AND h.ts >= ?2),
             (SELECT coalesce(sum(dropped), 0) FROM reports h WHERE h.site = r.site AND h.ts >= ?2),
             (SELECT clock_s FROM reports h WHERE h.site = r.site AND h.ts >= ?2
                  AND clock_s IS NOT NULL ORDER BY ts DESC LIMIT 1),
             (SELECT bearing_sigma FROM sites s WHERE s.site = r.site AND s.bearing_ts >= ?1)
         FROM reports r
         WHERE r.ts >= ?1 AND r.ts = (SELECT max(ts) FROM reports x WHERE x.site = r.site)
         GROUP BY r.site ORDER BY r.site",
    )?;
    let rows = st.query_map(params![now - 7 * DAY_MS, now - 3_600_000], |r| {
        Ok(Receiver {
            site: r.get(0)?,
            pos: LatLon::new(r.get(1)?, r.get(2)?),
            sigma_us: r.get(3)?,
            gps: r.get(4)?,
            last_ts: r.get(5)?,
            error: r.get(6)?,
            last_toga_us: r.get(7)?,
            hour_n: r.get(8)?,
            hour_dropped: r.get(9)?,
            clock_s: r.get(10)?,
            bearing_sigma_deg: r.get(11)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Drop raw arrivals after two days, and reports and node positions after eight; strikes
/// stay.
pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM togas WHERE t_us < ?1",
        [(now - 2 * DAY_MS) * 1000],
    )?;
    conn.execute("DELETE FROM reports WHERE ts < ?1", [now - 8 * DAY_MS])?;
    conn.execute("DELETE FROM nodes WHERE ts < ?1", [now - 8 * DAY_MS])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ridgewarden_client::mesh::testing::Fake;

    const T: i64 = 1_795_000_000_000;

    fn fake() -> Fake {
        Fake::new(vec![Box::new(Store)])
    }

    fn report(site: &str, gps: &str, togas: Value) -> Value {
        json!({"site": site, "lat": 35.3, "lon": -120.6, "sigma_us": 20.0, "gps": gps,
               "togas": togas, "dropped": 2, "bad": 0})
    }

    fn count(c: &Connection) -> i64 {
        c.query_row("SELECT count(*) FROM togas", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn reports_store_arrivals_and_receiver_status() {
        let mut f = fake();
        f.add(
            "lightning.toga",
            report(
                "TST-Cuesta",
                "locked",
                json!([[T * 1000 + 5, 0.01], [T * 1000 + 900, 0.02]]),
            ),
            T,
        );
        assert_eq!(count(&f.conn), 2);
        let r = receivers(&f.conn, T + 1000).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].site, "tst-cuesta");
        assert_eq!(r[0].hour_n, 2);
        assert_eq!(r[0].hour_dropped, 2);
        assert_eq!(r[0].last_toga_us, Some(T * 1000 + 900));
    }

    #[test]
    fn unlocked_and_malformed_arrivals_are_not_used() {
        let mut f = fake();
        f.add(
            "lightning.toga",
            report("tst-a", "unlocked", json!([[T * 1000, 0.01]])),
            T,
        );
        assert_eq!(count(&f.conn), 0);
        // the report itself still counts, so the page can say the GPS is unlocked
        assert_eq!(receivers(&f.conn, T).unwrap()[0].gps, "unlocked");
        f.add(
            "lightning.toga",
            report(
                "tst-a",
                "locked",
                json!([
                    [T * 1000 + 3 * DAY_MS * 1000, 0.01],
                    ["x", 1],
                    [T * 1000, -1.0],
                    [T * 1000 + 1, 0.5]
                ]),
            ),
            T,
        );
        assert_eq!(count(&f.conn), 1, "only the last arrival is sane");
        f.add(
            "lightning.toga",
            report("bad name!", "locked", json!([[T * 1000, 0.01]])),
            T,
        );
        f.add("lightning.toga", json!({"site": "tst-b", "lat": 95.0, "lon": 0.0, "sigma_us": 1, "gps": "locked", "togas": [[T * 1000, 0.1]]}), T);
        assert_eq!(count(&f.conn), 1);
    }

    #[test]
    fn a_site_belongs_to_the_first_station_that_reported_it() {
        let mut f = fake();
        f.add(
            "lightning.toga",
            report("tst-a", "locked", json!([[T * 1000, 0.01]])),
            T,
        );
        let mut e = f.add("lightning.toga", report("tst-x", "locked", json!([])), T);
        e.author.key = "someone-else".into();
        e.id = "other".into();
        e.payload = report("tst-a", "locked", json!([[T * 1000 + 7, 0.01]]));
        let tx = f.conn.transaction().unwrap();
        Store.apply(&tx, &e, T).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            count(&f.conn),
            1,
            "the other station's arrival at tst-a is ignored"
        );
    }

    fn strike_with(id: &str, resid: [f64; 4], bearing_resid: f64) -> Value {
        let t = T * 1000;
        json!({"id": id, "t_us": t, "lat": 35.3, "lon": -120.6,
            "major_km": 1.0, "minor_km": 0.5, "az_deg": 0.0, "rms_us": 1.0, "ambiguous": false,
            "sites": [["tst-a", t, resid[0]], ["tst-b", t, resid[1]],
                      ["tst-c", t, resid[2], 5.5, 41.0, bearing_resid], ["tst-d", t, resid[3]]]})
    }

    #[test]
    fn residuals_point_at_a_site_with_bad_timing_and_at_the_real_timing_error() {
        let mut f = fake();
        let mut list = vec![];
        for i in 0..20 {
            // tst-d is always 12 us late and tst-c's loops 8 degrees off; the rest scatter
            let r = if i % 2 == 0 { 2.0 } else { -2.0 };
            let mut st = strike_with(&format!("{i:02x}"), [r, -r, r, 12.0], 0.0);
            // half of them without tst-c's bearing, half with it in the fit, which pulls
            // the fit off the arrival times
            if i < 10 {
                st["sites"][2] = json!(["tst-c", T * 1000, r]);
            } else {
                st = strike_with(&format!("{i:02x}"), [3.0 * r, -r, r, 12.0], 8.0 + r);
            }
            list.push(st);
        }
        // three sites give no timing residuals, but a bearing left out of the fit still
        // says where its loops point
        let t = T * 1000;
        list.push(json!({"id": "ff", "t_us": t, "lat": 35.3, "lon": -120.6, "major_km": 1.0,
            "minor_km": 0.5, "az_deg": 0.0, "rms_us": null, "ambiguous": false,
            "sites": [["tst-a", t, 0.0], ["tst-b", t, 0.0], ["tst-e", t, 0.0, 5.5, 90.0, 40.0, true]]}));
        f.add("lightning.strikes", json!({"strikes": list}), T);
        let res = fits(&f.conn, 0).unwrap();
        let fits = &res.sites;
        assert_eq!(fits["tst-d"].median_us, Some(12.0));
        assert_eq!(fits["tst-b"].median_us, Some(0.0));
        assert_eq!(fits["tst-a"].strikes, 21);
        assert_eq!(fits["tst-c"].bearing_median_deg, Some(8.0));
        assert_eq!(fits["tst-c"].with_bearings, 10);
        assert_eq!(fits["tst-a"].bearing_median_deg, None);
        assert_eq!(fits["tst-e"].bearing_median_deg, Some(40.0));
        assert_eq!(fits["tst-e"].with_residuals, 0);
        // only the ten strikes fixed by their arrival times alone: (4 + 4 + 4 + 144) per
        // strike over one degree of freedom
        let (s, n) = res.timing.unwrap();
        assert_eq!(n, 10);
        assert!((s - 156f64.sqrt()).abs() < 1e-9, "{s}");
        // tst-c's 6 and 10 degrees; the left-out one doesn't count
        let (b, nb) = res.bearings.unwrap();
        assert_eq!(nb, 10);
        assert!((b - 68f64.sqrt()).abs() < 1e-9, "{b}");
    }

    #[test]
    fn bearing_residuals_either_side_of_the_wrap_are_close_together() {
        let m = |v: &[f64], period: f64| circular_median(&mut v.to_vec(), period).unwrap();
        // loops alone repeat every 180 degrees: these are all about 90 off
        assert!((m(&[88.0, 89.0, -89.0, -88.0, -87.0], 180.0).abs() - 90.0).abs() < 1.1);
        assert!((m(&[170.0, -170.0, -175.0, 175.0], 360.0).abs() - 180.0).abs() < 1e-9);
        assert_eq!(m(&[-1.0, 0.0, 1.0, 2.0], 360.0), 0.5);
        assert_eq!(m(&[6.0, 10.0, 10.0], 180.0), 10.0);
        assert_eq!(circular_median(&mut [], 360.0), None);
    }

    #[test]
    fn a_site_whose_bearings_stay_off_is_turned() {
        let mut f = fake();
        let t = T * 1000;
        // tst-c sends bearings with a 5 degree error, from loops alone
        let mut r = report("tst-c", "locked", json!([[t, 0.01, 4.0, 10.0]]));
        r["bearing_sigma_deg"] = json!(5.0);
        r["bearing_mod"] = json!(180);
        f.add("lightning.toga", r, T);
        let off = |ids: std::ops::Range<usize>| -> Vec<Value> {
            ids.map(|i| {
                // residuals either side of the wrap, as loops turned about 90 degrees give
                let b = if i % 2 == 0 { 89.0 } else { -89.0 };
                strike_with(&format!("{i:02x}"), [0.0; 4], b)
            })
            .collect()
        };
        f.add("lightning.strikes", json!({"strikes": off(0..9)}), T);
        let now = T + 60_000;
        assert!(
            turned(&f.conn, now).unwrap().is_empty(),
            "nine aren't enough"
        );
        f.add("lightning.strikes", json!({"strikes": off(9..12)}), T);
        let got = turned(&f.conn, now).unwrap();
        assert!((got["tst-c"].abs() - 90.0).abs() < 1.1, "{got:?}");
        assert_eq!(got.len(), 1);
        // a week later it has to show again
        assert!(turned(&f.conn, now + 8 * DAY_MS).unwrap().is_empty());
    }

    fn bearing_report(togas: Value) -> Value {
        let mut r = report("tst-a", "locked", togas);
        r["bearing_sigma_deg"] = json!(5.0);
        r["bearing_mod"] = json!(180);
        r
    }

    fn toga_row(c: &Connection, t_us: i64) -> (Option<f64>, Option<f64>, Option<f64>) {
        c.query_row(
            "SELECT impulse, bearing, bearing_mod FROM togas WHERE t_us = ?1",
            [t_us],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn reports_keep_each_arrivals_impulse_ratio_and_bearing() {
        let mut f = fake();
        let t = T * 1000;
        f.add(
            "lightning.toga",
            bearing_report(json!([
                [t, 0.01, 4.5, 123.4],
                [t + 1, 0.01, null, 190.0],
                [t + 2, 0.01, -1.0, 10.0],
                [t + 3, 0.01]
            ])),
            T,
        );
        assert_eq!(count(&f.conn), 4);
        assert_eq!(toga_row(&f.conn, t), (Some(4.5), Some(123.4), Some(180.0)));
        assert_eq!(toga_row(&f.conn, t + 1), (None, None, None), "past 180");
        assert_eq!(toga_row(&f.conn, t + 2), (None, Some(10.0), Some(180.0)));
        assert_eq!(
            toga_row(&f.conn, t + 3),
            (None, None, None),
            "as 0.1.0 sent them"
        );
        assert_eq!(
            receivers(&f.conn, T).unwrap()[0].bearing_sigma_deg,
            Some(5.0)
        );
        // without the receiver's bearing settings a bearing can't be read
        f.add(
            "lightning.toga",
            report("tst-a", "locked", json!([[t + 9, 0.01, 4.5, 123.4]])),
            T,
        );
        assert_eq!(toga_row(&f.conn, t + 9), (Some(4.5), None, None));
    }

    #[test]
    fn a_report_cant_claim_better_than_the_floors() {
        let mut f = fake();
        let t = T * 1000;
        let mut r = bearing_report(json!([[t, 0.01, 4.5, 123.4]]));
        r["sigma_us"] = json!(0.001);
        r["bearing_sigma_deg"] = json!(0.001);
        f.add("lightning.toga", r, T);
        let stored: (f64, f64) = f
            .conn
            .query_row(
                "SELECT sigma_us, bearing_sigma FROM togas WHERE t_us = ?1",
                [t],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, (1.0, 0.5));
        let rx = &receivers(&f.conn, T).unwrap()[0];
        assert_eq!((rx.sigma_us, rx.bearing_sigma_deg), (1.0, Some(0.5)));
    }

    #[test]
    fn node_positions_go_after_eight_days() {
        let mut f = fake();
        let poll = json!({"node": "tst-hub", "ok": true, "lat": 35.2, "lon": -120.5});
        f.add("node.poll", poll, T);
        prune(&f.conn, T + 7 * DAY_MS).unwrap();
        assert!(site_pos(&f.conn, "tst-hub").unwrap().is_some());
        prune(&f.conn, T + 9 * DAY_MS).unwrap();
        assert_eq!(site_pos(&f.conn, "tst-hub").unwrap(), None);
    }

    #[test]
    fn a_database_from_0_1_0_gets_the_new_columns() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE togas (site TEXT NOT NULL, t_us INTEGER NOT NULL, rms REAL NOT NULL,
                 lat REAL NOT NULL, lon REAL NOT NULL, sigma_us REAL NOT NULL,
                 PRIMARY KEY (site, t_us)) WITHOUT ROWID;
             CREATE TABLE sites (site TEXT PRIMARY KEY, author_key TEXT NOT NULL);
             CREATE TABLE strikes (id TEXT PRIMARY KEY, ts INTEGER NOT NULL, solver TEXT NOT NULL,
                 t_us INTEGER NOT NULL, lat REAL NOT NULL, lon REAL NOT NULL,
                 major_km REAL NOT NULL, minor_km REAL NOT NULL, az_deg REAL NOT NULL,
                 rms_us REAL, ambiguous INTEGER NOT NULL, sites TEXT NOT NULL);
             INSERT INTO togas VALUES ('tst-a', 1, 0.1, 35.0, -120.0, 20.0);
             INSERT INTO strikes VALUES ('0a', 1, 'N0CALL', 5, 35.0, -120.0, 1.0, 0.5, 0.0,
                 NULL, 0, '[[\"tst-a\",5,0.0],[\"tst-b\",6,0.0],[\"tst-c\",7,0.0]]');",
        )
        .unwrap();
        ridgewarden_client::core::init_db(&c, &[&Store]).unwrap();
        migrate(&c).unwrap();
        migrate(&c).unwrap();
        let old = strike(&c, "0a").unwrap().unwrap();
        assert!(!old.unchecked && old.sites.len() == 3);
        c.execute(
            "INSERT INTO togas VALUES ('tst-a', 2, 0.1, 35.0, -120.0, 20.0, 4.0, 90.0, 5.0, 360.0)",
            [],
        )
        .unwrap();
        assert_eq!(toga_row(&c, 1), (None, None, None));
        assert_eq!(toga_row(&c, 2), (Some(4.0), Some(90.0), Some(360.0)));
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM pragma_table_info('sites') WHERE name LIKE 'bearing%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn a_site_is_where_its_receiver_says_or_else_where_its_node_says() {
        let mut f = fake();
        let poll =
            |node: &str, lat: f64| json!({"node": node, "ok": true, "lat": lat, "lon": -120.5});
        f.add("node.poll", poll("TST-Hub", 35.2), T);
        f.add(
            "node.poll",
            json!({"node": "tst-hub", "ok": false, "error": "timeout"}),
            T + 1,
        );
        assert_eq!(
            site_pos(&f.conn, "tst-hub").unwrap(),
            Some(LatLon::new(35.2, -120.5))
        );
        // a newer poll moves it, an older one arriving late doesn't
        f.add("node.poll", poll("tst-hub", 35.25), T + 60_000);
        f.add("node.poll", poll("tst-hub", 35.9), T + 30_000);
        assert_eq!(site_pos(&f.conn, "tst-hub").unwrap().unwrap().lat, 35.25);
        f.add("node.poll", poll("tst-a", 36.0), T);
        f.add("lightning.toga", report("tst-a", "locked", json!([])), T);
        assert_eq!(
            site_pos(&f.conn, "tst-a").unwrap(),
            Some(LatLon::new(35.3, -120.6)),
            "the surveyed antenna wins"
        );
        f.add("node.poll", poll("tst-far", 95.0), T);
        assert_eq!(site_pos(&f.conn, "tst-far").unwrap(), None);
    }

    fn big_strike(i: usize) -> Strike {
        // node names are at most 63 characters
        let site = |k: usize| format!("tst-{k:02}-{}", "x".repeat(56));
        Strike {
            id: format!("{i:016x}"),
            t_us: T * 1000 + i as i64,
            pos: LatLon::new(35.123456, -120.123456),
            major_km: 12.345,
            minor_km: 1.234,
            az_deg: 123.4,
            rms_us: Some(12.34),
            ambiguous: false,
            unchecked: false,
            sites: (0..11)
                .map(|k| Arrival {
                    site: site(k),
                    t_us: T * 1000 + i as i64 + 100 * k as i64,
                    resid_us: -12.34,
                    impulse: Some(1234.56),
                    bearing_deg: Some(359.9),
                    bearing_resid_deg: Some(-89.9),
                    bearing_left_out: true,
                })
                .collect(),
        }
    }

    #[test]
    fn strike_events_stay_under_the_cores_size_limit() {
        let list: Vec<Strike> = (0..500).map(big_strike).collect();
        let events = strike_events(&list, 0.9922);
        assert!(events.len() > 2);
        let mut n = 0;
        for (k, e) in &events {
            assert!(e.to_string().len() <= 128 * 1024);
            let got = e["strikes"].as_array().unwrap();
            assert!(got.len() <= MAX_STRIKES);
            // the count says which strikes went out in it, for marking them solved
            assert_eq!(*k, got.len());
            for s in got {
                assert_eq!(parse_strike(s, T).unwrap(), list[n]);
                n += 1;
            }
        }
        assert_eq!(n, 500);
        // small strikes still go 200 to an event
        let mut small = big_strike(0);
        small.sites.truncate(3);
        assert_eq!(strike_events(&vec![small; 450], 0.9922).len(), 3);
    }

    #[test]
    fn agreements_are_checked_before_they_count() {
        let mut f = fake();
        let good = json!({"source": "test set", "from_ms": 1, "to_ms": 2, "window_ms": 50.0,
            "reference": 10, "inside": 8, "matched_inside": 7, "median_km": 1.2, "within_3km": 6,
            "ours": 9, "ours_matched": 7});
        let mut bad = good.clone();
        bad["matched_inside"] = json!(9);
        f.add("lightning.agreement", bad, T);
        f.add("lightning.agreement", good, T);
        let got = agreements(&f.conn, 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].2.within_3km, 6);
    }

    #[test]
    fn strikes_round_trip_and_bad_ones_are_dropped() {
        let mut f = fake();
        let good = json!({"id": "00ff", "t_us": T * 1000, "lat": 35.3, "lon": -120.6,
            "major_km": 1.5, "minor_km": 0.4, "az_deg": 30.0, "rms_us": 4.2, "ambiguous": false,
            "sites": [["tst-a", T * 1000 + 50, 1.0], ["tst-b", T * 1000 + 90, -2.0], ["tst-c", T * 1000 + 120, 0.5]]});
        let mut bad = good.clone();
        bad["id"] = json!("not hex");
        let mut two = good.clone();
        two["id"] = json!("01");
        two["sites"] = json!([["tst-a", 1, 0.0], ["tst-b", 2, 0.0]]);
        f.add("lightning.strikes", json!({"strikes": [good, bad, two]}), T);
        let got = strikes(&f.conn, 0, i64::MAX, 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].sites.len(), 3);
        assert_eq!(got[0].rms_us, Some(4.2));
        assert_eq!(strike(&f.conn, "00ff").unwrap().unwrap(), got[0]);
    }

    #[test]
    fn strikes_keep_each_arrivals_impulse_and_bearing() {
        let mut f = fake();
        let t = T * 1000;
        let st = json!({"id": "0a", "t_us": t, "lat": 35.3, "lon": -120.6,
            "major_km": 1.5, "minor_km": 0.4, "az_deg": 30.0, "rms_us": 1.2, "ambiguous": false,
            "unchecked": true,
            "sites": [["tst-a", t + 50, 1.0, 6.25, 41.5, -1.5], ["tst-b", t + 90, -2.0, null, 400.0, 2.0, true],
                      ["tst-c", t + 120, 0.5], ["tst-d", t + 150, 0.1, 3.0, 12.0, 38.5, true],
                      ["tst-e", t + 160, 0.2, null, 10.0, 500.0]]});
        f.add("lightning.strikes", json!({"strikes": [st]}), T);
        let got = strike(&f.conn, "0a").unwrap().unwrap();
        let bearing = |i: usize| {
            let a = &got.sites[i];
            (a.bearing_deg, a.bearing_resid_deg, a.bearing_left_out)
        };
        assert_eq!(got.sites[0].impulse, Some(6.25));
        assert_eq!(bearing(0), (Some(41.5), Some(-1.5), false));
        assert_eq!(bearing(1), (None, None, false), "not a bearing");
        assert_eq!(got.sites[2].impulse, None);
        assert_eq!(bearing(3), (Some(12.0), Some(38.5), true));
        assert_eq!(
            bearing(4),
            (Some(10.0), None, false),
            "no residual is that big"
        );
        assert!(got.unchecked);
        // and it goes back out the same way
        assert_eq!(parse_strike(&strike_json(&got), T).unwrap(), got);
        assert_eq!(
            strike_json(&got)["sites"][3],
            json!(["tst-d", t + 150, 0.1, 3.0, 12.0, 38.5, true])
        );
        assert_eq!(strike_json(&got)["sites"][0].as_array().unwrap().len(), 6);
    }
}
