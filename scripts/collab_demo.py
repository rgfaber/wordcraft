#!/usr/bin/env python3
"""The WordCraft + Macula automated collaboration demo, one command.

Runs the whole story without hands, over the real mesh: three WordCraft
instances on three different public stations, each starting from the
device-tier realm sign-in; the host mints an invite and the other two join
with it; the host approves them by hand (via the control channel), all three
type concurrently, each author's undo undoes only their own edit, presence
shows the others, one instance drops and rejoins from a snapshot catch-up —
and the run asserts the canonical dumps stayed equal, then leaves evidence:
a transcript, screenshots, and the station/link facts.

Usage:
  python3 scripts/collab_demo.py --bin target/release/wordcraft \
      --realm-key-file /path/to/io_macula_realm_key.hex \
      [--mode headless|visual] [--stations S1,S2,S3] [--out DIR] [--doc NAME] \
      [--record OUT.mp4]

  --record OUT.mp4 renders a short 16:9 clip of the story from the real
      windows' own frames (each instance's UI is captured through the
      control channel, three windows side by side — no desktop capture, no
      clutter). It shows the group typing converge live, the presence
      cursors, and ends on the per-author undo beat, then one window drops
      and rejoins. ffmpeg (libx264) is required.

  --mode headless  (default) run through, assert, collect, stop the instances.
  --mode visual    the same run, paced for a person to watch; the windows are
                   left open at the end (Ctrl+C to stop them).
  --stations       host:port:station_node_hex per instance (default: the three
                   fleet stations the acceptance runs used).
  --realm-key-file public trust anchor of the realm (hex; not a secret) —
                   or --realm-key-hex. Calls into the realm need it pinned.

Everything lands in --out (default ./collab-demo): transcript.jsonl,
facts.json, shots/*.png, logs/*.log, configs/*.json.
"""

import argparse
import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time

# The stations a demo dials, `host:port:node_hex`, comma-separated (MACULA_STATIONS);
# deployment details, so they are never committed.
DEFAULT_STATIONS = [s.strip() for s in os.environ.get("MACULA_STATIONS", "").split(",") if s.strip()]
REALM_NAME = "io.macula"
REALM_ID = hashlib.sha256(REALM_NAME.encode()).hexdigest()
PROFILE = "pq_hybrid"


def short(hexid):
    return hexid[:8]


class Demo:
    def __init__(self, args):
        self.args = args
        self.out = os.path.abspath(args.out)
        self.run_id = time.strftime("%d%H%M%S")
        self.doc = args.doc or f"demo{self.run_id}"
        self.instances = []
        self.failures = []
        self.started_at = time.time()
        self.recorder = None
        self.recording_seconds = 0.0

    # ---- evidence ---------------------------------------------------------------------------------

    def log(self, line):
        print(line, flush=True)
        with open(os.path.join(self.out, "transcript.jsonl"), "a") as f:
            f.write(json.dumps({"at": round(time.time() - self.started_at, 2), "line": line}) + "\n")

    def path(self, *parts):
        p = os.path.join(self.out, *parts)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        return p

    # ---- the instances ----------------------------------------------------------------------------

    def launch(self, name, port, station, config, env_extra=None):
        inst = Instance(self, name, port, station, config)
        inst.start(env_extra or {})
        self.instances.append(inst)
        return inst

    def stop_all(self):
        for inst in self.instances:
            inst.stop()

    def facts(self, extra):
        facts = {
            "doc": self.doc,
            "realm": REALM_NAME,
            "realmIdShort": short(REALM_ID),
            "stations": [short(s.rsplit(":", 1)[1]) for s in self.args.stations],
            "stationsHosts": [s.split(":")[0] for s in self.args.stations],
            "profile": PROFILE,
            "seconds": round(time.time() - self.started_at, 1),
            "startedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(self.started_at)),
        }
        facts.update(extra)
        with open(os.path.join(self.out, "facts.json"), "w") as f:
            json.dump(facts, f, indent=2)
        self.log(f"facts: doc={facts['doc']} realm={facts['realm']} stations={[short(s.rsplit(':', 1)[1]) for s in self.args.stations]}")

    def check(self, condition, what):
        mark = "ok" if condition else "FAIL"
        self.log(f"  [{mark}] {what}")
        if not condition:
            self.failures.append(what)
        return condition

    # ---- the story --------------------------------------------------------------------------------

    def run(self):
        os.makedirs(os.path.join(self.out, "shots"), exist_ok=True)
        os.makedirs(os.path.join(self.out, "logs"), exist_ok=True)
        os.makedirs(os.path.join(self.out, "configs"), exist_ok=True)
        self.log(f"WordCraft + Macula demo: {self.doc}, mode={self.args.mode}, stations={[short(s.rsplit(':', 1)[1]) for s in self.args.stations]}")

        # 1. The host: device-tier realm sign-in at connect, then shares the doc.
        host = self.launch("host", self.args.ports[0], self.args.stations[0], self.host_config(), {"MACULA_CRAFT_AUTOJOIN_REALM": REALM_NAME})
        host.wait_ready(90)
        self.log("host: up")
        host.wait_until(lambda i: i.stats()["active"] and i.stats()["joined"], 60, "host in the document")
        realm = host.wait_until(
            lambda i: ((r := i.request({"method": "ui.realmRefresh", "params": {}})["result"]["realm"]).get("signedIn") and r) or None,
            45,
            "the device-tier sign-in at connect landed",
        )
        self.check(realm.get("tier") == "device", f"host starts from the realm row: signed in to {realm.get('realm')} ({realm.get('tier')})")
        host.run("text.insert", {"text": "Host start. "})

        invited = host.request({"method": "engine.execute", "params": {"command": "collab.invite", "params": {
            "realm": REALM_ID, "realmKey": self.args.realm_key, "seed": self.args.stations[0], "profile": PROFILE, "role": "editor", "ttlMs": 86400000}}})
        invite = invited["result"]["invite"]
        self.log(f"host: minted an invite (token for {json.loads(invite)['doc']}, valid 24h)")

        # 2. Two joiners apply the invite; the host approves them by hand.
        j1 = self.launch("bob", self.args.ports[1], self.args.stations[1], self.joiner_config("bob", "Bob", invite), {"MACULA_CRAFT_AUTOJOIN_REALM": REALM_NAME})
        j2 = self.launch("carol", self.args.ports[2], self.args.stations[2], self.joiner_config("carol", "Carol", invite), {"MACULA_CRAFT_AUTOJOIN_REALM": REALM_NAME})
        j1.wait_ready(90)
        j2.wait_ready(90)
        self.log("joiners: up, asking for admission")

        names = self.approve_pending(host, expect=2)
        self.check(names == ["Bob", "Carol"], f"host sees the joiners waiting: {names}")
        for joiner, name in ((j1, "Bob"), (j2, "Carol")):
            joiner.wait_until(lambda i: i.stats()["joined"] and "Host start." in i.text(), 60, f"{name} joined and caught up")

        # 3-6. The story, either as the plain evidence run or as the camera
        # beats of the recorded clip (same checks in both).
        if self.args.record:
            self.camera_story(host, j1, j2, invite)
            for inst in (host, j1, j2):
                inst.run("ui.share", {})
            self.settle(host, j1, j2)
            for inst in (host, j1, j2):
                inst.screenshot(os.path.join(self.out, "shots", f"{inst.name}.png"))
            realm = host.request({"method": "ui.realmRefresh", "params": {}})["result"]["realm"]
            self.facts({
                "realmTier": realm.get("tier"),
                "realmHandle": realm.get("handle"),
                "finalText": host.text(),
                "converged": not self.failures,
                "video": self.args.record,
                "videoSeconds": round(self.recording_seconds, 1),
            })
            return

        # 3. Concurrent typing in the same paragraph.
        for inst in (host, j1, j2):
            inst.run("caret.set", {"pos": {"story": "body", "path": [0], "off": 0}})
        self.concurrent_type([(host, "Alpha. "), (j1, "Bravo. "), (j2, "Charlie. ")])
        host.wait_until(lambda i: all(label in host.text() for label in ("Alpha.", "Bravo.", "Charlie.")), 60, "all three edits arrived")
        self.settle(host, j1, j2)
        self.check(self.dumps_equal(host, j1, j2), "concurrent typing converged (canonical dumps equal)")

        # 4. Per-author undo: each author's undo takes back only their own edit.
        for inst, label in ((host, "Alpha."), (j1, "Bravo."), (j2, "Charlie.")):
            inst.run("edit.undo", {})
        self.settle(host, j1, j2)
        gone = all(all(label not in i.text() for label in ("Alpha.", "Bravo.", "Charlie.")) for i in (host, j1, j2))
        kept = all("Host start." in i.text() for i in (host, j1, j2))
        self.check(gone, "each author's undo removed exactly their own edit, on every replica")
        self.check(kept, "the untouched text stayed everywhere")
        self.check(self.dumps_equal(host, j1, j2), "undo left the replicas equal")

        # 5. Presence: each sees the other two.
        for inst, others in ((host, ("Bob", "Carol")), (j1, ("Host", "Carol")), (j2, ("Host", "Bob"))):
            seen = inst.peers()
            self.check(all(o in seen for o in others), f"{inst.name} sees {others} (sees {sorted(seen)})")

        # 6. One drops mid-session; the others keep typing; it rejoins from a snapshot.
        j2.stop()
        self.log("carol: dropped")
        host.run("caret.set", {"pos": {"story": "body", "path": [0], "off": 0}})
        j1.run("caret.set", {"pos": {"story": "body", "path": [0], "off": 0}})
        self.concurrent_type([(host, "Downtime-H. "), (j1, "Downtime-B. ")])
        host.wait_until(lambda i: "Downtime-B." in i.text(), 60, "the during-downtime edit reached the host")
        self.settle(host, j1)
        j2 = self.launch("carol", self.args.ports[2], self.args.stations[2], self.joiner_config("carol", "Carol", invite), {"MACULA_CRAFT_AUTOJOIN_REALM": REALM_NAME})
        j2.wait_ready(90)
        self.approve_pending(host)
        j2.wait_until(lambda i: i.stats()["joined"] and "Downtime-B." in i.text() and "Downtime-H." in i.text(), 90, "carol rejoined and caught up from the snapshot")
        self.settle(host, j1, j2)
        self.check(self.dumps_equal(host, j1, j2), "all three equal again after the catch-up")

        # 7. Evidence: screenshots + facts (the transcript is written as we go).
        for inst in (host, j1, j2):
            inst.run("ui.share", {})
        self.settle(host, j1, j2)
        for inst in (host, j1, j2):
            inst.screenshot(os.path.join(self.out, "shots", f"{inst.name}.png"))
        realm = host.request({"method": "ui.realmRefresh", "params": {}})["result"]["realm"]
        self.facts({
            "realmTier": realm.get("tier"),
            "realmHandle": realm.get("handle"),
            "ucanExpiresMs": realm.get("ucanExpiresMs"),
            "finalText": host.text(),
            "converged": not self.failures,
        })

    def camera_story(self, host, j1, j2, invite):
        """The recorded beats: group typing converging live, the per-author
        undo sell, then a drop and snapshot catch-up. The recorder captures
        the three real windows throughout; pacing keeps each beat on camera."""
        frames_dir = os.path.join(self.out, "frames")
        for inst in (host, j1, j2):
            inst.run("view.zoom", {"value": 160})
        self.recorder = Recorder(self, [host, j1, j2], frames_dir)
        self.recorder.start()
        started = time.time()
        self.log("recording: the camera beats")

        # Group typing, chunk by chunk at both the head and the tail of the
        # paragraph, so the presence cursors move and every window updates.
        chunks = [
            (host, "Three people, "),
            (j1, "one paragraph, "),
            (j2, "no server. "),
            (host, "Typed "),
            (j1, "over "),
            (j2, "the mesh "),
            (host, "at the same "),
            (j1, "time, "),
            (j2, "converging live. "),
        ]
        for inst, text in chunks:
            # Each author types at their own end of the paragraph, so the
            # presence cursors and labels travel with the text.
            end = len(inst.text())
            inst.run("caret.set", {"pos": {"story": "body", "path": [0], "off": end}})
            inst.run("text.insert", {"text": text})
            self.pause(0.9)
        self.pause(2.2)
        self.check(self.dumps_equal(host, j1, j2), "recorded run: the group typing converged")

        # The sell: the host's own line appears everywhere, then only that
        # line is taken back by the host's undo — every window updates.
        host.run("caret.set", {"pos": {"story": "body", "path": [0], "off": host.text().index("Typed ") if "Typed " in host.text() else 0}})
        host.run("text.insert", {"text": "THIS LINE IS MINE. "})
        self.pause(2.6)
        self.check(all("THIS LINE IS MINE." in i.text() for i in (host, j1, j2)), "recorded run: the host's line is on every window")
        host.run("edit.undo", {})
        self.pause(3.0)
        leftover = all("THIS LINE IS MINE." not in i.text() for i in (host, j1, j2))
        kept = all("converging live" in i.text() for i in (host, j1, j2))
        self.check(leftover and kept, "recorded run: the host's undo took back only the host's line, everywhere")
        self.check(self.dumps_equal(host, j1, j2), "recorded run: still equal after the undo")

        # A window drops, the others keep typing, it rejoins and catches up.
        j2.stop()
        self.pause(1.6)
        host.run("text.insert", {"text": "Carol dropped. "})
        self.pause(1.4)
        j1.run("text.insert", {"text": "Still here. "})
        self.pause(2.4)
        j2 = self.launch("carol", self.args.ports[2], self.args.stations[2], self.joiner_config("carol", "Carol", invite), {"MACULA_CRAFT_AUTOJOIN_REALM": REALM_NAME})
        j2.wait_ready(90)
        self.recorder.watch(j2)
        self.approve_pending(host)
        j2.wait_until(lambda i: i.stats()["joined"] and "Still here." in i.text(), 90, "the rejoined window caught up on camera")
        self.pause(2.6)
        self.check(self.dumps_equal(host, j1, j2), "recorded run: equal again after the catch-up")

        self.recorder.stop()
        self.recording_seconds = time.time() - started
        self.log(f"recording: {self.recorder.frame_count} frames over {self.recording_seconds:.1f}s")
        video = self.recorder.export(self.args.record, self.recording_seconds)
        self.log(f"video: {video}")

    def pause(self, seconds):
        time.sleep(seconds)

    def approve_pending(self, host, expect=1):
        """The host's manual yes for everyone waiting (returns their names).
        Waits until `expect` requests are in: cross-station arrivals stagger."""
        def pending(i):
            listing = i.request({"method": "engine.execute", "params": {"command": "collab.pending"}})["result"]["pending"]
            return listing if len(listing) >= expect else None

        items = host.wait_until(pending, 90, f"{expect} join request(s) pending")
        for item in items:
            host.request({"method": "engine.execute", "params": {"command": "collab.approve", "params": {"node": item["node"]}}})
        names = sorted(item["name"] for item in items)
        self.log(f"host: approved {names}")
        return names

    # ---- configs ----------------------------------------------------------------------------------

    def host_config(self):
        cfg = {
            "station": self.args.stations[0],
            "realm": REALM_ID,
            "realm_key": self.args.realm_key,
            "realm_name": REALM_NAME,
            "doc": self.doc,
            "name": "Host",
            "identity": f"demo{self.run_id}-host",
            "join": False,
            "auto_approve": False,
            "profile": PROFILE,
        }
        return self.write_config("host", cfg)

    def joiner_config(self, slug, name, invite):
        cfg = {"name": name, "identity": f"demo{self.run_id}-{slug}", "invite": invite}
        return self.write_config(slug, cfg)

    def write_config(self, slug, cfg):
        path = self.path("configs", f"{slug}.json")
        with open(path, "w") as f:
            json.dump(cfg, f, indent=1)
        return path

    # ---- assertions and timing --------------------------------------------------------------------

    def settle(self, *instances, seconds=6.0):
        """Give the mesh a beat, then a few one-sided exchanges."""
        for _ in range(int(seconds)):
            time.sleep(1)
        for _ in range(3):
            for inst in instances:
                inst.stats()

    def dumps_equal(self, *instances):
        dumps = [json.dumps(i.dump(), sort_keys=True) for i in instances]
        if len(set(dumps)) != 1:
            for inst, dump in zip(instances, dumps):
                self.log(f"  {inst.name}: {dump[:220]}")
        return len(set(dumps)) == 1

    def concurrent_type(self, pairs):
        import threading
        threads = []
        for inst, text in pairs:
            t = threading.Thread(target=inst.run, args=("text.insert", {"text": text}))
            t.start()
            threads.append(t)
        for t in threads:
            t.join()
        self.log(f"typed concurrently: {[t for _, t in pairs]}")

    def finish(self):
        ok = not self.failures
        self.log(f"{'PASS' if ok else 'FAIL'} — {len(self.failures)} failed check(s) in {round(time.time() - self.started_at, 1)}s")
        if self.args.mode == "visual":
            self.log("visual mode: windows left open (Ctrl+C when done); control ports: " + ", ".join(str(i.port) for i in self.instances))
        else:
            self.stop_all()
        return 0 if ok else 1


def png_size(path):
    """A PNG's (width, height) from its IHDR, no image library needed."""
    with open(path, "rb") as f:
        head = f.read(24)
    if len(head) < 24 or head[:8] != b"\x89PNG\r\n\x1a\n":
        raise RuntimeError(f"{path}: not a PNG")
    return int.from_bytes(head[16:20], "big"), int.from_bytes(head[20:24], "big")


class Recorder:
    """Captures each instance's own window frame through the control channel
    (the app's render, not a desktop grab) and composes a 16:9 triptych with
    labels, then encodes an mp4. One capture thread per window keeps the
    frame rate honest; a dead instance freezes on its last frame, which is
    exactly what a dropped window should look like."""

    def __init__(self, demo, instances, frames_dir):
        self.demo = demo
        self.instances = list(instances)
        self.dir = frames_dir
        os.makedirs(os.path.join(frames_dir, "raw"), exist_ok=True)
        self.frames = []
        self.latest = {}
        self.frame_count = 0
        self._stop = threading.Event()
        self._threads = {}
        self._serials = {}

    def watch(self, inst):
        """Follow a relaunched instance (the rejoin replaces its process)."""
        self.instances = [inst if i.name == inst.name else i for i in self.instances]
        if not any(i.name == inst.name for i in self.instances):
            self.instances.append(inst)

    def start(self):
        self._supervisor = threading.Thread(target=self._supervise, daemon=True)
        self._supervisor.start()

    def _supervise(self):
        n = 0
        while not self._stop.is_set():
            # A capture thread per window, started lazily and re-pointed
            # when a rejoin replaces the instance. Each thread gets its own
            # file serial so a relaunched window's captures can never
            # overwrite (or be overwritten by) the dead process's files.
            for inst in self.instances:
                current = self._threads.get(inst.name)
                if current is None or current[0] is not inst:
                    # Serials live here, keyed by name: a rejoin is a NEW
                    # Instance object, and its captures must never share
                    # filenames with the process it replaced.
                    serial = self._serials.get(inst.name, 0) + 1
                    self._serials[inst.name] = serial
                    t = threading.Thread(target=self._capture, args=(inst, serial), daemon=True)
                    self._threads[inst.name] = (inst, t)
                    t.start()
            if all(self.latest.get(i.name) for i in self.instances):
                self.frames.append({i.name: self.latest[i.name] for i in self.instances})
                n += 1
            time.sleep(0.25)

    def _capture(self, inst, serial):
        n = 0
        while not self._stop.is_set():
            path = os.path.join(self.dir, "raw", f"{inst.name}-{serial:02d}-{n:05d}.png")
            try:
                inst.screenshot(path, timeout=10)
                self.latest[inst.name] = path
            except Exception:
                time.sleep(0.2)  # a dead window must not spin hot
            n += 1

    def stop(self):
        self._stop.set()
        if getattr(self, "_supervisor", None):
            self._supervisor.join(timeout=10)
        self.frame_count = len(self.frames)

    def export(self, out_path, seconds):
        if not self.frames:
            raise RuntimeError("the recorder captured no frames")
        out_path = os.path.abspath(out_path)
        os.makedirs(os.path.dirname(out_path), exist_ok=True)
        # Three portrait columns on a dark 16:9 canvas; each window cropped
        # to its document area so the text is the star. Every column is
        # fitted into the same box (and padded), robust to any capture size.
        crop_y = 180
        box_w, box_h = 620, 744
        y = 320
        font = "/usr/share/fonts/TTF/FiraCodeNerdFont-Bold.ttf" if os.path.exists("/usr/share/fonts/TTF/FiraCodeNerdFont-Bold.ttf") else "Sans"
        cols = {"host": 10, "bob": 650, "carol": 1290}
        parts = []
        for idx, name in enumerate(("host", "bob", "carol")):
            width, height = png_size(self.frames[0][name])
            crop_h = max(120, min(height - crop_y - 16, 900))
            parts.append(
                f"[{idx}:v]crop={width}:{crop_h}:0:{crop_y},"
                f"scale={box_w}:{box_h}:force_original_aspect_ratio=decrease,"
                f"pad={box_w}:{box_h}:(ow-iw)/2:(oh-ih)/2:color=0x0a0e14[v{idx}]"
            )
        overlays = "[bg][v0]overlay=10:%d[o0];[o0][v1]overlay=650:%d[o1];[o1][v2]overlay=1290:%d[o2]" % (y, y, y)
        # The labels belong on the dark canvas, after the windows are placed.
        labels = (
            f";[o2]drawtext=fontfile={font}:text='Host':fontcolor=white:fontsize=44:x=30:y={y - 60}[l0]"
            f";[l0]drawtext=fontfile={font}:text='Bob':fontcolor=white:fontsize=44:x=670:y={y - 60}[l1]"
            f";[l1]drawtext=fontfile={font}:text='Carol':fontcolor=white:fontsize=44:x=1310:y={y - 60}[out]"
        )
        filter_complex = ";".join(parts) + ";color=c=0x141a22:s=1920x1080[bg];" + overlays + labels
        compose_dir = os.path.join(self.dir, "composed")
        os.makedirs(compose_dir, exist_ok=True)
        self.demo.log(f"recording: composing {len(self.frames)} frames")
        for i, frame in enumerate(self.frames):
            out = os.path.join(compose_dir, f"f{i:05d}.png")
            cmd = [
                "ffmpeg", "-y", "-loglevel", "error",
                "-i", frame["host"], "-i", frame["bob"], "-i", frame["carol"],
                "-filter_complex", filter_complex, "-map", "[out]", "-frames:v", "1", out,
            ]
            subprocess.run(cmd, check=True, capture_output=True)
        fps = max(1.0, len(self.frames) / max(1.0, seconds))
        # Hold the final state for a beat (the clip ends on the converged
        # document, not mid-keystroke), then encode at the capture rate so
        # playback is real time.
        hold = max(2, int(fps * 2.4))
        last = os.path.join(compose_dir, f"f{len(self.frames) - 1:05d}.png")
        for extra in range(hold):
            shutil.copyfile(last, os.path.join(compose_dir, f"f{len(self.frames) + extra:05d}.png"))
        subprocess.run(
            ["ffmpeg", "-y", "-loglevel", "error", "-framerate", f"{fps:.3f}",
             "-i", os.path.join(compose_dir, "f%05d.png"),
             "-c:v", "libx264", "-crf", "20", "-pix_fmt", "yuv420p", "-movflags", "+faststart", out_path],
            check=True, capture_output=True,
        )
        return out_path


class Instance:
    def __init__(self, demo, name, port, station, config):
        self.demo = demo
        self.name = name
        self.port = port
        self.station = station
        self.config = config
        self.proc = None
        self.next_id = 0
        # One size everywhere, laid out as a three-up row when the
        # compositor lets us place windows (Hyprland today): portrait
        # windows that fit the triptych without scaling the text down.
        # Three columns that fit the monitor side by side WITHOUT overlap
        # (the display runs at a ~1.2 scale; these logical sizes become
        # ~620x850 captures). Windows that overlap, hang off the edge, or
        # hide each other pause their frame delivery — and a paused window
        # answers no screenshots.
        positions = {"host": (8, 70), "bob": (526, 70), "carol": (1044, 70)}
        self.geometry = (positions.get(name, (400, 300)), 510, 700)

    def start(self, env_extra):
        env = dict(os.environ)
        env.update(env_extra)
        # A fixed window size for every instance (the captures and the clip
        # need a known geometry; the joinsers' windows otherwise land on
        # whatever the compositor hands out this time).
        cfg_home = os.path.join(self.demo.out, "cfg", self.name)
        os.makedirs(os.path.join(cfg_home, "wordcraft"), exist_ok=True)
        with open(os.path.join(cfg_home, "wordcraft", "ui.json"), "w") as f:
            json.dump({"window": {"position": list(self.geometry[0]), "width": self.geometry[1], "height": self.geometry[2], "maximized": False, "monitor": None}}, f)
        env["XDG_CONFIG_HOME"] = cfg_home
        log_path = self.demo.path("logs", f"{self.name}.log")
        self.log_file = open(log_path, "ab")
        self.proc = subprocess.Popen(
            [self.demo.args.bin, "--collab", self.config, "--control", str(self.port)],
            stdout=self.log_file,
            stderr=subprocess.STDOUT,
            env=env,
            preexec_fn=os.setsid,
        )
        return self

    def stop(self):
        if self.proc and self.proc.poll() is None:
            try:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(os.getpgid(self.proc.pid), signal.SIGKILL)
        if getattr(self, "log_file", None):
            self.log_file.close()
            self.log_file = None

    # ---- the control channel ----------------------------------------------------------------------

    def request(self, req, timeout=30):
        self.next_id += 1
        req = dict(req)
        req["id"] = self.next_id
        payload = json.dumps(req)
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            try:
                s = socket.create_connection(("127.0.0.1", self.port), timeout=max(1, int(deadline - time.time())))
                s.sendall((payload + "\n").encode())
                line = s.makefile().readline()
                s.close()
                reply = json.loads(line)
                with open(os.path.join(self.demo.out, "transcript.jsonl"), "a") as f:
                    f.write(json.dumps({"at": round(time.time() - self.demo.started_at, 2), "instance": self.name, "request": req, "reply": reply}) + "\n")
                if not reply.get("ok", False):
                    raise RuntimeError(f"{self.name}: {req.get('method')} failed: {reply.get('error')}")
                return reply
            except (ConnectionRefusedError, FileNotFoundError, OSError) as e:
                last = e
                time.sleep(0.5)
        raise RuntimeError(f"{self.name}: no control answer within {timeout}s ({last})")

    def wait_ready(self, seconds):
        deadline = time.time() + seconds
        while time.time() < deadline:
            try:
                self.request({"method": "collab.stats", "params": {}}, timeout=5)
                self.place()
                return
            except (RuntimeError, OSError):
                time.sleep(1)
        raise RuntimeError(f"{self.name}: control channel never answered")

    def place(self):
        """Floats this window and gives it the exact geometry the triptych
        wants, when the compositor offers a handle (Hyprland's hyprctl);
        otherwise the window stays wherever it landed and captures still
        work."""
        if not shutil.which("hyprctl"):
            return
        for _ in range(15):
            try:
                out = subprocess.run(["hyprctl", "clients", "-j"], capture_output=True, text=True, timeout=5)
                clients = json.loads(out.stdout)
            except Exception:
                return
            mine = [c for c in clients if c.get("pid") == self.proc.pid]
            if mine:
                addr = "address:" + mine[0]["address"]
                (_pos, w, h) = self.geometry
                subprocess.run(["hyprctl", "dispatch", "setfloating", addr], capture_output=True)
                subprocess.run(["hyprctl", "dispatch", f"resizewindowpixel exact {w} {h},{addr}"], capture_output=True)
                subprocess.run(["hyprctl", "dispatch", f"movewindowpixel exact {_pos[0]} {_pos[1]},{addr}"], capture_output=True)
                return
            time.sleep(0.5)

    def wait_until(self, cond, seconds, what):
        deadline = time.time() + seconds
        while time.time() < deadline:
            try:
                value = cond(self)
                if value:
                    self.demo.log(f"  [{self.name}] {what}")
                    return value
            except (RuntimeError, OSError):
                pass
            time.sleep(1.5)
        raise AssertionError(f"{self.name}: timed out waiting for {what}")

    def run(self, command, params):
        return self.request({"method": "engine.execute", "params": {"command": command, "params": params}})

    def stats(self):
        return self.request({"method": "engine.execute", "params": {"command": "collab.stats", "params": {}}})["result"]

    def dump(self):
        return self.request({"method": "engine.execute", "params": {"command": "document.inspect", "params": {}}})["result"]["blocks"]

    def text(self):
        blocks = self.dump()
        return blocks[0].get("text", "") if blocks else ""

    def peers(self):
        return {p["name"] for p in self.stats()["peers"]}

    def screenshot(self, path, timeout=20):
        self.request({"method": "ui.screenshot", "params": {"path": path}}, timeout=timeout)


def main():
    parser = argparse.ArgumentParser(description="WordCraft + Macula automated collaboration demo")
    parser.add_argument("--bin", default="target/release/wordcraft", help="the WordCraft binary")
    parser.add_argument("--mode", choices=["headless", "visual"], default="headless")
    parser.add_argument("--stations", default=",".join(DEFAULT_STATIONS), help="host:port:node_hex per instance")
    parser.add_argument("--ports", default="8791,8792,8793", help="control ports per instance")
    parser.add_argument("--out", default="collab-demo")
    parser.add_argument("--doc", default=None)
    parser.add_argument("--realm-key-file", default=None, help="file with the realm's public trust anchor (hex)")
    parser.add_argument("--realm-key-hex", default=None)
    parser.add_argument("--record", default=None, help="write a short 16:9 mp4 of the story to this path")
    args = parser.parse_args()

    args.stations = [s for s in args.stations.split(",") if s]
    args.ports = [int(p) for p in args.ports.split(",")]
    if len(args.stations) < 3 or len(args.ports) < 3:
        parser.error("three stations and three control ports are needed")
    if not os.path.exists(args.bin):
        parser.error(f"{args.bin} not found — build it first: cargo build --release -p wordcraft")
    if args.realm_key_hex:
        args.realm_key = args.realm_key_hex.strip()
    elif args.realm_key_file:
        with open(args.realm_key_file) as f:
            args.realm_key = f.read().strip()
    else:
        parser.error("the realm key is required for the sign-in step: --realm-key-file or --realm-key-hex")
    if len(args.realm_key) < 64:
        parser.error("the realm key looks too short to be the carried public key")

    demo = Demo(args)
    try:
        demo.run()
    except Exception as e:  # one clear failure line, evidence stays on disk
        demo.failures.append(str(e))
        demo.log(f"aborted: {e}")
    finally:
        code = demo.finish()
        if demo.args.mode == "visual":
            try:
                while any(i.proc and i.proc.poll() is None for i in demo.instances):
                    time.sleep(1)
            except KeyboardInterrupt:
                demo.stop_all()
    sys.exit(code)


if __name__ == "__main__":
    main()
