// Reads a stream's status, as peerflix returns it from its control URL, for
// the players and the search window alike. Its text says how it's going.

function done(s) {
  return s.state === "done";
}

// Whether the download can be paused or resumed: not while the data is
// checked, nor once it's all there.
function pausable(s) {
  return s.state === "downloading" || s.state === "paused";
}

module.exports = { done, pausable };
