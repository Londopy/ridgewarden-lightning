//! Positions on a spherical earth: great-circle distance, azimuth, moving along a bearing,
//! and the watch-area tests. A sphere is what vlfrx-tools' own solver (vtspot) uses, and
//! at county scale the sphere-versus-ellipsoid difference (under 0.5%) is far below the
//! timing error.

use serde::{Deserialize, Serialize};

pub const EARTH_KM: f64 = 6371.0;

/// Degrees.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

impl LatLon {
    pub fn new(lat: f64, lon: f64) -> Self {
        Self { lat, lon }
    }

    pub fn valid(&self) -> bool {
        self.lat.is_finite()
            && self.lon.is_finite()
            && (-90.0..=90.0).contains(&self.lat)
            && (-180.0..=180.0).contains(&self.lon)
    }

    fn nvec(&self) -> [f64; 3] {
        let (la, lo) = (self.lat.to_radians(), self.lon.to_radians());
        [la.cos() * lo.cos(), la.cos() * lo.sin(), la.sin()]
    }

    fn from_nvec(v: [f64; 3]) -> Self {
        Self {
            lat: v[2].atan2(v[0].hypot(v[1])).to_degrees(),
            lon: v[1].atan2(v[0]).to_degrees(),
        }
    }
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: [f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

/// Great-circle distance in km; accurate at any range, including a few metres.
pub fn distance_km(a: LatLon, b: LatLon) -> f64 {
    let (u, v) = (a.nvec(), b.nvec());
    norm(cross(u, v)).atan2(dot(u, v)) * EARTH_KM
}

/// Initial bearing from `a` to `b`, radians clockwise from north.
pub fn azimuth(a: LatLon, b: LatLon) -> f64 {
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dl = (b.lon - a.lon).to_radians();
    (dl.sin() * p2.cos()).atan2(p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos())
}

/// The point `km` from `a` along initial bearing `az` (radians).
pub fn destination(a: LatLon, az: f64, km: f64) -> LatLon {
    let d = km / EARTH_KM;
    let p1 = a.lat.to_radians();
    let p2 = (p1.sin() * d.cos() + p1.cos() * d.sin() * az.cos()).asin();
    let l2 =
        a.lon.to_radians() + (az.sin() * d.sin() * p1.cos()).atan2(d.cos() - p1.sin() * p2.sin());
    let lon = (l2.to_degrees() + 540.0).rem_euclid(360.0) - 180.0;
    LatLon::new(p2.to_degrees(), lon)
}

/// Mean position of a set of points (normalised mean of their unit vectors).
pub fn centroid(pts: &[LatLon]) -> Option<LatLon> {
    let mut s = [0.0; 3];
    for p in pts {
        let v = p.nvec();
        for i in 0..3 {
            s[i] += v[i];
        }
    }
    (norm(s) > 1e-9).then(|| LatLon::from_nvec(s))
}

/// Flat km offsets (east, north) from `origin`, for drawing a county-sized map.
pub fn local_km(origin: LatLon, p: LatLon) -> (f64, f64) {
    let x = (p.lon - origin.lon).to_radians() * origin.lat.to_radians().cos() * EARTH_KM;
    let y = (p.lat - origin.lat).to_radians() * EARTH_KM;
    (x, y)
}

/// Ray-casting point-in-polygon on lat/lon. Fine for areas a few hundred km across that
/// don't cross the antimeridian or a pole.
pub fn in_polygon(p: LatLon, poly: &[LatLon]) -> bool {
    let mut inside = false;
    let mut j = poly.len().wrapping_sub(1);
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a.lat > p.lat) != (b.lat > p.lat)
            && p.lon < (b.lon - a.lon) * (p.lat - a.lat) / (b.lat - a.lat) + a.lon
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_degree_of_latitude_is_about_111_km() {
        let d = distance_km(LatLon::new(35.0, -120.0), LatLon::new(36.0, -120.0));
        assert!((d - 111.19).abs() < 0.01, "{d}");
    }

    #[test]
    fn destination_and_azimuth_agree_with_distance() {
        let a = LatLon::new(35.28, -120.66);
        for deg in [0.0f64, 37.0, 90.0, 200.0, 311.0] {
            let b = destination(a, deg.to_radians(), 42.0);
            assert!((distance_km(a, b) - 42.0).abs() < 1e-9);
            let back = azimuth(a, b).to_degrees().rem_euclid(360.0);
            assert!((back - deg).abs() < 1e-6, "{deg} came back as {back}");
        }
    }

    #[test]
    fn polygon_contains_inside_points_only() {
        let sq = [
            LatLon::new(35.0, -121.0),
            LatLon::new(35.0, -120.0),
            LatLon::new(36.0, -120.0),
            LatLon::new(36.0, -121.0),
        ];
        assert!(in_polygon(LatLon::new(35.5, -120.5), &sq));
        assert!(!in_polygon(LatLon::new(36.5, -120.5), &sq));
        assert!(!in_polygon(LatLon::new(35.5, -119.9), &sq));
    }
}
