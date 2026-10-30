#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Measures v12n and Ontop on the same PostgreSQL and writes the results.

Started by bench/run.sh, which builds v12n, loads the data and creates the
Ontop container; see bench/README.md. Standard library only.

Every query is checked first: the results of each engine must be the same
solutions as Ontop's (as a bag, or in order with ORDER BY) before its
timings count. Timed runs then alternate between the engines.
"""

import argparse
import collections
import http.client
import json
import math
import os
import platform
import re
import shlex
import signal
import socket
import statistics
import struct
import subprocess
import sys
import threading
import time
import urllib.parse
from pathlib import Path

ACCEPT = "application/sparql-results+json"
XSD_STRING = "http://www.w3.org/2001/XMLSchema#string"
# The first solution, or the end of an empty result, in SPARQL JSON results.
FIRST_ROW = re.compile(rb'"bindings"\s*:\s*\[\s*[{\]]')
HTTP_TIMEOUT = 900
# What a statement in pg_stat_statements is, by the first pattern its text
# matches; any other statement is SQL the engine generated for the query.
# v12n's catalog statements (crates/v12n-postgres/src/catalog.rs and
# lib.rs) resolve table names with to_regclass and read the system
# catalogs, which SQL generated from a mapping over the application's
# tables does not. Generated SQL can name pg_catalog types and call
# pg_typeof, so neither marks a catalog statement. A `SELECT 1` is a
# connection check, which Ontop runs on some requests. The column probes
# (`table_probe` in v12n-core, only prepared) are the only statements with
# the alias p0.
STATEMENT_KINDS = (
    ("transaction", re.compile(r"^\s*(BEGIN|START\s+TRANSACTION|COMMIT|END|ROLLBACK|ABORT|SAVEPOINT|RELEASE)\b", re.I)),
    ("session", re.compile(r"^\s*((SET|RESET|SHOW|DISCARD)\b|SELECT\s+(\$\d+|\d+)\s*;?\s*$)|\bset_config\s*\(", re.I)),
    ("catalog", re.compile(
        r"\bto_regclass\s*\(|\bpg_(class|index|attribute|constraint|inherits|opclass|namespace|type)\b"
        r"|\sp0\s+LIMIT\s+(0|\$\d+)\s*$", re.I)),
)
KINDS = ("query", "catalog", "transaction", "session")


def log(message):
    print(f"[bench {time.strftime('%H:%M:%S')}] {message}", file=sys.stderr, flush=True)


def percentile(values, p):
    """Nearest-rank percentile."""
    ordered = sorted(values)
    return ordered[max(0, math.ceil(p / 100 * len(ordered)) - 1)]


def summary(values):
    if not values:
        return None
    return {
        "n": len(values),
        "median": round(statistics.median(values), 3),
        "p95": round(percentile(values, 95), 3),
        "min": round(min(values), 3),
        "max": round(max(values), 3),
    }


def statement_kind(sql):
    for kind, pattern in STATEMENT_KINDS:
        if pattern.search(sql):
            return kind
    return "query"


def per_request(statements, runs):
    return {
        "statements": round(sum(s["calls"] for s in statements) / runs, 2),
        "exec_ms": round(sum(s["total_exec_ms"] for s in statements) / runs, 3),
        "plan_ms": round(sum(s["total_plan_ms"] for s in statements) / runs, 3),
    }


def run(cmd, **kwargs):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kwargs).stdout


class Response:
    def __init__(self, status, ttfr_ms, total_ms, size, body, headers):
        self.status = status
        self.ttfr_ms = ttfr_ms
        self.total_ms = total_ms
        self.size = size
        self.body = body
        self.headers = headers


def read_stream(read, started, keep_body):
    """Reads a results stream to the end; returns (ttfr_ms, total_ms, size, body)."""
    ttfr = None
    prefix = b""
    chunks = []
    size = 0
    while True:
        chunk = read()
        if not chunk:
            break
        size += len(chunk)
        if keep_body:
            chunks.append(chunk)
        if ttfr is None:
            prefix += chunk
            if FIRST_ROW.search(prefix):
                ttfr = time.perf_counter()
                prefix = b""
    end = time.perf_counter()
    ttfr = end if ttfr is None else ttfr
    return (ttfr - started) * 1000, (end - started) * 1000, size, b"".join(chunks)


def post(port, query, keep_body=False):
    """A SPARQL 1.1 Protocol POST on a new connection, like a simple client."""
    body = urllib.parse.urlencode({"query": query}).encode()
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=HTTP_TIMEOUT)
    try:
        started = time.perf_counter()
        conn.request(
            "POST",
            "/sparql",
            body,
            {"Accept": ACCEPT, "Content-Type": "application/x-www-form-urlencoded"},
        )
        resp = conn.getresponse()
        ttfr, total, size, data = read_stream(
            lambda: resp.read1(1 << 16), started, keep_body or resp.status != 200
        )
        return Response(resp.status, ttfr, total, size, data, dict(resp.getheaders()))
    finally:
        conn.close()


def canonical(body):
    """Variables and rows of SPARQL JSON results, terms as comparable tuples."""
    data = json.loads(body)
    variables = sorted(data["head"]["vars"])

    def term(t):
        if t is None:
            return None
        kind = "literal" if t["type"] == "typed-literal" else t["type"]
        if kind != "literal":
            return (kind, t["value"])
        lang = t.get("xml:lang")
        if lang:
            return ("literal", t["value"], None, lang.lower())
        # RDF 1.1: a simple literal is an xsd:string.
        return ("literal", t["value"], t.get("datatype", XSD_STRING), None)

    rows = [tuple(term(b.get(v)) for v in variables) for b in data["results"]["bindings"]]
    return variables, rows


def same_results(expected, actual, ordered):
    """None when equal, else a short description of the difference."""
    (ev, er), (av, ar) = expected, actual
    if ev != av:
        return f"variables differ: {ev} vs {av}"
    if ordered:
        if er == ar:
            return None
        first = next((i for i, (a, b) in enumerate(zip(er, ar)) if a != b), min(len(er), len(ar)))
        return f"order or rows differ from row {first} ({len(er)} vs {len(ar)} rows)"
    ec, ac = collections.Counter(er), collections.Counter(ar)
    if ec == ac:
        return None
    only_e = sum((ec - ac).values())
    only_a = sum((ac - ec).values())
    return f"{only_e} solutions only in the reference, {only_a} only here"


class V12nServe:
    """`v12n serve` as a native process."""

    def __init__(self, name, binary, port, db_url, mapping, row_limit, log_path):
        self.name = name
        self.binary = binary
        self.port = port
        self.db_url = db_url
        self.mapping = mapping
        self.row_limit = row_limit
        self.log_path = log_path
        self.proc = None
        self.pid = None
        self.peak_kib = None

    def start(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("V12N_")}
        env.update(
            {
                "V12N_ADDRESS": "127.0.0.1",
                "V12N_PORT": str(self.port),
                "V12N_DATABASE_URL": self.db_url,
                "V12N_MAPPING": str(self.mapping),
                "V12N_ROW_LIMIT": str(self.row_limit),
                # Ontop has no statement timeout either.
                "V12N_STATEMENT_TIMEOUT": "0",
            }
        )
        cmd = [str(self.binary), "serve"]
        # macOS has no VmHWM; time -l reports the peak at exit.
        wrapped = sys.platform == "darwin"
        if wrapped:
            cmd = ["/usr/bin/time", "-l"] + cmd
        logfile = open(self.log_path, "a")
        self.proc = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=logfile)
        logfile.close()
        self.pid = self.proc.pid
        if wrapped:
            deadline = time.time() + 10
            while time.time() < deadline:
                out = subprocess.run(["pgrep", "-P", str(self.proc.pid)], capture_output=True, text=True).stdout.split()
                if out:
                    self.pid = int(out[0])
                    break
                time.sleep(0.002)

    def rss_kib(self):
        status = Path(f"/proc/{self.pid}/status")
        if status.exists():
            return proc_status(status.read_text())["VmRSS"]
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.pid)], capture_output=True, text=True).stdout
        return int(out.strip()) if out.strip() else None

    def stop(self):
        if self.proc is None:
            return
        status = Path(f"/proc/{self.pid}/status")
        if status.exists():
            self.peak_kib = proc_status(status.read_text())["VmHWM"]
        try:
            os.kill(self.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        if sys.platform == "darwin":
            text = Path(self.log_path).read_text(errors="replace")
            found = re.findall(r"(\d+)\s+maximum resident set size", text)
            if found:
                self.peak_kib = int(found[-1]) // 1024
        self.proc = None


class OntopContainer:
    """An Ontop endpoint container created by run.sh."""

    def __init__(self, compose, service, port):
        self.name = service
        self.compose = compose
        self.port = port
        self.peak_kib = None

    def start(self):
        run(self.compose + ["start", self.name])

    def stop(self):
        if not self.container():
            return
        self.peak_kib = self.memory().get("VmHWM")
        run(self.compose + ["stop", self.name])

    def container(self):
        return run(self.compose + ["ps", "-q", self.name]).strip()

    def memory(self):
        out = run(
            [
                "docker",
                "exec",
                self.container(),
                "sh",
                "-c",
                'grep -E "^Vm(RSS|HWM)" /proc/$(pgrep -o java)/status',
            ]
        )
        return proc_status(out)

    def rss_kib(self):
        return self.memory().get("VmRSS")

    def started_in(self):
        """Ontop's own start time (Spring Boot), from its latest start."""
        logs = run(self.compose + ["logs", "--no-log-prefix", self.name])
        found = re.findall(r"Started \S+ in ([0-9.]+) seconds", logs)
        return float(found[-1]) if found else None


class WireCounter:
    """A TCP proxy in front of PostgreSQL that counts round trips: the
    requests (frontend messages up to a Sync, or a simple Query) that a
    client sends while none of its earlier requests is still waiting for
    its ReadyForQuery. Requests sent together (pipelined) count once. A
    request of Close messages only, which tokio-postgres sends when a
    statement is dropped and never waits for, does not count.

    A client message is counted before it is forwarded and a ReadyForQuery
    after it is read and before it is forwarded, so a request that waits
    for an answer is always counted as a new round trip."""

    SSL_REQUEST = 80877103

    def __init__(self, pg_port):
        self.pg_port = pg_port
        self.lock = threading.Lock()
        self.round_trips = 0
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.port = self.listener.getsockname()[1]
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while True:
            try:
                client, _ = self.listener.accept()
            except OSError:
                return
            server = socket.create_connection(("127.0.0.1", self.pg_port))
            for sock in (client, server):
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            state = {"pending": collections.deque(), "request": [], "ssl": False}
            threading.Thread(target=self.pipe, args=(client, server, state, True), daemon=True).start()
            threading.Thread(target=self.pipe, args=(server, client, state, False), daemon=True).start()

    def pipe(self, src, dst, state, from_client):
        buf = b""
        # The client's first message (startup, or an SSLRequest that the
        # server answers with one byte) has no type byte.
        startup = from_client
        try:
            while True:
                chunk = src.recv(1 << 16)
                if not chunk:
                    break
                buf += chunk
                with self.lock:
                    while True:
                        if not from_client and state["ssl"]:
                            if not buf:
                                break
                            state["ssl"] = False
                            state["pending"].clear()
                            buf = buf[1:]
                        elif startup:
                            if len(buf) < 8:
                                break
                            length, code = struct.unpack("!II", buf[:8])
                            if len(buf) < length:
                                break
                            buf = buf[length:]
                            state["ssl"] = startup = code == self.SSL_REQUEST
                            self.request(state, ["startup"])
                        else:
                            if len(buf) < 5:
                                break
                            kind = chr(buf[0])
                            (length,) = struct.unpack("!I", buf[1:5])
                            if len(buf) < 1 + length:
                                break
                            buf = buf[1 + length:]
                            if not from_client:
                                if kind == "Z" and state["pending"]:
                                    state["pending"].popleft()
                            elif kind in "SQ":
                                self.request(state, state["request"] + [kind])
                                state["request"] = []
                            elif kind != "X":
                                state["request"].append(kind)
                dst.sendall(chunk)
        except OSError:
            pass
        finally:
            for sock in (src, dst):
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def request(self, state, kinds):
        """Called with the lock held, for each complete request."""
        waits = any(k not in ("C", "S") for k in kinds)
        if waits and not any(state["pending"]):
            self.round_trips += 1
        state["pending"].append(waits)

    def count(self):
        with self.lock:
            return self.round_trips

    def close(self):
        self.listener.close()


def proc_status(text):
    values = {}
    for line in text.splitlines():
        m = re.match(r"(Vm\w+):\s+(\d+) kB", line)
        if m:
            values[m.group(1)] = int(m.group(2))
    return values


def wait_ready(engine, query, timeout=300):
    """Milliseconds until the engine answers `query` with 200."""
    started = time.perf_counter()
    deadline = started + timeout
    while time.perf_counter() < deadline:
        try:
            if post(engine.port, query).status == 200:
                return (time.perf_counter() - started) * 1000
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.02)
    raise RuntimeError(f"{engine.name} not ready after {timeout}s")


class Postgres:
    def __init__(self, compose):
        self.compose = compose

    def query_json(self, sql):
        out = run(
            self.compose
            + ["exec", "-T", "postgres", "psql", "-X", "-At", "-v", "ON_ERROR_STOP=1", "-U", "v12n", "-d", "mdr", "-c", sql]
        )
        return json.loads(out)

    def reset_statements(self):
        run(self.compose + ["exec", "-T", "postgres", "psql", "-X", "-q", "-U", "v12n", "-d", "mdr", "-c", "SELECT pg_stat_statements_reset()"])

    def statements(self):
        """Per role, each statement with its kind, the one that took the
        most time first."""
        each = self.query_json(
            """SELECT coalesce(json_object_agg(rolname, t), '{}') FROM (
                 SELECT rolname, json_agg(json_build_object(
                   'calls', calls, 'total_exec_ms', total_exec_time, 'total_plan_ms', total_plan_time,
                   'mean_exec_ms', mean_exec_time, 'mean_plan_ms', mean_plan_time, 'rows', rows,
                   'query', query) ORDER BY total_exec_time DESC) AS t
                 FROM (SELECT r.rolname, s.* FROM pg_stat_statements s
                       JOIN pg_roles r ON r.oid = s.userid
                       WHERE r.rolname LIKE 'bench\\_%') y
                 GROUP BY rolname) x"""
        )
        for statements in each.values():
            for s in statements:
                s["kind"] = statement_kind(s["query"])
        return each


def cli_query(binary, mapping, query_path, db_url):
    env = {k: v for k, v in os.environ.items() if not k.startswith("V12N_")}
    cmd = [str(binary), "query", str(mapping), str(query_path), "--db", db_url, "--row-limit", "0", "--timeout", "0", "--format", "json"]
    started = time.perf_counter()
    proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    ttfr, _, _, body = read_stream(lambda: proc.stdout.read1(1 << 16), started, True)
    err = proc.stderr.read()
    code = proc.wait()
    # Until the process has exited, not only until stdout closed.
    total = (time.perf_counter() - started) * 1000
    return code, ttfr, total, body, err


def machine_info(pg):
    info = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "cpus": os.cpu_count(),
    }
    if sys.platform == "darwin":
        info["cpu"] = run(["sysctl", "-n", "machdep.cpu.brand_string"]).strip()
        info["memory_gib"] = round(int(run(["sysctl", "-n", "hw.memsize"])) / 2**30, 1)
    else:
        cpuinfo = Path("/proc/cpuinfo").read_text()
        m = re.search(r"model name\s*:\s*(.+)", cpuinfo)
        info["cpu"] = m.group(1) if m else platform.processor()
        m = re.search(r"MemTotal:\s+(\d+) kB", Path("/proc/meminfo").read_text())
        info["memory_gib"] = round(int(m.group(1)) / 2**20, 1) if m else None
    docker = json.loads(run(["docker", "info", "--format", "{{json .}}"]))
    info["docker"] = {
        "server": docker.get("ServerVersion"),
        "os": docker.get("OperatingSystem"),
        "arch": docker.get("Architecture"),
        "cpus": docker.get("NCPU"),
        "memory_gib": round(docker.get("MemTotal", 0) / 2**30, 1),
    }
    info["postgres"] = pg.query_json(
        """SELECT json_build_object('version', version(),
             'shared_buffers', current_setting('shared_buffers'),
             'work_mem', current_setting('work_mem'),
             'jit', current_setting('jit'),
             'max_parallel_workers_per_gather', current_setting('max_parallel_workers_per_gather'))"""
    )
    info["rows"] = pg.query_json(
        """SELECT json_build_object('assets', (SELECT count(*) FROM assets),
             'alerts', (SELECT count(*) FROM alerts), 'links', (SELECT count(*) FROM links),
             'observables', (SELECT count(*) FROM observables))"""
    )
    try:
        info["rustc"] = run(["rustc", "-V"]).strip()
    except (OSError, subprocess.CalledProcessError):
        info["rustc"] = None
    return info


def round_trips(compose, pg_port):
    """Average `SELECT 1` latency (extended protocol, as both engines use)
    on each path to PostgreSQL: from a container on the Compose network
    (ontop), from a container through the published port (ontop-host), and
    from the host (v12n)."""
    script = (
        "echo 'SELECT 1;' > /tmp/s.sql && PGPASSWORD=bench-dev pgbench -n -M extended "
        "-f /tmp/s.sql -T 5 -h {host} -p {port} -U bench_v12n mdr"
    )
    paths = {
        "container_network_ms": ("postgres", 5432),
        "container_via_host_ms": ("host.docker.internal", pg_port),
    }
    result = {}

    def latency(out):
        m = re.search(r"latency average = ([0-9.]+) ms", out)
        return float(m.group(1)) if m else None

    for key, (host, port) in paths.items():
        try:
            cmd = ["run", "--rm", "--no-deps", "-T", "--entrypoint", "sh", "postgres", "-c", script.format(host=host, port=port)]
            result[key] = latency(run(compose + cmd))
        except subprocess.CalledProcessError as e:
            log(f"pgbench in a container failed: {e.stderr.strip()[:200]}")
    try:
        result["host_ms"] = latency(run(["sh", "-c", script.format(host="127.0.0.1", port=pg_port)]))
    except (OSError, subprocess.CalledProcessError):
        log("no pgbench on the host; skipping the host round trip")
    return result


def load_phase(engine, mix, clients, seconds):
    """Several clients send the queries in `mix` for `seconds`."""
    latencies = []
    errors = [0]
    lock = threading.Lock()
    stop_at = time.perf_counter() + seconds
    rss = []
    sampling = threading.Event()

    def client(i):
        k = i
        while time.perf_counter() < stop_at:
            query = mix[k % len(mix)]
            k += 1
            try:
                r = post(engine.port, query)
                ok = r.status == 200
            except (OSError, http.client.HTTPException):
                ok = False
            with lock:
                if ok:
                    latencies.append(r.total_ms)
                else:
                    errors[0] += 1

    def sampler():
        while not sampling.wait(1.0):
            try:
                value = engine.rss_kib()
                if value:
                    rss.append(value)
            except (OSError, subprocess.CalledProcessError):
                pass

    threads = [threading.Thread(target=client, args=(i,)) for i in range(clients)]
    watcher = threading.Thread(target=sampler)
    started = time.perf_counter()
    watcher.start()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    elapsed = time.perf_counter() - started
    sampling.set()
    watcher.join()
    return {
        "clients": clients,
        "seconds": round(elapsed, 1),
        "requests": len(latencies),
        "errors": errors[0],
        "per_second": round(len(latencies) / elapsed, 1),
        "latency_ms": summary(latencies),
        "rss_max_sampled_mib": round(max(rss) / 1024, 1) if rss else None,
    }


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--compose", required=True, help="docker compose command with project and file")
    p.add_argument("--v12n", type=Path, required=True)
    p.add_argument("--mapping", type=Path, required=True)
    p.add_argument("--queries", type=Path, required=True)
    p.add_argument("--pg-port", type=int, required=True)
    p.add_argument("--ontop-port", type=int, required=True)
    p.add_argument("--v12n-port", type=int, required=True)
    p.add_argument("--runs", type=int, default=20)
    p.add_argument("--warmup", type=int, default=3)
    p.add_argument("--cli-runs", type=int, default=10)
    p.add_argument("--starts", type=int, default=3)
    p.add_argument("--load-clients", type=int, default=8)
    p.add_argument("--load-seconds", type=int, default=15)
    p.add_argument("--settings", type=json.loads, default={})
    args = p.parse_args()

    compose = shlex.split(args.compose)
    pg = Postgres(compose)
    queries = {q.stem: q.read_text() for q in sorted(args.queries.glob("*.rq"))}
    ready_query = queries[sorted(queries)[0]]
    db = "postgresql://{user}:bench-dev@127.0.0.1:%d/mdr" % args.pg_port
    v12n = V12nServe("v12n", args.v12n, args.v12n_port, db.format(user="bench_v12n"), args.mapping, 0, args.out / "v12n.log")
    v12n_limit = V12nServe(
        "v12n-row-limit", args.v12n, args.v12n_port + 1, db.format(user="bench_v12n_limit"), args.mapping, 10000, args.out / "v12n-row-limit.log"
    )
    ontop = OntopContainer(compose, "ontop", args.ontop_port)
    ontop_host = OntopContainer(compose, "ontop-host", args.ontop_port + 1)
    results = {"settings": args.settings, "machine": machine_info(pg), "startup": {}, "memory": {}}
    log(f"machine: {results['machine']['cpu']}, rows {results['machine']['rows']}")

    try:
        # Start-up: from launch to the first answered query.
        for engine in (v12n, ontop):
            times, own = [], []
            for i in range(args.starts):
                engine.start()
                times.append(wait_ready(engine, ready_query))
                if engine is ontop:
                    own.append(ontop.started_in())
                if i < args.starts - 1:
                    engine.stop()
            results["startup"][engine.name] = {"ready_ms": summary(times), "runs_ms": [round(t, 1) for t in times]}
            if own and all(own):
                results["startup"][engine.name]["spring_boot_started_s"] = own
            log(f"{engine.name} ready in {[round(t) for t in times]} ms")
        v12n_limit.start()
        ontop_host.start()
        wait_ready(v12n_limit, ready_query)
        wait_ready(ontop_host, ready_query)
        results["round_trip_select1"] = round_trips(compose, args.pg_port)
        log(f"SELECT 1 round trips: {results['round_trip_select1']}")
        time.sleep(2)
        for engine in (v12n, ontop):
            results["memory"][engine.name] = {"idle_rss_mib": round(engine.rss_kib() / 1024, 1)}

        engines = [v12n, v12n_limit, ontop, ontop_host]
        results["queries"] = {}
        for name, text in queries.items():
            ordered = re.search(r"^\s*ORDER\s+BY", text, re.I | re.M) is not None
            entry = {"ordered": ordered, "engines": {}}
            reference = None
            for engine in [ontop, v12n, v12n_limit, ontop_host]:
                checked = {}
                for w in range(args.warmup):
                    r = post(engine.port, text, keep_body=(w == 0))
                    if r.status != 200:
                        checked = {"status": r.status, "error": r.body[:500].decode(errors="replace")}
                        break
                    if w == 0:
                        solutions = canonical(r.body)
                        checked = {"rows": len(solutions[1]), "bytes": r.size}
                        truncated = any(k.lower() == "v12n-truncated" for k in r.headers)
                        if truncated:
                            checked["truncated"] = True
                        if engine is ontop:
                            reference = solutions
                        else:
                            diff = same_results(reference, solutions, ordered) if reference else "no reference"
                            checked["matches_ontop"] = diff is None
                            if diff:
                                checked["difference"] = diff
                entry["engines"][engine.name] = checked
            entry["rows"] = entry["engines"]["ontop"].get("rows")
            valid = [e for e in engines if "rows" in entry["engines"][e.name]
                     and (e is ontop or entry["engines"][e.name].get("matches_ontop") or entry["engines"][e.name].get("truncated"))]
            timings = {e.name: {"ttfr": [], "total": []} for e in valid}
            pg.reset_statements()
            for i in range(args.runs):
                for e in valid[i % len(valid):] + valid[: i % len(valid)]:
                    r = post(e.port, text)
                    if r.status != 200:
                        raise RuntimeError(f"{e.name} {name}: HTTP {r.status}")
                    timings[e.name]["ttfr"].append(r.ttfr_ms)
                    timings[e.name]["total"].append(r.total_ms)
            stats = pg.statements()
            role = {
                "v12n": "bench_v12n",
                "v12n-row-limit": "bench_v12n_limit",
                "ontop": "bench_ontop",
                "ontop-host": "bench_ontop_host",
            }
            for e in valid:
                record = entry["engines"][e.name]
                record["ttfr_ms"] = summary(timings[e.name]["ttfr"])
                record["total_ms"] = summary(timings[e.name]["total"])
                statements = stats.get(role[e.name])
                if statements:
                    record["db_per_request"] = dict(
                        per_request(statements, args.runs),
                        by_kind={k: per_request([s for s in statements if s["kind"] == k], args.runs) for k in KINDS},
                    )
                    # Statements that were only prepared, such as v12n's
                    # column probes, have an entry without calls.
                    top = next((s for s in statements if s["kind"] == "query" and s["calls"]), None)
                    if top:
                        record["top_statement"] = top
                    record["statements"] = statements
            results["queries"][name] = entry
            log(f"{name}: " + ", ".join(
                f"{e}={v['total_ms']['median']}ms" if v.get("total_ms") else f"{e}=n/a"
                for e, v in entry["engines"].items()))

        # Round trips to PostgreSQL per request, counted by a proxy in
        # front of it for a separate `v12n serve`, so that the proxy adds
        # nothing to the timings above. Requests run one at a time, so they
        # reuse one pooled connection after the warm-up.
        counter = WireCounter(args.pg_port)
        v12n_wire = V12nServe(
            "v12n-wire", args.v12n, args.v12n_port + 2,
            "postgresql://bench_v12n:bench-dev@127.0.0.1:%d/mdr" % counter.port,
            args.mapping, 0, args.out / "v12n-wire.log",
        )
        try:
            v12n_wire.start()
            wait_ready(v12n_wire, ready_query)
            for name, text in queries.items():
                counts = []
                for i in range(args.warmup + 5):
                    before = counter.count()
                    r = post(v12n_wire.port, text)
                    if r.status != 200:
                        raise RuntimeError(f"v12n-wire {name}: HTTP {r.status}")
                    if i >= args.warmup:
                        counts.append(counter.count() - before)
                results["queries"][name]["engines"]["v12n"]["round_trips"] = counts
                log(f"{name}: round trips per request {counts}")
        finally:
            v12n_wire.stop()
            counter.close()

        # The CLI: process start, mapping, connection and query per run.
        results["cli"] = {}
        for name, text in queries.items():
            path = args.queries / f"{name}.rq"
            ordered = results["queries"][name]["ordered"]
            ttfr, total, check = [], [], {}
            for i in range(args.cli_runs + 1):
                code, t1, t2, body, err = cli_query(args.v12n, args.mapping, path, db.format(user="bench_v12n"))
                if code != 0:
                    check = {"exit": code, "error": err.decode(errors="replace")[:500]}
                    break
                if i == 0:
                    ontop_rows = results["queries"][name]["engines"]["ontop"].get("rows")
                    solutions = canonical(body)
                    reference_ok = ontop_rows is not None
                    check = {"rows": len(solutions[1])}
                    if reference_ok:
                        # Compared with the v12n serve result, which matched Ontop.
                        serve = post(v12n.port, text, keep_body=True)
                        diff = same_results(canonical(serve.body), solutions, ordered)
                        check["matches_serve"] = diff is None
                        if diff:
                            check["difference"] = diff
                            break
                    continue
                ttfr.append(t1)
                total.append(t2)
            check["ttfr_ms"] = summary(ttfr)
            check["total_ms"] = summary(total)
            results["cli"][name] = check
            log(f"cli {name}: {check.get('total_ms', {}) and check['total_ms']['median']} ms")

        # Several clients at once, one engine at a time.
        mix_names = [n for n in queries if re.match(r"q0[1237]-", n)]
        mix = [queries[n] for n in mix_names]
        results["load"] = {"queries": mix_names}
        for engine in (v12n, ontop):
            results["load"][engine.name] = load_phase(engine, mix, args.load_clients, args.load_seconds)
            log(f"load {engine.name}: {results['load'][engine.name]['per_second']} requests/s")
        for engine in (v12n, ontop):
            results["memory"][engine.name]["after_rss_mib"] = round(engine.rss_kib() / 1024, 1)
    finally:
        for engine in (v12n, v12n_limit, ontop, ontop_host):
            try:
                engine.stop()
            except Exception as e:  # noqa: BLE001 - cleanup must go on
                log(f"stopping {engine.name}: {e}")
    for engine in (v12n, ontop):
        if engine.peak_kib and engine.name in results["memory"]:
            results["memory"][engine.name]["peak_rss_mib"] = round(engine.peak_kib / 1024, 1)

    translate = args.out / "translate.jsonl"
    if translate.exists():
        rows = [json.loads(line) for line in translate.read_text().splitlines() if line.strip()]
        results["translation"] = {row["query"]: row for row in rows}

    (args.out / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    (args.out / "results.md").write_text(markdown(results))
    log(f"wrote {args.out / 'results.md'}")


def fmt(stat, key="median"):
    return "-" if not stat else f"{stat[key]:.1f}"


def markdown(r):
    m = r["machine"]
    lines = [
        "# Benchmark results",
        "",
        f"- Machine: {m['cpu']}, {m['cpus']} CPUs, {m['memory_gib']} GiB, {m['platform']}",
        f"- Docker: {m['docker']['server']} on {m['docker']['os']} ({m['docker']['arch']}, {m['docker']['cpus']} CPUs, {m['docker']['memory_gib']} GiB)",
        f"- PostgreSQL: {m['postgres']['version'].split(' on ')[0]}, shared_buffers {m['postgres']['shared_buffers']}, work_mem {m['postgres']['work_mem']}, jit {m['postgres']['jit']}",
        f"- Rows: {m['rows']}",
        f"- Settings: {json.dumps(r['settings'])}",
        f"- `SELECT 1` round trip: {r.get('round_trip_select1')}",
        "",
        "## Start-up and memory",
        "",
        "| Engine | Ready (median of runs) | Runs | Idle RSS | RSS after load | Peak RSS |",
        "|---|---:|---|---:|---:|---:|",
    ]
    for name in ("v12n", "ontop"):
        s = r["startup"].get(name, {})
        mem = r["memory"].get(name, {})
        lines.append(
            f"| {name} | {fmt(s.get('ready_ms'))} ms | {s.get('runs_ms')} | {mem.get('idle_rss_mib', '-')} MiB | "
            f"{mem.get('after_rss_mib', '-')} MiB | {mem.get('peak_rss_mib', '-')} MiB |"
        )
    lines += [
        "",
        "## Queries over HTTP (ms, median / p95)",
        "",
        "| Query | Rows | v12n total | Ontop total | Ontop / v12n | v12n first row | Ontop first row | v12n, row limit 10000 | Ontop via published port | Results |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---|",
    ]
    for name, q in r.get("queries", {}).items():
        e = q["engines"]
        v, o, vl, oh = e.get("v12n", {}), e.get("ontop", {}), e.get("v12n-row-limit", {}), e.get("ontop-host", {})
        ratio = "-"
        if v.get("total_ms") and o.get("total_ms"):
            ratio = f"{o['total_ms']['median'] / v['total_ms']['median']:.2f}"
        check = "match" if v.get("matches_ontop") else f"v12n: {v.get('difference') or v.get('error', '?')}"
        if vl.get("truncated"):
            check += "; row limit truncates"
        elif vl and not vl.get("matches_ontop"):
            check += f"; row limit: {vl.get('difference') or vl.get('error', '?')}"
        if oh and not oh.get("matches_ontop"):
            check += f"; ontop-host: {oh.get('difference') or oh.get('error', '?')}"
        lines.append(
            f"| {name} | {q.get('rows')} | {fmt(v.get('total_ms'))} / {fmt(v.get('total_ms'), 'p95')} | "
            f"{fmt(o.get('total_ms'))} / {fmt(o.get('total_ms'), 'p95')} | {ratio} | {fmt(v.get('ttfr_ms'))} | "
            f"{fmt(o.get('ttfr_ms'))} | {fmt(vl.get('total_ms'))} / {fmt(vl.get('total_ms'), 'p95')} | "
            f"{fmt(oh.get('total_ms'))} / {fmt(oh.get('total_ms'), 'p95')} | {check} |"
        )
    lines += [
        "",
        "## Where the time goes (per request, median ms unless noted)",
        "",
        "| Query | Translate (µs) | SQL render (µs) | v12n round trips | v12n generated query exec (mean) | Ontop generated query exec (mean) | CLI total | CLI first row |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for name, q in r.get("queries", {}).items():
        t = r.get("translation", {}).get(name, {})
        v, o = q["engines"].get("v12n", {}), q["engines"].get("ontop", {})
        vq = v.get("db_per_request", {}).get("by_kind", {}).get("query", {})
        oq = o.get("db_per_request", {}).get("by_kind", {}).get("query", {})
        c = r.get("cli", {}).get(name, {})
        lines.append(
            f"| {name} | {t.get('translate_us', {}).get('median', '-')} | {t.get('render_us', {}).get('median', '-')} | "
            f"{statistics.median(v['round_trips']) if v.get('round_trips') else '-'} | {vq.get('exec_ms', '-')} | {oq.get('exec_ms', '-')} | "
            f"{fmt(c.get('total_ms'))} | {fmt(c.get('ttfr_ms'))} |"
        )
    lines += [
        "",
        "## Statements in PostgreSQL per request (statements / exec ms, mean of the timed runs)",
        "",
        "| Query | Engine | Generated query | Catalog | Transaction control | Session | All |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    for name, q in r.get("queries", {}).items():
        for engine, record in q["engines"].items():
            d = record.get("db_per_request")
            if not d:
                continue
            cells = [d["by_kind"].get(k) for k in KINDS] + [d]
            lines.append(f"| {name} | {engine} | " + " | ".join(
                f"{c['statements']} / {c['exec_ms']}" if c and c["statements"] else "-" for c in cells) + " |")
    load = r.get("load", {})
    if load:
        lines += [
            "",
            f"## Load: {load.get('v12n', {}).get('clients')} clients, {', '.join(load.get('queries', []))} in turn",
            "",
            "| Engine | Requests/s | Latency median / p95 (ms) | Errors | RSS max (sampled) |",
            "|---|---:|---:|---:|---:|",
        ]
        for name in ("v12n", "ontop"):
            l = load.get(name, {})
            lines.append(
                f"| {name} | {l.get('per_second')} | {fmt(l.get('latency_ms'))} / {fmt(l.get('latency_ms'), 'p95')} | "
                f"{l.get('errors')} | {l.get('rss_max_sampled_mib')} MiB |"
            )
    lines += ["", "## Generated SQL per query (the query statement that took the most time)", ""]
    for name, q in r.get("queries", {}).items():
        for engine in ("v12n", "ontop"):
            top = q["engines"].get(engine, {}).get("top_statement")
            if top:
                lines += [
                    f"### {name}, {engine}: {top['calls']} calls, {top['mean_exec_ms']:.3f} ms mean",
                    "",
                    "```sql",
                    top["query"].strip(),
                    "```",
                    "",
                ]
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    main()
