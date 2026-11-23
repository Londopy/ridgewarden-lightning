//! This site's receiver: follow vttoga's output, check the GPS, and report arrivals to the
//! core every few seconds as `lightning.toga` telemetry events.
//!
//! vttoga's output is read from one of three places: a file it appends to, the directory
//! it writes with `-d` (one file per hour by default, named like 261119-140000 in UTC,
//! so name order is time order), or a command's stdout. Files are read from their end
//! when the app starts: arrivals from before then are too old to matter.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::app::App;
use crate::config::{Bearings, ReceiverCfg, Source};
use crate::toga::{Toga, parse_line};

/// Arrivals held while the core can't be reached, at most.
const MAX_PENDING: usize = 20_000;
/// Report at least this often, so a quiet receiver doesn't look dead.
const HEARTBEAT_MS: i64 = 60_000;

#[derive(Default)]
pub struct Pending {
    pub togas: Vec<Toga>,
    pub dropped: u64,
    pub bad: u64,
    pub error: Option<String>,
}

impl Pending {
    pub fn take_lines(&mut self, lines: &[String], cfg: &ReceiverCfg) {
        for l in lines {
            match parse_line(l) {
                Ok(Some(t)) if cfg.keep(&t) => {
                    if self.togas.len() < MAX_PENDING {
                        self.togas.push(t);
                    } else {
                        self.dropped += 1;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if self.bad == 0 {
                        tracing::warn!("{e}");
                    }
                    self.bad += 1;
                }
            }
        }
    }
}

/// Reads what has been appended to one file since last time, in whole lines.
pub struct Tail {
    path: PathBuf,
    pos: u64,
    partial: Vec<u8>,
}

impl Tail {
    pub fn new(path: PathBuf, pos: u64) -> Self {
        Self {
            path,
            pos,
            partial: vec![],
        }
    }

    pub fn at_end(path: PathBuf) -> Self {
        let pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self::new(path, pos)
    }

    pub fn read(&mut self) -> std::io::Result<Vec<String>> {
        let mut f = File::open(&self.path)?;
        let len = f.metadata()?.len();
        if len < self.pos {
            // truncated or replaced: start over
            self.pos = 0;
            self.partial.clear();
        }
        f.seek(SeekFrom::Start(self.pos))?;
        let mut buf = vec![];
        let n = f.take(4 << 20).read_to_end(&mut buf)?;
        self.pos += n as u64;
        self.partial.extend_from_slice(&buf);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Ok(vec![]);
        };
        let rest = self.partial.split_off(end + 1);
        let done = std::mem::replace(&mut self.partial, rest);
        Ok(String::from_utf8_lossy(&done)
            .lines()
            .map(str::to_string)
            .collect())
    }
}

fn hourly_name(n: &str) -> bool {
    let b = n.as_bytes();
    b.len() == 13
        && b[6] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 6 || c.is_ascii_digit())
}

fn hourly_files(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut v: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| hourly_name(n))
        .collect();
    v.sort();
    Ok(v)
}

/// Follows the newest file in vttoga's `-d` directory, moving on as new ones appear.
pub struct DirTail {
    dir: PathBuf,
    cur: Option<(String, Tail)>,
}

impl DirTail {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, cur: None }
    }

    pub fn read(&mut self) -> std::io::Result<Vec<String>> {
        let files = hourly_files(&self.dir)?;
        let mut lines = vec![];
        let Some((name, tail)) = &mut self.cur else {
            if let Some(newest) = files.last() {
                self.cur = Some((newest.clone(), Tail::at_end(self.dir.join(newest))));
            }
            return Ok(lines);
        };
        lines.extend(tail.read()?);
        let newer: Vec<String> = files
            .into_iter()
            .filter(|f| f.as_str() > name.as_str())
            .collect();
        for next in newer {
            let mut t = Tail::new(self.dir.join(&next), 0);
            lines.extend(t.read()?);
            *name = next;
            *tail = t;
        }
        Ok(lines)
    }
}

fn set_error(p: &Mutex<Pending>, e: Option<String>) {
    let mut p = p.lock().unwrap();
    if p.error != e {
        if let Some(e) = &e {
            tracing::warn!("receiver: {e}");
        }
        p.error = e;
    }
}

/// Longest line kept; anything longer is cut, and the rest comes back as further lines
/// that won't parse. vttoga and APRS lines are well under 512 bytes.
const MAX_LINE: u64 = 8192;

/// The next line without its line ending, with bad UTF-8 replaced instead of ending the
/// stream (a noisy serial link or a binary blip shouldn't stop the reader), and at most
/// 8 KB of it, so a misbehaving server can't fill memory with one line.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    buf.clear();
    if (&mut *r).take(MAX_LINE).read_until(b'\n', buf).await? == 0 {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(buf)
            .trim_end_matches(['\r', '\n'])
            .to_string(),
    ))
}

/// What the end of the configured vttoga output holds: for `check-source`, to see a new
/// receiver's setup working before waiting for strikes.
pub fn check_source(cfg: &ReceiverCfg, now: i64) -> Result<String> {
    let path = match cfg.source() {
        Source::File(p) => p,
        Source::Dir(d) => {
            let files = hourly_files(&d).with_context(|| format!("reading {}", d.display()))?;
            let Some(newest) = files.last() else {
                bail!(
                    "{} has no vttoga output files (named like 261119-140000) yet",
                    d.display()
                );
            };
            d.join(newest)
        }
        Source::Command(c) => {
            bail!("the source is a command; run it yourself to see its output: {c}")
        }
    };
    let len = std::fs::metadata(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    let mut tail = Tail::new(path.clone(), len.saturating_sub(64 * 1024));
    let mut lines = tail.read()?;
    if len > 64 * 1024 && !lines.is_empty() {
        // the first one is probably cut
        lines.remove(0);
    }
    let everything = ReceiverCfg {
        min_rms: 0.0,
        min_impulse: 0.0,
        ..cfg.clone()
    };
    let mut p = Pending::default();
    p.take_lines(&lines, &everything);
    let mut out = format!(
        "{}: {} lines read from the end, {} arrivals, {} that didn't parse",
        path.display(),
        lines.len(),
        p.togas.len(),
        p.bad
    );
    match p.togas.iter().map(|t| t.t_us).max() {
        Some(t) => {
            let ago = (now * 1000 - t) as f64 / 1e6;
            out.push_str(&format!(
                "\nnewest arrival {}, {ago:.1} s before this computer's clock",
                crate::web::strike_time(t)
            ));
            if ago < -1.0 {
                out.push_str(
                    "\nit's in the future: check the GPS timing (vttime) or this computer's clock",
                );
            }
            let weak = p.togas.iter().filter(|t| !cfg.keep(t)).count();
            if weak > 0 {
                out.push_str(&format!(
                    "\n{weak} of them are under min_rms or min_impulse and would be dropped"
                ));
            }
            let bearings = p.togas.iter().filter(|t| t.bearing.is_some()).count();
            let how = match cfg.bearings {
                Bearings::Axis => "used as lines through the site (bearings = \"axis\")",
                Bearings::Full => "used as directions (bearings = \"full\")",
                Bearings::Off => "not used (bearings = \"off\")",
            };
            if bearings == 0 {
                out.push_str("\nno bearings (vttoga adds them with crossed loops, -p)");
            } else {
                out.push_str(&format!("\n{bearings} with a bearing, {how}"));
            }
        }
        None => out.push_str("\nno H or M records yet: is vttoga running and triggering?"),
    }
    Ok(out)
}

type ReadFn = Box<dyn FnMut() -> std::io::Result<Vec<String>>>;

fn poll_files(cfg: &ReceiverCfg, pending: &Mutex<Pending>) {
    let (what, mut read): (String, ReadFn) = match cfg.source() {
        Source::File(p) => {
            let mut t = Tail::at_end(p.clone());
            (p.display().to_string(), Box::new(move || t.read()))
        }
        Source::Dir(d) => {
            let mut t = DirTail::new(d.clone());
            (d.display().to_string(), Box::new(move || t.read()))
        }
        Source::Command(_) => unreachable!("commands are read by run_command"),
    };
    loop {
        match read() {
            Ok(lines) => {
                set_error(pending, None);
                pending.lock().unwrap().take_lines(&lines, cfg);
            }
            Err(e) => set_error(pending, Some(format!("can't read {what}: {e}"))),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

async fn run_command(cmd: String, cfg: ReceiverCfg, pending: Arc<Mutex<Pending>>) {
    loop {
        let child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                set_error(&pending, Some(format!("can't start toga_command: {e}")));
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };
        set_error(&pending, None);
        let mut out = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut buf = vec![];
        loop {
            match read_line(&mut out, &mut buf).await {
                Ok(Some(l)) => pending.lock().unwrap().take_lines(&[l], &cfg),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("reading toga_command: {e}");
                    // it may still be running, and nobody is reading its output now
                    let _ = child.start_kill();
                    break;
                }
            }
        }
        let status = child
            .wait()
            .await
            .map(|s| s.to_string())
            .unwrap_or_else(|e| e.to_string());
        set_error(
            &pending,
            Some(format!(
                "toga_command exited ({status}); restarting in 10 s"
            )),
        );
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

/// What gpsd last said, by our clock: the fix mode (2 = 2D, 3 = 3D) and when, and the
/// last time the fix was lost (no fix, a gap in reports, or gpsd gone).
#[derive(Default)]
struct Gps {
    mode: u64,
    at: i64,
    lost_at: i64,
}

type Fix = Mutex<Gps>;

fn saw_tpv(fix: &Fix, mode: u64, now: i64) {
    let mut g = fix.lock().unwrap();
    if mode < 2 || now - g.at > 15_000 {
        g.lost_at = now;
    }
    g.mode = mode;
    g.at = now;
}

async fn watch_gpsd(addr: &str, fix: &Fix) -> Result<()> {
    let mut s = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(addr),
    )
    .await??;
    s.write_all(b"?WATCH={\"enable\":true,\"json\":true};\n")
        .await?;
    let mut r = BufReader::new(s);
    let mut buf = vec![];
    loop {
        let Some(l) =
            tokio::time::timeout(Duration::from_secs(30), read_line(&mut r, &mut buf)).await??
        else {
            bail!("gpsd closed the connection");
        };
        if let Ok(v) = serde_json::from_str::<Value>(&l)
            && v.get("class").and_then(Value::as_str) == Some("TPV")
        {
            let mode = v.get("mode").and_then(Value::as_u64).unwrap_or(0);
            saw_tpv(fix, mode, ridgewarden_client::util::now_ms());
        }
    }
}

/// "locked" with a 2D or 3D fix in the last 15 s that hasn't been lost since `since`,
/// "unlocked" otherwise (including when gpsd can't be reached), "unchecked" with no
/// gpsd configured.
fn gps_state(cfg: &ReceiverCfg, fix: &Fix, now: i64, since: i64) -> &'static str {
    if cfg.gpsd.is_none() {
        return "unchecked";
    }
    let g = fix.lock().unwrap();
    if g.mode >= 2 && now - g.at < 15_000 && g.lost_at < since {
        "locked"
    } else {
        "unlocked"
    }
}

/// One report for the core. `clock_s` is how far the newest arrival time is from this
/// computer's clock: vttoga's times come from GPS, so a big gap means the GPS timing or
/// the system clock is wrong. Bearings go out folded into 0 to 180 for loops alone, and
/// not at all when they're off.
pub fn report(
    site: &str,
    cfg: &ReceiverCfg,
    gps: &str,
    p: &Pending,
    togas: &[Toga],
    dropped: u64,
    now: i64,
) -> Value {
    let clock_s = togas
        .iter()
        .map(|t| t.t_us)
        .max()
        .map(|t| ((t / 1000 - now) as f64 / 100.0).round() / 10.0);
    let m = cfg.bearings.modulus();
    let bearing = |t: &Toga| {
        let m = m?;
        // to vttoga's one decimal, which can round up onto the modulus itself
        Some(((t.bearing? % m) * 10.0).round() / 10.0 % m)
    };
    json!({
        "site": site,
        "lat": cfg.lat,
        "lon": cfg.lon,
        "sigma_us": cfg.timing_sigma_us,
        "bearing_sigma_deg": m.map(|_| cfg.bearing_sigma_deg),
        "bearing_mod": m,
        "gps": gps,
        "togas": togas
            .iter()
            .map(|t| json!([t.t_us, t.rms, t.impulse, bearing(t)]))
            .collect::<Vec<_>>(),
        "dropped": dropped,
        "bad": p.bad,
        "error": p.error,
        "clock_s": clock_s,
    })
}

async fn site_name(app: &App) -> Option<String> {
    if let Some(n) = &app.cfg.receiver.as_ref()?.name {
        return Some(ridgewarden_client::util::norm_node(n));
    }
    let node = app.whoami().await.ok()?.node;
    (!node.is_empty()).then_some(node)
}

/// Read the receiver and report to the core, forever.
pub async fn run(app: Arc<App>) {
    let Some(cfg) = app.cfg.receiver.clone() else {
        return;
    };
    let pending = Arc::new(Mutex::new(Pending::default()));
    match cfg.source() {
        Source::Command(cmd) => {
            tokio::spawn(run_command(cmd, cfg.clone(), pending.clone()));
        }
        _ => {
            let (c, p) = (cfg.clone(), pending.clone());
            std::thread::spawn(move || poll_files(&c, &p));
        }
    }
    let fix: Arc<Fix> = Arc::default();
    if let Some(addr) = cfg.gpsd.clone() {
        let fix = fix.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = watch_gpsd(&addr, &fix).await {
                    tracing::warn!("gpsd at {addr}: {e:#}");
                }
                fix.lock().unwrap().lost_at = ridgewarden_client::util::now_ms();
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }
    let mut last_sent = 0;
    let mut last_flush = ridgewarden_client::util::now_ms();
    loop {
        tokio::time::sleep(Duration::from_secs(cfg.batch_s)).await;
        let Some(site) = site_name(&app).await else {
            tracing::debug!("no site name yet; waiting for the core to poll its node");
            continue;
        };
        let now = ridgewarden_client::util::now_ms();
        // arrivals read since the last flush count only if the GPS held its fix throughout
        let gps = gps_state(&cfg, &fix, now, last_flush);
        last_flush = now;
        let (payload, togas, dropped) = {
            let mut p = pending.lock().unwrap();
            if p.togas.is_empty() && now - last_sent < HEARTBEAT_MS {
                continue;
            }
            let mut togas = std::mem::take(&mut p.togas);
            let mut dropped = std::mem::take(&mut p.dropped);
            if gps == "unlocked" {
                dropped += togas.len() as u64;
                togas.clear();
            }
            togas.sort_by_key(|t| t.t_us);
            togas.dedup_by_key(|t| t.t_us);
            // the rest go in the next report
            let rest = togas.split_off(togas.len().min(cfg.max_per_batch));
            p.togas.extend(rest);
            let payload = report(&site, &cfg, gps, &p, &togas, dropped, now);
            p.bad = 0;
            (payload, togas, dropped)
        };
        match app.core.append_telemetry("lightning.toga", payload).await {
            Ok(_) => last_sent = now,
            Err(e) => {
                tracing::warn!("reporting {} arrivals to the core: {e:#}", togas.len());
                // keep them for the next try, or at least count them
                let mut p = pending.lock().unwrap();
                p.dropped += dropped;
                if p.togas.len() + togas.len() <= MAX_PENDING {
                    p.togas.extend(togas);
                } else {
                    p.dropped += togas.len() as u64;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn append(path: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn tail_returns_whole_new_lines_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("toga");
        append(&p, "H 1.000000 1e-02 3.00\n");
        let mut t = Tail::at_end(p.clone());
        assert!(t.read().unwrap().is_empty(), "starts at the end");
        append(&p, "H 2.000000 1e-02 3.00\nH 3.0000");
        assert_eq!(t.read().unwrap(), vec!["H 2.000000 1e-02 3.00"]);
        append(&p, "00 1e-02 3.00\n");
        assert_eq!(t.read().unwrap(), vec!["H 3.000000 1e-02 3.00"]);
        // truncated by a log rotation: read from the start again
        std::fs::write(&p, "H 4.000000 1e-02 3.00\n").unwrap();
        assert_eq!(t.read().unwrap(), vec!["H 4.000000 1e-02 3.00"]);
    }

    #[test]
    fn dir_tail_moves_on_to_the_next_hour() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        append(&d.join("261119-130000"), "H 1.000000 1e-02 3.00\n");
        append(&d.join("notes.txt"), "not toga\n");
        let mut t = DirTail::new(d.to_path_buf());
        assert!(t.read().unwrap().is_empty());
        append(&d.join("261119-130000"), "H 2.000000 1e-02 3.00\n");
        append(&d.join("261119-140000"), "H 3.000000 1e-02 3.00\n");
        let got = t.read().unwrap();
        assert_eq!(got, vec!["H 2.000000 1e-02 3.00", "H 3.000000 1e-02 3.00"]);
        append(&d.join("261119-140000"), "H 4.000000 1e-02 3.00\n");
        assert_eq!(t.read().unwrap(), vec!["H 4.000000 1e-02 3.00"]);
    }

    fn cfg(extra: &str) -> ReceiverCfg {
        toml::from_str(&format!(
            "lat = 35.0\nlon = -120.0\ntoga_file = \"x\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn check_source_reports_what_the_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("toga");
        append(
            &p,
            "H 1795000000.000000 1e-02 3.00\nH x\nH 1795000010.500000 1e-02 1.10 87.5\n",
        );
        let mut cfg = cfg("min_impulse = 2.0");
        cfg.toga_file = Some(p);
        let s = check_source(&cfg, 1_795_000_012_500).unwrap();
        assert!(s.contains("2 arrivals, 1 that didn't parse"), "{s}");
        assert!(s.contains("2.0 s before"), "{s}");
        assert!(
            s.contains("1 of them are under min_rms or min_impulse"),
            "{s}"
        );
        assert!(s.contains("1 with a bearing, used as lines"), "{s}");
        let s = check_source(&cfg, 1_795_000_000_000).unwrap();
        assert!(s.contains("in the future"), "{s}");
    }

    #[test]
    fn pending_counts_bad_lines_and_skips_weak_or_dull_sferics() {
        let mut p = Pending::default();
        let lines: Vec<String> = [
            "H 1.000000 5.0e-02 3.00",
            "H 2.000000 1.0e-04 3.00",
            "H 3.000000 5.0e-02 1.05",
            "H x",
            "E",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        p.take_lines(&lines, &cfg("min_rms = 1e-3\nmin_impulse = 1.5"));
        assert_eq!(p.togas.len(), 1);
        assert_eq!(p.togas[0].t_us, 1_000_000);
        assert_eq!(p.bad, 1);
    }

    fn togas_of(r: &Value) -> Vec<Value> {
        r["togas"].as_array().unwrap().clone()
    }

    #[test]
    fn reports_carry_impulse_ratios_and_bearings_as_configured() {
        let togas = [
            Toga {
                t_us: 1,
                rms: 0.01,
                impulse: Some(4.71),
                bearing: Some(300.0),
            },
            Toga {
                t_us: 2,
                rms: 0.01,
                impulse: None,
                bearing: Some(179.96),
            },
            Toga {
                t_us: 3,
                rms: 0.01,
                impulse: None,
                bearing: Some(360.0),
            },
        ];
        let p = Pending::default();
        let axis = report("tst-a", &cfg(""), "locked", &p, &togas, 0, 0);
        assert_eq!(axis["bearing_mod"], json!(180.0));
        assert_eq!(axis["bearing_sigma_deg"], json!(5.0));
        assert_eq!(
            togas_of(&axis),
            vec![
                json!([1, 0.01, 4.71, 120.0]),
                json!([2, 0.01, null, 0.0]),
                json!([3, 0.01, null, 0.0])
            ]
        );
        let full = report(
            "tst-a",
            &cfg("bearings = \"full\""),
            "locked",
            &p,
            &togas,
            0,
            0,
        );
        assert_eq!(togas_of(&full)[0], json!([1, 0.01, 4.71, 300.0]));
        assert_eq!(togas_of(&full)[2], json!([3, 0.01, null, 0.0]));
        let off = report(
            "tst-a",
            &cfg("bearings = \"off\""),
            "locked",
            &p,
            &togas,
            0,
            0,
        );
        assert_eq!(off["bearing_mod"], Value::Null);
        assert_eq!(togas_of(&off)[0], json!([1, 0.01, 4.71, null]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_output_is_read_and_its_exit_reported() {
        let p = Arc::new(Mutex::new(Pending::default()));
        let task = tokio::spawn(run_command(
            "printf 'H 1795000000.000001 1.0e-02 3.00\nE\n'".into(),
            cfg(""),
            p.clone(),
        ));
        for _ in 0..50 {
            if p.lock().unwrap().error.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        task.abort();
        let p = p.lock().unwrap();
        assert_eq!(p.togas.len(), 1);
        assert!(p.error.as_deref().unwrap_or("").contains("exited"));
    }

    #[tokio::test]
    async fn long_lines_are_cut_and_bad_bytes_replaced() {
        let mut data = vec![b'x'; 10_000];
        data.extend_from_slice(b"\nH 1.000000 1e-02 3.00\r\n\xff\n");
        let mut r = BufReader::new(&data[..]);
        let mut buf = vec![];
        assert_eq!(
            read_line(&mut r, &mut buf).await.unwrap().unwrap().len(),
            8192
        );
        assert_eq!(
            read_line(&mut r, &mut buf).await.unwrap().unwrap().len(),
            1808
        );
        assert_eq!(
            read_line(&mut r, &mut buf).await.unwrap().unwrap(),
            "H 1.000000 1e-02 3.00"
        );
        assert_eq!(
            read_line(&mut r, &mut buf).await.unwrap().unwrap(),
            "\u{fffd}"
        );
        assert_eq!(read_line(&mut r, &mut buf).await.unwrap(), None);
    }

    #[test]
    fn gps_without_a_recent_fix_is_unlocked() {
        let mut cfg = cfg("");
        let fix = Fix::default();
        assert_eq!(gps_state(&cfg, &fix, 0, 0), "unchecked");
        cfg.gpsd = Some("127.0.0.1:2947".into());
        assert_eq!(gps_state(&cfg, &fix, 0, 0), "unlocked");
        saw_tpv(&fix, 3, 1_000);
        assert_eq!(gps_state(&cfg, &fix, 5_000, 2_000), "locked");
        assert_eq!(gps_state(&cfg, &fix, 20_000, 2_000), "unlocked", "stale");
        // lost and found again between two flushes: that batch is still dropped
        saw_tpv(&fix, 1, 6_000);
        saw_tpv(&fix, 3, 7_000);
        assert_eq!(gps_state(&cfg, &fix, 8_000, 5_000), "unlocked");
        assert_eq!(gps_state(&cfg, &fix, 18_000, 8_000), "locked");
    }
}
