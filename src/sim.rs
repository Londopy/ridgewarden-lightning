//! The `simulate` subcommand: vttoga H records for synthetic strikes at a set of
//! receivers, with Gaussian timing noise, so the whole pipeline (reader, events, solver,
//! map, alerts) can be run without VLF hardware.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::geo::{LatLon, azimuth, distance_km};

/// xorshift64*: small, seedable, good enough for noise in tests and demos.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    pub fn gauss(&mut self) -> f64 {
        let u = 1.0 - self.unit();
        let w = self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * w).cos()
    }
}

/// "3.215e-02", the way C's %.3e writes it (and vttoga prints it).
fn c_exp(x: f64) -> String {
    let s = format!("{x:.3e}");
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    let e: i32 = e.parse().unwrap_or(0);
    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

pub fn h_record(t_us: i64, rms: f64, impulse: f64, bearing: Option<f64>) -> String {
    let mut s = format!(
        "H {}.{:06} {} {impulse:.2}",
        t_us.div_euclid(1_000_000),
        t_us.rem_euclid(1_000_000),
        c_exp(rms)
    );
    if let Some(b) = bearing {
        s.push_str(&format!(" {b:.1}"));
    }
    s
}

/// Bearings for simulated receivers with crossed loops: how they repeat (180 without a
/// whip, 360 with one) and their error, degrees.
#[derive(Clone, Copy)]
pub struct Loops {
    pub modulus: f64,
    pub sigma_deg: f64,
}

/// One H record per receiver for a strike at `strike`, emitted at `t0_us`.
pub fn strike_records(
    strike: LatLon,
    t0_us: i64,
    receivers: &[(String, LatLon)],
    v_km_s: f64,
    sigma_us: f64,
    loops: Option<Loops>,
    rng: &mut Rng,
) -> Vec<(String, String)> {
    receivers
        .iter()
        .map(|(name, pos)| {
            let d = distance_km(strike, *pos);
            let t = t0_us as f64 + d / v_km_s * 1e6 + sigma_us * rng.gauss();
            // field strength falls off roughly as 1/distance this close in
            let rms = 5.0 / (d + 10.0) * (0.7 + 0.6 * rng.unit());
            let bearing = loops.map(|l| {
                let b = azimuth(*pos, strike).to_degrees() + l.sigma_deg * rng.gauss();
                // under the modulus, as vttoga keeps it; to one decimal it can read as 360.0
                b.rem_euclid(l.modulus)
            });
            (
                name.clone(),
                h_record(t.round() as i64, rms, 3.0 + 5.0 * rng.unit(), bearing),
            )
        })
        .collect()
}

pub fn parse_point(s: &str) -> Result<LatLon> {
    let (a, b) = s
        .split_once(',')
        .with_context(|| format!("{s:?} should look like 35.30,-120.66"))?;
    let p = LatLon::new(a.trim().parse()?, b.trim().parse()?);
    if !p.valid() {
        bail!("{s:?} is not a position in degrees");
    }
    Ok(p)
}

pub fn parse_receiver(s: &str) -> Result<(String, LatLon)> {
    let (name, at) = s
        .split_once('=')
        .with_context(|| format!("{s:?} should look like tst-cuesta=35.35,-120.65"))?;
    Ok((name.trim().to_string(), parse_point(at)?))
}

pub struct Plan {
    pub receivers: Vec<(String, LatLon)>,
    pub strikes: Vec<LatLon>,
    pub every_s: f64,
    pub sigma_us: f64,
    pub loops: Option<Loops>,
    pub v_km_s: f64,
    pub start_us: i64,
}

/// Append each receiver's records to `<out>/<name>.toga`. Returns how many lines went out.
pub fn write(plan: &Plan, out: &Path, rng: &mut Rng) -> Result<usize> {
    if plan.receivers.len() < 3 {
        bail!("give at least three --receiver; fewer can't locate anything");
    }
    std::fs::create_dir_all(out)?;
    let mut n = 0;
    for (i, s) in plan.strikes.iter().enumerate() {
        let t0 = plan.start_us + (i as f64 * plan.every_s * 1e6) as i64;
        let records = strike_records(
            *s,
            t0,
            &plan.receivers,
            plan.v_km_s,
            plan.sigma_us,
            plan.loops,
            rng,
        );
        for (name, line) in records {
            let path = out.join(format!("{name}.toga"));
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            writeln!(f, "{line}")?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toga::parse_line;

    #[test]
    fn records_read_back_through_the_real_parser() {
        let line = h_record(1_795_000_000_000_042, 0.03215, 4.712, None);
        assert_eq!(line, "H 1795000000.000042 3.215e-02 4.71");
        let t = parse_line(&line).unwrap().unwrap();
        assert_eq!(t.t_us, 1_795_000_000_000_042);
        assert_eq!(t.impulse, Some(4.71));
        let t = parse_line(&h_record(1, 0.1, 2.0, Some(359.94)))
            .unwrap()
            .unwrap();
        assert_eq!(t.bearing, Some(359.9));
        let t = parse_line(&h_record(1, 0.1, 2.0, Some(359.96)))
            .unwrap()
            .unwrap();
        assert_eq!(t.bearing, Some(360.0));
    }

    #[test]
    fn simulated_loops_point_at_the_strike() {
        let strike = LatLon::new(35.3, -120.6);
        let bearing = |rx: LatLon, modulus: f64| {
            let loops = Loops {
                modulus,
                sigma_deg: 0.0,
            };
            let recs = strike_records(
                strike,
                0,
                &[("x".to_string(), rx)],
                297_000.0,
                0.0,
                Some(loops),
                &mut Rng::new(1),
            );
            parse_line(&recs[0].1).unwrap().unwrap().bearing.unwrap()
        };
        // degrees between two bearings that repeat every m
        let apart = |a: f64, b: f64, m: f64| {
            let d = (a - b).rem_euclid(m);
            d.min(m - d)
        };
        let (south, north) = (LatLon::new(35.0, -120.6), LatLon::new(35.6, -120.6));
        assert!(apart(bearing(south, 360.0), 0.0, 360.0) < 0.05);
        assert!(apart(bearing(north, 360.0), 180.0, 360.0) < 0.05);
        // loops alone can't tell south from north
        assert!(apart(bearing(north, 180.0), 0.0, 180.0) < 0.05);
        assert!(apart(bearing(north, 360.0), 0.0, 360.0) > 179.0);
    }

    #[test]
    fn arrival_order_follows_distance() {
        let rx = vec![
            ("near".to_string(), LatLon::new(35.30, -120.60)),
            ("far".to_string(), LatLon::new(35.90, -120.60)),
            ("mid".to_string(), LatLon::new(35.55, -120.60)),
        ];
        let recs = strike_records(
            LatLon::new(35.3, -120.6),
            0,
            &rx,
            297_000.0,
            0.0,
            None,
            &mut Rng::new(1),
        );
        let t = |n: &str| {
            let l = &recs.iter().find(|r| r.0 == n).unwrap().1;
            parse_line(l).unwrap().unwrap().t_us
        };
        assert!(t("near") < t("mid") && t("mid") < t("far"));
        // 66.7 km at 297000 km/s is 224.6 us
        assert!((t("far") - t("near") - 225).abs() <= 1);
    }
}
