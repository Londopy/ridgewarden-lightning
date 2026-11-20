//! The strike map: inline SVG on a flat km grid centered on the receivers (equirectangular,
//! which is accurate to well under 1% across a county), with no map tiles, so it works
//! with the internet down. Each strike is drawn with its 95% error ellipse.

use std::fmt::Write as _;

use ridgewarden_client::util::esc;

use crate::config::WatchArea;
use crate::coverage::{bin, bin_label};
use crate::geo::{EARTH_KM, LatLon, centroid, distance_km, local_km};
use crate::store::{Receiver, Strike};

pub const CSS: &str = "
svg.map{width:100%;height:auto;display:block;background:var(--card);border-radius:6px}
.map .area{fill:none;stroke:var(--mute);stroke-dasharray:6 4}
.map .lbl{fill:var(--fg);font-size:12px;paint-order:stroke;stroke:var(--card);stroke-width:3px}
.map .ring{fill:none;stroke:var(--fg);stroke-width:1;stroke-opacity:.45}
.map .cv{stroke:none;shape-rendering:crispEdges}.map .cv0,.key .cv0{fill:#cde2fb;background:#cde2fb}.map .cv1,.key .cv1{fill:#86b6ef;background:#86b6ef}
.map .cv2,.key .cv2{fill:#3987e5;background:#3987e5}.map .cv3,.key .cv3{fill:#1c5cab;background:#1c5cab}.map .cv4,.key .cv4{fill:#0d366b;background:#0d366b}
@media (prefers-color-scheme:dark){.map .cv0,.key .cv0{fill:#104281;background:#104281}.map .cv1,.key .cv1{fill:#1c5cab;background:#1c5cab}
.map .cv2,.key .cv2{fill:#2a78d6;background:#2a78d6}.map .cv3,.key .cv3{fill:#6da7ec;background:#6da7ec}.map .cv4,.key .cv4{fill:#b7d3f6;background:#b7d3f6}}.map .small{fill:var(--mute);font-size:11px}
.map .rx{fill:#2a7ab8;stroke:var(--bg)}
.map .ell{stroke-width:1.2}.map .dot{stroke:none}
.map .new .ell{fill:var(--red-bg);stroke:var(--red)}.map .new .dot{fill:var(--red)}
.map .hour .ell{fill:var(--yellow-bg);stroke:var(--yellow)}.map .hour .dot{fill:var(--yellow)}
.map .old .ell{fill:none;stroke:var(--mute)}.map .old .dot{fill:var(--mute)}
.map .bar{stroke:var(--fg);stroke-width:2}
@media (prefers-color-scheme:dark){main a{color:#8cc4ff}}
.key span{display:inline-block;width:10px;height:10px;border-radius:5px;margin:0 4px 0 12px}
";

const W: f64 = 800.0;
const H: f64 = 560.0;

pub struct Map<'a> {
    pub receivers: &'a [Receiver],
    pub strikes: &'a [Strike],
    pub areas: &'a [WatchArea],
    pub now_us: i64,
    /// the ring of receivers (their convex hull), outlined when not empty
    pub ring: &'a [LatLon],
    /// expected 95% semi-major axis (km) at a point, to shade the map by; the map then
    /// shows more of the country around the network
    pub coverage: Option<&'a (dyn Fn(LatLon) -> Option<f64> + Sync)>,
}

/// Shading cells, in SVG units.
const CELL: f64 = 16.0;

struct Frame {
    origin: LatLon,
    cx: f64,
    cy: f64,
    scale: f64,
}

impl Frame {
    fn px(&self, p: LatLon) -> (f64, f64) {
        let (x, y) = local_km(self.origin, p);
        (
            W / 2.0 + (x - self.cx) * self.scale,
            H / 2.0 - (y - self.cy) * self.scale,
        )
    }

    fn latlon(&self, px: f64, py: f64) -> LatLon {
        let x = (px - W / 2.0) / self.scale + self.cx;
        let y = (H / 2.0 - py) / self.scale + self.cy;
        let lat = self.origin.lat + (y / EARTH_KM).to_degrees();
        let lon =
            self.origin.lon + (x / (EARTH_KM * self.origin.lat.to_radians().cos())).to_degrees();
        LatLon::new(lat, lon)
    }
}

fn frame(m: &Map) -> Option<Frame> {
    let mut pts: Vec<LatLon> = m.receivers.iter().map(|r| r.pos).collect();
    pts.extend(m.areas.iter().flat_map(WatchArea::extent));
    let origin = centroid(&pts)
        .or_else(|| centroid(&m.strikes.iter().map(|s| s.pos).collect::<Vec<_>>()))?;
    // strikes far outside the network would squash it into a dot
    pts.extend(
        m.strikes
            .iter()
            // a huge ellipse (a strike far outside the network) would do the same
            .filter(|s| distance_km(origin, s.pos) < 300.0 && s.major_km <= 100.0)
            .flat_map(|s| {
                // both ends of the ellipse, so it isn't cut off at the edge
                let az = s.az_deg.to_radians();
                [
                    crate::geo::destination(s.pos, az, s.major_km),
                    crate::geo::destination(s.pos, az + std::f64::consts::PI, s.major_km),
                ]
            }),
    );
    let xy: Vec<(f64, f64)> = pts.iter().map(|p| local_km(origin, *p)).collect();
    let (mut x0, mut x1, mut y0, mut y1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for (x, y) in &xy {
        (x0, x1, y0, y1) = (x0.min(*x), x1.max(*x), y0.min(*y), y1.max(*y));
    }
    if xy.is_empty() {
        (x0, x1, y0, y1) = (0.0, 0.0, 0.0, 0.0);
    }
    let zoom = if m.coverage.is_some() { 2.2 } else { 1.2 };
    let span_x = (x1 - x0).max(20.0) * zoom;
    let span_y = (y1 - y0).max(20.0) * zoom;
    Some(Frame {
        origin,
        cx: (x0 + x1) / 2.0,
        cy: (y0 + y1) / 2.0,
        scale: (W / span_x).min(H / span_y),
    })
}

fn scale_bar(f: &Frame, out: &mut String) {
    let want = W / 5.0 / f.scale;
    let km = [1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0]
        .into_iter()
        .rev()
        .find(|k| *k <= want)
        .unwrap_or(1.0);
    let len = km * f.scale;
    let _ = write!(
        out,
        "<line class=bar x1=20 y1={y:.0} x2={x2:.1} y2={y:.0} /><text class=small x=20 y={ty:.0}>{km} km</text>\
         <line class=bar x1={nx} y1=44 x2={nx} y2=20 /><text class=lbl x={tx} y=58>N</text>",
        y = H - 20.0,
        x2 = 20.0 + len,
        ty = H - 28.0,
        nx = W - 24.0,
        tx = W - 28.0,
    );
}

fn age_class(now_us: i64, t_us: i64) -> &'static str {
    match now_us - t_us {
        a if a < 15 * 60_000_000 => "new",
        a if a < 3_600_000_000 => "hour",
        _ => "old",
    }
}

fn shade(f: &Frame, cov: &dyn Fn(LatLon) -> Option<f64>, s: &mut String) {
    let (nx, ny) = ((W / CELL) as usize, (H / CELL) as usize);
    for i in 0..nx {
        for j in 0..ny {
            let (x, y) = (i as f64 * CELL, j as f64 * CELL);
            let p = f.latlon(x + CELL / 2.0, y + CELL / 2.0);
            let Some(km) = cov(p) else { continue };
            let b = bin(km);
            let _ = write!(
                s,
                "<rect class=\"cv cv{b}\" x={x} y={y} width={CELL} height={CELL}><title>{:.3}, {:.3}: about {km:.1} km ({})</title></rect>",
                p.lat,
                p.lon,
                bin_label(b)
            );
        }
    }
}

/// The map as an SVG string, or None when there is nothing to place on it yet.
pub fn render(m: &Map, label: &dyn Fn(&str) -> String) -> Option<String> {
    let f = frame(m)?;
    let mut s = format!(
        "<svg class=map viewBox=\"0 0 {W} {H}\" role=img aria-label=\"Map of strikes and receivers\">"
    );
    if let Some(cov) = m.coverage {
        shade(&f, cov, &mut s);
    }
    if !m.ring.is_empty() {
        let pts: Vec<String> = m
            .ring
            .iter()
            .map(|p| {
                let (x, y) = f.px(*p);
                format!("{x:.1},{y:.1}")
            })
            .collect();
        let _ = write!(
            s,
            "<polygon class=ring points=\"{}\"><title>ring of receivers</title></polygon>",
            pts.join(" ")
        );
    }
    for a in m.areas {
        let (lx, ly) = match (a.center, a.radius_km) {
            (Some(c), Some(r)) => {
                let (x, y) = f.px(LatLon::new(c[0], c[1]));
                let _ = write!(
                    s,
                    "<circle class=area cx={x:.1} cy={y:.1} r={:.1} />",
                    r * f.scale
                );
                (x, y - r * f.scale - 4.0)
            }
            _ => {
                let pts: Vec<(f64, f64)> = a.corners().into_iter().map(|p| f.px(p)).collect();
                let list: Vec<String> = pts.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect();
                let _ = write!(s, "<polygon class=area points=\"{}\"/>", list.join(" "));
                let top = pts.iter().map(|p| p.1).fold(f64::MAX, f64::min);
                let (l, r) = pts
                    .iter()
                    .fold((f64::MAX, f64::MIN), |(l, r), p| (l.min(p.0), r.max(p.0)));
                ((l + r) / 2.0, top - 4.0)
            }
        };
        let _ = write!(
            s,
            "<text class=small x={lx:.1} y={ly:.1} text-anchor=middle>{}</text>",
            esc(&a.name)
        );
    }
    // oldest first, so the newest strikes draw on top
    for st in m.strikes.iter().rev() {
        let (x, y) = f.px(st.pos);
        let _ = write!(
            s,
            "<a href=\"/strike?id={id}\"><g class={cls}><title>{when}, {lat:.4} {lon:.4}, 95% ellipse {a:.1} x {b:.1} km, {n} sites</title>\
             <ellipse class=ell cx={x:.1} cy={y:.1} rx={rx:.1} ry={ry:.1} transform=\"rotate({rot:.1} {x:.1} {y:.1})\"/>\
             <circle class=dot cx={x:.1} cy={y:.1} r=3 /></g></a>",
            id = esc(&st.id),
            cls = age_class(m.now_us, st.t_us),
            when = esc(&crate::web::strike_time(st.t_us)),
            lat = st.pos.lat,
            lon = st.pos.lon,
            a = st.major_km,
            b = st.minor_km,
            n = st.sites.len(),
            rx = (st.major_km * f.scale).max(2.0),
            ry = (st.minor_km * f.scale).max(2.0),
            // SVG's x axis points east; the major axis' bearing is measured from north
            rot = st.az_deg - 90.0,
        );
    }
    for r in m.receivers {
        let (x, y) = f.px(r.pos);
        let _ = write!(
            s,
            "<path class=rx d=\"M{x:.1},{:.1} l6,10 h-12 z\"><title>receiver {name}</title></path>\
             <text class=lbl x={:.1} y={:.1}>{name}</text>",
            y - 7.0,
            x + 9.0,
            y + 4.0,
            name = esc(&label(&r.site)),
        );
    }
    scale_bar(&f, &mut s);
    s.push_str("</svg>");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Arrival;

    fn strike(id: &str, lat: f64, lon: f64, t_us: i64) -> Strike {
        Strike {
            id: id.into(),
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

    fn rx(site: &str, lat: f64, lon: f64) -> Receiver {
        Receiver {
            site: site.into(),
            pos: LatLon::new(lat, lon),
            sigma_us: 20.0,
            gps: "locked".into(),
            last_ts: 0,
            last_toga_us: None,
            hour_n: 0,
            hour_dropped: 0,
            error: None,
            clock_s: None,
        }
    }

    #[test]
    fn empty_map_is_none() {
        let m = Map {
            receivers: &[],
            strikes: &[],
            areas: &[],
            now_us: 0,
            ring: &[],
            coverage: None,
        };
        assert!(render(&m, &|s| s.to_string()).is_none());
    }

    #[test]
    fn north_up_east_right_and_ellipses_turned_by_bearing() {
        let rxs = [rx("tst-a", 35.0, -121.0), rx("tst-b", 35.5, -120.5)];
        let st = [strike("n", 35.4, -120.75, 0), strike("e", 35.25, -120.6, 0)];
        let m = Map {
            receivers: &rxs,
            strikes: &st,
            areas: &[],
            now_us: 0,
            ring: &[],
            coverage: None,
        };
        let f = frame(&m).unwrap();
        let (_, yn) = f.px(st[0].pos);
        let (xe, _) = f.px(st[1].pos);
        let (xa, ya) = f.px(rxs[0].pos);
        assert!(yn < ya && xe > xa);
        let svg = render(&m, &|s| s.to_uppercase()).unwrap();
        // a north-south major axis is a 90 degree turn of SVG's east-west rx
        assert!(svg.contains("rotate(-90.0"));
        assert!(svg.contains(">TST-A</text>"));
        assert!(svg.contains("href=\"/strike?id=n\""));
        // an unquoted value right before "/>" swallows the slash and leaves the tag open
        assert!(
            svg.match_indices("/>")
                .all(|(i, _)| matches!(&svg[i - 1..i], " " | "\""))
        );
    }

    #[test]
    fn labels_are_escaped() {
        let rxs = [rx("tst-a", 35.0, -121.0)];
        let m = Map {
            receivers: &rxs,
            strikes: &[],
            areas: &[],
            now_us: 0,
            ring: &[],
            coverage: None,
        };
        let svg = render(&m, &|_| "<b>".into()).unwrap();
        assert!(svg.contains("&lt;b&gt;") && !svg.contains("<b>"));
    }
}
