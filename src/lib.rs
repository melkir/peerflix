//! Finding torrents and streaming them over HTTP.
//!
//! [`search`] queries torrent sites, each a [`providers::Provider`], in
//! parallel. [`torrent`] adds a torrent to a librqbit session and serves the
//! picked file over a local HTTP server with range support; reads prioritize
//! the pieces around the player's read position, so playback starts as soon
//! as the first pieces arrive and seeking works.

pub mod providers;
pub mod search;
pub mod stream;
#[cfg(test)]
mod testutil;
pub mod torrent;
pub mod util;
