// huddle background blur shim.
//
// Loaded on demand (dynamic <script>) by crates/web/src/effects.rs when the
// user first toggles blur. Wraps the vendored legacy MediaPipe Selfie
// Segmentation solution and drives a per-frame compositing loop whose output
// canvas is captured as the replacement camera track.
//
// Deliberately plain ES2018: no bundler, no imports; same-origin assets only.
//
// e2e-only knobs (only honored when ?test=1 is also present):
//   blur-fail=1  init() always rejects (model-load failure path)
//   blur-slow=1  injects a ~60ms delay per frame (slow-pipeline auto-disable)
/* global SelfieSegmentation */
(function () {
  "use strict";

  var BASE = "/assets/effects";
  var DEGRADE_EMA_MS = 25; // sustained per-frame cost that counts as "too slow"
  var DEGRADE_STREAK = 30; // consecutive slow frames before auto-disable
  var EMA_ALPHA = 0.1;

  function isSlowTick() {
    var p = new URLSearchParams(location.search);
    return p.get("test") === "1" && p.has("blur-slow");
  }
  function isFailTick() {
    var p = new URLSearchParams(location.search);
    return p.get("test") === "1" && p.has("blur-fail");
  }

  function loadScript(src) {
    return new Promise(function (resolve, reject) {
      var s = document.createElement("script");
      s.src = src;
      s.onload = function () {
        resolve();
      };
      s.onerror = function () {
        reject(new Error("failed to load " + src));
      };
      document.head.appendChild(s);
    });
  }

  var seg = null;
  var initPromise = null;

  function init() {
    if (initPromise) {
      return initPromise;
    }
    initPromise = loadScript(BASE + "/selfie_segmentation.js").then(function () {
      if (isFailTick()) {
        throw new Error("blur-fail: simulated model load failure");
      }
      seg = new SelfieSegmentation({
        locateFile: function (f) {
          return BASE + "/" + f;
        },
      });
      seg.setOptions({ modelSelection: 1 }); // landscape 144x256 model
      seg.onResults(onSegResults);
      return seg.initialize();
    });
    return initPromise;
  }

  // ---- per-frame state, shared between the rVFC pump and onResults ----
  var run = null;

  function onSegResults(results) {
    if (!run) {
      return;
    }
    var ctx = run.ctx;
    var w = run.canvas.width;
    var h = run.canvas.height;

    // Person layer: mask (white = person) masked with the camera frame.
    var pctx = run.person.getContext("2d");
    pctx.clearRect(0, 0, w, h);
    pctx.drawImage(results.segmentationMask, 0, 0, w, h);
    pctx.globalCompositeOperation = "source-in";
    pctx.drawImage(results.image, 0, 0, w, h);
    pctx.globalCompositeOperation = "source-over";

    // Background: blurred copy of the frame.
    if (run.ctxFilter) {
      ctx.clearRect(0, 0, w, h);
      ctx.filter = "blur(12px)";
      ctx.drawImage(results.image, 0, 0, w, h);
      ctx.filter = "none";
    } else {
      // No ctx.filter support: approximate with downscale + upscale.
      var d = run.blurBuf;
      var dctx = run.blurBufCtx;
      d.width = Math.max(1, Math.round(w / 16));
      d.height = Math.max(1, Math.round(h / 16));
      dctx.clearRect(0, 0, d.width, d.height);
      dctx.drawImage(results.image, 0, 0, d.width, d.height);
      ctx.imageSmoothingEnabled = true;
      ctx.clearRect(0, 0, w, h);
      ctx.drawImage(d, 0, 0, w, h);
      ctx.drawImage(d, 0, 0, w, h); // second pass deepens the blur
    }
    // Person drawn sharp on top.
    ctx.drawImage(run.person, 0, 0);
  }

  /**
   * start(video, rawTrack, onDegrade) ->
   *   Promise<{ track: MediaStreamTrack, stop(): void }>
   *
   * `video` must already be playing the raw camera stream. The returned
   * track is the captureStream() of the composited canvas; `stop()` tears
   * the whole loop down and stops the returned track.
   */
  function start(video, rawTrack, onDegrade) {
    return init().then(function () {
      var canvas = document.createElement("canvas");
      canvas.width = video.videoWidth || 640;
      canvas.height = video.videoHeight || 480;
      var ctx = canvas.getContext("2d");
      // Feature-detect ctx.filter once (Safari < 18 lacks it).
      var ctxFilter = true;
      try {
        ctx.filter = "blur(1px)";
        ctxFilter = ctx.filter === "blur(1px)";
        ctx.filter = "none";
      } catch (e) {
        ctxFilter = false;
      }

      run = {
        video: video,
        rawTrack: rawTrack,
        canvas: canvas,
        ctx: ctx,
        ctxFilter: ctxFilter,
        person: document.createElement("canvas"),
        blurBuf: document.createElement("canvas"),
        framesIn: 0,
        framesOut: 0,
        ema: 0,
        slowStreak: 0,
        degraded: false,
        stopped: false,
        pending: null,
        rvfcId: 0,
      };
      run.person.width = canvas.width;
      run.person.height = canvas.height;
      canvas.style.display = "none";
      document.body.appendChild(canvas);

      var stats = window.__huddleEffectsStats;
      var slowTick = isSlowTick();

      function step() {
        if (!run || run.stopped) {
          return;
        }
        if (run.pending) {
          // Previous frame still compositing: drop this one rather than
          // queueing an unbounded backlog.
          schedule();
          return;
        }
        var t0 = performance.now();
        run.framesIn++;
        var p;
        if (run.rawTrack.enabled) {
          p = seg.send({ image: run.video }).then(function () {
            if (slowTick) {
              return new Promise(function (r) {
                setTimeout(r, 60);
              });
            }
          });
        } else {
          // Camera "off": emit black frames, skip segmentation.
          run.ctx.clearRect(0, 0, run.canvas.width, run.canvas.height);
          p = Promise.resolve();
        }
        run.pending = p
          .then(function () {
            var ms = performance.now() - t0;
            run.pending = null;
            if (!run || run.stopped) {
              return;
            }
            run.framesOut++;
            run.ema = run.ema === 0 ? ms : run.ema + EMA_ALPHA * (ms - run.ema);
            if (stats) {
              stats.framesIn = run.framesIn;
              stats.framesOut = run.framesOut;
              stats.avgMs = Math.round(run.ema * 10) / 10;
            }
            if (run.ema > DEGRADE_EMA_MS) {
              run.slowStreak++;
              if (run.slowStreak >= DEGRADE_STREAK && !run.degraded && onDegrade) {
                run.degraded = true;
                if (stats) {
                  stats.degraded = true;
                }
                try {
                  onDegrade();
                } catch (e) {
                  /* the Rust side owns recovery */
                }
              }
            } else {
              run.slowStreak = 0;
            }
          })
          .catch(function () {
            run.pending = null;
          });
        schedule();
      }

      function schedule() {
        if (!run || run.stopped) {
          return;
        }
        if (typeof run.video.requestVideoFrameCallback === "function") {
          run.video.requestVideoFrameCallback(step);
        } else {
          setTimeout(step, 33); // ~30fps fallback pump
        }
      }

      // Prime the canvas with opaque black so captureStream() never
      // surfaces transparent frames before the first result arrives.
      run.ctx.fillStyle = "#000";
      run.ctx.fillRect(0, 0, canvas.width, canvas.height);

      var track = canvas.captureStream(30).getVideoTracks()[0];
      schedule();
      return {
        track: track,
        stop: function () {
          if (!run || run.stopped) {
            return;
          }
          run.stopped = true;
          try {
            track.stop();
          } catch (e) {
            /* already stopped */
          }
          if (canvas.parentNode) {
            canvas.parentNode.removeChild(canvas);
          }
          run = null;
        },
      };
    });
  }

  window.__huddleBlur = {
    init: init,
    start: start,
  };
})();
