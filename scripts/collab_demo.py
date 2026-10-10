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
      [--mode headless|visual] [--stations S1,S2,S3] [--out DIR] [--doc NAME]

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
import signal
import socket
import subprocess
import sys
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

        names = self.approve_pending(host)
        self.check(names == ["Bob", "Carol"], f"host sees the joiners waiting: {names}")
        for joiner, name in ((j1, "Bob"), (j2, "Carol")):
            joiner.wait_until(lambda i: i.stats()["joined"] and "Host start." in i.text(), 60, f"{name} joined and caught up")

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

    def approve_pending(self, host):
        """The host's manual yes for everyone waiting (returns their names)."""
        items = host.wait_until(
            lambda i: (p := i.request({"method": "engine.execute", "params": {"command": "collab.pending"}})["result"]["pending"]) or None,
            60,
            "join requests pending",
        )
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


class Instance:
    def __init__(self, demo, name, port, station, config):
        self.demo = demo
        self.name = name
        self.port = port
        self.station = station
        self.config = config
        self.proc = None
        self.next_id = 0

    def start(self, env_extra):
        env = dict(os.environ)
        env.update(env_extra)
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
                return
            except (RuntimeError, OSError):
                time.sleep(1)
        raise RuntimeError(f"{self.name}: control channel never answered")

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

    def screenshot(self, path):
        self.request({"method": "ui.screenshot", "params": {"path": path}}, timeout=60)


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
