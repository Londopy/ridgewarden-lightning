//! The accuracy page: how well the network can locate strikes across the region (a map
//! shaded by the expected 95% ellipse), what the residuals say the timing really is,
//! which receiver the network can least afford to lose, each watch area's worst case,
//! and how located strikes compared with reference data.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::State;
use axum::response::Html;
use ridgewarden_client::util::{esc, now_ms, short_time};

use crate::agreement::summary;
use crate::app::App;
use crate::coverage::{BINS, area_worst, bin_label, inside, ring_summary, without_each};
use crate::geo::LatLon;
use crate::map::{self, Map};
use crate::solve::expected;
use crate::store;
use crate::web::{Fail, page, ring_of};

const WEEK_MS: i64 = 7 * 86_400_000;

fn legend() -> String {
    let mut b = String::from("<p class=\"key small\">Expected 95% error (semi-major axis):");
    for i in 0..=BINS.len() {
        let _ = write!(b, "<span class=cv{i}></span>{}", bin_label(i));
    }
    b.push_str("; unshaded where the receivers can't fix a point.</p>");
    b
}

fn agreements_section(app: &App) -> Result<String, Fail> {
    let list = store::agreements(&app.db(), 20)?;
    let mut b = String::from(
        "<h2>Compared with reference data</h2><p class=\"mute small\">The spec's measure is strikes within 3 km of \
         public lightning maps. Export a reference you may use as time,lat,lon lines and run \
         <code>ridgewarden-lightning compare --reference file.csv --source \"...\" --publish</code> on any site.</p>",
    );
    if list.is_empty() {
        b.push_str("<p class=mute>No comparisons logged yet.</p>");
        return Ok(b);
    }
    b.push_str("<ul class=plain>");
    for (ts, author, a) in list {
        let _ = write!(
            b,
            "<li><b>{}</b> <span class=\"mute small\">{} to {}, run by {} on {}</span><br><span class=small>{}</span></li>",
            esc(&a.source),
            esc(&short_time(a.from_ms)),
            esc(&short_time(a.to_ms)),
            esc(&author),
            esc(&short_time(ts)),
            esc(&summary(&a))
        );
    }
    b.push_str("</ul>");
    Ok(b)
}

fn timing_section(
    sites: &[(LatLon, f64)],
    v: f64,
    ring: &[LatLon],
    est: Option<(f64, usize)>,
    period: &str,
) -> String {
    let Some((sigma_us, n)) = est.filter(|e| e.1 >= 10) else {
        return "<p class=\"mute small\">Once ten or more strikes have been located by four or more receivers, \
                their residuals show what the timing error really is.</p>"
            .into();
    };
    let (lo, hi) = sites.iter().fold((f64::MAX, f64::MIN), |(lo, hi), s| {
        (lo.min(s.1 * 1e6), hi.max(s.1 * 1e6))
    });
    let configured = if (hi - lo).abs() < 0.05 {
        format!("{lo:.0} us")
    } else {
        format!("{lo:.0} to {hi:.0} us")
    };
    // strikes whose residuals are too big are refused by max_chi2 and never get here, so
    // a much larger measured value is a floor, not an estimate
    let amount = if sigma_us > 1.5 * hi {
        format!("at least <b>{sigma_us:.1} us</b>")
    } else {
        format!("about <b>{sigma_us:.1} us</b>")
    };
    let mut b = format!(
        "<p>The residuals of {n} strikes located by four or more receivers in {period} point to {amount} \
         of timing error per arrival; the receivers are configured for {configured}. \
         If those differ a lot, set <code>timing_sigma_us</code> to the measured value so the ellipses tell the truth, \
         and look again after a few storms."
    );
    let measured: Vec<(LatLon, f64)> = sites.iter().map(|s| (s.0, sigma_us * 1e-6)).collect();
    if let Some(s) = ring_summary(&measured, v, ring) {
        let _ = write!(
            b,
            " With that timing, the median inside the ring is {:.1} km.",
            s.median_km
        );
    }
    b.push_str("</p>");
    b
}

fn losses_section(app: &App, named: &[(String, LatLon, f64)], v: f64, ring: &[LatLon]) -> String {
    if named.len() < 4 {
        return String::new();
    }
    let mut b = String::from(
        "<h2>If one receiver is out</h2><p class=\"mute small\">The ring's expected accuracy with each receiver \
         missing, the most painful loss first.</p><div class=wrap><table><tr><th>Without</th>\
         <th>Median inside the ring</th><th>Worst inside the ring</th></tr>",
    );
    for (site, s) in without_each(named, v, ring) {
        let (m, w) = match s {
            Some(s) => (
                format!("{:.1} km", s.median_km),
                format!("{:.1} km", s.worst_km),
            ),
            None => ("can't locate".into(), String::new()),
        };
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{m}</td><td>{w}</td></tr>",
            esc(&app.cfg.label(&site))
        );
    }
    b.push_str("</table></div>");
    b
}

fn areas_section(app: &App, sites: &[(LatLon, f64)], v: f64, ring: &[LatLon]) -> String {
    if app.cfg.watch.is_empty() {
        return String::new();
    }
    let mut b = String::from(
        "<h2>Watch areas</h2><div class=wrap><table><tr><th>Area</th><th>Worst expected accuracy</th>\
         <th>Share inside the ring</th></tr>",
    );
    for a in &app.cfg.watch {
        let pts = a.samples();
        let worst = area_worst(sites, v, &pts)
            .map(|k| format!("{k:.1} km"))
            .unwrap_or_else(|| "can't locate".into());
        let share = pts.iter().filter(|p| inside(**p, ring)).count() as f64 / pts.len() as f64;
        let within = format!("{:.0}%", 100.0 * share);
        let _ = write!(
            b,
            "<tr><td>{}</td><td>{worst}</td><td>{within}</td></tr>",
            esc(&a.name)
        );
    }
    b.push_str("</table></div>");
    b
}

pub async fn page_handler(State(app): State<Arc<App>>) -> Result<Html<String>, Fail> {
    let now = now_ms();
    let v = app.cfg.velocity();
    let rx = store::receivers(&app.db(), now)?;
    let ring = ring_of(&rx);
    let (_, est) = store::fits(&app.db(), (now - WEEK_MS) * 1000)?;
    let named: Vec<(String, LatLon, f64)> = rx
        .iter()
        .map(|r| (r.site.clone(), r.pos, r.sigma_us * 1e-6))
        .collect();
    let sites: Vec<(LatLon, f64)> = named.iter().map(|n| (n.1, n.2)).collect();
    let mut b = format!(
        "<h1>Accuracy</h1><style>{}</style><p class=\"mute small\">How well the receivers that reported in the last \
         week can locate a strike, worked out from their positions and timing errors: the same calculation that \
         gives each strike its ellipse, done for every point on the map, assuming every receiver hears the strike.</p>",
        map::CSS
    );
    if rx.len() < 3 {
        let _ = write!(
            b,
            "<p class=mute>It takes three receivers to locate anything; {} reported in the last week.</p>",
            rx.len()
        );
    } else {
        let cov = |p: LatLon| expected(&sites, v, p).map(|e| e.major_km);
        let m = Map {
            receivers: &rx,
            strikes: &[],
            areas: &app.cfg.watch,
            now_us: now * 1000,
            ring: &ring,
            coverage: Some(&cov),
        };
        if let Some(svg) = map::render(&m, &|s| app.cfg.label(s)) {
            b.push_str(&svg);
            b.push_str(&legend());
        }
        match ring_summary(&sites, v, &ring) {
            Some(s) => {
                let _ = write!(
                    b,
                    "<p>Inside the ring of receivers the expected 95% error is <b>{:.1} km</b> at the median \
                     (best {:.1} km, worst {:.1} km). The spec's target is 1 to 3 km inside the ring with four to six \
                     sites.</p>",
                    s.median_km, s.best_km, s.worst_km
                );
            }
            None => b.push_str(
                "<p>The receivers are in a line, so there is no ring to speak of: add one off to the side.</p>",
            ),
        }
        let recent = store::strikes(&app.db(), (now - WEEK_MS) * 1000, i64::MAX, store::FIT_MAX)?;
        let period = if recent.len() == store::FIT_MAX {
            format!("the newest {} strikes", store::FIT_MAX)
        } else {
            "the last week".to_string()
        };
        b.push_str(&timing_section(&sites, v, &ring, est, &period));
        let mut axes: Vec<f64> = recent
            .iter()
            .filter(|s| inside(s.pos, &ring))
            .map(|s| s.major_km)
            .collect();
        if let Some(med) = ridgewarden_client::util::median(&mut axes) {
            let _ = write!(
                b,
                "<p>Strikes located inside the ring in {period}: {}, with a median 95% error of {med:.1} km.</p>",
                axes.len()
            );
        }
        b.push_str(&losses_section(&app, &named, v, &ring));
        b.push_str(&areas_section(&app, &sites, v, &ring));
    }
    b.push_str(&agreements_section(&app)?);
    Ok(page(&app, "/accuracy", "Accuracy", &b, false).await)
}
