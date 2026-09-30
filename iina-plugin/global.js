// The search window, and the players it opens on peerflix's streams.
// peerflix does the searching and streaming; see --json in its README.

const { global, http, menu, preferences, standaloneWindow: win, utils } = iina;
const { describe } = require("./status.js");

// How often the streams' status is shown anew, in milliseconds.
const POLL = 1000;

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
// picked, with the torrent's source and its control URL.
let pending = null;
// The id of the search window's pick being started, if any. A newer pick, or
// the window giving it up, replaces it, and a peerflix started for a pick
// that isn't current is stopped rather than played.
let current = null;
// The streams players were opened on, each a name and control URL, by
// player ID, until the player closes.
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
  win.onMessage("choose", choose);
  win.onMessage("play", play);
  win.onMessage("cancel", cancel);
  win.open();
}

function findPeerflix() {
  const path = preferences.get("peerflix_path");
  for (const p of path ? [path] : CANDIDATES) {
    const resolved = p.startsWith("~") ? utils.resolvePath(p) : p;
    if (utils.fileInPath(resolved)) return resolved;
  }
  return null;
}

// Runs peerflix with args and returns the first line it writes, leaving it
// running if it doesn't exit. Throws peerflix's error when it fails.
async function run(args) {
  const bin = findPeerflix();
  if (!bin) {
    throw new Error("peerflix wasn't found; set its path in the plugin's preferences.");
  }
  const res = await utils.exec("/bin/sh", ["-c", FIRST_LINE, "sh", bin, ...args]);
  if (res.status !== 0) {
    if (res.stderr.includes("unexpected argument '--json'")) {
      throw new Error(`${bin} is too old for this plugin; update it.`);
    }
    // The first error line says what went wrong; later ones tend to be
    // advice, as with mise's shim.
    const lines = res.stderr.trim().split("\n");
    const line = lines.find((l) => /error/i.test(l)) || lines[lines.length - 1];
    throw new Error(line.replace(/^error: /, "") || `peerflix exited with status ${res.status}.`);
  }
  return res.stdout;
}

async function search({ category, query }) {
  const id = ++searches;
  try {
    const out = JSON.parse(await run(["--json", "-c", category, "--", query]));
    if (id !== searches) return;
    const failed = out.unanswered.map((site) => `${site} didn't answer.`);
    const note = failed.concat(out.errors.map((e) => `${e}.`)).join(" ");
    win.postMessage("results", { category, query, results: out.results, note });
  } catch (e) {
    if (id === searches) win.postMessage("searchFailed", { category, query, text: e.message });
  }
}

// The search window shows how pick is starting on its row.
function progress(pick, text) {
  win.postMessage("progress", { pick, text });
}

function failed(pick, text) {
  if (pick !== current) return;
  current = null;
  win.postMessage("failed", { pick, text });
}

// Starts peerflix on source, which answers with the torrent's files and
// episodes and the control URL to pick one from, then plays the file at
// index, or the torrent's largest video; or, when neither is given and it
// holds several episodes, lists them to pick from, keeping peerflix waiting.
// The search window shows "Fetching files…" meanwhile.
async function choose({ pick, source, title, index = null }) {
  cancelPending();
  current = pick;
  try {
    const { files, episodes, control } = JSON.parse(await run(["--json", "--", source]));
    if (!control) throw new Error("peerflix is too old for this plugin; update it.");
    if (pick !== current) return stop(control);
    if (index != null || episodes.length < 2) {
      progress(pick, "Starting…");
      return start(pick, control, index);
    }
    pending = { source, control };
    current = null;
    const byIndex = Object.fromEntries(files.map((f) => [f.index, f]));
    win.postMessage("episodes", { pick, source, title, episodes: episodes.map((i) => byIndex[i]) });
  } catch (e) {
    failed(pick, e.message);
  }
}

// Plays the episode at index of source, picked from the list the waiting
// peerflix sent, or with a new peerflix once that one has started one. The
// search window shows "Starting…" meanwhile.
function play({ pick, source, title, index }) {
  if (pending?.source !== source) return choose({ pick, source, title, index });
  const { control } = pending;
  pending = null;
  current = pick;
  start(pick, control, index);
}

// Gives up the pick being started, and the episodes being picked from.
function cancel() {
  current = null;
  cancelPending();
}

// Stops the peerflix waiting for an episode to be picked, if any.
function cancelPending() {
  if (pending) stop(pending.control);
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

function stop(control) {
  http.delete(control, {}).catch(() => {});
}

// Asks peerflix at control to stream the file at index, or its largest
// video, and plays the stream in a new player, unless pick was given up
// meanwhile.
async function start(pick, control, index) {
  let stream;
  try {
    const res = await onMain(http.put(index == null ? control : `${control}?index=${index}`, {}));
    stream = body(res);
  } catch (res) {
    // peerflix waits for another pick after one of a file there isn't.
    stop(control);
    return failed(pick, res.text?.trim() || "peerflix didn't answer.");
  }
  if (pick !== current) return stop(control);
  current = null;
  const id = global.createPlayerInstance({ url: stream.url, label: "peerflix", enablePlugins: true });
  // The player's main.js has run by now, though its video may not have loaded.
  global.postMessage(id, "subtitles", stream.subtitles.map((s) => s.url));
  win.postMessage("started", { pick });
  streams.set(id, { name: stream.name, control });
  watch();
}

// Sends each stream's status to its player, and all of them to the search
// window, anew every POLL until none is left.
async function watch() {
  if (watching) return;
  watching = true;
  while (streams.size) {
    const shown = [];
    for (const [id, { name, control }] of streams) {
      try {
        const res = await onMain(http.get(control, {}));
        const status = body(res);
        global.postMessage(id, "status", status);
        shown.push({ name, text: describe(status) });
      } catch {
        // peerflix is gone; there's nothing to show.
      }
    }
    win.postMessage("streams", shown);
    await sleep(POLL);
  }
  win.postMessage("streams", []);
  watching = false;
}

// A player that closes is done with its stream. Should this not get through,
// peerflix still stops on its own once no player has been connected for 30
// seconds. IINA names the players it opens for plugins "<id>-<plugin
// identifier>".
global.onMessage("closed", (_, player) => {
  const id = parseInt(player, 10);
  const stream = streams.get(id);
  if (!stream) return;
  streams.delete(id);
  stop(stream.control);
});
