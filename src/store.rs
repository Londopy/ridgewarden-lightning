//! The views built from the feed: every site's receiver reports (`lightning.toga`, in the
//! telemetry log) and every solver's located strikes (`lightning.strikes`, main log).
//!
//! A receiver report:
//!   {"site": "tst-cuesta", "lat": 35.35, "lon": -120.65, "sigma_us": 20,
//!    "gps": "locked" | "unlocked" | "unchecked", "togas": [[t_us, rms], ...],
//!    "dropped": 0, "bad": 0, "error": null}
//! Reports come every few seconds while sferics arrive and once a minute otherwise, so a
//! silent receiver is visible. Arrivals in a report marked "unlocked" are never used.
//!
//! A strikes event:
//!   {"velocity_factor": 0.9922, "strikes": [{"id", "t_us", "lat", "lon", "major_km",
//!    "minor_km", "az_deg", "rms_us", "ambiguous", "sites": [[site, t_us, resid_us], ...]}]}

use std::collections::BTreeMap;

use anyhow::Result;
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use ridgewarden_client::mesh::{f, s};
use ridgewarden_client::util::{norm_node, valid_node};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use crate::geo::LatLon;

pub struct Store;

/// Most arrivals taken from one report; the core caps an event at 128 KB anyway.
pub const MAX_TOGAS: usize = 1500;
pub const MAX_STRIKES: usize = 200;
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
    PRIMARY KEY (site, t_us)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS togas_t ON togas(t_us);

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

-- a site name belongs to the station key that first reported it
CREATE TABLE IF NOT EXISTS sites (
    site       TEXT PRIMARY KEY,
    author_key TEXT NOT NULL
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
    sites     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS strikes_t ON strikes(t_us);
"#;

fn clip(v: &Value, k: &str, n: usize) -> Option<String> {
    s(v, k).map(|x| x.chars().take(n).collect())
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
             DELETE FROM agreements;",
        )?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        match e.kind.as_str() {
            "lightning.toga" => apply_report(tx, e, ts),
            "lightning.strikes" => apply_strikes(tx, e, ts),
            "lightning.agreement" => apply_agreement(tx, e, ts),
            _ => Ok(()),
        }
    }
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
    let mut n = 0;
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
            n += tx.execute(
                "INSERT OR IGNORE INTO togas (site, t_us, rms, lat, lon, sigma_us)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![site, t_us, rms, pos.lat, pos.lon, sigma],
            )?;
        }
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
                     az_deg, rms_us, ambiguous, sites)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
}

pub type Fits = BTreeMap<String, SiteFit>;

/// Most strikes the statistics look at: the newest this many in the period.
pub const FIT_MAX: usize = 20_000;

/// Per-site residuals and participation in strikes since `from_us`, plus the timing error
/// per arrival the residuals point to: sqrt(sum of squared residuals / sum of (n - 3)),
/// which is what timing_sigma_us should be if every site is about as good. The second
/// value is that error and how many strikes it comes from.
pub fn fits(conn: &Connection, from_us: i64) -> Result<(Fits, Option<(f64, usize)>)> {
    let list = strikes(conn, from_us, i64::MAX, FIT_MAX)?;
    let mut by_site: BTreeMap<String, (usize, Vec<f64>)> = BTreeMap::new();
    let (mut sq, mut dof, mut used) = (0.0, 0usize, 0usize);
    for s in &list {
        let n = s.sites.len();
        for a in &s.sites {
            let e = by_site.entry(a.site.clone()).or_default();
            e.0 += 1;
            if n > 3 {
                e.1.push(a.resid_us);
                sq += a.resid_us * a.resid_us;
            }
        }
        if n > 3 {
            dof += n - 3;
            used += 1;
        }
    }
    let fits = by_site
        .into_iter()
        .map(|(site, (strikes, mut r))| {
            let fit = SiteFit {
                strikes,
                with_residuals: r.len(),
                median_us: ridgewarden_client::util::median(&mut r),
            };
            (site, fit)
        })
        .collect();
    let sigma = (dof > 0).then(|| ((sq / dof as f64).sqrt(), used));
    Ok((fits, sigma))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Arrival {
    pub site: String,
    pub t_us: i64,
    pub resid_us: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Strike {
    pub id: String,
    pub t_us: i64,
    pub pos: LatLon,
    pub major_km: f64,
    pub minor_km: f64,
    pub az_deg: f64,
    /// RMS residual; None when exactly determined (three receivers)
    pub rms_us: Option<f64>,
    pub ambiguous: bool,
    pub sites: Vec<Arrival>,
}

pub fn sites_json(sites: &[Arrival]) -> String {
    Value::Array(
        sites
            .iter()
            .map(|a| json!([a.site, a.t_us, (a.resid_us * 100.0).round() / 100.0]))
            .collect(),
    )
    .to_string()
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
    let mut sites = vec![];
    for a in v.get("sites")?.as_array()?.iter().take(MAX_SITES) {
        sites.push(Arrival {
            site: site_name(a.get(0))?,
            t_us: a.get(1)?.as_i64()?,
            resid_us: a.get(2)?.as_f64().filter(|r| r.is_finite())?,
        });
    }
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
        sites,
    })
}

pub fn strike_json(st: &Strike) -> Value {
    json!({
        "id": st.id, "t_us": st.t_us, "lat": st.pos.lat, "lon": st.pos.lon,
        "major_km": st.major_km, "minor_km": st.minor_km, "az_deg": st.az_deg,
        "rms_us": st.rms_us, "ambiguous": st.ambiguous,
        "sites": serde_json::from_str::<Value>(&sites_json(&st.sites)).unwrap_or_default(),
    })
}

fn row_strike(r: &rusqlite::Row) -> rusqlite::Result<Strike> {
    let sites: String = r.get(9)?;
    let sites = serde_json::from_str::<Value>(&sites)
        .ok()
        .and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| {
                        Some(Arrival {
                            site: x.get(0)?.as_str()?.to_string(),
                            t_us: x.get(1)?.as_i64()?,
                            resid_us: x.get(2)?.as_f64()?,
                        })
                    })
                    .collect()
            })
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
        sites,
    })
}

const STRIKE_COLS: &str =
    "id, t_us, lat, lon, major_km, minor_km, az_deg, rms_us, ambiguous, sites";

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
}

/// Every site that has reported in the last week, from its latest report.
pub fn receivers(conn: &Connection, now: i64) -> Result<Vec<Receiver>> {
    let mut st = conn.prepare(
        "SELECT r.site, r.lat, r.lon, r.sigma_us, r.gps, r.ts, r.error,
             (SELECT max(t_us) FROM togas t WHERE t.site = r.site),
             (SELECT coalesce(sum(n), 0) FROM reports h WHERE h.site = r.site AND h.ts >= ?2),
             (SELECT coalesce(sum(dropped), 0) FROM reports h WHERE h.site = r.site AND h.ts >= ?2),
             (SELECT clock_s FROM reports h WHERE h.site = r.site AND h.ts >= ?2
                  AND clock_s IS NOT NULL ORDER BY ts DESC LIMIT 1)
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
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Drop raw arrivals after two days and reports after a week; strikes stay.
pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM togas WHERE t_us < ?1",
        [(now - 2 * DAY_MS) * 1000],
    )?;
    conn.execute("DELETE FROM reports WHERE ts < ?1", [now - 8 * DAY_MS])?;
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

    fn strike_with(id: &str, resid: [f64; 4]) -> Value {
        json!({"id": id, "t_us": T * 1000, "lat": 35.3, "lon": -120.6,
            "major_km": 1.0, "minor_km": 0.5, "az_deg": 0.0, "rms_us": 1.0, "ambiguous": false,
            "sites": [["tst-a", T * 1000, resid[0]], ["tst-b", T * 1000, resid[1]],
                      ["tst-c", T * 1000, resid[2]], ["tst-d", T * 1000, resid[3]]]})
    }

    #[test]
    fn residuals_point_at_a_site_with_bad_timing_and_at_the_real_timing_error() {
        let mut f = fake();
        let mut list = vec![];
        for i in 0..20 {
            // tst-d is always 12 us late; the others scatter by 2 us
            let r = if i % 2 == 0 { 2.0 } else { -2.0 };
            list.push(strike_with(&format!("{i:02x}"), [r, -r, r, 12.0]));
        }
        f.add("lightning.strikes", json!({"strikes": list}), T);
        let (fits, sigma) = fits(&f.conn, 0).unwrap();
        assert_eq!(fits["tst-d"].median_us, Some(12.0));
        assert_eq!(fits["tst-a"].median_us, Some(0.0));
        assert_eq!(fits["tst-a"].strikes, 20);
        let (s, n) = sigma.unwrap();
        assert_eq!(n, 20);
        // (4 + 4 + 4 + 144) per strike over one degree of freedom
        assert!((s - 156f64.sqrt()).abs() < 1e-9);
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
}
