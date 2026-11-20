//! Rain for the dry-lightning check, from APRS weather stations (most of them CWOP
//! members) heard over APRS-IS.
//!
//! The app logs in read-only (`pass -1`) with a budlist filter (`b/CALL1/CALL2`) so the
//! server sends only the configured stations' packets, and keeps each station's latest
//! report. Point it at a public server (rotate.aprs2.net:14580) or at an APRS-IS server
//! on the mesh, which keeps working with the internet down.
//!
//! Weather packets follow the APRS 1.0.1 spec, chapter 12: after the position and the `_`
//! weather symbol come wind direction/speed (`ddd/sss`) and then fixed-width fields, each
//! a letter and digits, with dots or spaces for "no data": `g` gust, `t` temperature,
//! `r` rain in the last hour, `p` rain in the last 24 hours, `P` rain since midnight (all
//! three in hundredths of an inch), `h` humidity, `b` pressure, `L`/`l` luminosity.
//! Positionless reports (`_MMDDHHMM` then `c`, `s`, `g`, ...) are read too.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use ridgewarden_client::Event;
use ridgewarden_client::core::Projection;
use rusqlite::{Connection, Transaction, params};
use tokio::io::{AsyncWriteExt, BufReader};

use crate::app::App;
use crate::config::{RainCfg, valid_call};
use crate::receiver::read_line;

/// Rain the weather app logs as `weather.rain` telemetry:
///   {"v": 1, "readings": [{"station", "source": "aprs" | "nws", "at_ms", "rain_1h_mm",
///    "rain_24h_mm", "text"}]}
/// A reading with a field missing leaves the last value of that field alone, as with
/// APRS-IS, and readings keep the time they were taken, not when they arrived.
pub struct WeatherFeed;

const FEED_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS feed_rain (
    station  TEXT PRIMARY KEY,
    source   TEXT NOT NULL,
    rain_1h  REAL,
    ts_1h    INTEGER,
    rain_24h REAL,
    ts_24h   INTEGER
);";

/// Most readings taken from one event, as the weather app sends at most.
const MAX_READINGS: usize = 200;

fn mm(v: &serde_json::Value, k: &str) -> Option<f64> {
    v.get(k)
        .and_then(serde_json::Value::as_f64)
        .filter(|m| m.is_finite() && (0.0..1000.0).contains(m))
}

impl Projection for WeatherFeed {
    fn schema(&self) -> &'static str {
        FEED_SCHEMA
    }

    fn reset(&self, tx: &Transaction) -> Result<()> {
        tx.execute("DELETE FROM feed_rain", [])?;
        Ok(())
    }

    fn apply(&self, tx: &Transaction, e: &Event, ts: i64) -> Result<()> {
        let p = &e.payload;
        if e.kind != "weather.rain" || p.get("v").and_then(serde_json::Value::as_u64) != Some(1) {
            return Ok(());
        }
        for r in p
            .get("readings")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_READINGS)
        {
            let Some(station) = r
                .get("station")
                .and_then(serde_json::Value::as_str)
                .filter(|s| valid_call(s))
            else {
                continue;
            };
            let Some(source) = r
                .get("source")
                .and_then(serde_json::Value::as_str)
                .filter(|s| matches!(*s, "aprs" | "nws"))
            else {
                continue;
            };
            // not from the future, and not older than the two days the app sends
            let Some(at) = r
                .get("at_ms")
                .and_then(serde_json::Value::as_i64)
                .filter(|at| *at <= ts + 600_000 && *at >= ts - 3 * 86_400_000)
            else {
                continue;
            };
            let (r1, r24) = (mm(r, "rain_1h_mm"), mm(r, "rain_24h_mm"));
            // newer readings win field by field, whatever order the events come in
            tx.execute(
                "INSERT INTO feed_rain (station, source, rain_1h, ts_1h, rain_24h, ts_24h)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(station) DO UPDATE SET source = excluded.source,
                     rain_1h = CASE WHEN excluded.ts_1h >= coalesce(ts_1h, 0) THEN excluded.rain_1h ELSE rain_1h END,
                     ts_1h = CASE WHEN excluded.ts_1h >= coalesce(ts_1h, 0) THEN excluded.ts_1h ELSE ts_1h END,
                     rain_24h = CASE WHEN excluded.ts_24h >= coalesce(ts_24h, 0) THEN excluded.rain_24h ELSE rain_24h END,
                     ts_24h = CASE WHEN excluded.ts_24h >= coalesce(ts_24h, 0) THEN excluded.ts_24h ELSE ts_24h END",
                params![station, source, r1, r1.map(|_| at), r24, r24.map(|_| at)],
            )?;
        }
        Ok(())
    }
}

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS rain_gauges (
    station  TEXT PRIMARY KEY,
    rain_1h  REAL,
    ts_1h    INTEGER,
    rain_24h REAL,
    ts_24h   INTEGER
);";

const HUNDREDTH_INCH_MM: f64 = 0.254;

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Wx {
    pub rain_1h_mm: Option<f64>,
    pub rain_24h_mm: Option<f64>,
    /// where the wind blows from, degrees true (APRS `c`, or `ddd` of `ddd/sss`)
    pub wind_dir_deg: Option<f64>,
    /// sustained one-minute wind speed, miles per hour (APRS `s` in positionless
    /// reports, or `sss` of `ddd/sss`)
    pub wind_mph: Option<f64>,
    /// peak gust in the last five minutes, miles per hour (APRS `g`)
    pub gust_mph: Option<f64>,
}

fn digits(v: &[u8]) -> Option<f64> {
    std::str::from_utf8(v)
        .ok()
        .filter(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
        .and_then(|v| v.parse().ok())
}

fn width(field: u8) -> Option<usize> {
    Some(match field {
        b'c' | b's' | b'g' | b't' | b'r' | b'p' | b'P' | b'L' | b'l' | b'#' => 3,
        b'h' => 2,
        b'b' => 5,
        _ => return None,
    })
}

/// The lettered fields. In a positionless report `c` and `s` are wind direction and
/// speed; after a position the wind comes as "ddd/sss" instead and `s` is snowfall.
fn fields(mut b: &[u8], positionless: bool, mut wx: Wx) -> Wx {
    while let Some((&k, rest)) = b.split_first() {
        let Some(w) = width(k) else { break };
        let Some(val) = rest.get(..w) else { break };
        let n = digits(val);
        match k {
            b'r' => wx.rain_1h_mm = n.map(|n| n * HUNDREDTH_INCH_MM),
            b'p' => wx.rain_24h_mm = n.map(|n| n * HUNDREDTH_INCH_MM),
            b'g' => wx.gust_mph = n,
            b'c' if positionless => wx.wind_dir_deg = n.filter(|d| *d <= 360.0),
            b's' if positionless => wx.wind_mph = n,
            _ => {}
        }
        b = &rest[w..];
    }
    wx
}

/// After a position: skip the uncompressed (19 bytes) or compressed (13 bytes) position,
/// check it carries the weather symbol, and read wind "ddd/sss" if present.
fn after_position(b: &[u8]) -> Option<(&[u8], Wx)> {
    let (symbol, rest) = if b.first()?.is_ascii_digit() {
        (*b.get(18)?, b.get(19..)?)
    } else {
        (*b.get(9)?, b.get(13..)?)
    };
    if symbol != b'_' {
        return None;
    }
    let wind = rest.len() >= 7
        && rest[3] == b'/'
        && rest[..7]
            .iter()
            .enumerate()
            .all(|(i, c)| i == 3 || c.is_ascii_digit() || *c == b'.' || *c == b' ');
    if !wind {
        return Some((rest, Wx::default()));
    }
    let wx = Wx {
        wind_dir_deg: digits(&rest[..3]).filter(|d| *d <= 360.0),
        wind_mph: digits(&rest[4..7]),
        ..Wx::default()
    };
    Some((&rest[7..], wx))
}

/// The source callsign, rain and wind of an APRS weather packet in TNC2 form
/// ("SRC>DEST,PATH:info"); None for anything else.
pub fn parse_packet(line: &str) -> Option<(String, Wx)> {
    let (head, info) = line.trim_end().split_once(':')?;
    let (src, _) = head.split_once('>')?;
    let b = info.as_bytes();
    let wx = match *b.first()? {
        b'!' | b'=' => {
            let (body, wind) = after_position(&b[1..])?;
            fields(body, false, wind)
        }
        b'/' | b'@' => {
            let (body, wind) = after_position(b.get(8..)?)?;
            fields(body, false, wind)
        }
        b'_' => fields(b.get(9..)?, true, Wx::default()),
        _ => return None,
    };
    Some((src.trim().to_ascii_uppercase(), wx))
}

/// Keep each rain field with its own time, so a report that leaves one out doesn't
/// erase the other.
pub fn store(conn: &Connection, station: &str, wx: Wx, now: i64) -> Result<()> {
    let at = |v: Option<f64>| v.map(|_| now);
    conn.execute(
        "INSERT INTO rain_gauges (station, rain_1h, ts_1h, rain_24h, ts_24h)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(station) DO UPDATE SET
             rain_1h = coalesce(excluded.rain_1h, rain_1h),
             ts_1h = coalesce(excluded.ts_1h, ts_1h),
             rain_24h = coalesce(excluded.rain_24h, rain_24h),
             ts_24h = coalesce(excluded.ts_24h, ts_24h)",
        params![
            station,
            wx.rain_1h_mm,
            at(wx.rain_1h_mm),
            wx.rain_24h_mm,
            at(wx.rain_24h_mm)
        ],
    )?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Dry { mm: f64, station: String, ts: i64 },
    Wet { mm: f64, station: String, ts: i64 },
    Missing(String),
}

impl Verdict {
    /// One sentence for pages and alerts, naming the source.
    pub fn text(&self, cfg: Option<&RainCfg>) -> String {
        let (h, dry) = cfg.map_or((24, 2.5), |c| (c.lookback_h, c.dry_mm));
        let when = |ts: i64| ridgewarden_client::util::short_time(ts);
        match self {
            Verdict::Dry { mm, station, ts } => format!(
                "Dry: {mm:.1} mm of rain in the last {h} h at {station}, reported {}; under {dry} mm.",
                when(*ts)
            ),
            Verdict::Wet { mm, station, ts } => format!(
                "Not dry: {mm:.1} mm of rain in the last {h} h at {station}, reported {}.",
                when(*ts)
            ),
            Verdict::Missing(why) => format!("No rain data: {why}. Can't say whether it's dry."),
        }
    }
}

/// Dry when every fresh report from the area's stations is under the threshold, wet when
/// any is at or over it, missing when there is no fresh report with the needed field.
pub fn verdict(
    conn: &Connection,
    cfg: Option<&RainCfg>,
    stations: &[String],
    now: i64,
) -> Result<Verdict> {
    let Some(cfg) = cfg else {
        return Ok(Verdict::Missing("no [rain] source is configured".into()));
    };
    if stations.is_empty() {
        return Ok(Verdict::Missing("this area has no rain_stations".into()));
    }
    let col = if cfg.lookback_h == 1 { "1h" } else { "24h" };
    let mut st = conn.prepare(&format!(
        "SELECT station, ts_{col}, rain_{col} FROM rain_gauges
         WHERE station = ?1 AND ts_{col} IS NOT NULL AND ?2
         UNION ALL
         SELECT station || ' (' || upper(source) || ' via the weather app)', ts_{col}, rain_{col} FROM feed_rain
         WHERE station = ?1 AND ts_{col} IS NOT NULL AND ?3"
    ))?;
    let mut readings = vec![];
    for s in stations {
        let rows = st.query_map(params![s, cfg.from_aprs_is, cfg.from_weather_app], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        })?;
        for r in rows {
            let (station, ts, mm) = r?;
            let age = now - ts;
            // a wet reading stays true for the whole lookback even if the gauge goes
            // quiet; only a dry one has to be fresh, so a silent wet gauge can't let a dry
            // neighbour call the area dry
            if let Some(mm) = mm
                && (age <= cfg.max_age_min as i64 * 60_000
                    || (mm >= cfg.dry_mm && age <= cfg.lookback_h as i64 * 3_600_000))
            {
                readings.push((station, ts, mm));
            }
        }
    }
    let Some((station, ts, mm)) = readings.into_iter().max_by(|a, b| a.2.total_cmp(&b.2)) else {
        return Ok(Verdict::Missing(format!(
            "no report with {}-hour rain from {} in the last {} min",
            cfg.lookback_h,
            stations.join(", "),
            cfg.max_age_min
        )));
    };
    Ok(if mm >= cfg.dry_mm {
        Verdict::Wet { mm, station, ts }
    } else {
        Verdict::Dry { mm, station, ts }
    })
}

fn stations(app: &App) -> Vec<String> {
    let mut v: Vec<String> = app
        .cfg
        .watch
        .iter()
        .flat_map(|w| w.rain_stations.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

async fn session(app: &App, cfg: &RainCfg, stations: &[String]) -> Result<()> {
    let s = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::net::TcpStream::connect(&cfg.aprs_is),
    )
    .await??;
    let (r, mut w) = s.into_split();
    let login = format!(
        "user {} pass -1 vers ridgewarden-lightning {} filter b/{}\r\n",
        cfg.login,
        env!("CARGO_PKG_VERSION"),
        stations.join("/")
    );
    w.write_all(login.as_bytes()).await?;
    let mut r = BufReader::new(r);
    let mut buf = vec![];
    loop {
        // servers send a comment line every 20 s or so; silence means a dead link
        let Some(l) =
            tokio::time::timeout(Duration::from_secs(300), read_line(&mut r, &mut buf)).await??
        else {
            bail!("server closed the connection");
        };
        if l.starts_with('#') {
            continue;
        }
        // a report with neither rain field says nothing about rain; don't let it
        // refresh the last one
        if let Some((src, wx)) = parse_packet(&l)
            && stations.contains(&src)
            && (wx.rain_1h_mm.is_some() || wx.rain_24h_mm.is_some())
        {
            tracing::debug!("rain from {src}: {wx:?}");
            store(&app.db(), &src, wx, ridgewarden_client::util::now_ms())?;
        }
    }
}

/// Keep a connection to APRS-IS for the watch areas' rain stations, forever.
pub async fn run(app: Arc<App>) {
    let Some(cfg) = app.cfg.rain.clone().filter(|c| c.from_aprs_is) else {
        return;
    };
    let stations = stations(&app);
    if stations.is_empty() {
        return;
    }
    loop {
        if let Err(e) = session(&app, &cfg, &stations).await {
            tracing::warn!("APRS-IS {}: {e:#}", cfg.aprs_is);
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rain_from_position_reports_with_a_timestamp() {
        // a CWOP report as heard on APRS-IS
        let (src, wx) = parse_packet(
            "CW0000>APRS,TCPIP*,qAC,AMBCWOP-2:@192335z3223.07N/11252.00W_160/005g006t072r010p125P000h38b10784AmbientCWOP.com",
        )
        .unwrap();
        assert_eq!(src, "CW0000");
        assert!((wx.rain_1h_mm.unwrap() - 2.54).abs() < 1e-9);
        assert!((wx.rain_24h_mm.unwrap() - 31.75).abs() < 1e-9);
    }

    #[test]
    fn field_order_varies_and_dots_mean_no_data() {
        let (_, wx) = parse_packet(
            "N0CALL-15>APRS,TCPIP*,qAC,FOURTH:!5200.45N/01525.33E_250/003g003t046h95r029p...P012b09844L000 BATT",
        )
        .unwrap();
        assert!((wx.rain_1h_mm.unwrap() - 7.366).abs() < 1e-9);
        assert_eq!(wx.rain_24h_mm, None);
    }

    #[test]
    fn reads_compressed_and_positionless_reports() {
        let (_, wx) = parse_packet(
            "N0CALL-1>APLRG1,TCPIP*,qAC,T2NL:=L7qmCP?I-_ !G.../...g...t047h..r000p004b10129",
        )
        .unwrap();
        assert_eq!(wx.rain_24h_mm, Some(4.0 * 0.254));
        let (_, wx) =
            parse_packet("N0CALL>APRS:_10090556c220s004g005t077r000p000P000h50b09900wRSW").unwrap();
        assert_eq!(wx.rain_1h_mm, Some(0.0));
    }

    #[test]
    fn reads_wind_and_gust_in_both_layouts() {
        let (_, wx) = parse_packet(
            "CW0000>APRS,TCPIP*:@192335z3223.07N/11252.00W_160/005g006t072r010p125P000h38b10784",
        )
        .unwrap();
        assert_eq!(
            (wx.wind_dir_deg, wx.wind_mph, wx.gust_mph),
            (Some(160.0), Some(5.0), Some(6.0))
        );
        let (_, wx) =
            parse_packet("N0CALL>APRS:_10090556c220s004g005t077r000p000P000h50b09900wRSW").unwrap();
        assert_eq!(
            (wx.wind_dir_deg, wx.wind_mph, wx.gust_mph),
            (Some(220.0), Some(4.0), Some(5.0))
        );
        // after a position `s` is snowfall, not wind; dots are no data
        let (_, wx) = parse_packet("N0CALL>APRS:!3523.07N/12052.00W_.../...g...t040s002").unwrap();
        assert_eq!(
            (wx.wind_dir_deg, wx.wind_mph, wx.gust_mph),
            (None, None, None)
        );
    }

    #[test]
    fn non_weather_packets_are_ignored() {
        assert!(parse_packet("N0CALL>APRS:!3523.07N/12052.00W>mobile").is_none());
        assert!(parse_packet("N0CALL>APRS::N0CALL-1 :hello").is_none());
        assert!(parse_packet("# aprsc 2.1").is_none());
    }

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(SCHEMA).unwrap();
        c.execute_batch(FEED_SCHEMA).unwrap();
        c
    }

    fn cfg() -> RainCfg {
        toml::from_str("").unwrap()
    }

    const NOW: i64 = 1_795_000_000_000;

    #[test]
    fn dry_only_with_fresh_data_under_the_threshold() {
        let c = db();
        let st = vec!["CW0001".to_string(), "CW0002".to_string()];
        let v = verdict(&c, Some(&cfg()), &st, NOW).unwrap();
        assert!(matches!(v, Verdict::Missing(_)), "no data is not dry");
        store(
            &c,
            "CW0001",
            Wx {
                rain_1h_mm: Some(0.0),
                rain_24h_mm: Some(0.254),
                ..Wx::default()
            },
            NOW - 60_000,
        )
        .unwrap();
        assert!(matches!(
            verdict(&c, Some(&cfg()), &st, NOW).unwrap(),
            Verdict::Dry { .. }
        ));
        // one wet gauge in the area makes it not dry
        store(
            &c,
            "CW0002",
            Wx {
                rain_1h_mm: Some(0.0),
                rain_24h_mm: Some(3.0),
                ..Wx::default()
            },
            NOW - 60_000,
        )
        .unwrap();
        let v = verdict(&c, Some(&cfg()), &st, NOW).unwrap();
        assert_eq!(
            v,
            Verdict::Wet {
                mm: 3.0,
                station: "CW0002".into(),
                ts: NOW - 60_000
            }
        );
        // an hour later the dry report is stale, but the wet one still counts
        assert!(matches!(
            verdict(&c, Some(&cfg()), &st, NOW + 3_600_000).unwrap(),
            Verdict::Wet { .. }
        ));
        // and once the lookback has passed there is nothing left
        assert!(matches!(
            verdict(&c, Some(&cfg()), &st, NOW + 25 * 3_600_000).unwrap(),
            Verdict::Missing(_)
        ));
    }

    #[test]
    fn a_quiet_wet_gauge_still_keeps_the_area_from_being_dry() {
        let c = db();
        let st = vec!["CW0001".to_string(), "CW0002".to_string()];
        store(
            &c,
            "CW0001",
            Wx {
                rain_1h_mm: None,
                rain_24h_mm: Some(6.0),
                ..Wx::default()
            },
            NOW - 2 * 3_600_000,
        )
        .unwrap();
        store(
            &c,
            "CW0002",
            Wx {
                rain_1h_mm: None,
                rain_24h_mm: Some(0.0),
                ..Wx::default()
            },
            NOW - 60_000,
        )
        .unwrap();
        assert!(matches!(
            verdict(&c, Some(&cfg()), &st, NOW).unwrap(),
            Verdict::Wet { .. }
        ));
        // after the lookback the old wet reading no longer counts
        let later = NOW - 2 * 3_600_000 + 25 * 3_600_000;
        store(
            &c,
            "CW0002",
            Wx {
                rain_1h_mm: None,
                rain_24h_mm: Some(0.0),
                ..Wx::default()
            },
            later,
        )
        .unwrap();
        assert!(matches!(
            verdict(&c, Some(&cfg()), &st, later).unwrap(),
            Verdict::Dry { .. }
        ));
    }

    fn feed() -> ridgewarden_client::mesh::testing::Fake {
        let f = ridgewarden_client::mesh::testing::Fake::new(vec![Box::new(WeatherFeed)]);
        f.conn.execute_batch(SCHEMA).unwrap();
        f
    }

    fn readings(list: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"v": 1, "readings": list})
    }

    #[test]
    fn the_weather_apps_readings_count_with_the_same_rules() {
        let mut f = feed();
        let st = vec!["KTST".to_string()];
        f.add(
            "weather.rain",
            readings(serde_json::json!([
                {"station": "KTST", "source": "nws", "at_ms": NOW - 120_000, "rain_1h_mm": 0.0,
                 "rain_24h_mm": null, "text": "Clear"},
                {"station": "CW0001", "source": "aprs", "at_ms": NOW - 60_000, "rain_1h_mm": 0.0,
                 "rain_24h_mm": 0.5}
            ])),
            NOW,
        );
        let one_hour: RainCfg = toml::from_str("lookback_h = 1").unwrap();
        let v = verdict(&f.conn, Some(&one_hour), &st, NOW).unwrap();
        assert!(
            matches!(&v, Verdict::Dry { station, .. } if station.contains("(NWS via the weather app)")),
            "{v:?}"
        );
        // NWS gives no 24-hour figure, so a 24-hour lookback has nothing from it
        assert!(matches!(
            verdict(&f.conn, Some(&cfg()), &st, NOW).unwrap(),
            Verdict::Missing(_)
        ));
        assert!(matches!(
            verdict(&f.conn, Some(&cfg()), &["CW0001".into()], NOW).unwrap(),
            Verdict::Dry { mm, .. } if mm == 0.5
        ));
        // a wet reading counts for the whole lookback, a dry one only while fresh
        f.add(
            "weather.rain",
            readings(
                serde_json::json!([{"station": "CW0001", "source": "aprs", "at_ms": NOW,
                "rain_1h_mm": 3.0, "rain_24h_mm": 4.0}]),
            ),
            NOW,
        );
        assert!(matches!(
            verdict(
                &f.conn,
                Some(&cfg()),
                &["CW0001".into()],
                NOW + 5 * 3_600_000
            )
            .unwrap(),
            Verdict::Wet { .. }
        ));
        // switched off, the app's readings don't count
        let off: RainCfg = toml::from_str("from_weather_app = false").unwrap();
        assert!(matches!(
            verdict(&f.conn, Some(&off), &["CW0001".into()], NOW).unwrap(),
            Verdict::Missing(_)
        ));
    }

    #[test]
    fn bad_or_older_readings_from_the_feed_are_ignored() {
        let mut f = feed();
        let ok = serde_json::json!({"station": "CW0001", "source": "aprs", "at_ms": NOW - 60_000,
            "rain_1h_mm": 0.0, "rain_24h_mm": 0.5});
        f.add("weather.rain", readings(serde_json::json!([ok])), NOW);
        // an older reading arriving later doesn't replace the newer one
        let mut old = ok.clone();
        old["at_ms"] = serde_json::json!(NOW - 3_600_000);
        old["rain_24h_mm"] = serde_json::json!(9.0);
        let mut future = ok.clone();
        future["at_ms"] = serde_json::json!(NOW + 3_600_000);
        future["rain_24h_mm"] = serde_json::json!(7.0);
        let mut bad = ok.clone();
        bad["station"] = serde_json::json!("cw0001; drop");
        bad["rain_24h_mm"] = serde_json::json!(8.0);
        f.add(
            "weather.rain",
            readings(serde_json::json!([old, future, bad])),
            NOW,
        );
        f.add(
            "weather.rain",
            serde_json::json!({"v": 2, "readings": [ok]}),
            NOW,
        );
        assert!(matches!(
            verdict(&f.conn, Some(&cfg()), &["CW0001".into()], NOW).unwrap(),
            Verdict::Dry { mm, .. } if mm == 0.5
        ));
    }

    #[test]
    fn untrusted_stations_rain_is_never_used() {
        use ridgewarden_client::core::{Feed, FeedItem, apply_page};
        let mut f = feed();
        let e = f.add("x", serde_json::json!({}), NOW);
        let mut fake = e.clone();
        fake.kind = "weather.rain".into();
        fake.payload = readings(serde_json::json!([{"station": "CW0001", "source": "aprs",
            "at_ms": NOW, "rain_1h_mm": 0.0, "rain_24h_mm": 0.0}]));
        let page = Feed {
            instance: "a".into(),
            events: vec![FeedItem {
                seq: 1,
                event: fake,
                trusted: false,
                seen_ms: Some(NOW),
            }],
        };
        apply_page(&mut f.conn, &page, &[&WeatherFeed]).unwrap();
        assert!(matches!(
            verdict(&f.conn, Some(&cfg()), &["CW0001".into()], NOW).unwrap(),
            Verdict::Missing(_)
        ));
    }

    #[test]
    fn a_report_without_one_field_keeps_the_other() {
        let c = db();
        let st = vec!["CW0001".to_string()];
        store(
            &c,
            "CW0001",
            Wx {
                rain_1h_mm: Some(0.0),
                rain_24h_mm: Some(0.5),
                ..Wx::default()
            },
            NOW - 60_000,
        )
        .unwrap();
        store(
            &c,
            "CW0001",
            Wx {
                rain_1h_mm: Some(0.0),
                rain_24h_mm: None,
                ..Wx::default()
            },
            NOW,
        )
        .unwrap();
        assert_eq!(
            verdict(&c, Some(&cfg()), &st, NOW).unwrap(),
            Verdict::Dry {
                mm: 0.5,
                station: "CW0001".into(),
                ts: NOW - 60_000
            }
        );
    }

    #[test]
    fn a_station_without_the_rain_field_is_missing_data() {
        let c = db();
        store(
            &c,
            "CW0001",
            Wx {
                rain_1h_mm: Some(0.0),
                rain_24h_mm: None,
                ..Wx::default()
            },
            NOW,
        )
        .unwrap();
        let v = verdict(&c, Some(&cfg()), &["CW0001".into()], NOW).unwrap();
        assert!(matches!(v, Verdict::Missing(_)), "{v:?}");
        assert!(v.text(Some(&cfg())).contains("Can't say"));
    }
}
