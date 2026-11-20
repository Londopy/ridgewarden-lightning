//! vlfrx-tools TOGA records, as written by `vttoga` (and read by its solver, `vtspot -m`).
//!
//! vttoga prints one `H` record per sferic it accepts:
//!
//!   H <unix seconds, 6 decimals> <RMS amplitude, %.3e> <impulse ratio, %.2f> [bearing, degrees]
//!
//! e.g. `H 1795000000.123456 3.215e-02 4.71`. The timestamp is the time of group arrival
//! (earliest across bands) at 1 microsecond resolution, from the GPS-disciplined sample
//! stream. With `-e` options extended records (B, S, T, P, Q) follow, ended by an `E`
//! record; those are skipped. vtspot also accepts `M` records, `M <ref time> <toga offset>
//! <edge offset> <rms> <x> [bearing]`, whose arrival time is ref + toga offset; they are
//! read the same way.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Toga {
    /// time of group arrival, unix microseconds
    pub t_us: i64,
    pub rms: f64,
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
            num(f.next()).ok_or_else(bad)?;
            Ok(Some(Toga { t_us, rms }))
        }
        Some("M") => {
            let base = f.next().and_then(parse_us).ok_or_else(bad)?;
            let off = num(f.next()).filter(|o| o.abs() < 1.0).ok_or_else(bad)?;
            num(f.next()).ok_or_else(bad)?;
            let rms = num(f.next()).ok_or_else(bad)?;
            Ok(Some(Toga {
                t_us: base + (off * 1e6).round() as i64,
                rms,
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
                rms: 0.03215
            }))
        );
        // with a bearing from a crossed-loop receiver
        assert_eq!(
            parse_line("H 1795000000.000001 1.000e+00 2.00 123.4")
                .unwrap()
                .unwrap()
                .t_us,
            1_795_000_000_000_001
        );
    }

    #[test]
    fn reads_m_records_as_reference_plus_offset() {
        let t = parse_line("M 1795000000.100000 0.000250 -1 2.5e-02 0").unwrap();
        assert_eq!(t.unwrap().t_us, 1_795_000_000_100_250);
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
        assert!(parse_line("H -5.0 1 1").is_err());
    }
}
