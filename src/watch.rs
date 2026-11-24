//! Watch areas: strikes inside them raise an alert, and a strike there with under
//! `dry_mm` of rain in the lookback (from the area's rain stations) raises a critical
//! dry-lightning alert. With no fresh rain data the alert says so and never calls it
//! dry. A strike counts as inside when its located center is; strikes marked ambiguous
//! (two positions fit) or unchecked (three arrival times their bearings spoke against)
//! don't count. An area around a site is centered wherever the site
//! is known to be (see `store::site_pos`), and waits until it is known.

use anyhow::Result;
use ridgewarden_client::alerts::{Alert, Open, Severity, open_alerts};
use ridgewarden_client::util::{norm_node, short_time};
use rusqlite::Connection;

use crate::config::{Config, WatchArea};
use crate::rain::{Verdict, verdict};
use crate::store::{Strike, site_pos, strikes_in_box};

/// Most strikes looked at per area; beyond this the alert says "more than".
pub const AREA_MAX: usize = 5000;

/// The area as it lies on the map: a site's circle gets the site's position for its
/// center. None for a site whose position isn't known yet.
pub fn place(conn: &Connection, area: &WatchArea) -> Result<Option<WatchArea>> {
    let Some(site) = &area.site else {
        return Ok(Some(area.clone()));
    };
    Ok(site_pos(conn, &norm_node(site))?.map(|p| WatchArea {
        center: Some([p.lat, p.lon]),
        ..area.clone()
    }))
}

/// Every area that can be placed yet.
pub fn placed(conn: &Connection, areas: &[WatchArea]) -> Result<Vec<WatchArea>> {
    let mut out = vec![];
    for a in areas {
        out.extend(place(conn, a)?);
    }
    Ok(out)
}

pub struct AreaView {
    /// placed, or as configured while its site's position is unknown
    pub area: WatchArea,
    pub placed: bool,
    /// strikes inside in the alert window, newest first
    pub strikes: Vec<Strike>,
    /// there were more than AREA_MAX strikes around the area; only the newest are here
    pub capped: bool,
    pub rain: Verdict,
}

pub fn views(conn: &Connection, cfg: &Config, now: i64) -> Result<Vec<AreaView>> {
    let from = (now - cfg.watch_window_min as i64 * 60_000) * 1000;
    let to = (now + 600_000) * 1000;
    cfg.watch
        .iter()
        .map(|configured| {
            let rain = verdict(conn, cfg.rain.as_ref(), &configured.rain_stations, now)?;
            let Some(area) = place(conn, configured)? else {
                return Ok(AreaView {
                    area: configured.clone(),
                    placed: false,
                    strikes: vec![],
                    capped: false,
                    rain,
                });
            };
            let around = strikes_in_box(conn, from, to, area.bbox(), AREA_MAX)?;
            Ok(AreaView {
                capped: around.len() == AREA_MAX,
                strikes: around
                    .into_iter()
                    .filter(|s| !s.ambiguous && !s.unchecked && area.contains(s.pos))
                    .collect(),
                area,
                placed: true,
                rain,
            })
        })
        .collect()
}

/// The alerts that are true now. `dry_open` says whether an area's dry-lightning alert
/// is open: if the rain data has gone missing since, the alert stays open (data going
/// stale isn't the same as rain) instead of resolving.
pub fn alerts(views: &[AreaView], cfg: &Config, dry_open: &dyn Fn(&str) -> bool) -> Vec<Alert> {
    let mut out = vec![];
    for v in views {
        let Some(last) = v.strikes.first() else {
            continue;
        };
        let name = &v.area.name;
        let first = v.strikes.last().unwrap_or(last);
        let text = format!(
            "{}{} strike{} in {name} since {}, the latest at {:.4}, {:.4} (95% ellipse {:.1} by {:.1} km). {}",
            if v.capped { "More than " } else { "" },
            v.strikes.len(),
            if v.strikes.len() == 1 { "" } else { "s" },
            short_time(first.t_us / 1000),
            last.pos.lat,
            last.pos.lon,
            last.major_km,
            last.minor_km,
            v.rain.text(cfg.rain.as_ref())
        );
        let still_dry = matches!(v.rain, Verdict::Missing(_)) && dry_open(name);
        let [dry, strike] = keys(name);
        out.push(match v.rain {
            _ if still_dry => Alert {
                key: dry,
                severity: Severity::Critical,
                title: format!("Dry lightning in {name}"),
                text: format!("{text} It was dry when this alert opened."),
            },
            Verdict::Dry { .. } => Alert {
                key: dry,
                severity: Severity::Critical,
                title: format!("Dry lightning in {name}"),
                text,
            },
            _ => Alert {
                key: strike,
                severity: Severity::Warning,
                title: format!("Lightning in {name}"),
                text,
            },
        });
    }
    out
}

/// An area's alert keys: its dry-lightning alert's, then its plain one's.
pub fn keys(area: &str) -> [String; 2] {
    [format!("dry:{area}"), format!("strike:{area}")]
}

/// The watch areas as the alert loop sees them.
pub struct Status {
    pub views: Vec<AreaView>,
    /// the alerts that are true now
    pub due: Vec<Alert>,
    /// the alerts already open
    pub open: Vec<Open>,
}

/// Each area's view, the alerts that are true now and the ones already open: what the
/// alert loop acts on and `/api/areas` shows.
pub fn status(conn: &Connection, cfg: &Config, now: i64) -> Result<Status> {
    let views = views(conn, cfg, now)?;
    let open = open_alerts(conn)?;
    let dry_open = |area: &str| {
        let [dry, _] = keys(area);
        open.iter().any(|a| a.key == dry)
    };
    let due = alerts(&views, cfg, &dry_open);
    Ok(Status { views, due, open })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::LatLon;
    use crate::rain::{SCHEMA, Wx, store};
    use crate::store::{Arrival, Store};
    use ridgewarden_client::mesh::testing::Fake;
    use serde_json::json;

    const NOW: i64 = 1_795_000_000_000;

    fn cfg() -> Config {
        toml::from_str(
            r#"
            [rain]
            [[watch]]
            name = "Backcountry"
            polygon = [[35.5, -121.0], [35.5, -120.5], [35.2, -120.5], [35.2, -121.0]]
            rain_stations = ["CW0001"]
            [[watch]]
            name = "Town"
            center = [35.0, -120.0]
            radius_km = 5
            "#,
        )
        .unwrap()
    }

    fn with_strike_at(p: LatLon) -> Fake {
        let mut f = Fake::new(vec![Box::new(Store), Box::new(crate::rain::WeatherFeed)]);
        f.conn.execute_batch(SCHEMA).unwrap();
        let st = Strike {
            id: "ab".into(),
            t_us: (NOW - 60_000) * 1000,
            pos: p,
            major_km: 1.2,
            minor_km: 0.5,
            az_deg: 10.0,
            rms_us: None,
            ambiguous: false,
            unchecked: false,
            sites: ["tst-a", "tst-b", "tst-c"]
                .iter()
                .map(|s| Arrival {
                    site: s.to_string(),
                    t_us: (NOW - 60_000) * 1000,
                    resid_us: 0.0,
                    impulse: None,
                    bearing_deg: None,
                    bearing_resid_deg: None,
                    bearing_left_out: false,
                })
                .collect(),
        };
        f.add(
            "lightning.strikes",
            json!({"strikes": [crate::store::strike_json(&st)]}),
            NOW,
        );
        f
    }

    #[test]
    fn a_storm_elsewhere_doesnt_hide_strikes_in_an_area() {
        let mut f = with_strike_at(LatLon::new(35.3, -120.7));
        // 6000 newer strikes far from both areas
        for chunk in 0..30 {
            let list: Vec<serde_json::Value> = (0..200)
                .map(|i| {
                    let mut v = crate::store::strike_json(&Strike {
                        id: format!("{:04x}", chunk * 200 + i),
                        t_us: (NOW - 1_000) * 1000,
                        pos: LatLon::new(37.0, -122.0),
                        major_km: 1.0,
                        minor_km: 1.0,
                        az_deg: 0.0,
                        rms_us: None,
                        ambiguous: false,
                        unchecked: false,
                        sites: vec![],
                    });
                    v["sites"] = json!([["tst-a", 0, 0.0], ["tst-b", 0, 0.0], ["tst-c", 0, 0.0]]);
                    v
                })
                .collect();
            f.add("lightning.strikes", json!({"strikes": list}), NOW);
        }
        let c = cfg();
        let v = views(&f.conn, &c, NOW).unwrap();
        assert_eq!(v[0].strikes.len(), 1);
        assert!(!v[0].capped);
    }

    #[test]
    fn dry_strike_in_an_area_is_critical_and_names_its_rain_source() {
        let f = with_strike_at(LatLon::new(35.3, -120.7));
        store(
            &f.conn,
            "CW0001",
            Wx {
                rain_1h_mm: Some(0.0),
                rain_24h_mm: Some(0.5),
                ..Wx::default()
            },
            NOW - 120_000,
        )
        .unwrap();
        let c = cfg();
        let a = alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|_| false);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].key, "dry:Backcountry");
        assert_eq!(a[0].severity, Severity::Critical);
        assert!(a[0].text.contains("CW0001"), "{}", a[0].text);
    }

    #[test]
    fn without_rain_data_it_is_only_a_warning_that_says_so() {
        let f = with_strike_at(LatLon::new(35.3, -120.7));
        let c = cfg();
        let a = alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|_| false);
        assert_eq!(a[0].key, "strike:Backcountry");
        assert_eq!(a[0].severity, Severity::Warning);
        assert!(a[0].text.contains("No rain data"));
    }

    #[test]
    fn an_open_dry_alert_stays_open_when_the_rain_data_goes_stale() {
        let f = with_strike_at(LatLon::new(35.3, -120.7));
        let c = cfg();
        let a = alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|n| {
            n == "Backcountry"
        });
        assert_eq!(a[0].key, "dry:Backcountry");
        assert_eq!(a[0].severity, Severity::Critical);
        assert!(a[0].text.contains("No rain data"));
    }

    #[test]
    fn unchecked_or_ambiguous_strikes_raise_nothing() {
        let c = cfg();
        for flag in ["unchecked", "ambiguous"] {
            let f = with_strike_at(LatLon::new(35.01, -120.01));
            let raised = |f: &Fake| alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|_| false);
            assert_eq!(raised(&f).len(), 1);
            f.conn
                .execute(&format!("UPDATE strikes SET {flag} = 1"), [])
                .unwrap();
            assert!(raised(&f).is_empty(), "{flag}");
        }
    }

    #[test]
    fn strikes_outside_every_area_or_too_old_raise_nothing() {
        let f = with_strike_at(LatLon::new(36.3, -120.7));
        let c = cfg();
        assert!(alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|_| false).is_empty());
        let f = with_strike_at(LatLon::new(35.01, -120.01));
        assert_eq!(
            alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|_| false)[0].key,
            "strike:Town"
        );
        assert!(
            alerts(&views(&f.conn, &c, NOW + 3_600_000).unwrap(), &c, &|_| {
                false
            })
            .is_empty()
        );
    }

    fn near_sites() -> Config {
        toml::from_str(
            r#"
            [[watch]]
            name = "Near A"
            site = "TST-A"
            radius_km = 10
            [[watch]]
            name = "Near the hub"
            site = "tst-hub"
            radius_km = 10
            [[watch]]
            name = "Near nowhere"
            site = "tst-gone"
            radius_km = 50
            "#,
        )
        .unwrap()
    }

    #[test]
    fn a_strike_near_a_site_alerts_for_that_site() {
        // tst-a's receiver is 7 km from the strike, the hub's node about 27 km
        let mut f = with_strike_at(LatLon::new(35.3, -120.7));
        f.add(
            "lightning.toga",
            json!({"site": "tst-a", "lat": 35.3, "lon": -120.623, "sigma_us": 20.0,
                   "gps": "locked", "togas": []}),
            NOW,
        );
        f.add(
            "node.poll",
            json!({"node": "tst-hub", "ok": true, "lat": 35.3, "lon": -120.4}),
            NOW,
        );
        let c = near_sites();
        let v = views(&f.conn, &c, NOW).unwrap();
        assert_eq!(
            v.iter()
                .map(|v| (v.placed, v.strikes.len()))
                .collect::<Vec<_>>(),
            vec![(true, 1), (true, 0), (false, 0)]
        );
        let a = alerts(&v, &c, &|_| false);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].key, "strike:Near A");
        // the hub's circle reaches the strike once it is bigger
        let mut c = near_sites();
        c.watch[1].radius_km = Some(30.0);
        let a = alerts(&views(&f.conn, &c, NOW).unwrap(), &c, &|_| false);
        assert_eq!(a.len(), 2);
        assert_eq!(placed(&f.conn, &c.watch).unwrap().len(), 2);
    }
}
