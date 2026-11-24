//! What the tests of the JSON and of `top` share: an app whose database holds receivers,
//! a strike of each kind, a dry rain gauge and the alerts they open, and its pages on a
//! free port.

use std::path::Path;
use std::sync::Arc;

use ridgewarden_client::alerts;
use ridgewarden_client::mesh::testing::Fake;
use ridgewarden_client::util::now_ms;
use serde_json::{Value, json};

use crate::app::{App, PROJECTIONS};
use crate::config::Config;
use crate::geo::LatLon;
use crate::store::{Arrival, Strike, strike_json};
use crate::{watch, web};

pub const RX: [(&str, f64, f64); 4] = [
    ("tst-north", 35.55, -120.70),
    ("tst-east", 35.30, -120.35),
    ("tst-south", 35.05, -120.62),
    ("tst-west", 35.32, -120.95),
];

/// A config with three watch areas, one around a site nobody knows; the core token is
/// read from `local.token` in `dir` if it's there.
pub fn config(dir: &Path) -> Config {
    let d = dir.display().to_string().replace('\\', "/");
    let c: Config = toml::from_str(&format!(
        r#"
        data_dir = "{d}"
        [core]
        url = "http://127.0.0.1:9"
        token_file = "{d}/local.token"
        [solver]
        enabled = true
        min_sites = 3
        [rain]
        from_aprs_is = false
        [site_names]
        "tst-north" = "North ridge"
        [[watch]]
        name = "Backcountry"
        polygon = [[35.36, -120.62], [35.52, -120.50], [35.48, -120.30], [35.30, -120.38]]
        rain_stations = ["CW0001"]
        [[watch]]
        name = "Near the hub"
        center = [35.30, -120.66]
        radius_km = 8
        [[watch]]
        name = "Near the new site"
        site = "tst-new"
        radius_km = 10
        "#
    ))
    .unwrap();
    c.validate().unwrap();
    c
}

/// A checked strike heard by the first `sites` receivers, each with a bearing, the
/// second one's left out of the fit.
pub fn strike(id: &str, t_us: i64, at: (f64, f64), sites: usize) -> Strike {
    Strike {
        id: id.into(),
        t_us,
        pos: LatLon::new(at.0, at.1),
        major_km: 1.24,
        minor_km: 0.61,
        az_deg: 37.0,
        rms_us: Some(2.1),
        ambiguous: false,
        unchecked: false,
        sites: RX[..sites]
            .iter()
            .enumerate()
            .map(|(i, (site, ..))| Arrival {
                site: site.to_string(),
                t_us: t_us + 100 * i as i64,
                resid_us: -0.04,
                impulse: Some(4.5),
                bearing_deg: Some(41.0),
                bearing_resid_deg: Some(-1.26),
                bearing_left_out: i == 1,
            })
            .collect(),
    }
}

/// Sign events as another station would and run them through the app's projections.
pub fn feed(app: &App, events: Vec<(&str, Value)>, ts: i64) {
    let mut fake = Fake::new(vec![]);
    let db = app.db();
    let tx = db.unchecked_transaction().unwrap();
    for (kind, payload) in events {
        let e = fake.add(kind, payload, ts);
        for p in PROJECTIONS {
            p.apply(&tx, &e, ts).unwrap();
        }
    }
    tx.commit().unwrap();
}

/// The app with four receivers (tst-west's GPS unlocked), strikes 0a (checked, in
/// Backcountry), 0b (exact, near the hub), 0c (unchecked) and 0d (ambiguous, outside the
/// ring), CW0001 reading 0.3 mm, and the alerts that opens.
pub fn storm(dir: &Path) -> App {
    let cfg = config(dir);
    let app = App::new(cfg.clone()).unwrap();
    let now = now_ms();
    let ago_us = |min: i64| (now - min * 60_000) * 1000;
    let mut events: Vec<(&str, Value)> = RX
        .iter()
        .map(|(site, lat, lon)| {
            let gps = if *site == "tst-west" {
                "unlocked"
            } else {
                "locked"
            };
            let r = json!({"site": site, "lat": lat, "lon": lon, "sigma_us": 5.0, "gps": gps, "togas": []});
            ("lightning.toga", r)
        })
        .collect();
    let checked = strike("0a", ago_us(2), (35.38, -120.55), 4);
    let exact = Strike {
        rms_us: None,
        ..strike("0b", ago_us(9), (35.25, -120.70), 3)
    };
    let unchecked = Strike {
        unchecked: true,
        ..strike("0c", ago_us(20), (35.22, -120.78), 3)
    };
    let ambiguous = Strike {
        ambiguous: true,
        rms_us: None,
        ..strike("0d", ago_us(70), (35.00, -121.20), 3)
    };
    let all = [&checked, &exact, &unchecked, &ambiguous].map(strike_json);
    events.push(("lightning.strikes", json!({"strikes": all})));
    let gauge =
        json!({"station": "CW0001", "source": "aprs", "at_ms": now - 60_000, "rain_24h_mm": 0.3});
    events.push(("weather.rain", json!({"v": 1, "readings": [gauge]})));
    feed(&app, events, now);
    {
        let db = app.db();
        let due = watch::status(&db, &cfg, now).unwrap().due;
        alerts::update(&db, &due, &|_| false, now).unwrap();
    }
    app
}

/// Serve the app's pages on a free port; returns their base URL.
pub async fn serve(app: App) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, web::router(Arc::new(app))).await });
    base
}
