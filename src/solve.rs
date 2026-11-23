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
//! A receiver with crossed loops also gives a bearing, which goes into the same weighted
//! least squares as one more equation (time of arrival and direction finding combined;
//! vtspot's cost function weighs both too): the bearing from the receiver to the
//! strike, with the receiver's bearing error. Moving the strike sideways by x km at d km
//! from the receiver turns that bearing by x / d radians (x / (R sin(d/R)) on the sphere).
//! A bearing from loops alone could point either way along its line, so its residual is
//! taken modulo 180 degrees.
//!
//! The error ellipse comes from the fit's covariance, (J^T W J)^-1 with W = 1/sigma^2 from
//! each receiver's timing and bearing errors, so it reflects the geometry (it stretches
//! outside the ring of receivers) and the configured errors, not the residuals. With 3
//! arrival times and no bearing there are as many equations as unknowns: the fit is
//! exact, residuals are zero by construction, and nothing checks the ellipse against the
//! data. It can also have two valid solutions, which are flagged. With more equations
//! than unknowns (a fourth receiver, or bearings) the fit is least squares and the
//! residuals (chi-square) say whether the arrivals really fit one strike; a bearing
//! usually rules out the second solution of three arrival times.

use crate::geo::{EARTH_KM, LatLon, azimuth, centroid, destination, distance_km};

/// Speed of light, km/s.
pub const C_KM_S: f64 = 299_792.458;
/// Scale from a 1-sigma ellipse to a 95% one in 2D: sqrt(chi-square(2 dof, 0.95)).
pub const K95: f64 = 2.447_746_830_680_816;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bearing {
    /// radians clockwise from true north, from the receiver toward the strike
    pub az: f64,
    /// radians (1 sigma)
    pub sigma: f64,
    /// radians after which the bearing repeats: PI from loops alone, TAU with a whip
    pub modulus: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub site: LatLon,
    /// seconds, relative to any common reference
    pub t: f64,
    /// timing error, seconds (1 sigma)
    pub sigma: f64,
    pub bearing: Option<Bearing>,
}

/// A receiver as the accuracy maps see it: where it is and how good its measurements are.
#[derive(Clone, Copy, Debug)]
pub struct Rx {
    pub pos: LatLon,
    /// timing error, seconds (1 sigma)
    pub sigma: f64,
    /// bearing error, radians (1 sigma), if it reports bearings
    pub bearing_sigma: Option<f64>,
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
    /// sum of squared normalised residuals; ~0 when exactly determined
    pub chi2: f64,
    /// equations beyond the three unknowns: arrival times plus bearings, minus 3
    pub dof: usize,
    /// observed minus predicted arrival time per receiver, seconds
    pub resid: Vec<f64>,
    /// observed minus predicted bearing per receiver, radians, where it gave one
    pub bearing_resid: Vec<Option<f64>>,
    /// whether each receiver's bearing went into the fit
    pub bearing_used: Vec<bool>,
    /// another position fits as well (three receivers, with no bearing to tell them apart)
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

#[derive(Default)]
struct Step {
    normal: M3,
    grad: [f64; 3],
    chi2: f64,
    resid_km: Vec<f64>,
    resid_az: Vec<Option<f64>>,
}

impl Step {
    /// One equation: its derivatives by (east km, north km, tau km), its residual and
    /// its weight.
    fn add(&mut self, row: [f64; 3], r: f64, w: f64) {
        for i in 0..3 {
            self.grad[i] += w * row[i] * r;
            for k in 0..3 {
                self.normal[i][k] += w * row[i] * row[k];
            }
        }
        self.chi2 += w * r * r;
    }
}

/// `x` folded into [-m/2, m/2).
fn wrap(x: f64, m: f64) -> f64 {
    (x + m / 2.0).rem_euclid(m) - m / 2.0
}

/// Observed minus predicted bearing for a strike at `p`, radians, if the arrival has one.
pub fn bearing_resid(a: &Arrival, p: LatLon) -> Option<f64> {
    a.bearing
        .map(|b| wrap(b.az - azimuth(a.site, p), b.modulus))
}

/// Linearise at `p`, `tau` (emission time times v, km).
fn linearise(arr: &[Arrival], v: f64, p: LatLon, tau: f64) -> Step {
    let mut s = Step::default();
    for a in arr {
        let d = distance_km(p, a.site);
        let az = azimuth(p, a.site);
        let r = v * a.t - tau - d;
        s.add([-az.sin(), -az.cos(), 1.0], r, 1.0 / (v * a.sigma).powi(2));
        s.resid_km.push(r);
        let rb = bearing_resid(a, p);
        if let (Some(b), Some(r)) = (a.bearing, rb) {
            // km across the line of sight per radian at the receiver, kept off zero for a
            // strike right on top of it
            let across = ((d / EARTH_KM).sin() * EARTH_KM).max(0.1);
            s.add(
                [-az.cos() / across, az.sin() / across, 0.0],
                r,
                1.0 / b.sigma.powi(2),
            );
        }
        s.resid_az.push(rb);
    }
    s
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
    let bad_sigma = |s: f64| s.is_nan() || s <= 0.0;
    if arr.len() < 3
        || arr
            .iter()
            .any(|a| bad_sigma(a.sigma) || a.bearing.is_some_and(|b| bad_sigma(b.sigma)))
    {
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
        dof: arr.len() + arr.iter().filter(|a| a.bearing.is_some()).count() - 3,
        resid: s.resid_km.iter().map(|r| r / v).collect(),
        bearing_resid: s.resid_az,
        bearing_used: arr.iter().map(|a| a.bearing.is_some()).collect(),
        ambiguous,
    })
}

/// The 95% ellipse a strike at `at` would get if every one of these receivers heard it:
/// the same covariance the solver reports, worked out before any strike. None where the
/// receivers can't fix a point there.
pub fn expected(sites: &[Rx], v: f64, at: LatLon) -> Option<Ellipse> {
    if sites.len() < 3 {
        return None;
    }
    let arr: Vec<Arrival> = sites
        .iter()
        .map(|rx| Arrival {
            site: rx.pos,
            t: 0.0,
            sigma: rx.sigma,
            bearing: rx.bearing_sigma.map(|sigma| Bearing {
                az: 0.0,
                sigma,
                modulus: std::f64::consts::TAU,
            }),
        })
        .collect();
    // the normal matrix depends on the geometry only, not on the arrival times
    inverse(&linearise(&arr, v, at, 0.0).normal).map(|c| ellipse(&c))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::sim::Rng;
    use std::f64::consts::{PI, TAU};

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
                bearing: None,
            })
            .collect()
    }

    /// Give every arrival the bearing its receiver would measure, with `sigma` radians of
    /// noise, as a receiver reports it (0 to 180 degrees from loops alone).
    fn with_bearings(arr: &mut [Arrival], strike: LatLon, sigma: f64, rng: &mut Rng) {
        for a in arr {
            a.bearing = Some(Bearing {
                az: (azimuth(a.site, strike) + sigma * rng.gauss()).rem_euclid(PI),
                sigma,
                modulus: PI,
            });
        }
    }

    /// How far `strike` is from the fix, in units of the 95% ellipse (1 is its edge).
    pub fn ellipse_units(f: &Fix, strike: LatLon) -> f64 {
        let d = distance_km(f.pos, strike);
        let az = azimuth(f.pos, strike) - f.ellipse.az_deg.to_radians();
        let (u, w) = (d * az.cos(), d * az.sin());
        ((u / f.ellipse.major_km).powi(2) + (w / f.ellipse.minor_km).powi(2)).sqrt()
    }

    fn exact(sites: &[LatLon], strike: LatLon) -> Vec<Arrival> {
        sites
            .iter()
            .map(|s| Arrival {
                site: *s,
                t: 0.25 + distance_km(strike, *s) / V,
                sigma: 1e-6,
                bearing: None,
            })
            .collect()
    }

    #[test]
    fn exact_arrivals_give_the_exact_position() {
        let strike = LatLon::new(35.31, -120.62);
        let g = solve(&exact(&ring(), strike), V, 300.0).unwrap();
        assert!(distance_km(g.pos, strike) < 0.001, "{:?}", g.pos);
        assert!((g.t0 - 0.25).abs() < 1e-8);
        assert!(g.chi2 < 1e-6);
        assert_eq!(g.dof, 1);
        assert!(!g.ambiguous);
    }

    /// How many of 400 noisy strikes inside the ring fall inside their 95% ellipse.
    fn coverage(seed: u64, sigma: f64, bearing_sigma: Option<f64>) -> usize {
        let mut rng = Rng::new(seed);
        let mut inside = 0;
        for _ in 0..400 {
            let strike = LatLon::new(35.1 + 0.4 * rng.unit(), -120.85 + 0.45 * rng.unit());
            let mut arr = arrivals(&ring(), strike, sigma, &mut rng);
            if let Some(b) = bearing_sigma {
                with_bearings(&mut arr, strike, b, &mut rng);
            }
            let f = solve(&arr, V, 300.0).unwrap();
            if ellipse_units(&f, strike) <= 1.0 {
                inside += 1;
            }
        }
        inside
    }

    #[test]
    fn noisy_strikes_fall_inside_the_95_percent_ellipse_about_95_percent_of_the_time() {
        let inside = coverage(42, 20e-6, None);
        assert!((364..=392).contains(&inside), "{inside} of 400 inside");
    }

    #[test]
    fn with_bearings_the_95_percent_ellipse_still_holds_95_percent() {
        // 2 degrees at 20 to 40 km beats 20 us of timing across the line of sight, so the
        // bearings carry most of the fix here and a wrong bearing term would show
        let inside = coverage(43, 20e-6, Some(2f64.to_radians()));
        assert!((364..=392).contains(&inside), "{inside} of 400 inside");
    }

    #[test]
    fn bearings_shrink_the_ellipse_and_the_accuracy_map_agrees() {
        let strike = LatLon::new(35.42, -120.71);
        let mut rng = Rng::new(5);
        let mut arr = arrivals(&ring(), strike, 1e-9, &mut rng);
        for a in &mut arr {
            a.sigma = 20e-6;
        }
        let without = solve(&arr, V, 300.0).unwrap();
        with_bearings(&mut arr, strike, 1e-9, &mut rng);
        for a in &mut arr {
            a.bearing.as_mut().unwrap().sigma = 2f64.to_radians();
        }
        let with = solve(&arr, V, 300.0).unwrap();
        assert_eq!((without.dof, with.dof), (1, 5));
        assert!(with.ellipse.major_km < without.ellipse.major_km / 2.0);
        let rx: Vec<Rx> = ring()
            .iter()
            .map(|p| Rx {
                pos: *p,
                sigma: 20e-6,
                bearing_sigma: Some(2f64.to_radians()),
            })
            .collect();
        let e = expected(&rx, V, strike).unwrap();
        assert!((e.major_km - with.ellipse.major_km).abs() < 1e-3);
        assert!((e.minor_km - with.ellipse.minor_km).abs() < 1e-3);
    }

    #[test]
    fn a_bearing_from_loops_alone_fits_either_way_along_its_line() {
        let strike = LatLon::new(35.31, -120.62);
        let mut arr = exact(&ring(), strike);
        let back = (azimuth(arr[0].site, strike) + PI).rem_euclid(TAU);
        let sigma = 2f64.to_radians();
        arr[0].bearing = Some(Bearing {
            az: back,
            sigma,
            modulus: TAU,
        });
        let full = solve(&arr, V, 300.0).unwrap();
        assert!(full.chi2 > 100.0, "a whip says it came from the other side");
        arr[0].bearing = Some(Bearing {
            az: back.rem_euclid(PI),
            sigma,
            modulus: PI,
        });
        let axis = solve(&arr, V, 300.0).unwrap();
        assert!(axis.chi2 < 1e-3, "{}", axis.chi2);
        assert!(distance_km(axis.pos, strike) < 0.01);
        assert!(axis.bearing_resid[0].unwrap().abs() < 1e-4);
        assert_eq!(axis.bearing_resid[1], None);
    }

    #[test]
    fn a_bearing_that_points_elsewhere_shows_in_chi_square() {
        let strike = LatLon::new(35.31, -120.62);
        let mut arr = exact(&ring(), strike);
        let toward = azimuth(arr[2].site, strike);
        let sigma = 5f64.to_radians();
        arr[2].bearing = Some(Bearing {
            az: (toward + 30f64.to_radians()).rem_euclid(TAU),
            sigma,
            modulus: TAU,
        });
        let off = solve(&arr, V, 300.0).unwrap();
        // six sigma off, and the four arrival times hold the position where it is
        assert!(off.chi2 > 30.0, "{}", off.chi2);
        assert!(distance_km(off.pos, strike) < 0.5);
        let r = off.bearing_resid[2].unwrap().to_degrees();
        assert!((r - 30.0).abs() < 1.0, "{r}");
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

    fn rxs(sigma: f64) -> Vec<Rx> {
        ring()
            .iter()
            .map(|p| Rx {
                pos: *p,
                sigma,
                bearing_sigma: None,
            })
            .collect()
    }

    #[test]
    fn expected_ellipse_matches_what_the_solver_reports_there() {
        let strike = LatLon::new(35.42, -120.71);
        let mut arr = exact(&ring(), strike);
        for a in &mut arr {
            a.sigma = 20e-6;
        }
        let f = solve(&arr, V, 300.0).unwrap();
        let e = expected(&rxs(20e-6), V, strike).unwrap();
        assert!((e.major_km - f.ellipse.major_km).abs() < 1e-3);
        assert!((e.minor_km - f.ellipse.minor_km).abs() < 1e-3);
        // halving the timing error halves the ellipse
        let h = expected(&rxs(10e-6), V, strike).unwrap();
        assert!((h.major_km * 2.0 - e.major_km).abs() < 1e-6);
        assert!(expected(&rxs(20e-6)[..2], V, strike).is_none());
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
                bearing: None,
            })
            .collect();
        // a strike on the receivers' line: every point of a line through it fits
        assert!(solve(&arr, V, 300.0).is_none_or(|f| f.ellipse.major_km > 50.0));
    }

    #[test]
    fn a_bearing_tells_the_two_three_site_solutions_apart() {
        // outside these three receivers, where three arrival times have two answers
        let strike = LatLon::new(35.9449, -120.1609);
        let mut arr = exact(&ring()[..3], strike);
        let without = solve(&arr, V, 300.0).unwrap();
        assert!(without.ambiguous);
        assert_eq!(without.dof, 0);
        // from loops alone, so the line could point either way
        arr[0].bearing = Some(Bearing {
            az: azimuth(arr[0].site, strike).rem_euclid(PI),
            sigma: 3f64.to_radians(),
            modulus: PI,
        });
        let with = solve(&arr, V, 300.0).unwrap();
        assert!(!with.ambiguous);
        assert_eq!(with.dof, 1);
        assert_eq!(with.bearing_used, vec![true, false, false]);
        assert!(distance_km(with.pos, strike) < 0.1, "{:?}", with.pos);
    }

    #[test]
    fn right_on_top_of_a_receiver_its_bearing_still_gives_an_ellipse() {
        // a bearing turns without limit as the strike moves sideways there
        let rx: Vec<Rx> = ring()
            .iter()
            .map(|p| Rx {
                pos: *p,
                sigma: 20e-6,
                bearing_sigma: Some(5f64.to_radians()),
            })
            .collect();
        let e = expected(&rx, V, ring()[0]).unwrap();
        assert!(e.major_km.is_finite() && e.major_km < 10.0, "{e:?}");
    }

    #[test]
    fn a_bearing_without_a_usable_error_is_refused() {
        let strike = LatLon::new(35.31, -120.62);
        for sigma in [0.0, -1.0, f64::NAN] {
            let mut arr = exact(&ring(), strike);
            arr[0].bearing = Some(Bearing {
                az: azimuth(arr[0].site, strike),
                sigma,
                modulus: TAU,
            });
            assert!(solve(&arr, V, 300.0).is_none(), "{sigma}");
        }
    }
}
