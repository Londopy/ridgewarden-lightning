//! vlfrx-tools TOGA records, as written by `vttoga` (and read by its solver, `vtspot -m`).
//!
//! vttoga prints one `H` record per sferic it accepts:
//!
//!   H <unix seconds, 6 decimals> <RMS amplitude, %.3e> <impulse ratio, %.2f> [bearing, degrees]
//!
//! e.g. `H 1795000000.123456 3.215e-02 4.71`. The timestamp is the time of group arrival
//! (earliest across bands) at 1 microsecond resolution, from the GPS-disciplined sample
//! stream. The impulse ratio is the signal energy in the 200 us after the trigger over the
//! energy in the 200 us before, so a sharp sferic onset is well above 1 and steady noise
//! is near it (vttoga can trigger on it with `-i`). The bearing is there only with crossed
//! loops (`-p`): 0 to 180 degrees when there is no whip (E-field) channel to tell which
//! way along the line the source is, 0 to 360 with one; printed to one decimal, just under
//! the top reads as 180.0 or 360.0. vtspot needs the first three fields and takes a fourth
//! as a bearing only when it is a number, and these are read no more strictly. With `-e`
//! options extended records (B, S, T, P, Q) follow, ended by an `E` record; those are
//! skipped. `M` records, `M <ref time> <toga offset> <edge offset> <rms> ...`, are read as
//! an arrival at ref + toga offset, using nothing after the RMS.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Toga {
    /// time of group arrival, unix microseconds
    pub t_us: i64,
    pub rms: f64,
    /// vttoga's impulse ratio; None for M records
    pub impulse: Option<f64>,
    /// degrees from true north toward the source, from crossed loops
    pub bearing: Option<f64>,
}

/// "1795000000.123456" to microseconds, without going through f64 (which would lose
/// the last digit at today's timestamps).
fn parse_us(s: &str) -> Option<i64> {
    let (secs, frac) = s.split_once('.').unwrap_or((s, ""));
    let secs: i64 = secs.parse().ok()?;
    if secs < 0 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut digits: Vec<u8> = frac.bytes().take(7).collect();
    digits.resize(7, b'0');
    let tenths_us: i64 = std::str::from_utf8(&digits).ok()?.parse().ok()?;
    Some(secs * 1_000_000 + (tenths_us + 5) / 10)
}

fn num(s: Option<&str>) -> Option<f64> {
    s?.parse::<f64>().ok().filter(|x| x.is_finite())
}

/// One line of vttoga output: `Ok(Some)` for an arrival, `Ok(None)` for records that
/// carry none (extended output, blank lines), `Err` for an H or M record that doesn't
/// parse.
pub fn parse_line(line: &str) -> Result<Option<Toga>, String> {
    let mut f = line.split_whitespace();
    let bad = || {
        format!(
            "bad TOGA record {:?}",
            line.chars().take(80).collect::<String>()
        )
    };
    match f.next() {
        Some("H") => {
            let t_us = f.next().and_then(parse_us).ok_or_else(bad)?;
            let rms = num(f.next()).ok_or_else(bad)?;
            let impulse = num(f.next()).ok_or_else(bad)?;
            Ok(Some(Toga {
                t_us,
                rms,
                impulse: Some(impulse).filter(|i| *i >= 0.0),
                bearing: num(f.next()).filter(|b| (0.0..=360.0).contains(b)),
            }))
        }
        Some("M") => {
            let base = f.next().and_then(parse_us).ok_or_else(bad)?;
            let off = num(f.next()).filter(|o| o.abs() < 1.0).ok_or_else(bad)?;
            num(f.next()).ok_or_else(bad)?;
            let rms = num(f.next()).ok_or_else(bad)?;
            Ok(Some(Toga {
                t_us: base + (off * 1e6).round() as i64,
                rms,
                impulse: None,
                bearing: None,
            }))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_h_records_to_the_microsecond() {
        assert_eq!(
            parse_line("H 1795000000.123456 3.215e-02 4.71\n"),
            Ok(Some(Toga {
                t_us: 1_795_000_000_123_456,
                rms: 0.03215,
                impulse: Some(4.71),
                bearing: None,
            }))
        );
        // with a bearing from a crossed-loop receiver
        let t = parse_line("H 1795000000.000001 1.000e+00 2.00 123.4")
            .unwrap()
            .unwrap();
        assert_eq!(t.t_us, 1_795_000_000_000_001);
        assert_eq!((t.impulse, t.bearing), (Some(2.0), Some(123.4)));
        // vttoga's %.1f of a bearing just under 360
        let t = parse_line("H 1795000000.000001 1.000e+00 2.00 360.0").unwrap();
        assert_eq!(t.unwrap().bearing, Some(360.0));
    }

    #[test]
    fn reads_m_records_as_reference_plus_offset() {
        let t = parse_line("M 1795000000.100000 0.000250 -1 2.5e-02 0")
            .unwrap()
            .unwrap();
        assert_eq!(t.t_us, 1_795_000_000_100_250);
        assert_eq!((t.impulse, t.bearing), (None, None));
        // nothing past the RMS is read, so nothing there is needed
        let t = parse_line("M 1795000000.100000 0.000250 -1 2.5e-02").unwrap();
        assert_eq!(t.unwrap().t_us, 1_795_000_000_100_250);
        let t = parse_line("M 1795000000.100000 0.000250 -1 2.5e-02 0 271.0").unwrap();
        assert_eq!(t.unwrap().bearing, None);
    }

    #[test]
    fn skips_extended_records_and_rejects_broken_ones() {
        for l in [
            "B 1795000000.123000 1.2e-03 4000 17000 0.000420 1.1e-03 12.0",
            "S 1000.0 0 1.2e-03 45.00",
            "Q 0.91 0.80 0.75",
            "E",
            "",
        ] {
            assert_eq!(parse_line(l), Ok(None), "{l}");
        }
        assert!(parse_line("H 1795000000.12x456 3.2e-02 4.71").is_err());
        assert!(parse_line("H 1795000000.123456").is_err());
        assert!(parse_line("H 1795000000.123456 3.2e-02 dull").is_err());
        assert!(parse_line("H -5.0 1 1").is_err());
        // as vtspot reads them: the arrival stands without a ratio or bearing it can't use
        let fields = |l: &str| {
            let t = parse_line(l).unwrap().unwrap();
            (t.t_us, t.impulse, t.bearing)
        };
        let t = 1_795_000_000_123_456;
        assert_eq!(fields("H 1795000000.123456 3.2e-02 -1.00"), (t, None, None));
        for b in ["360.1", "-0.1", "north"] {
            let l = format!("H 1795000000.123456 3.2e-02 4.71 {b}");
            assert_eq!(fields(&l), (t, Some(4.71), None), "{b}");
        }
    }
}
