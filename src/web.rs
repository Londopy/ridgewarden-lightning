//! The map page, strike history, one page per strike, alert acknowledgements, and the
//! strikes as GeoJSON and CSV. The accuracy page is in `accuracy`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use ridgewarden_client::alerts;
use ridgewarden_client::ui::{Color, Page, pill};
use ridgewarden_client::util::{esc, human_duration, ms_to_rfc3339, now_ms, short_time};
use serde_json::{Value, json};

use crate::app::{App, NAME};
use crate::coverage;
use crate::geo::LatLon;
use crate::map::{self, Map};
use crate::rain::Verdict;
use crate::store::{self, Fits, Receiver, SiteFit, Strike};
use crate::watch;

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(map_page))
        .route("/strikes", get(history_page))
        .route("/strike", get(strike_page))
        .route("/accuracy", get(crate::accuracy::page_handler))
        .route("/ack/{token}", get(ack_page).post(ack_post))
        .route("/api/strikes.geojson", get(geojson))
        .route("/api/strikes.csv", get(csv))
        .route("/api/receivers", get(receivers_json))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

pub struct Fail(anyhow::Error);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        tracing::error!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for Fail {
    fn from(e: E) -> Self {
        Fail(e.into())
    }
}

/// "2026-11-19 14:02:31.482 UTC"
pub fn strike_time(t_us: i64) -> String {
    let ms = t_us.div_euclid(1000);
    let s = ms_to_rfc3339(ms.div_euclid(1000) * 1000);
    format!("{} {}.{:03} UTC", &s[..10], &s[11..19], ms.rem_euclid(1000))
}

/// A receiver with no report for this long counts as silent; it reports once a minute
/// even with nothing to say.
const SILENT_MS: i64 = 180_000;

/// Residuals this far from zero (or the site's own timing error, if bigger), as a median
/// over enough strikes, mean the site's timing is off rather than noisy.
const OFFSET_US: f64 = 5.0;
/// Arrival times this far from the reporting computer's clock mean one of them is wrong.
const CLOCK_S: f64 = 60.0;

pub fn receiver_state(r: &Receiver, fit: Option<&SiteFit>, now: i64) -> (Color, String) {
    if now - r.last_ts > SILENT_MS {
        return (
            Color::Red,
            format!("silent for {}", human_duration(now - r.last_ts)),
        );
    }
    if r.gps == "unlocked" {
        return (Color::Red, "GPS not locked, arrivals dropped".into());
    }
    if r.error.is_some() {
        return (Color::Yellow, "can't read vttoga output".into());
    }
    if let Some(c) = r.clock_s.filter(|c| c.abs() > CLOCK_S) {
        return (
            Color::Yellow,
            format!("arrival times {c:+.0} s off the clock"),
        );
    }
    if let Some(m) = fit
        .filter(|f| f.with_residuals >= 10)
        .and_then(|f| f.median_us)
        .filter(|m| m.abs() > OFFSET_US.max(r.sigma_us))
    {
        return (Color::Yellow, format!("timing looks {m:+.0} us off"));
    }
    (Color::Green, "receiving".into())
}

/// The ring of receivers: the convex hull of every receiver that reported this week.
pub fn ring_of(rx: &[Receiver]) -> Vec<LatLon> {
    coverage::hull(&rx.iter().map(|r| r.pos).collect::<Vec<_>>())
}

pub async fn page(app: &App, active: &str, title: &str, body: &str, refresh: bool) -> Html<String> {
    let who = match app.whoami().await {
        Ok(w) if w.node.is_empty() => w.call,
        Ok(w) => format!("{} on {}", w.call, w.node),
        Err(_) => "core unreachable".into(),
    };
    let p = Page {
        app: NAME,
        nav: &[
            ("/", "Map"),
            ("/strikes", "Strikes"),
            ("/accuracy", "Accuracy"),
        ],
        active,
        who: &who,
        refresh,
    };
    Html(p.render(title, body))
}

fn open_alerts(app: &App) -> Result<String, Fail> {
    let open = alerts::open_alerts(&app.db())?;
    let mut b = String::new();
    if open.is_empty() {
        return Ok(b);
    }
    b.push_str("<h2>Open alerts</h2><ul class=plain>");
    for a in open {
        let color = if a.severity == "critical" {
            Color::Red
        } else {
            Color::Yellow
        };
        let _ = write!(
            b,
            "<li>{} {} <span class=\"mute small\">since {}{}</span><br><span class=small>{}</span></li>",
            pill(color, &a.severity),
            esc(&a.title),
            esc(&short_time(a.opened_ms)),
            if a.acked { ", acknowledged" } else { "" },
            esc(&a.text)
        );
    }
    b.push_str("</ul>");
    Ok(b)
}

fn areas_section(app: &App, now: i64) -> Result<String, Fail> {
    if app.cfg.watch.is_empty() {
        return Ok(String::new());
    }
    let views = watch::views(&app.db(), &app.cfg, now)?;
    let mut b = format!(
        "<h2>Watch areas</h2><div class=wrap><table><tr><th>Area</th><th>Strikes, last {} min</th><th>Rain</th></tr>",
        app.cfg.watch_window_min
    );
    for v in &views {
        let rain = match &v.rain {
            Verdict::Dry { .. } => pill(Color::Red, "dry"),
            Verdict::Wet { .. } => pill(Color::Green, "wet"),
            Verdict::Missing(_) => pill(Color::Unknown, "no data"),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}{}</td><td>{rain} {}</td></tr>",
            esc(&v.area.name),
            if v.capped { "more than " } else { "" },
            v.strikes.len(),
            esc(&v.rain.text(app.cfg.rain.as_ref()))
        );
    }
    b.push_str("</table></div>");
    Ok(b)
}

fn receivers_table(app: &App, rx: &[Receiver], fits: &Fits, now: i64) -> String {
    let mut b = String::from("<h2>Receivers</h2>");
    if rx.is_empty() {
        b.push_str(
            "<p class=mute>No receiver has reported in the last week. Each site with a VLF receiver runs this app \
             with a [receiver] section; its reports reach here through the core.</p>",
        );
        return b;
    }
    b.push_str(
        "<div class=wrap><table><tr><th>Site</th><th>State</th><th>GPS</th><th>Sferics, last hour</th>\
         <th>Last sferic</th><th>In strikes, 7 d</th><th>Timing</th><th>Position</th></tr>",
    );
    for r in rx {
        let fit = fits.get(&r.site);
        let (color, why) = receiver_state(r, fit, now);
        let part = fit.map_or(0, |f| f.strikes);
        let median = fit
            .and_then(|f| f.median_us)
            .map(|m| format!("<br><span class=\"mute small\">median residual {m:+.1} us</span>"))
            .unwrap_or_default();
        let err = r
            .error
            .as_ref()
            .map(|e| format!("<br><span class=\"mute small\">{}</span>", esc(e)))
            .unwrap_or_default();
        let dropped = if r.hour_dropped > 0 {
            format!(
                " <span class=\"mute small\">({} dropped)</span>",
                r.hour_dropped
            )
        } else {
            String::new()
        };
        let last = r
            .last_toga_us
            .map(|t| format!("{} ago", human_duration(now - t / 1000)))
            .unwrap_or_else(|| "none in 2 days".into());
        let gps = match r.gps.as_str() {
            "unchecked" => "not checked",
            g => g,
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}{err}</td><td>{}</td><td>{}{dropped}</td><td>{}</td><td>{part}</td>\
             <td>{:.0} us{median}</td><td class=small>{:.4}, {:.4}</td></tr>",
            esc(&app.cfg.label(&r.site)),
            pill(color, &why),
            esc(gps),
            r.hour_n,
            esc(&last),
            r.sigma_us,
            r.pos.lat,
            r.pos.lon
        );
    }
    b.push_str("</table></div>");
    b
}

fn strike_rows(app: &App, strikes: &[Strike], ring: &[LatLon], b: &mut String) {
    b.push_str(
        "<div class=wrap><table><tr><th>Time</th><th>Position</th><th>95% ellipse</th><th>Sites</th>\
         <th>Fit</th><th>Areas</th></tr>",
    );
    for s in strikes {
        let fit = match s.rms_us {
            Some(r) => format!("{r:.1} us RMS"),
            None => "exact (3 sites)".into(),
        };
        let areas: Vec<String> = app
            .cfg
            .watch
            .iter()
            .filter(|a| a.contains(s.pos))
            .map(|a| esc(&a.name))
            .collect();
        let _ = write!(
            b,
            "<tr><td><a href=\"/strike?id={}\">{}</a></td><td>{:.4}, {:.4}{}</td><td>{:.1} x {:.1} km</td>\
             <td>{}</td><td>{fit}</td><td>{}</td></tr>",
            esc(&s.id),
            esc(&strike_time(s.t_us)),
            s.pos.lat,
            s.pos.lon,
            note(s, ring),
            s.major_km,
            s.minor_km,
            s.sites.len(),
            areas.join(", ")
        );
    }
    b.push_str("</table></div>");
}

/// "(ambiguous)" or "(outside the ring)" after a position, where it applies.
fn note(s: &Strike, ring: &[LatLon]) -> &'static str {
    if s.ambiguous {
        " <span class=\"mute small\">(ambiguous)</span>"
    } else if !ring.is_empty() && !coverage::inside(s.pos, ring) {
        " <span class=\"mute small\">(outside the ring)</span>"
    } else {
        ""
    }
}

fn hours(q: &HashMap<String, String>) -> i64 {
    match q.get("h").map(String::as_str) {
        Some("1") => 1,
        Some("24") => 24,
        Some("168") => 168,
        _ => 3,
    }
}

async fn map_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let h = hours(&q);
    let from_us = (now - h * 3_600_000) * 1000;
    let rx = store::receivers(&app.db(), now)?;
    let ring = ring_of(&rx);
    let (fits, _) = store::fits(&app.db(), (now - 7 * 86_400_000) * 1000)?;
    let strikes = store::strikes(&app.db(), from_us, i64::MAX, MAP_MAX)?;
    let mut b = format!(
        "<h1>Lightning</h1><p class=\"mute small\">Strikes located from the arrival times of each strike's radio \
         pulse (sferic) at the VLF receivers. Ellipses show where each strike is with 95% confidence.</p><style>{}</style>",
        map::CSS
    );
    b.push_str(&open_alerts(&app)?);
    let _ = write!(
        b,
        "<h2>Strikes, last {}</h2><p class=small>",
        if h == 1 {
            "hour".into()
        } else {
            format!("{h} hours")
        }
    );
    for (v, label) in [(1, "1 h"), (3, "3 h"), (24, "24 h"), (168, "7 d")] {
        if v == h {
            let _ = write!(b, "<b>{label}</b> ");
        } else {
            let _ = write!(b, "<a href=\"/?h={v}\">{label}</a> ");
        }
    }
    b.push_str("</p>");
    let m = Map {
        receivers: &rx,
        strikes: &strikes,
        areas: &app.cfg.watch,
        now_us: now * 1000,
        ring: &ring,
        coverage: None,
    };
    match map::render(&m, &|s| app.cfg.label(s)) {
        Some(svg) => {
            b.push_str(&svg);
            b.push_str(
                "<p class=\"key small\"><span style=\"background:var(--red)\"></span>last 15 min\
                 <span style=\"background:var(--yellow)\"></span>last hour\
                 <span style=\"background:var(--mute)\"></span>older; triangles are receivers, the solid line their ring, dashed lines watch areas</p>",
            );
        }
        None => b.push_str("<p class=mute>Nothing to map yet: no receiver has reported and no watch area is set.</p>"),
    }
    if strikes.len() == MAP_MAX {
        let _ = write!(
            b,
            "<p>More than {MAP_MAX} strikes in this period; the map shows the latest {MAP_MAX}."
        );
    } else {
        let _ = write!(b, "<p>{} strikes in this period.", strikes.len());
    }
    if !app.cfg.solver.enabled {
        b.push_str(" <span class=\"mute small\">This site doesn't run the solver; strikes come from the site that does.</span>");
    }
    b.push_str("</p>");
    b.push_str(&areas_section(&app, now)?);
    b.push_str(&receivers_table(&app, &rx, &fits, now));
    if !strikes.is_empty() {
        b.push_str("<h2>Latest strikes</h2>");
        strike_rows(&app, &strikes[..strikes.len().min(10)], &ring, &mut b);
        b.push_str("<p><a href=\"/strikes\">All strikes</a></p>");
    }
    Ok(page(&app, "/", "Lightning", &b, true).await)
}

const PAGE: usize = 100;
/// Most strikes drawn on the map at once.
const MAP_MAX: usize = 1000;

async fn history_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Html<String>, Fail> {
    let before = q
        .get("before")
        .and_then(|b| b.parse::<i64>().ok())
        .unwrap_or(i64::MAX);
    let strikes = store::strikes(&app.db(), i64::MIN, before, PAGE)?;
    let ring = ring_of(&store::receivers(&app.db(), now_ms())?);
    let mut b = String::from("<h1>Strikes</h1>");
    if strikes.is_empty() {
        b.push_str("<p class=mute>No strikes located yet.</p>");
    } else {
        strike_rows(&app, &strikes, &ring, &mut b);
    }
    if strikes.len() == PAGE
        && let Some(last) = strikes.last()
    {
        let _ = write!(
            b,
            "<p><a href=\"/strikes?before={}\">Older</a></p>",
            last.t_us
        );
    }
    b.push_str(
        "<p class=\"mute small\">The last 24 hours also as <a href=\"/api/strikes.geojson?h=24\">GeoJSON</a> \
         and <a href=\"/api/strikes.csv?h=24\">CSV</a> (h=1, 3, 24 or 168).</p>",
    );
    Ok(page(&app, "/strikes", "Strikes", &b, false).await)
}

async fn strike_page(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let id = q.get("id").cloned().unwrap_or_default();
    let Some(s) = store::strike(&app.db(), &id)? else {
        let body =
            "<h1>Not found</h1><p>No strike with that id. <a href=\"/strikes\">All strikes</a></p>";
        return Ok((
            StatusCode::NOT_FOUND,
            page(&app, "", "Not found", body, false).await,
        )
            .into_response());
    };
    let now = now_ms();
    let rx = store::receivers(&app.db(), now)?;
    let ring = ring_of(&rx);
    let mut b = format!(
        "<h1>Strike at {}</h1><style>{}</style><p>{:.5}, {:.5}. The strike is inside an ellipse {:.2} km by {:.2} km \
         (semi-axes), its long axis at {:.0} degrees from north, with 95% confidence, if each receiver's timing is as good \
         as configured.</p>",
        esc(&strike_time(s.t_us)),
        map::CSS,
        s.pos.lat,
        s.pos.lon,
        s.major_km,
        s.minor_km,
        s.az_deg
    );
    match s.rms_us {
        Some(r) => {
            let _ = write!(
                b,
                "<p>{} receivers: more than the three unknowns (position and time), so the residuals below show how \
                 well the arrivals fit one strike: {r:.1} us RMS.</p>",
                s.sites.len()
            );
        }
        None => b.push_str(
            "<p>Three receivers: exactly as many arrival times as unknowns, so the fit is exact and nothing checks it. \
             The ellipse comes from the receivers' geometry and timing alone.</p>",
        ),
    }
    if s.ambiguous {
        b.push_str(
            "<p class=banner>Ambiguous: another position fits these three arrival times just as well. \
             A fourth receiver would settle it.</p>",
        );
    } else if !ring.is_empty() && !coverage::inside(s.pos, &ring) {
        b.push_str(
            "<p class=banner>Outside the ring of receivers. Accuracy falls off out here, and the real \
             uncertainty bends away from the network more than the ellipse shows.</p>",
        );
    }
    let near: Vec<Receiver> = rx
        .into_iter()
        .filter(|r| s.sites.iter().any(|a| a.site == r.site))
        .collect();
    let m = Map {
        receivers: &near,
        strikes: std::slice::from_ref(&s),
        areas: &app.cfg.watch,
        now_us: now * 1000,
        ring: &ring,
        coverage: None,
    };
    if let Some(svg) = map::render(&m, &|x| app.cfg.label(x)) {
        b.push_str(&svg);
    }
    b.push_str("<h2>Arrivals</h2><div class=wrap><table><tr><th>Site</th><th>Arrival</th><th>After the strike</th><th>Residual</th></tr>");
    for a in &s.sites {
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{}</td><td>{} us</td><td>{:.1} us</td></tr>",
            esc(&app.cfg.label(&a.site)),
            esc(&strike_time(a.t_us)),
            a.t_us - s.t_us,
            // round first so a tiny negative residual doesn't show as "-0.0"
            (a.resid_us * 10.0).round() / 10.0 + 0.0
        );
    }
    b.push_str("</table></div><p><a href=\"/strikes\">All strikes</a></p>");
    Ok(page(&app, "", "Strike", &b, false).await.into_response())
}

async fn ack_page(State(app): State<Arc<App>>, Path(token): Path<String>) -> Html<String> {
    // a GET only shows the button, so link previews in chat apps can't acknowledge anything
    let token: String = token.chars().filter(char::is_ascii_alphanumeric).collect();
    let b = format!(
        "<h1>Acknowledge alert</h1><p>Acknowledging stops repeats of this alert. You'll still hear \
         when it's resolved.</p><form method=post action=\"/ack/{token}\"><button type=submit>Acknowledge</button></form>"
    );
    page(&app, "", "Acknowledge", &b, false).await
}

async fn ack_post(
    State(app): State<Arc<App>>,
    Path(token): Path<String>,
) -> Result<Response, Fail> {
    let done = alerts::ack(&app.db(), &token, now_ms())?;
    let (code, b) = match done {
        Some(title) => (
            StatusCode::OK,
            format!("<h1>Acknowledged</h1><p>{}</p><p><a href=\"/\">Map</a></p>", esc(&title)),
        ),
        None => (
            StatusCode::NOT_FOUND,
            "<h1>Nothing to acknowledge</h1><p>That alert is already resolved, or the link is wrong.</p>".to_string(),
        ),
    };
    Ok((code, page(&app, "", "Acknowledge", &b, false).await).into_response())
}

/// Strikes as a GeoJSON FeatureCollection of points, ellipse and fit in the properties.
pub fn feature_collection(strikes: &[Strike]) -> Value {
    json!({
        "type": "FeatureCollection",
        "features": strikes.iter().map(|s| json!({
            "type": "Feature",
            "id": s.id,
            "geometry": {"type": "Point", "coordinates": [s.pos.lon, s.pos.lat]},
            "properties": {
                "time": strike_time(s.t_us), "t_us": s.t_us,
                "major_km": s.major_km, "minor_km": s.minor_km, "az_deg": s.az_deg,
                "confidence": 0.95, "rms_us": s.rms_us, "ambiguous": s.ambiguous,
                "sites": s.sites.iter().map(|a| &a.site).collect::<Vec<_>>(),
            },
        })).collect::<Vec<_>>(),
    })
}

async fn geojson(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let from_us = (now_ms() - hours(&q) * 3_600_000) * 1000;
    let strikes = store::strikes(&app.db(), from_us, i64::MAX, 5000)?;
    Ok((
        [(header::CONTENT_TYPE, "application/geo+json")],
        feature_collection(&strikes).to_string(),
    )
        .into_response())
}

/// Strikes as CSV, one per line, oldest first: for spreadsheets and the personal
/// weather model.
pub fn csv_text(strikes: &[Strike]) -> String {
    let mut out =
        String::from("time_utc,t_us,lat,lon,major_km,minor_km,az_deg,sites,rms_us,ambiguous\n");
    for s in strikes.iter().rev() {
        let t = ms_to_rfc3339(s.t_us.div_euclid(1000));
        let _ = writeln!(
            out,
            "{},{},{:.6},{:.6},{:.3},{:.3},{:.1},{},{},{}",
            t,
            s.t_us,
            s.pos.lat,
            s.pos.lon,
            s.major_km,
            s.minor_km,
            s.az_deg,
            s.sites.len(),
            s.rms_us.map(|r| format!("{r:.2}")).unwrap_or_default(),
            s.ambiguous
        );
    }
    out
}

async fn csv(
    State(app): State<Arc<App>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, Fail> {
    let from_us = (now_ms() - hours(&q) * 3_600_000) * 1000;
    let strikes = store::strikes(&app.db(), from_us, i64::MAX, 50_000)?;
    Ok((
        [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
        csv_text(&strikes),
    )
        .into_response())
}

async fn receivers_json(State(app): State<Arc<App>>) -> Result<Json<Value>, Fail> {
    let now = now_ms();
    let rx = store::receivers(&app.db(), now)?;
    let (fits, _) = store::fits(&app.db(), (now - 7 * 86_400_000) * 1000)?;
    Ok(Json(json!({
        "generated": now,
        "receivers": rx.iter().map(|r| json!({
            "site": r.site, "lat": r.pos.lat, "lon": r.pos.lon, "sigma_us": r.sigma_us,
            "gps": r.gps, "state": receiver_state(r, fits.get(&r.site), now).1,
            "last_report_ms": r.last_ts, "last_sferic_us": r.last_toga_us,
            "sferics_last_hour": r.hour_n, "dropped_last_hour": r.hour_dropped, "error": r.error,
            "clock_s": r.clock_s,
            "strikes_7d": fits.get(&r.site).map_or(0, |f| f.strikes),
            "median_residual_us": fits.get(&r.site).and_then(|f| f.median_us),
        })).collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strike_time_keeps_milliseconds() {
        assert_eq!(
            strike_time(1_795_000_000_123_456),
            "2026-11-18 11:06:40.123 UTC"
        );
    }

    #[test]
    fn silent_and_unlocked_receivers_are_red() {
        let mut r = Receiver {
            site: "tst-a".into(),
            pos: crate::geo::LatLon::new(35.0, -120.0),
            sigma_us: 20.0,
            gps: "locked".into(),
            last_ts: 1_000_000,
            last_toga_us: None,
            hour_n: 0,
            hour_dropped: 0,
            error: None,
            clock_s: None,
        };
        assert_eq!(receiver_state(&r, None, 1_060_000).0, Color::Green);
        assert_eq!(
            receiver_state(&r, None, 1_000_000 + SILENT_MS + 1).0,
            Color::Red
        );
        // consistently late residuals flag the site, once there are enough of them
        let mut fit = SiteFit {
            strikes: 30,
            median_us: Some(35.0),
            with_residuals: 5,
        };
        assert_eq!(receiver_state(&r, Some(&fit), 1_060_000).0, Color::Green);
        fit.with_residuals = 20;
        assert_eq!(
            receiver_state(&r, Some(&fit), 1_060_000),
            (Color::Yellow, "timing looks +35 us off".into())
        );
        fit.median_us = Some(-12.0);
        assert_eq!(
            receiver_state(&r, Some(&fit), 1_060_000).0,
            Color::Green,
            "within its 20 us"
        );
        r.clock_s = Some(-3600.0);
        assert_eq!(receiver_state(&r, None, 1_060_000).0, Color::Yellow);
        r.gps = "unlocked".into();
        assert_eq!(receiver_state(&r, None, 1_060_000).0, Color::Red);
    }

    #[test]
    fn csv_is_oldest_first_with_a_header() {
        let s = |t_us: i64| Strike {
            id: "ab".into(),
            t_us,
            pos: LatLon::new(35.1, -120.2),
            major_km: 1.0,
            minor_km: 0.5,
            az_deg: 10.0,
            rms_us: None,
            ambiguous: false,
            sites: vec![],
        };
        let text = csv_text(&[s(2_000_000), s(1_000_000)]);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("time_utc,t_us,lat,lon"));
        assert!(lines[1].contains(",1000000,35.100000,-120.200000,"));
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn geojson_is_lon_lat() {
        let s = Strike {
            id: "ab".into(),
            t_us: 0,
            pos: crate::geo::LatLon::new(35.1, -120.2),
            major_km: 1.0,
            minor_km: 0.5,
            az_deg: 0.0,
            rms_us: None,
            ambiguous: false,
            sites: vec![],
        };
        let v = feature_collection(&[s]);
        assert_eq!(
            v["features"][0]["geometry"]["coordinates"],
            json!([-120.2, 35.1])
        );
    }
}
