// Runs in each player. In the ones the search window opened on a stream, it
// loads the stream's subtitles once the video loads.

const { event, global, mpv } = iina;

if (global.getLabel() === "peerflix") {
  let loaded = false;
  event.on("iina.file-loaded", () => {
    if (loaded) return;
    loaded = true;
    global.postMessage("loaded", null);
  });

  global.onMessage("subtitles", (urls) => {
    // A lone subtitle is shown; among several, which to show is the viewer's
    // pick from the Subtitles menu.
    const flag = urls.length === 1 ? "select" : "auto";
    for (const url of urls) mpv.command("sub-add", [url, flag]);
  });
}
