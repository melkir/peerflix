// The search window, and the players it opens on peerflix's streams.
// peerflix does the searching and streaming; see --json in its README.

const { global, http, menu, preferences, standaloneWindow: win, utils } = iina;
const { describe, sleep } = require("./status.js");

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
      throw new Error(`${bin} is too old; the plugin needs peerflix 0.5.0 or later.`);
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

// The search window shows how a picked torrent is starting on its row.
function progress(text) {
  win.postMessage("progress", { text });
}

function failed(e) {
  win.postMessage("failed", { text: e.message });
}

// Plays source, or lists its episodes to pick from when it has several. The
// search window shows "Fetching files…" meanwhile.
async function choose({ source, title }) {
  try {
    const { files, episodes } = JSON.parse(await run(["--json", "--list", "--", source]));
    if (episodes.length < 2) {
      progress("Starting…");
      return play({ source, index: null });
    }
    const byIndex = Object.fromEntries(files.map((f) => [f.index, f]));
    win.postMessage("episodes", { source, title, episodes: episodes.map((i) => byIndex[i]) });
  } catch (e) {
    failed(e);
  }
}

// Streams file index of source, or the torrent's largest video, in a new
// player. peerflix serves it until the player closes. The search window shows
// "Starting…" meanwhile.
async function play({ source, index }) {
  const args = ["--json"];
  if (index != null) args.push("-i", String(index));
  args.push("--", source);
  try {
    const stream = JSON.parse(await run(args));
    // exec resolves on the main thread, where IINA must create windows.
    const id = global.createPlayerInstance({ url: stream.url, label: "peerflix", enablePlugins: true });
    // The player's main.js has run by now, though its video may not have loaded.
    global.postMessage(id, "subtitles", stream.subtitles.map((s) => s.url));
    win.postMessage("started", null);
    // peerflix 0.5.0 has no control URL to get the status from.
    if (stream.control) {
      streams.set(id, { name: stream.name, control: stream.control });
      watch();
    }
  } catch (e) {
    failed(e);
  }
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
        const res = await http.get(control, {});
        const status = res.data || JSON.parse(res.text);
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
  http.delete(stream.control, {}).catch(() => {});
});
