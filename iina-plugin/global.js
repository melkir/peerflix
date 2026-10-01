// The search window, and the players it opens on peerflix's streams.
// peerflix does the searching and streaming; see its docs/json.md.

const { global, http, menu, preferences, standaloneWindow: win, utils } = iina;
const { pausable } = require("./status.js");

// The peerflix this plugin works with, of the same release; cargo release
// bumps it with the plugin's version.
const VERSION = "0.9.0";

// How often the streams' status is shown anew, in milliseconds.
const POLL = 1000;

// How long a stopped peerflix gets to exit before another streams its
// torrent anyway, in milliseconds.
const EXIT_WAIT = 10000;

// Where peerflix is looked for when its preference is empty. Apps started
// from the Dock don't get the shell's PATH. mise's shim comes last, as it
// only runs outside a shell when mise has a global version set.
const CANDIDATES = [
  "peerflix",
  "~/.cargo/bin/peerflix",
  "/opt/homebrew/bin/peerflix",
  "/usr/local/bin/peerflix",
  "~/.local/share/mise/shims/peerflix",
];

// Runs a command, "$@", in the background and prints the first line it
// writes, as soon as it's written, so that a stream peerflix keeps serving
// doesn't hold up exec, which runs one command at a time. If the command
// exits first, prints its errors and exits with its status; if it writes
// nothing for two minutes, such as a magnet no one seeds, stops it.
const FIRST_LINE = `
out=$(/usr/bin/mktemp) err=$(/usr/bin/mktemp)
"$@" >"$out" 2>"$err" &
pid=$!
i=0
while [ $(/usr/bin/wc -l <"$out") -eq 0 ]; do
  if ! kill -0 $pid 2>/dev/null; then
    wait $pid
    status=$?
    /bin/cat "$err" >&2
    /bin/rm -f "$out" "$err"
    exit $(( status == 0 ? 1 : status ))
  fi
  if [ $i -ge 1200 ]; then
    kill $pid
    echo "error: peerflix got nothing for two minutes" >&2
    /bin/rm -f "$out" "$err"
    exit 1
  fi
  i=$((i + 1))
  /bin/sleep 0.1
done
/usr/bin/head -n 1 "$out"
/bin/rm -f "$out" "$err"
`;

// The peerflix that listed a torrent's episodes and waits for one to be
// picked, with the torrent's source, info hash and control URL.
let pending = null;
// The id of the search window's pick being started, if any. A newer pick, or
// the window giving it up, replaces it, and a peerflix started for a pick
// that isn't current is stopped rather than played.
let current = null;
// The info hash of each torrent peerflix listed, by source, to tell a pick
// of a torrent that's playing.
const hashes = new Map();
// The streams players were opened on, each a name, the torrent's info hash,
// a control URL and the latest status, by player ID, until the player closes
// or the stream is stopped.
const streams = new Map();
// Whether the streams' status is being sent to their players and the search
// window.
let watching = false;

// Counts searches, so that a slow one can't replace a newer one's results.
let searches = 0;

menu.addItem(menu.item("Search Torrents…", showWindow));

function showWindow() {
  win.loadFile("search.html");
  // IINA adds " — peerflix", the plugin's name.
  win.setProperty({ title: "Search Torrents", resizable: true, hideTitleBar: false });
  win.setFrame(560, 640);
  win.onMessage("search", search);
  win.onMessage("open", open);
  win.onMessage("playEpisode", playEpisode);
  win.onMessage("cancel", cancel);
  win.onMessage("pause", ({ id }) => togglePause(id));
  win.onMessage("stop", ({ id }) => stopStream(id));
  win.open();
}

// The peerflix found and checked to be VERSION, which another found replaces.
let checked = null;

// Returns the peerflix to run, the one its preference names or the first of
// CANDIDATES there is, once it's checked to be VERSION.
async function findPeerflix() {
  const path = preferences.get("peerflix_path");
  const bin = (path ? [path] : CANDIDATES)
    .map((p) => (p.startsWith("~") ? utils.resolvePath(p) : p))
    .find((p) => utils.fileInPath(p));
  if (!bin) {
    throw new Error("peerflix wasn't found; set its path in the plugin's preferences.");
  }
  if (bin === checked) return bin;
  const res = await utils.exec("/bin/sh", ["-c", '"$@"', "sh", bin, "--version"]);
  // "peerflix 0.7.0", or "peerflix v0.7.0" from a release.
  const version = res.stdout.trim().split(" ").pop().replace(/^v/, "");
  if (res.status !== 0 || version !== VERSION) {
    throw new Error(
      `${bin} is peerflix ${version || "of an unknown version"}; this plugin needs peerflix ` +
        `${VERSION}. Update it, or set its path in the plugin's preferences.`
    );
  }
  checked = bin;
  return bin;
}

// Runs peerflix with args and returns the first line it writes, leaving it
// running if it doesn't exit. Throws peerflix's error when it fails.
async function run(args) {
  const bin = await findPeerflix();
  const res = await utils.exec("/bin/sh", ["-c", FIRST_LINE, "sh", bin, ...args]);
  if (res.status !== 0) {
    // The first error line says what went wrong; later ones tend to be
    // advice, as with mise's shim.
    const lines = res.stderr.trim().split("\n");
    const line = lines.find((l) => /error/i.test(l)) || lines[lines.length - 1];
    throw new Error(line.replace(/^error: /, "") || `peerflix exited with status ${res.status}.`);
  }
  return res.stdout;
}

// Sends data to the search window. IINA hands it over as JSON pasted into a
// template literal, unescaped, so a title with a backtick, a quote or a
// backslash would lose the message; percent-encoded, it has none of them.
function post(name, data) {
  win.postMessage(name, encodeURIComponent(JSON.stringify(data)));
}

async function search({ category, query }) {
  const id = ++searches;
  try {
    const { results, summary } = JSON.parse(await run(["--json", "-c", category, "--", query]));
    if (id !== searches) return;
    post("results", { category, query, results, summary });
  } catch (e) {
    if (id === searches) post("searchFailed", { category, query, text: e.message });
  }
}

// The search window shows how pick is starting on its row.
function progress(pick, text) {
  post("progress", { pick, text });
}

function failed(pick, text) {
  if (pick !== current) return;
  current = null;
  post("failed", { pick, text });
}

// Starts peerflix on source, which answers with the torrent's files and
// episodes and the control URL to pick one from, then plays the file at
// index, or the torrent's largest video; or, when neither is given and it
// holds several episodes, lists them to pick from, keeping peerflix waiting.
// The search window shows "Fetching files…" meanwhile.
async function open({ pick, source, title, index = null }) {
  cancelPending();
  current = pick;
  try {
    const out = JSON.parse(await run(["--json", "--", source]));
    const { files, episodes, control, info_hash: infoHash } = out;
    hashes.set(source, infoHash);
    if (pick !== current) return stopPeerflix(control);
    if (index != null || episodes.length < 2) {
      progress(pick, "Starting…");
      return streamFile(pick, control, index, infoHash);
    }
    pending = { source, control, infoHash };
    current = null;
    const byIndex = Object.fromEntries(files.map((f) => [f.index, f]));
    post("episodes", { pick, source, title, episodes: episodes.map((i) => byIndex[i]) });
  } catch (e) {
    failed(pick, e.message);
  }
}

// Plays the episode at index of source, picked from the list the waiting
// peerflix sent, or with a new peerflix once that one has started one. The
// stream of the torrent playing, if any, stops first, so the new peerflix
// has its peers. The search window shows "Starting…" meanwhile.
async function playEpisode({ pick, source, title, index }) {
  current = pick;
  const infoHash = hashes.get(source);
  if (playing(infoHash)) {
    progress(pick, "Stopping the stream of this torrent…");
    await replace(infoHash);
    if (pick !== current) return;
    progress(pick, "Starting…");
  }
  if (pending?.source !== source) return open({ pick, source, title, index });
  const { control } = pending;
  pending = null;
  streamFile(pick, control, index, infoHash);
}

// Gives up the pick being started, and the episodes being picked from.
function cancel() {
  current = null;
  cancelPending();
}

// Stops the peerflix waiting for an episode to be picked, if any.
function cancelPending() {
  if (pending) stopPeerflix(pending.control);
  pending = null;
}

// Settles as promise does, on the main thread, where IINA must create
// players and touch windows: its http settles off it, and timers fire on it.
function onMain(promise) {
  return promise.finally(() => sleep(0));
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// The JSON peerflix answered with.
function body(res) {
  return res.data || JSON.parse(res.text);
}

// Stops the peerflix at control.
function stopPeerflix(control) {
  http.delete(control, {}).catch(() => {});
}

// Asks peerflix at control to stream the file at index, or its largest
// video, of the torrent with infoHash, and plays the stream in a new player,
// unless pick was given up meanwhile. A stream of the same torrent still
// playing, as when it's picked from the results again, is stopped first, as
// two peerflix downloading the same files would race each other.
async function streamFile(pick, control, index, infoHash) {
  if (playing(infoHash)) {
    progress(pick, "Stopping the stream of this torrent…");
    await replace(infoHash);
    if (pick !== current) return stopPeerflix(control);
    progress(pick, "Starting…");
  }
  let served;
  try {
    const res = await onMain(http.put(index == null ? control : `${control}?index=${index}`, {}));
    served = body(res);
  } catch (res) {
    // peerflix waits for another pick after one of a file there isn't.
    stopPeerflix(control);
    return failed(pick, res.text?.trim() || "peerflix didn't answer.");
  }
  if (pick !== current) return stopPeerflix(control);
  current = null;
  const id = global.createPlayerInstance({ url: served.url, label: "peerflix", enablePlugins: true });
  // The player's main.js has run by now, though its video may not have loaded.
  global.postMessage(id, "subtitles", served.subtitles.map((s) => s.url));
  post("started", { pick });
  streams.set(id, { name: served.name, infoHash, control, status: null });
  watch();
}

// Whether a stream of the torrent with infoHash is playing.
function playing(infoHash) {
  return [...streams.values()].some((s) => s.infoHash === infoHash);
}

// Stops the streams of the torrent with infoHash, closing their players, and
// waits for their peerflix to exit, as its control stops answering then.
async function replace(infoHash) {
  const old = [...streams].filter(([, s]) => s.infoHash === infoHash);
  for (const [id] of old) stopStream(id);
  const deadline = Date.now() + EXIT_WAIT;
  for (const [, { control }] of old) {
    while (Date.now() < deadline) {
      try {
        await onMain(http.get(control, {}));
      } catch {
        break;
      }
      await sleep(200);
    }
  }
}

// Sends each stream's status to its player, and all of them to the search
// window, anew every POLL until none is left.
async function watch() {
  if (watching) return;
  watching = true;
  while (streams.size) {
    for (const [id, stream] of streams) await refresh(id, stream);
    showStreams();
    await sleep(POLL);
  }
  showStreams();
  watching = false;
}

// Asks peerflix for stream's status, and sends it to its player, id.
async function refresh(id, stream) {
  try {
    stream.status = body(await onMain(http.get(stream.control, {})));
    global.postMessage(id, "status", stream.status);
  } catch {
    // peerflix is gone; there's nothing to show.
    stream.status = null;
  }
}

// Sends the streams whose status is known to the search window, each with how
// it's going and whether it can be paused.
function showStreams() {
  const shown = [];
  for (const [id, { name, status }] of streams) {
    if (!status) continue;
    const paused = status.state === "paused";
    shown.push({ id, name, text: status.text, paused, pausable: pausable(status) });
  }
  post("streams", shown);
}

// Pauses the download of the stream player id plays, or resumes it, from
// the player's menu or the search window, and shows it straight away.
async function togglePause(id) {
  const stream = streams.get(id);
  if (!stream?.status || !pausable(stream.status)) return;
  const action = stream.status.state === "paused" ? "resume" : "pause";
  try {
    await onMain(http.put(`${stream.control}?${action}`, {}));
  } catch {
    // Such as pausing twice from both places at once; the status tells.
  }
  await refresh(id, stream);
  showStreams();
}

// Stops the stream player id plays, from the search window, closing the
// player.
function stopStream(id) {
  const stream = streams.get(id);
  if (!stream) return;
  streams.delete(id);
  stopPeerflix(stream.control);
  global.postMessage(id, "stop", null);
  showStreams();
}

global.onMessage("pause", (_, player) => togglePause(parseInt(player, 10)));

// A player that closes is done with its stream. Should this not get through,
// peerflix still stops on its own once no player has been connected for 30
// seconds. IINA names the players it opens for plugins "<id>-<plugin
// identifier>".
global.onMessage("closed", (_, player) => {
  const id = parseInt(player, 10);
  const stream = streams.get(id);
  if (!stream) return;
  streams.delete(id);
  stopPeerflix(stream.control);
});
