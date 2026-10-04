// Runs in each player. In the ones the search window opened on a stream, it
// loads the stream's subtitles once the video loads, shows the download's
// status the search window sends until it's done, lets the download be paused
// from the Plugin menu, and tells the search window when the player closes,
// to end the stream; it closes when the search window stops the stream.

const { core, event, global, menu, mpv, overlay } = iina;

// How long the status stays once the download is done, in milliseconds.
const DONE_FOR = 5000;

if (global.getLabel() === "peerflix") {
  let subtitles = [];
  let fileLoaded = false;
  let windowLoaded = false;
  let overlayReady = false;
  // The latest status, and whether to show it.
  let status = null;
  let shown = true;
  // Set once a finished download's status has been shown for DONE_FOR.
  let expired = false;
  let expiring = null;

  // The search window sends the subtitles as soon as it has opened this
  // player, which can be before the video loads.
  global.onMessage("subtitles", (urls) => {
    subtitles = urls;
    if (fileLoaded) addSubtitles();
  });

  event.on("iina.file-loaded", () => {
    fileLoaded = true;
    addSubtitles();
  });

  global.onMessage("status", (s) => {
    status = s;
    updatePauseItem();
    render();
  });

  global.onMessage("stop", () => core.stop());

  event.on("iina.window-loaded", () => {
    windowLoaded = true;
    render();
  });

  event.on("iina.window-will-close", () => global.postMessage("closed", null));

  const item = menu.item("Show Download Status", toggle, { selected: shown });
  menu.addItem(item);
  const pauseItem = menu.item("Pause Download", () => global.postMessage("pause", null), {
    enabled: false,
  });
  menu.addItem(pauseItem);

  // Titles the pause item for the download's state, refreshing the menu only
  // when that changes.
  function updatePauseItem() {
    const title = status.state === "paused" ? "Resume Download" : "Pause Download";
    const enabled = status.pausable;
    if (title === pauseItem.title && enabled === pauseItem.enabled) return;
    pauseItem.title = title;
    pauseItem.enabled = enabled;
    menu.forceUpdate();
  }

  function toggle() {
    shown = !shown;
    item.selected = shown;
    menu.forceUpdate();
    // Showing it again shows a finished download's status for DONE_FOR too.
    expired = false;
    render();
  }

  function addSubtitles() {
    // A lone subtitle is shown; among several, which to show is the viewer's
    // pick from the Subtitles menu.
    const flag = subtitles.length === 1 ? "select" : "auto";
    for (const url of subtitles.splice(0)) mpv.command("sub-add", [url, flag]);
  }

  function render() {
    // The overlay can only be set up once the window has loaded.
    if (!status || !windowLoaded) return;
    if (!overlayReady) {
      overlay.simpleMode();
      overlay.setStyle(STYLE);
      overlayReady = true;
    }
    if (!shown || expired) {
      overlay.hide();
      return;
    }
    overlay.setContent(`<div class="status">${status.text}</div>`);
    overlay.show();
    if (status.state === "done" && !expiring) {
      expiring = setTimeout(() => {
        expired = true;
        expiring = null;
        overlay.hide();
      }, DONE_FOR);
    }
  }
}

const STYLE = `
  .status {
    position: absolute;
    top: 12px;
    right: 12px;
    padding: 4px 10px;
    border-radius: 6px;
    background: rgba(0, 0, 0, 0.55);
    color: #fff;
    font: 12px -apple-system, BlinkMacSystemFont, sans-serif;
    font-variant-numeric: tabular-nums;
    /* The status keeps peerflix's columns. */
    white-space: pre;
  }
`;
