//! Where the network can locate strikes, and how well: the 95% ellipse a strike would get
//! at each point if every receiver heard it, from the same covariance the solver
//! reports (`solve::expected`). Inside the ring of receivers (their convex hull) the
//! ellipse is small and round; outside it stretches along the line back to the network.

use crate::geo::{LatLon, centroid, in_polygon, local_km};
use crate::solve::{Rx, expected};

/// Upper edges of the accuracy classes, km (95% semi-major axis); beyond the last is
/// the worst class.
pub const BINS: [f64; 4] = [1.0, 3.0, 10.0, 30.0];

pub fn bin(km: f64) -> usize {
    BINS.iter().position(|b| km <= *b).unwrap_or(BINS.len())
}

pub fn bin_label(i: usize) -> String {
    match i {
        0 => format!("under {} km", BINS[0]),
        i if i < BINS.len() => format!("{} to {} km", BINS[i - 1], BINS[i]),
        _ => format!("over {} km", BINS[BINS.len() - 1]),
    }
}

fn cross(o: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - o.0) * (b.1 - o.1) - (a.1 - o.1) * (b.0 - o.0)
}

/// The convex hull of the receivers (Andrew's monotone chain on a local km grid),
/// counter-clockwise; empty with fewer than three distinct points.
pub fn hull(pts: &[LatLon]) -> Vec<LatLon> {
    let Some(origin) = centroid(pts) else {
        return vec![];
    };
    let mut xy: Vec<((f64, f64), LatLon)> =
        pts.iter().map(|p| (local_km(origin, *p), *p)).collect();
    xy.sort_by(|a, b| a.0.0.total_cmp(&b.0.0).then(a.0.1.total_cmp(&b.0.1)));
    xy.dedup_by(|a, b| (a.0.0 - b.0.0).abs() < 1e-9 && (a.0.1 - b.0.1).abs() < 1e-9);
    if xy.len() < 3 {
        return vec![];
    }
    let mut lower: Vec<((f64, f64), LatLon)> = vec![];
    for p in &xy {
        while lower.len() >= 2
            && cross(lower[lower.len() - 2].0, lower[lower.len() - 1].0, p.0) <= 0.0
        {
            lower.pop();
        }
        lower.push(*p);
    }
    let mut upper: Vec<((f64, f64), LatLon)> = vec![];
    for p in xy.iter().rev() {
        while upper.len() >= 2
            && cross(upper[upper.len() - 2].0, upper[upper.len() - 1].0, p.0) <= 0.0
        {
            upper.pop();
        }
        upper.push(*p);
    }
    lower.pop();
    upper.pop();
    let out: Vec<LatLon> = lower.into_iter().chain(upper).map(|p| p.1).collect();
    if out.len() < 3 { vec![] } else { out }
}

pub fn inside(p: LatLon, hull: &[LatLon]) -> bool {
    hull.len() >= 3 && in_polygon(p, hull)
}

/// Outside the ring, where accuracy falls off; nothing is while there is no ring.
pub fn outside(p: LatLon, ring: &[LatLon]) -> bool {
    !ring.is_empty() && !inside(p, ring)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    /// grid points inside the ring that were sampled
    pub points: usize,
    pub best_km: f64,
    pub median_km: f64,
    pub worst_km: f64,
}

/// Expected accuracy over a 30 x 30 grid of points inside `hull`. None if the network
/// can't fix points there.
pub fn ring_summary(sites: &[Rx], v: f64, hull: &[LatLon]) -> Option<Summary> {
    let (s, n, w, e) = hull.iter().fold(
        (f64::MAX, f64::MIN, f64::MAX, f64::MIN),
        |(s, n, w, e), p| (s.min(p.lat), n.max(p.lat), w.min(p.lon), e.max(p.lon)),
    );
    let mut km = vec![];
    for i in 0..30 {
        for j in 0..30 {
            let p = LatLon::new(
                s + (n - s) * (i as f64 + 0.5) / 30.0,
                w + (e - w) * (j as f64 + 0.5) / 30.0,
            );
            if !inside(p, hull) {
                continue;
            }
            // a point the receivers can't fix counts as hopeless, not as missing
            km.push(expected(sites, v, p).map_or(f64::INFINITY, |el| el.major_km));
        }
    }
    if km.is_empty() || km.iter().all(|k| k.is_infinite()) {
        return None;
    }
    km.sort_by(f64::total_cmp);
    Some(Summary {
        points: km.len(),
        best_km: km[0],
        median_km: km[km.len() / 2],
        worst_km: km[km.len() - 1],
    })
}

/// The ring's accuracy with each receiver left out in turn, worst loss first: which site
/// the network can least afford to lose.
pub fn without_each(
    sites: &[(String, Rx)],
    v: f64,
    hull: &[LatLon],
) -> Vec<(String, Option<Summary>)> {
    let mut out: Vec<(String, Option<Summary>)> = sites
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let rest: Vec<Rx> = sites
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, (_, rx))| *rx)
                .collect();
            (name.clone(), ring_summary(&rest, v, hull))
        })
        .collect();
    out.sort_by(|a, b| {
        let k = |s: &Option<Summary>| s.map_or(f64::INFINITY, |s| s.median_km);
        k(&b.1).total_cmp(&k(&a.1))
    });
    out
}

/// The worst expected accuracy over a set of points (an area's samples); None if the
/// receivers can't fix one of them.
pub fn area_worst(sites: &[Rx], v: f64, pts: &[LatLon]) -> Option<f64> {
    pts.iter()
        .map(|p| expected(sites, v, *p).map(|e| e.major_km))
        .try_fold(0.0f64, |w, k| k.map(|k| w.max(k)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solve::C_KM_S;
    use crate::solve::tests::ring;

    const V: f64 = 0.9922 * C_KM_S;

    #[test]
    fn hull_drops_inner_points() {
        let mut pts = ring();
        pts.push(LatLon::new(35.3, -120.6));
        let h = hull(&pts);
        assert_eq!(h.len(), 4);
        assert!(!h.contains(&LatLon::new(35.3, -120.6)));
        assert!(inside(LatLon::new(35.3, -120.6), &h));
        assert!(!inside(LatLon::new(36.3, -120.6), &h));
        assert!(hull(&pts[..2]).is_empty());
    }

    fn rxs(sigma: f64, bearing_sigma: Option<f64>) -> Vec<Rx> {
        ring()
            .iter()
            .map(|p| Rx {
                pos: *p,
                sigma,
                bearing_sigma,
            })
            .collect()
    }

    #[test]
    fn accuracy_inside_the_ring_scales_with_timing_error() {
        let h = hull(&ring());
        let at = |sigma: f64| ring_summary(&rxs(sigma, None), V, &h).unwrap();
        let (fine, coarse) = (at(5e-6), at(20e-6));
        assert!(fine.points > 100);
        assert!(fine.best_km <= fine.median_km && fine.median_km <= fine.worst_km);
        assert!((coarse.median_km / fine.median_km - 4.0).abs() < 0.01);
        // this 4-site, ~50 km network meets the spec's 1-3 km target with 2 us timing, not with 5
        assert!(at(2e-6).median_km < 3.0 && fine.median_km > 3.0, "{fine:?}");
        // bearings of 2 degrees bring it under 3 km with the 5 us
        let loops = ring_summary(&rxs(5e-6, Some(2f64.to_radians())), V, &h).unwrap();
        assert!(loops.median_km < 3.0, "{loops:?}");
    }

    #[test]
    fn losing_a_site_makes_the_ring_worse() {
        let named: Vec<(String, Rx)> = rxs(5e-6, None)
            .into_iter()
            .enumerate()
            .map(|(i, rx)| (format!("tst-{i}"), rx))
            .collect();
        let all: Vec<Rx> = named.iter().map(|(_, rx)| *rx).collect();
        let h = hull(&ring());
        let full = ring_summary(&all, V, &h).unwrap();
        let w = without_each(&named, V, &h);
        assert_eq!(w.len(), 4);
        assert!(w.iter().all(|(_, s)| s.unwrap().median_km > full.median_km));
        let k = |i: usize| w[i].1.unwrap().median_km;
        assert!(k(0) >= k(3), "worst loss first");
    }

    #[test]
    fn bins_label_their_ranges() {
        assert_eq!(bin(0.5), 0);
        assert_eq!(bin(2.0), 1);
        assert_eq!(bin(1000.0), 4);
        assert_eq!(bin_label(0), "under 1 km");
        assert_eq!(bin_label(2), "3 to 10 km");
        assert_eq!(bin_label(4), "over 30 km");
    }
}
