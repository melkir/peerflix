//! Finding torrents and streaming them over HTTP.
//!
//! [`search`] queries torrent sites, each a [`providers::Provider`], in
//! parallel. [`play`] streams a torrent from listing its files to naming the
//! ones that finish: [`torrent`] adds it to a librqbit session and [`http`]
//! serves the picked file with range support; reads prioritize the pieces
//! around the player's read position, so playback starts as soon as the first
//! pieces arrive and seeking works. [`player`] opens the stream in IINA.

pub mod http;
pub mod play;
pub mod player;
pub mod providers;
pub mod search;
#[cfg(test)]
mod testutil;
pub mod torrent;
pub mod util;
