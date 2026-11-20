//! Locating one strike from its arrival times at three or more receivers (time difference
//! of arrival), on a sphere.
//!
//! Unknowns: the strike's position and its emission time. Each receiver i gives
//! `v * t_i = v * t0 + d(strike, site_i)`, where d is the great-circle distance and v the
//! VLF group velocity (0.9922 c by default, vtspot's default). Everything is worked in km
//! (times are multiplied by v) so the three unknowns are on the same scale. Gauss-Newton
//! steps are taken in a local east/north frame at the current estimate and then moved
//! along the sphere, so the fit itself has no flat-earth approximation; the derivative of
//! d with respect to moving east/north is exact for small steps (minus the sin/cos of the
//! azimuth toward the site).
//!
//! The error ellipse comes from the fit's covariance, (J^T W J)^-1 with W = 1/sigma^2 from
//! each receiver's timing error, so it reflects the geometry (it stretches outside the
//! ring of receivers) and the configured timing error, not the residuals. With 3
//! receivers there are as many equations as unknowns: the fit is exact, residuals are
//! zero by construction, and nothing checks the ellipse against the data. It can also
//! have two valid solutions, which are flagged. With 4 or more the fit is least squares
//! and the residuals (chi-square) say whether the arrivals really fit one strike.

use crate::geo::{LatLon, azimuth, centroid, destination, distance_km};

/// Speed of light, km/s.
pub const C_KM_S: f64 = 299_792.458;
/// Scale from a 1-sigma ellipse to a 95% one in 2D: sqrt(chi-square(2 dof, 0.95)).
pub const K95: f64 = 2.447_746_830_680_816;

#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub site: LatLon,
    /// seconds, relative to any common reference
    pub t: f64,
    /// timing error, seconds (1 sigma)
    pub sigma: f64,
}

/// 95% error ellipse: semi-axes in km and the major axis' bearing in degrees from north.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ellipse {
    pub major_km: f64,
    pub minor_km: f64,
    pub az_deg: f64,
}

#[derive(Clone, Debug)]
pub struct Fix {
    pub pos: LatLon,
    /// emission time, seconds on the same reference as the arrivals
    pub t0: f64,
    pub ellipse: Ellipse,
    /// sum of squared normalised residuals; ~0 with three receivers
    pub chi2: f64,
    /// observed minus predicted arrival time per receiver, seconds
    pub resid: Vec<f64>,
    /// another position fits as well (only happens with three receivers)
    pub ambiguous: bool,
}

type M3 = [[f64; 3]; 3];

fn inverse(m: &M3) -> Option<M3> {
    let c = |r: usize, k: usize| {
        let (r1, r2) = ((r + 1) % 3, (r + 2) % 3);
        let (k1, k2) = ((k + 1) % 3, (k + 2) % 3);
        m[r1][k1] * m[r2][k2] - m[r1][k2] * m[r2][k1]
    };
    let det = m[0][0] * c(0, 0) + m[0][1] * c(0, 1) + m[0][2] * c(0, 2);
    let scale = m.iter().flatten().fold(0.0f64, |a, x| a.max(x.abs()));
    if !det.is_finite() || det.abs() <= 1e-12 * scale.powi(3) {
        return None;
    }
    let mut inv = [[0.0; 3]; 3];
    for (r, row) in inv.iter_mut().enumerate() {
        for (k, x) in row.iter_mut().enumerate() {
            *x = c(k, r) / det;
        }
    }
    Some(inv)
}

struct Step {
    normal: M3,
    grad: [f64; 3],
    chi2: f64,
    resid_km: Vec<f64>,
}

/// Linearise at `p`, `tau` (emission time times v, km).
fn linearise(arr: &[Arrival], v: f64, p: LatLon, tau: f64) -> Step {
    let mut normal = [[0.0; 3]; 3];
    let mut grad = [0.0; 3];
    let mut chi2 = 0.0;
    let mut resid_km = Vec::with_capacity(arr.len());
    for a in arr {
        let d = distance_km(p, a.site);
        let az = azimuth(p, a.site);
        let row = [-az.sin(), -az.cos(), 1.0];
        let r = v * a.t - tau - d;
        let w = 1.0 / (v * a.sigma).powi(2);
        for i in 0..3 {
            grad[i] += w * row[i] * r;
            for k in 0..3 {
                normal[i][k] += w * row[i] * row[k];
            }
        }
        chi2 += w * r * r;
        resid_km.push(r);
    }
    Step {
        normal,
        grad,
        chi2,
        resid_km,
    }
}

fn best_tau(arr: &[Arrival], v: f64, p: LatLon) -> f64 {
    let (mut s, mut ws) = (0.0, 0.0);
    for a in arr {
        let w = 1.0 / a.sigma.powi(2);
        s += w * (v * a.t - distance_km(p, a.site));
        ws += w;
    }
    s / ws
}

/// Gauss-Newton from one starting point, halving steps that don't lower chi-square
/// (near a receiver the distance has a kink and full steps can circle the minimum).
/// Returns the converged position and tau.
fn descend(arr: &[Arrival], v: f64, start: LatLon) -> Option<(LatLon, f64)> {
    let mut p = start;
    let mut tau = best_tau(arr, v, p);
    for _ in 0..100 {
        let s = linearise(arr, v, p, tau);
        let inv = inverse(&s.normal)?;
        let mut d = [0.0; 3];
        for (i, di) in d.iter_mut().enumerate() {
            *di = (0..3).map(|k| inv[i][k] * s.grad[k]).sum();
        }
        // a full step from far away can overshoot to the far side of the earth
        let len = d[0].hypot(d[1]);
        let mut k = if len > 300.0 { 300.0 / len } else { 1.0 };
        loop {
            let q = destination(p, d[0].atan2(d[1]), len * k);
            if linearise(arr, v, q, tau + d[2] * k).chi2 <= s.chi2 {
                p = q;
                tau += d[2] * k;
                break;
            }
            k /= 2.0;
            if k < 1e-6 {
                return Some((p, tau));
            }
        }
        if len * k < 1e-5 && (d[2] * k).abs() < 1e-5 {
            return Some((p, tau));
        }
    }
    None
}

fn ellipse(cov: &M3) -> Ellipse {
    let (a, b, c) = (cov[0][0], cov[0][1], cov[1][1]);
    let mid = (a + c) / 2.0;
    let rad = (((a - c) / 2.0).powi(2) + b * b).sqrt();
    let (l1, l2) = (mid + rad, (mid - rad).max(0.0));
    // angle of the major axis from east toward north
    let theta = 0.5 * (2.0 * b).atan2(a - c);
    Ellipse {
        major_km: K95 * l1.sqrt(),
        minor_km: K95 * l2.sqrt(),
        az_deg: (90.0 - theta.to_degrees()).rem_euclid(180.0),
    }
}

fn starts(sites: &[LatLon], rings: &[f64], per_ring: usize) -> Vec<LatLon> {
    let Some(c) = centroid(sites) else {
        return vec![];
    };
    let r = sites
        .iter()
        .map(|s| distance_km(c, *s))
        .fold(10.0f64, f64::max);
    let mut out = vec![c];
    for ring in rings {
        for k in 0..per_ring {
            let az = k as f64 * std::f64::consts::TAU / per_ring as f64;
            out.push(destination(c, az, ring * r));
        }
    }
    out
}

/// Locate a strike. `v` is the group velocity in km/s; solutions further than
/// `max_range_km` from the receivers' centroid are ignored.
pub fn solve(arr: &[Arrival], v: f64, max_range_km: f64) -> Option<Fix> {
    if arr.len() < 3 || arr.iter().any(|a| a.sigma.is_nan() || a.sigma <= 0.0) {
        return None;
    }
    let sites: Vec<LatLon> = arr.iter().map(|a| a.site).collect();
    let center = centroid(&sites)?;
    // three receivers can have two exact solutions far apart, and only enough starting
    // points, near and far, find both; with more the fit has one clear minimum
    let starts = if arr.len() == 3 {
        starts(&sites, &[0.7, 1.5, 3.0, 6.0, 12.0, 25.0], 16)
    } else {
        starts(&sites, &[1.5, 6.0], 8)
    };
    let mut found: Vec<(LatLon, f64, f64)> = vec![];
    for s in starts {
        let Some((p, tau)) = descend(arr, v, s) else {
            continue;
        };
        if distance_km(center, p) > max_range_km {
            continue;
        }
        let chi2 = linearise(arr, v, p, tau).chi2;
        if !found.iter().any(|(q, _, _)| distance_km(*q, p) < 0.05) {
            found.push((p, tau, chi2));
        }
    }
    found.sort_by(|a, b| a.2.total_cmp(&b.2));
    let &(pos, tau, chi2) = found.first()?;
    let s = linearise(arr, v, pos, tau);
    let cov = inverse(&s.normal)?;
    // the other solution of an exactly determined fit fits just as well
    let ambiguous = found[1..].iter().any(|f| f.2 <= chi2 + 1.0);
    Some(Fix {
        pos,
        t0: tau / v,
        ellipse: ellipse(&cov),
        chi2,
        resid: s.resid_km.iter().map(|r| r / v).collect(),
        ambiguous,
    })
}

/// The 95% ellipse a strike at `at` would get if every one of these receivers (position,
/// timing error in seconds) heard it: the same covariance the solver reports, worked
/// out before any strike. None where the receivers can't fix a point there.
pub fn expected(sites: &[(LatLon, f64)], v: f64, at: LatLon) -> Option<Ellipse> {
    if sites.len() < 3 {
        return None;
    }
    let arr: Vec<Arrival> = sites
        .iter()
        .map(|(site, sigma)| Arrival {
            site: *site,
            t: 0.0,
            sigma: *sigma,
        })
        .collect();
    // the normal matrix depends on the geometry only, not on the arrival times
    inverse(&linearise(&arr, v, at, 0.0).normal).map(|c| ellipse(&c))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::sim::Rng;

    const V: f64 = 0.9922 * C_KM_S;

    pub fn ring() -> Vec<LatLon> {
        vec![
            LatLon::new(35.50, -120.95),
            LatLon::new(35.60, -120.40),
            LatLon::new(35.10, -120.30),
            LatLon::new(35.05, -120.90),
        ]
    }

    fn arrivals(sites: &[LatLon], strike: LatLon, sigma: f64, rng: &mut Rng) -> Vec<Arrival> {
        sites
            .iter()
            .map(|s| Arrival {
                site: *s,
                t: 0.25 + distance_km(strike, *s) / V + sigma * rng.gauss(),
                sigma,
            })
            .collect()
    }

    #[test]
    fn exact_arrivals_give_the_exact_position() {
        let strike = LatLon::new(35.31, -120.62);
        let exact: Vec<Arrival> = ring()
            .iter()
            .map(|s| Arrival {
                site: *s,
                t: 0.25 + distance_km(strike, *s) / V,
                sigma: 1e-6,
            })
            .collect();
        let g = solve(&exact, V, 300.0).unwrap();
        assert!(distance_km(g.pos, strike) < 0.001, "{:?}", g.pos);
        assert!((g.t0 - 0.25).abs() < 1e-8);
        assert!(g.chi2 < 1e-6);
        assert!(!g.ambiguous);
    }

    #[test]
    fn noisy_strikes_fall_inside_the_95_percent_ellipse_about_95_percent_of_the_time() {
        let mut rng = Rng::new(42);
        let sigma = 20e-6;
        let (mut inside, mut n) = (0, 0);
        for _ in 0..400 {
            let strike = LatLon::new(35.1 + 0.4 * rng.unit(), -120.85 + 0.45 * rng.unit());
            let f = solve(&arrivals(&ring(), strike, sigma, &mut rng), V, 300.0).unwrap();
            n += 1;
            // distance from the fix in the ellipse's own axes
            let d = distance_km(f.pos, strike);
            let az = azimuth(f.pos, strike) - f.ellipse.az_deg.to_radians();
            let (u, w) = (d * az.cos(), d * az.sin());
            if (u / f.ellipse.major_km).powi(2) + (w / f.ellipse.minor_km).powi(2) <= 1.0 {
                inside += 1;
            }
        }
        let frac = inside as f64 / n as f64;
        assert!((0.91..=0.98).contains(&frac), "{inside} of {n} inside");
    }

    #[test]
    fn three_receivers_fit_exactly_and_still_give_an_ellipse() {
        let sites = &ring()[..3];
        let strike = LatLon::new(35.4, -120.55);
        let mut rng = Rng::new(7);
        let f = solve(&arrivals(sites, strike, 5e-6, &mut rng), V, 300.0).unwrap();
        assert!(f.chi2 < 1e-6, "exactly determined, chi2 {}", f.chi2);
        assert!(f.resid.iter().all(|r| r.abs() < 1e-9));
        assert!(f.ellipse.major_km > 0.0 && f.ellipse.major_km < 10.0);
        assert!(distance_km(f.pos, strike) < 3.0 * f.ellipse.major_km);
    }

    #[test]
    fn ellipse_grows_outside_the_ring() {
        let mut rng = Rng::new(1);
        let inside = solve(
            &arrivals(&ring(), LatLon::new(35.3, -120.6), 1e-9, &mut rng),
            V,
            500.0,
        )
        .unwrap();
        let outside = solve(
            &arrivals(&ring(), LatLon::new(36.6, -119.2), 1e-9, &mut rng),
            V,
            500.0,
        )
        .unwrap();
        assert!(outside.ellipse.major_km > 5.0 * inside.ellipse.major_km);
        // and it points roughly back toward the receivers (radial error dominates)
        let toward = azimuth(LatLon::new(36.6, -119.2), LatLon::new(35.3, -120.6))
            .to_degrees()
            .rem_euclid(180.0);
        let diff = (outside.ellipse.az_deg - toward).abs();
        assert!(
            diff.min(180.0 - diff) < 25.0,
            "{} vs {toward}",
            outside.ellipse.az_deg
        );
    }

    #[test]
    fn expected_ellipse_matches_what_the_solver_reports_there() {
        let strike = LatLon::new(35.42, -120.71);
        let arr: Vec<Arrival> = ring()
            .iter()
            .map(|s| Arrival {
                site: *s,
                t: distance_km(strike, *s) / V,
                sigma: 20e-6,
            })
            .collect();
        let f = solve(&arr, V, 300.0).unwrap();
        let sites: Vec<(LatLon, f64)> = ring().iter().map(|s| (*s, 20e-6)).collect();
        let e = expected(&sites, V, strike).unwrap();
        assert!((e.major_km - f.ellipse.major_km).abs() < 1e-3);
        assert!((e.minor_km - f.ellipse.minor_km).abs() < 1e-3);
        // halving the timing error halves the ellipse
        let half: Vec<(LatLon, f64)> = ring().iter().map(|s| (*s, 10e-6)).collect();
        let h = expected(&half, V, strike).unwrap();
        assert!((h.major_km * 2.0 - e.major_km).abs() < 1e-6);
        assert!(expected(&sites[..2], V, strike).is_none());
    }

    #[test]
    fn arrivals_that_dont_fit_one_strike_give_a_large_chi2() {
        let mut rng = Rng::new(3);
        let mut arr = arrivals(&ring(), LatLon::new(35.3, -120.6), 2e-6, &mut rng);
        arr[2].t += 200e-6;
        let f = solve(&arr, V, 300.0).unwrap();
        assert!(f.chi2 > 50.0, "{}", f.chi2);
    }

    #[test]
    fn collinear_receivers_cant_locate_anything() {
        let line: Vec<LatLon> = (0..3)
            .map(|i| LatLon::new(35.0, -121.0 + 0.2 * i as f64))
            .collect();
        let arr: Vec<Arrival> = line
            .iter()
            .map(|s| Arrival {
                site: *s,
                t: distance_km(LatLon::new(35.0, -121.6), *s) / V,
                sigma: 1e-6,
            })
            .collect();
        // a strike on the receivers' line: every point of a line through it fits
        assert!(solve(&arr, V, 300.0).is_none_or(|f| f.ellipse.major_km > 50.0));
    }
}
