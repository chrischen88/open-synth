"""Long-lived helper: keeps one Serum 2 instance loaded and serves requests over stdio.

    python -m serum_host.server

Protocol: one JSON object per line in each direction.
    request:  {"id": 7, "cmd": "render", "args": {...}}
    response: {"id": 7, "ok": true, "result": {...}}   or   {"id": 7, "ok": false, "error": "..."}
On start the server prints {"event": "started"}, then loads Serum on the main thread (pedalboard
requires plugin load and processing there) and prints {"event": "ready", "loadS": ...}. A reader
thread answers ping/status at once; every other request runs in order on the main thread.

Commands:
    ping                                    -> {"pong": true, "serumLoaded": bool}
    status                                  -> load time, request counts
    render  {preset, out, notes?, velocity?, duration?, macros?}   (same result as render.py)
    analyze {audio, out, compare?, start?, end?}                   (same result as analyze.py)
    play    {wav, device?}                  -> starts playback on the speakers, returns at once
    devices                                 -> output device names
    shutdown

Renders reuse the loaded instance after resetting it to its baseline state, which is
bit-identical to rendering in a fresh instance (verified, including after reverb/delay presets).
"""
import json
import os
import queue
import sys
import threading
import time
import traceback

from .render import InputError, log


class Server:
    def __init__(self, out_fd):
        self.out_fd = out_fd
        self.write_lock = threading.Lock()
        self.serum = None
        self.load_error = None
        self.loaded = threading.Event()
        self.load_s = None
        self.counts = {}

    def emit(self, obj):
        data = (json.dumps(obj, allow_nan=False) + "\n").encode()
        with self.write_lock:
            os.write(self.out_fd, data)

    def load(self):
        t0 = time.time()
        try:
            from .host import Serum
            self.serum = Serum()
            self.load_s = round(time.time() - t0, 2)
            self.emit({"event": "ready", "loadS": self.load_s})
        except Exception as e:  # noqa: BLE001
            self.load_error = f"{type(e).__name__}: {e}"
            self.emit({"event": "load_failed", "error": self.load_error})
        finally:
            self.loaded.set()

    def need_serum(self):
        self.loaded.wait()
        if self.serum is None:
            raise RuntimeError(f"Serum failed to load: {self.load_error}")
        return self.serum

    # -- commands -------------------------------------------------------------------
    def cmd_ping(self, _):
        return {"pong": True, "serumLoaded": self.serum is not None}

    def cmd_status(self, _):
        return {"serumLoaded": self.serum is not None, "loadS": self.load_s, "loadError": self.load_error, "requests": self.counts}

    def cmd_render(self, a):
        from .render import DEFAULT_NOTES, render_job
        serum = self.need_serum()
        return render_job(a["preset"], a["out"], a.get("notes") or DEFAULT_NOTES, a.get("velocity", 100),
                          a.get("duration"), a.get("macros") or "", serum=serum)

    def cmd_analyze(self, a):
        from .analyze import analyze_job
        return analyze_job(a["audio"], a["out"], a.get("compare"), a.get("start") or 0.0, a.get("end"))

    def cmd_devices(self, _):
        from pedalboard.io import AudioStream
        return {"outputs": list(AudioStream.output_device_names), "default": AudioStream.default_output_device_name}

    def cmd_play(self, a):
        from .analyze import load_audio
        from .render import SR
        audio = load_audio(a["wav"])
        device = a.get("device")
        if device:
            from pedalboard.io import AudioStream
            names = list(AudioStream.output_device_names)
            if device not in names:
                raise InputError(f"unknown output device {device!r}; available: {names}")

        def run():
            try:
                from pedalboard.io import AudioStream
                AudioStream.play(audio, SR, device) if device else AudioStream.play(audio, SR)
            except Exception as e:  # noqa: BLE001
                log(f"playback failed: {e}")

        threading.Thread(target=run, daemon=True).start()
        return {"playing": a["wav"], "durationS": round(audio.shape[1] / SR, 2)}

    # -- loop -----------------------------------------------------------------------
    def handle(self, line):
        try:
            req = json.loads(line)
        except json.JSONDecodeError as e:
            self.emit({"id": None, "ok": False, "error": f"bad JSON: {e}"})
            return True
        rid, cmd, args = req.get("id"), req.get("cmd"), req.get("args") or {}
        if cmd == "shutdown":
            self.emit({"id": rid, "ok": True, "result": {"bye": True}})
            return False
        fn = getattr(self, f"cmd_{cmd}", None)
        if fn is None:
            self.emit({"id": rid, "ok": False, "error": f"unknown command {cmd!r}"})
            return True
        self.counts[cmd] = self.counts.get(cmd, 0) + 1
        try:
            self.emit({"id": rid, "ok": True, "result": fn(args)})
        except (InputError, KeyError) as e:
            self.emit({"id": rid, "ok": False, "error": f"bad input: {e}"})
        except Exception as e:  # noqa: BLE001
            log(traceback.format_exc())
            self.emit({"id": rid, "ok": False, "error": f"{type(e).__name__}: {e}"})
        return True


QUICK = {"ping", "status"}


def main():
    real_out = os.dup(1)
    os.dup2(2, 1)  # native/plugin prints to stdout must not corrupt the protocol
    server = Server(real_out)
    server.emit({"event": "started"})
    jobs = queue.Queue()

    def reader():
        for line in sys.stdin:
            if not line.strip():
                continue
            try:
                cmd = json.loads(line).get("cmd")
            except (json.JSONDecodeError, AttributeError):
                cmd = None
            if cmd in QUICK:
                server.handle(line)
            else:
                jobs.put(line)
        jobs.put(None)  # stdin closed

    threading.Thread(target=reader, daemon=True).start()
    server.load()
    while True:
        line = jobs.get()
        if line is None or not server.handle(line):
            break
    sys.stderr.flush()
    os._exit(0)  # skip slow plugin teardown


if __name__ == "__main__":
    main()
