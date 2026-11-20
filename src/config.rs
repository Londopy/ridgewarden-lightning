//! `lightning.toml`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ridgewarden_client::alerts::AlertCfg;
use ridgewarden_client::util::{norm_node, valid_node};
use serde::{Deserialize, Serialize};

use crate::geo::LatLon;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub core: CoreCfg,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// this site's VLF receiver; leave it out at sites without one
    #[serde(default)]
    pub receiver: Option<ReceiverCfg>,
    #[serde(default)]
    pub solver: SolverCfg,
    /// where rain figures for the dry-lightning check come from
    #[serde(default)]
    pub rain: Option<RainCfg>,
    #[serde(default)]
    pub watch: Vec<WatchArea>,
    /// a watch-area alert stays open while it has strikes this recent
    #[serde(default = "d_30")]
    pub watch_window_min: u64,
    /// friendly site names: node name -> label, e.g. "tst-cuesta" = "Cuesta Ridge"
    #[serde(default)]
    pub site_names: BTreeMap<String, String>,
    /// Configure this on one site only, or every site sends the same alert.
    #[serde(default)]
    pub alerts: AlertCfg,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CoreCfg {
    #[serde(default = "d_core_url")]
    pub url: String,
    /// the core's `local.token`; needed to log detections and strikes
    #[serde(default = "d_token")]
    pub token_file: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct ReceiverCfg {
    /// name the receiver reports under; defaults to this site's node name
    #[serde(default)]
    pub name: Option<String>,
    /// the antenna's position; surveyed to within a few metres
    pub lat: f64,
    pub lon: f64,
    /// timing error of one arrival (1 sigma), microseconds: GPS plus vttoga's own
    #[serde(default = "d_sigma")]
    pub timing_sigma_us: f64,
    /// a file vttoga's output is appended to (`vttoga ... >> file`)
    #[serde(default)]
    pub toga_file: Option<PathBuf>,
    /// vttoga's `-d` output directory (files named like 261119-140000)
    #[serde(default)]
    pub toga_dir: Option<PathBuf>,
    /// a shell command whose stdout is vttoga output, restarted if it exits
    #[serde(default)]
    pub toga_command: Option<String>,
    /// drop sferics weaker than this RMS amplitude (vttoga's units)
    #[serde(default)]
    pub min_rms: f64,
    /// seconds between reports to the core
    #[serde(default = "d_10")]
    pub batch_s: u64,
    /// most sferics in one report; the rest are counted as dropped
    #[serde(default = "d_1000")]
    pub max_per_batch: usize,
    /// gpsd to check for a fix, e.g. "127.0.0.1:2947"; no fix means detections are dropped
    #[serde(default)]
    pub gpsd: Option<String>,
}

pub enum Source {
    File(PathBuf),
    Dir(PathBuf),
    Command(String),
}

impl ReceiverCfg {
    pub fn pos(&self) -> LatLon {
        LatLon::new(self.lat, self.lon)
    }

    pub fn source(&self) -> Source {
        match (&self.toga_file, &self.toga_dir, &self.toga_command) {
            (Some(f), _, _) => Source::File(f.clone()),
            (_, Some(d), _) => Source::Dir(d.clone()),
            (_, _, Some(c)) => Source::Command(c.clone()),
            _ => unreachable!("validated: one source is set"),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SolverCfg {
    /// run the solver here; one site is enough
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_3")]
    pub min_sites: usize,
    /// VLF group velocity as a fraction of c (vtspot's default)
    #[serde(default = "d_vf")]
    pub velocity_factor: f64,
    /// wait this long for every site's detections to arrive before solving a moment;
    /// make it longer than the core's sync interval
    #[serde(default = "d_180")]
    pub settle_s: u64,
    /// ignore solutions further than this from the middle of the receivers
    #[serde(default = "d_300f")]
    pub max_range_km: f64,
    /// drop strikes whose 95% ellipse is longer than this (semi-major axis)
    #[serde(default = "d_30f")]
    pub max_axis_km: f64,
    /// with 4+ receivers, reject fits whose chi-square per degree of freedom is above this
    #[serde(default = "d_9f")]
    pub max_chi2: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RainCfg {
    /// APRS-IS server (host:port), read-only; a server on the mesh works offline
    #[serde(default = "d_aprs")]
    pub aprs_is: String,
    /// callsign to log in with (read-only, so no passcode)
    #[serde(default = "d_login")]
    pub login: String,
    /// 1 or 24: which APRS rain field counts (last hour or last 24 hours)
    #[serde(default = "d_24")]
    pub lookback_h: u32,
    /// a report older than this is no data
    #[serde(default = "d_30")]
    pub max_age_min: u64,
    /// under this much rain in the lookback counts as dry
    #[serde(default = "d_dry")]
    pub dry_mm: f64,
    /// read the watch areas' stations from APRS-IS directly
    #[serde(default = "d_true")]
    pub from_aprs_is: bool,
    /// also use rain the weather app logs (`weather.rain` events from trusted stations)
    #[serde(default = "d_true")]
    pub from_weather_app: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct WatchArea {
    pub name: String,
    /// [lat, lon] corners
    #[serde(default)]
    pub polygon: Vec<[f64; 2]>,
    /// or a circle: [lat, lon] and a radius
    #[serde(default)]
    pub center: Option<[f64; 2]>,
    #[serde(default)]
    pub radius_km: Option<f64>,
    /// APRS weather stations whose rain gauges cover the area, e.g. ["CW0000"]
    #[serde(default)]
    pub rain_stations: Vec<String>,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden-lightning")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8796".parse().unwrap()
}
fn d_core_url() -> String {
    "http://127.0.0.1:8790".into()
}
fn d_token() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden/local.token")
}
fn d_sigma() -> f64 {
    20.0
}
fn d_3() -> usize {
    3
}
fn d_10() -> u64 {
    10
}
fn d_30() -> u64 {
    30
}
fn d_180() -> u64 {
    180
}
fn d_1000() -> usize {
    1000
}
fn d_24() -> u32 {
    24
}
fn d_vf() -> f64 {
    0.9922
}
fn d_300f() -> f64 {
    300.0
}
fn d_30f() -> f64 {
    30.0
}
fn d_9f() -> f64 {
    9.0
}
fn d_true() -> bool {
    true
}
fn d_dry() -> f64 {
    2.5
}
fn d_aprs() -> String {
    "rotate.aprs2.net:14580".into()
}
fn d_login() -> String {
    "N0CALL".into()
}

impl Default for CoreCfg {
    fn default() -> Self {
        Self {
            url: d_core_url(),
            token_file: d_token(),
        }
    }
}

impl Default for SolverCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            min_sites: 3,
            velocity_factor: d_vf(),
            settle_s: 180,
            max_range_km: 300.0,
            max_axis_km: 30.0,
            max_chi2: 9.0,
        }
    }
}

/// An APRS callsign with optional SSID, e.g. "CW0000" or "N0CALL-13".
pub fn valid_call(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 9
        && c.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-')
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(r) = &self.receiver {
            r.validate()?;
        }
        let s = &self.solver;
        if s.min_sites < 3 {
            bail!("solver.min_sites must be at least 3: two arrival times can't fix a point");
        }
        if !(0.8..=1.05).contains(&s.velocity_factor) {
            bail!("solver.velocity_factor is a fraction of c, normally 0.98 to 1.0");
        }
        if s.settle_s < 10 {
            bail!("solver.settle_s must be at least 10");
        }
        if !(s.max_range_km > 0.0 && s.max_axis_km > 0.0 && s.max_chi2 > 0.0) {
            bail!("solver.max_range_km, max_axis_km and max_chi2 must be above 0");
        }
        if let Some(r) = &self.rain {
            if !matches!(r.lookback_h, 1 | 24) {
                bail!("rain.lookback_h must be 1 or 24 (APRS reports rain for those two periods)");
            }
            if !r.dry_mm.is_finite() || r.dry_mm <= 0.0 || r.max_age_min == 0 {
                bail!("rain.dry_mm and rain.max_age_min must be above 0");
            }
            if !r.from_aprs_is && !r.from_weather_app {
                bail!("[rain] needs from_aprs_is or from_weather_app (or both) on");
            }
            if !r.aprs_is.contains(':') {
                bail!("rain.aprs_is looks like host:port, e.g. rotate.aprs2.net:14580");
            }
            if !valid_call(&r.login) {
                bail!("rain.login must be a callsign like N0CALL");
            }
        }
        if self.watch_window_min == 0 {
            bail!("watch_window_min must be at least 1");
        }
        for w in &self.watch {
            w.validate(self.rain.is_some())?;
        }
        self.alerts.validate()
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("lightning.db")
    }

    pub fn label(&self, node: &str) -> String {
        self.site_names
            .iter()
            .find(|(k, _)| norm_node(k) == node)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| node.to_string())
    }

    pub fn velocity(&self) -> f64 {
        self.solver.velocity_factor * crate::solve::C_KM_S
    }
}

impl ReceiverCfg {
    fn validate(&self) -> Result<()> {
        if !self.pos().valid() {
            bail!("receiver.lat and receiver.lon must be a real position in degrees");
        }
        if !(self.timing_sigma_us > 0.0 && self.timing_sigma_us <= 1000.0) {
            bail!("receiver.timing_sigma_us must be above 0 and at most 1000");
        }
        let sources = [
            self.toga_file.is_some(),
            self.toga_dir.is_some(),
            self.toga_command.is_some(),
        ];
        if sources.iter().filter(|s| **s).count() != 1 {
            bail!("set exactly one of receiver.toga_file, toga_dir or toga_command");
        }
        if !(1..=60).contains(&self.batch_s) {
            bail!("receiver.batch_s must be 1 to 60");
        }
        if self.max_per_batch == 0 || self.max_per_batch > 1500 {
            bail!("receiver.max_per_batch must be 1 to 1500 (an event's size limit)");
        }
        if let Some(n) = &self.name
            && !valid_node(&norm_node(n))
        {
            bail!("receiver.name {n:?} must be a node-style name (letters, digits, hyphens)");
        }
        Ok(())
    }
}

impl WatchArea {
    fn validate(&self, have_rain: bool) -> Result<()> {
        let n = &self.name;
        if n.trim().is_empty() || n.len() > 60 {
            bail!("each watch area needs a name of up to 60 characters");
        }
        match (self.polygon.len(), self.center, self.radius_km) {
            (0, Some(c), Some(r)) if r > 0.0 && LatLon::new(c[0], c[1]).valid() => {}
            (3.., None, None) if self.corners().iter().all(LatLon::valid) => {}
            _ => bail!(
                "watch area {n:?} needs either polygon (3 or more [lat, lon] corners) or center and radius_km"
            ),
        }
        for s in &self.rain_stations {
            if !valid_call(s) {
                bail!("watch area {n:?}: rain station {s:?} must be an upper-case callsign");
            }
        }
        if !self.rain_stations.is_empty() && !have_rain {
            bail!("watch area {n:?} lists rain stations but there's no [rain] section");
        }
        Ok(())
    }

    pub fn corners(&self) -> Vec<LatLon> {
        self.polygon
            .iter()
            .map(|c| LatLon::new(c[0], c[1]))
            .collect()
    }

    /// The polygon's corners, or the circle's north, east, south and west points.
    pub fn extent(&self) -> Vec<LatLon> {
        match (self.center, self.radius_km) {
            (Some(c), Some(r)) => (0..4)
                .map(|k| {
                    let az = k as f64 * std::f64::consts::FRAC_PI_2;
                    crate::geo::destination(LatLon::new(c[0], c[1]), az, r)
                })
                .collect(),
            _ => self.corners(),
        }
    }

    /// Points spread over the area (a 12 x 12 grid over its bounds, kept where inside,
    /// plus its outline points), for judging the whole area rather than its middle.
    pub fn samples(&self) -> Vec<LatLon> {
        let (s, n, w, e) = self.bbox();
        let mut out = self.extent();
        for i in 0..12 {
            for j in 0..12 {
                let p = LatLon::new(
                    s + (n - s) * (i as f64 + 0.5) / 12.0,
                    w + (e - w) * (j as f64 + 0.5) / 12.0,
                );
                if self.contains(p) {
                    out.push(p);
                }
            }
        }
        out
    }

    /// (south, north, west, east) bounds in degrees.
    pub fn bbox(&self) -> (f64, f64, f64, f64) {
        self.extent().iter().fold(
            (f64::MAX, f64::MIN, f64::MAX, f64::MIN),
            |(s, n, w, e), p| (s.min(p.lat), n.max(p.lat), w.min(p.lon), e.max(p.lon)),
        )
    }

    pub fn contains(&self, p: LatLon) -> bool {
        match (self.center, self.radius_km) {
            (Some(c), Some(r)) => crate::geo::distance_km(LatLon::new(c[0], c[1]), p) <= r,
            _ => crate::geo::in_polygon(p, &self.corners()),
        }
    }
}

pub const EXAMPLE: &str = r#"# ridgewarden-lightning configuration
data_dir = "/var/lib/ridgewarden-lightning"
listen = "0.0.0.0:8796"
# a watch-area alert stays open while it has strikes this recent
watch_window_min = 30

[core]
url = "http://127.0.0.1:8790"
token_file = "/var/lib/ridgewarden/local.token"

# This site's VLF receiver, running vlfrx-tools' vttoga. Remove the section at sites
# without one.
# [receiver]
# name = "tst-cuesta"            # defaults to this site's node name
# lat = 35.3500                  # the antenna, surveyed
# lon = -120.6500
# timing_sigma_us = 20           # one arrival's timing error, 1 sigma
# toga_dir = "/var/lib/vlf/toga" # vttoga -d output; or toga_file, or toga_command
# min_rms = 0.0
# batch_s = 10
# max_per_batch = 1000
# gpsd = "127.0.0.1:2947"        # drop detections while the GPS has no fix

# Locate strikes from every site's detections. One site is enough.
[solver]
enabled = false
min_sites = 3                    # 4 once there are four or more receivers
velocity_factor = 0.9922
settle_s = 180                   # longer than the core's sync interval
max_range_km = 300
max_axis_km = 30
max_chi2 = 9

# Rain for the dry-lightning check, from APRS weather stations (CWOP) over APRS-IS.
# [rain]
# from_aprs_is = true            # read the stations from APRS-IS
# from_weather_app = true        # and use rain the weather app logs on the mesh
# aprs_is = "rotate.aprs2.net:14580"
# login = "N0CALL"
# lookback_h = 24                # 1 or 24
# max_age_min = 30
# dry_mm = 2.5

# [[watch]]
# name = "Backcountry"
# polygon = [[35.55, -120.95], [35.62, -120.45], [35.30, -120.40]]
# rain_stations = ["CW0000"]
# [[watch]]
# name = "Near the hub"
# center = [35.30, -120.66]
# radius_km = 10

[site_names]
# "tst-cuesta" = "Cuesta Ridge"

# Alerts: set these up on one site only, or every site sends the same alert.
[alerts]
# ntfy = "http://10.1.2.3/mesh-alerts"
# discord = "https://discord.com/api/webhooks/..."
# webhooks = []
# public_url = "http://tst-hub-pi.local.mesh:8796"
# quiet_hours = "22-6"
# repeat_hours = 4
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(EXAMPLE).unwrap();
        c.validate().unwrap();
    }

    #[test]
    fn example_with_everything_uncommented_parses() {
        let text: String = EXAMPLE
            .lines()
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("# ") && (t[2..].contains(" = ") || t[2..].starts_with('[')) {
                    t[2..].to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        // two watch areas, one of each shape, and a receiver with one source
        let c: Config = toml::from_str(&text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.watch.len(), 2);
        assert!(c.receiver.is_some() && c.rain.is_some());
    }

    #[test]
    fn receiver_needs_exactly_one_source() {
        let mut c: Config = toml::from_str(
            "[receiver]\nlat = 35.0\nlon = -120.0\ntoga_file = \"/tmp/x\"\ntoga_dir = \"/tmp\"",
        )
        .unwrap();
        assert!(c.validate().is_err());
        c.receiver.as_mut().unwrap().toga_dir = None;
        c.validate().unwrap();
    }
}
