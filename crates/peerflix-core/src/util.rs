//! Helpers the rest share: formatting and reading sizes, formatting dates,
//! building magnets, and raising the open file limit.

pub fn human_bytes(n: u64) -> String {
    const UNIT: u64 = 1024;
    if n < UNIT {
        return format!("{n} B");
    }
    let (mut div, mut exp) = (UNIT, 0);
    let mut m = n / UNIT;
    while m >= UNIT {
        div *= UNIT;
        exp += 1;
        m /= UNIT;
    }
    format!(
        "{:.1} {}iB",
        n as f64 / div as f64,
        char::from(b"KMGTPE"[exp])
    )
}

/// Reads a size as human_bytes writes it, or as nyaa does, such as 1.2 GiB
/// or 940 Bytes, into bytes. None if it isn't one.
pub fn parse_bytes(s: &str) -> Option<u64> {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let (n, unit) = s.trim().split_once(' ')?;
    let n: f64 = n.parse().ok()?;
    let exp = match unit {
        "Byte" | "Bytes" => 0,
        unit => UNITS.iter().position(|&u| u == unit)?,
    };
    (n >= 0.).then(|| (n * 1024f64.powi(exp as i32)) as u64)
}

/// Raises the soft limit on open files as far as the system allows. Peer
/// connections alone can come close to macOS's default of 256: streaming one
/// episode of a big season pack held 151 open.
pub fn raise_open_file_limit() {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit and setrlimit only read and write lim.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        // macOS refuses more than kern.maxfilesperproc even when the hard
        // limit is unlimited, so fall back to OPEN_MAX, which it always takes.
        for want in [lim.rlim_max, 10240] {
            if want <= lim.rlim_cur {
                return;
            }
            let new = libc::rlimit {
                rlim_cur: want,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new) == 0 {
                return;
            }
        }
    }
}

/// The trackers put in magnets built from a bare info hash.
pub const TRACKERS: [&str; 5] = [
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://open.dstud.io:6969/announce",
];

/// Builds a magnet link for an info hash, naming it name.
pub fn magnet(hash: &str, name: &str) -> String {
    let mut url =
        reqwest::Url::parse(&format!("magnet:?xt=urn:btih:{hash}")).expect("magnet URLs parse");
    let mut query = url.query_pairs_mut();
    query.append_pair("dn", name);
    for tr in TRACKERS {
        query.append_pair("tr", tr);
    }
    drop(query);
    url.into()
}

/// Formats a Unix time as a UTC YYYY-MM-DD date.
pub fn unix_date(secs: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes() {
        for (n, want) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (1 << 20, "1.0 MiB"),
            (5 << 30, "5.0 GiB"),
            (3 << 40, "3.0 TiB"),
        ] {
            assert_eq!(human_bytes(n), want);
        }
    }

    #[test]
    fn parses_sizes() {
        for (s, want) in [
            ("1023 B", Some(1023)),
            ("940 Bytes", Some(940)),
            ("1 Byte", Some(1)),
            ("1.5 KiB", Some(1536)),
            (" 2.0 GiB ", Some(2 << 30)),
            ("", None),
            ("big", None),
            ("3 parsecs", None),
            ("-1 KiB", None),
        ] {
            assert_eq!(parse_bytes(s), want, "{s:?}");
        }
        for n in [0, 1023, 1536, 5 << 30] {
            assert_eq!(parse_bytes(&human_bytes(n)), Some(n));
        }
    }

    #[test]
    fn unix_dates() {
        for (secs, want) in [
            (0, "1970-01-01"),
            (951_782_400, "2000-02-29"),
            (1_790_612_897, "2026-09-28"),
            (-86_400, "1969-12-31"),
        ] {
            assert_eq!(unix_date(secs), want, "{secs}");
        }
    }
}
