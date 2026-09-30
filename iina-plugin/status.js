// Describes a stream's status, as peerflix returns it from its control URL,
// for the players and the search window alike.

function describe(s) {
  if (s.checking) return "Checking downloaded data…";
  if (done(s)) return `Downloaded ${bytes(s.size)}`;
  const percent = ((100 * s.downloaded) / Math.max(s.size, 1)).toFixed(1);
  const peers = `${s.peers} ${s.peers === 1 ? "peer" : "peers"}`;
  return `${percent}% of ${bytes(s.size)} · ${bytes(s.download_speed)}/s · ${peers}`;
}

function done(s) {
  return !s.checking && s.downloaded >= s.size;
}

// Formats n bytes as peerflix does on the command line.
function bytes(n) {
  if (n < 1024) return `${n} B`;
  let exp = 0;
  let div = 1024;
  while (n / div >= 1024 && exp < 5) {
    div *= 1024;
    exp++;
  }
  return `${(n / div).toFixed(1)} ${"KMGTPE"[exp]}iB`;
}

module.exports = { describe, done };
