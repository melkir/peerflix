//! Choosing what to stream from a torrent's files: the episode to offer or
//! pick, and the subtitles that go with it.

use std::{cmp::Ordering, path::Path};

use anyhow::{Context, bail};
use librqbit::ListOnlyResponse;

use crate::http::content_type;

#[derive(Clone)]
pub struct TorrentFile {
    pub path: String,
    pub len: u64,
    /// BEP 47 padding files only exist to align the next file to a piece.
    pub padding: bool,
}

pub fn torrent_files(meta: &ListOnlyResponse) -> Vec<TorrentFile> {
    meta.info
        .iter_file_details()
        .map(|d| TorrentFile {
            path: d.filename.to_pathbuf().to_string_lossy().into_owned(),
            len: d.len,
            padding: d.attrs().padding,
        })
        .collect()
}

/// Returns the video files worth choosing between, episodes first, each group
/// in natural path order: those at least a tenth the size of the largest,
/// which leaves out samples.
pub fn episodes(files: &[TorrentFile]) -> Vec<usize> {
    let videos = || {
        files
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.padding && is_video(f))
    };
    let largest = videos().map(|(_, f)| f.len).max().unwrap_or(0);
    // Episodes, tagged like S01E02, before extras such as featurettes.
    let mut eps: Vec<(bool, usize)> = videos()
        .filter(|(_, f)| f.len.saturating_mul(10) >= largest)
        .map(|(i, f)| (episode_tag(&f.path.to_lowercase()).is_none(), i))
        .collect();
    eps.sort_by(|&(a_untagged, a), &(b_untagged, b)| {
        a_untagged
            .cmp(&b_untagged)
            .then_with(|| natural_cmp(&files[a].path, &files[b].path))
    });
    eps.into_iter().map(|(_, i)| i).collect()
}

/// Compares strings with runs of digits compared as numbers, so Episode 2
/// sorts before Episode 10.
fn natural_cmp(mut a: &str, mut b: &str) -> Ordering {
    loop {
        let (Some(x), Some(y)) = (a.chars().next(), b.chars().next()) else {
            return a.len().cmp(&b.len());
        };
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (na, ra) = a.split_at(leading_digits(a));
            let (nb, rb) = b.split_at(leading_digits(b));
            let (na, nb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
            let ord = na.len().cmp(&nb.len()).then(na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
            (a, b) = (ra, rb);
        } else if x != y {
            return x.cmp(&y);
        } else {
            (a, b) = (&a[x.len_utf8()..], &b[y.len_utf8()..]);
        }
    }
}

/// Returns the subtitle files that go with video file id: those named after
/// it (Show.S01E02.en.srt), in a folder named after it (Subs/Show.S01E02/),
/// or tagged with the same episode, or all of them when the torrent holds a
/// single video.
pub fn subtitles(files: &[TorrentFile], id: usize, single: bool) -> Vec<usize> {
    let lower_stem = |p: &str| {
        Path::new(p)
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().to_lowercase())
    };
    let video = &files[id].path;
    let stem = lower_stem(video);
    let tag = episode_tag(&stem);
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding && is_subtitle(f))
        .filter(|(_, f)| {
            let path = f.path.to_lowercase();
            let sub_stem = lower_stem(&path);
            let named = sub_stem
                .strip_prefix(&stem)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric()));
            let in_dir = Path::new(&path)
                .parent()
                .is_some_and(|d| d.iter().any(|c| c.to_string_lossy() == stem));
            single || named || in_dir || tag.is_some_and(|t| episode_tag(&sub_stem) == Some(t))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Returns the season and episode of the first S01E02 tag in the lowercase
/// string s.
fn episode_tag(s: &str) -> Option<(u32, u32)> {
    s.match_indices('s').find_map(|(i, _)| {
        let rest = &s[i + 1..];
        let n = leading_digits(rest);
        let season = rest[..n].parse().ok()?;
        let rest = rest[n..].strip_prefix('e')?;
        let episode = rest[..leading_digits(rest)].parse().ok()?;
        Some((season, episode))
    })
}

/// Returns the length in bytes of the ASCII digits s starts with.
fn leading_digits(s: &str) -> usize {
    s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len()
}

fn is_subtitle(f: &TorrentFile) -> bool {
    let ext = Path::new(&f.path).extension().and_then(|e| e.to_str());
    ext.is_some_and(|e| ["srt", "ass", "ssa", "vtt"].contains(&e.to_ascii_lowercase().as_str()))
}

fn is_video(f: &TorrentFile) -> bool {
    content_type(&f.path).starts_with("video/")
}

/// Returns the index of the file at index, or of the largest video file
/// (falling back to the largest file) when index is None.
pub fn pick_file(files: &[TorrentFile], index: Option<usize>) -> anyhow::Result<usize> {
    if let Some(i) = index {
        if i >= files.len() {
            bail!(
                "file index {i} out of range (torrent has {} files)",
                files.len()
            );
        }
        return Ok(i);
    }
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.padding)
        .max_by_key(|(_, f)| (is_video(f), f.len))
        .map(|(i, _)| i)
        .context("torrent has no files")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(spec: &[(&str, u64, bool)]) -> Vec<TorrentFile> {
        spec.iter()
            .map(|&(path, len, padding)| TorrentFile {
                path: path.into(),
                len,
                padding,
            })
            .collect()
    }

    #[test]
    fn picks_largest_video() {
        let fs = files(&[
            ("sample.mkv", 10, false),
            ("movie.MP4", 100, false),
            ("extras.zip", 1000, false),
            (".pad/5000", 5000, true),
            ("subs/en.srt", 4, false),
        ]);
        assert_eq!(pick_file(&fs, None).unwrap(), 1);
        assert_eq!(pick_file(&fs, Some(4)).unwrap(), 4);
        assert!(pick_file(&fs, Some(5)).is_err());
        assert!(pick_file(&[], None).is_err());
    }

    #[test]
    fn finds_episodes() {
        let fs = files(&[
            ("Show/Episode 10.mkv", 900, false),
            ("Show/Sample/sample.mkv", 50, false),
            ("Show/Episode 2.mkv", 1000, false),
            (".pad/100", 100, true),
            ("Show/Episode 1.mkv", 700, false),
            ("Show/cover.jpg", 5000, false),
        ]);
        assert_eq!(episodes(&fs), [4, 2, 0]);
        let pack = files(&[
            ("Featurettes/Making of.mkv", 900, false),
            ("Season 1/Show - S01E02.mkv", 1000, false),
            ("Season 1/Show - S01E01.mkv", 1000, false),
        ]);
        assert_eq!(episodes(&pack), [2, 1, 0]);
        let one = files(&[("movie.mkv", 1000, false), ("sample.mkv", 20, false)]);
        assert_eq!(episodes(&one), [0]);
    }

    #[test]
    fn finds_subtitles() {
        let fs = files(&[
            ("Show/Show.S01E01.mkv", 1000, false),
            ("Show/Show.S01E02.mkv", 1000, false),
            ("Show/Show.S01E01.en.srt", 5, false),
            ("Show/Show.S01E02.srt", 5, false),
            ("Show/Subs/Show.S01E01/2_English.srt", 5, false),
            ("Show/Subs/s01e01.French.ass", 5, false),
            ("Show/Show.S01E010.srt", 5, false),
            ("Show/notes.txt", 5, false),
        ]);
        let single = episodes(&fs).len() <= 1;
        assert!(!single);
        assert_eq!(subtitles(&fs, 0, single), [2, 4, 5]);
        assert_eq!(subtitles(&fs, 1, single), [3]);

        let prefixes = files(&[
            ("Ep 1.mkv", 1000, false),
            ("Ep 10.mkv", 1000, false),
            ("Ep 1.srt", 5, false),
            ("Ep 10.srt", 5, false),
        ]);
        assert_eq!(subtitles(&prefixes, 0, false), [2]);

        let movie = files(&[
            ("Movie (2010)/Movie.mp4", 1000, false),
            ("Movie (2010)/Subs/English.srt", 5, false),
            ("Movie (2010)/Subs/Spanish.srt", 5, false),
        ]);
        assert_eq!(subtitles(&movie, 0, episodes(&movie).len() <= 1), [1, 2]);
    }

    #[test]
    fn episode_tags() {
        for (s, want) in [
            ("show.s01e02.720p", Some((1, 2))),
            ("seasons.s1e10", Some((1, 10))),
            ("show s01", None),
            ("movie", None),
        ] {
            assert_eq!(episode_tag(s), want, "{s}");
        }
    }

    #[test]
    fn natural_order() {
        use Ordering::*;
        for (a, b, want) in [
            ("ep2", "ep10", Less),
            ("S01E09", "S01E10", Less),
            ("S02E01", "S01E10", Greater),
            ("ep007", "ep7", Equal),
            ("a", "b", Less),
            ("ep1", "ep1.5", Less),
            ("", "", Equal),
        ] {
            assert_eq!(natural_cmp(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn falls_back_to_largest_file() {
        let fs = files(&[("small.txt", 1, false), ("big.bin", 50, false)]);
        assert_eq!(pick_file(&fs, None).unwrap(), 1);
    }
}
