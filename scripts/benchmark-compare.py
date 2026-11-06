#!/usr/bin/env python3
"""
benchmark-compare.py: Side-by-side benchmark comparison between Rust and Go implementations of agy-pool.
Measures:
  1. Binary footprint (file size)
  2. Cold start latency (CLI invocation)
  3. Idle daemon footprint (VmRSS, VmSize, OS threads)
  4. Local HTTP throughput & latency (QPS, P50, P95, P99)
"""

import argparse
import http.client
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor

def get_binary_size(path):
    size_bytes = os.path.getsize(path)
    return size_bytes, size_bytes / (1024 * 1024)

def measure_cold_start(bin_path, n=50):
    latencies = []
    for _ in range(n):
        t0 = time.perf_counter()
        subprocess.run([bin_path, "--version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        latencies.append((time.perf_counter() - t0) * 1000)
    latencies.sort()
    return {
        "mean": statistics.mean(latencies),
        "median": statistics.median(latencies),
        "p95": latencies[int(n * 0.95)],
        "min": min(latencies),
        "max": max(latencies),
    }

def read_proc_status(pid):
    status_path = f"/proc/{pid}/status"
    rss_kb = 0
    vmsize_kb = 0
    threads = 0
    if os.path.exists(status_path):
        with open(status_path, "r") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    rss_kb = int(line.split()[1])
                elif line.startswith("VmSize:"):
                    vmsize_kb = int(line.split()[1])
                elif line.startswith("Threads:"):
                    threads = int(line.split()[1])
    return {
        "rss_mb": rss_kb / 1024.0,
        "vmsize_mb": vmsize_kb / 1024.0,
        "threads": threads,
    }

def start_daemon(bin_path, port):
    bin_path = os.path.abspath(bin_path)
    tmp_dir = tempfile.mkdtemp(prefix="agy-bench-")
    cfg_dir = os.path.join(tmp_dir, "config")
    data_dir = os.path.join(tmp_dir, "data")
    os.makedirs(cfg_dir, exist_ok=True)
    os.makedirs(data_dir, exist_ok=True)

    cfg_file = os.path.join(cfg_dir, "config.json")
    with open(cfg_file, "w") as f:
        f.write(f'{{"version":1,"server":{{"listen":"127.0.0.1","port":{port}}}}}')

    env = os.environ.copy()
    env["AGY_DATA_DIR"] = data_dir
    env["AGY_CONFIG_FILE"] = cfg_file
    env["AGY_POOL_DAEMON_CHILD"] = "1"
    env["AGY_POOL_DAEMON_PORT"] = str(port)

    proc = subprocess.Popen([bin_path], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    
    # Wait for daemon to respond on /healthz
    deadline = time.perf_counter() + 8.0
    ready = False
    last_err = ""
    while time.perf_counter() < deadline:
        if proc.poll() is not None:
            out, err = proc.communicate()
            shutil.rmtree(tmp_dir, ignore_errors=True)
            raise RuntimeError(
                f"Daemon at {bin_path} exited prematurely with code {proc.returncode}.\n"
                f"STDOUT: {out.decode('utf-8', 'replace')}\n"
                f"STDERR: {err.decode('utf-8', 'replace')}"
            )
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=0.5)
            conn.request("GET", "/healthz")
            resp = conn.getresponse()
            if resp.status == 200:
                conn.close()
                ready = True
                break
            last_err = f"HTTP {resp.status}"
            conn.close()
        except Exception as e:
            last_err = str(e)
        time.sleep(0.1)

    if not ready:
        proc.terminate()
        try:
            out, err = proc.communicate(timeout=2)
        except Exception:
            out, err = b"", b""
        shutil.rmtree(tmp_dir, ignore_errors=True)
        raise RuntimeError(
            f"Daemon at {bin_path} failed to respond on port {port} within timeout (last error: {last_err}).\n"
            f"STDOUT: {out.decode('utf-8', 'replace')}\n"
            f"STDERR: {err.decode('utf-8', 'replace')}"
        )

    return proc, tmp_dir

def http_worker(host, port, path, count):
    latencies = []
    conn = http.client.HTTPConnection(host, port, timeout=5)
    for _ in range(count):
        t0 = time.perf_counter()
        conn.request("GET", path)
        resp = conn.getresponse()
        resp.read()
        latencies.append((time.perf_counter() - t0) * 1000)
    conn.close()
    return latencies

def measure_throughput_and_latency(host, port, total=5000, concurrency=20):
    req_per_thread = total // concurrency
    t_start = time.perf_counter()
    with ThreadPoolExecutor(max_workers=concurrency) as ex:
        futs = [ex.submit(http_worker, host, port, "/healthz", req_per_thread) for _ in range(concurrency)]
        all_lats = []
        for f in futs:
            all_lats.extend(f.result())
    duration = time.perf_counter() - t_start
    all_lats.sort()
    actual = len(all_lats)
    return {
        "qps": actual / duration,
        "mean_ms": statistics.mean(all_lats),
        "median_ms": statistics.median(all_lats),
        "p95_ms": all_lats[int(actual * 0.95)],
        "p99_ms": all_lats[int(actual * 0.99)],
        "max_ms": max(all_lats),
    }

def main():
    parser = argparse.ArgumentParser(description="Benchmark Go vs Rust agy-pool implementations")
    parser.add_argument("--rust-bin", required=True, help="Path to agy-pool Rust binary")
    parser.add_argument("--go-bin", required=True, help="Path to agy-pool Go binary")
    parser.add_argument("--requests", type=int, default=5000, help="Total HTTP benchmark requests")
    parser.add_argument("--concurrency", type=int, default=20, help="Concurrent workers")
    parser.add_argument("--cold-start-runs", type=int, default=50, help="Cold start benchmark iterations")
    parser.add_argument("--markdown-file", help="File to write GitHub Markdown summary table to")
    args = parser.parse_args()

    print(f"[*] Benchmarking:")
    print(f"    Rust: {args.rust_bin}")
    print(f"    Go  : {args.go_bin}")

    # 1. Binary Footprint
    _, rust_size_mb = get_binary_size(args.rust_bin)
    _, go_size_mb = get_binary_size(args.go_bin)

    # 2. Cold Start Latency
    print(f"[*] Measuring cold start latency ({args.cold_start_runs} iterations)...")
    rust_cold = measure_cold_start(args.rust_bin, args.cold_start_runs)
    go_cold = measure_cold_start(args.go_bin, args.cold_start_runs)

    # 3. Idle Memory Footprint & Throughput
    print(f"[*] Starting daemons for idle footprint & HTTP load test...")
    rust_proc, rust_tmp = start_daemon(args.rust_bin, 18891)
    go_proc, go_tmp = start_daemon(args.go_bin, 18892)

    try:
        # Settle for 1 second
        time.sleep(1)
        rust_mem = read_proc_status(rust_proc.pid)
        go_mem = read_proc_status(go_proc.pid)

        # 4. HTTP Load Benchmark
        print(f"[*] Running HTTP load test ({args.requests} requests, concurrency={args.concurrency})...")
        rust_bench = measure_throughput_and_latency("127.0.0.1", 18891, total=args.requests, concurrency=args.concurrency)
        go_bench = measure_throughput_and_latency("127.0.0.1", 18892, total=args.requests, concurrency=args.concurrency)
    finally:
        rust_proc.terminate()
        go_proc.terminate()
        rust_proc.wait()
        go_proc.wait()
        shutil.rmtree(rust_tmp, ignore_errors=True)
        shutil.rmtree(go_tmp, ignore_errors=True)

    # Calculate Deltas
    def delta_str(val_rust, val_go, higher_is_better=True):
        if val_go == 0:
            return "N/A"
        pct = ((val_rust - val_go) / val_go) * 100.0
        if higher_is_better:
            return f"**{pct:+.1f}%**" if pct > 0 else f"{pct:+.1f}%"
        else:
            return f"**{pct:+.1f}%**" if pct < 0 else f"{pct:+.1f}%"

    md = []
    md.append("# ⚡ Antigravity Quota Pool: Rust vs Go Performance Benchmark\n")
    md.append(f"> **Environment:** Linux ({os.uname().machine}) | **Total Requests:** {args.requests} | **Concurrency:** {args.concurrency}\n")
    md.append("### 📊 Comprehensive Scorecard\n")
    md.append("| Metric | Go Implementation | Rust Implementation | Delta (Rust vs Go) | Winner |")
    md.append("| :--- | :---: | :---: | :---: | :---: |")
    
    # Binary Size
    size_delta = delta_str(rust_size_mb, go_size_mb, higher_is_better=False)
    size_win = "🦀 Rust" if rust_size_mb <= go_size_mb else "🐹 Go"
    md.append(f"| **Binary Size** | {go_size_mb:.2f} MiB | {rust_size_mb:.2f} MiB | {size_delta} | {size_win} |")

    # Cold Start
    cs_delta = delta_str(rust_cold['median'], go_cold['median'], higher_is_better=False)
    cs_win = "🦀 Rust" if rust_cold['median'] <= go_cold['median'] else "🐹 Go"
    md.append(f"| **Cold Start (Median)** | {go_cold['median']:.2f} ms | {rust_cold['median']:.2f} ms | {cs_delta} | {cs_win} |")

    # RSS Memory
    rss_delta = delta_str(rust_mem['rss_mb'], go_mem['rss_mb'], higher_is_better=False)
    rss_win = "🦀 Rust" if rust_mem['rss_mb'] <= go_mem['rss_mb'] else "🐹 Go"
    md.append(f"| **Idle Memory (VmRSS)** | {go_mem['rss_mb']:.2f} MiB | {rust_mem['rss_mb']:.2f} MiB | {rss_delta} | {rss_win} |")

    # VmSize Virtual Memory
    vmsize_delta = delta_str(rust_mem['vmsize_mb'], go_mem['vmsize_mb'], higher_is_better=False)
    vmsize_win = "🦀 Rust" if rust_mem['vmsize_mb'] <= go_mem['vmsize_mb'] else "🐹 Go"
    md.append(f"| **Virtual Memory (VmSize)** | {go_mem['vmsize_mb']:.1f} MiB | {rust_mem['vmsize_mb']:.1f} MiB | {vmsize_delta} | {vmsize_win} |")

    # OS Threads
    threads_delta = delta_str(rust_mem['threads'], go_mem['threads'], higher_is_better=False)
    threads_win = "🦀 Rust" if rust_mem['threads'] <= go_mem['threads'] else "🐹 Go"
    md.append(f"| **OS Threads** | {go_mem['threads']} threads | {rust_mem['threads']} threads | {threads_delta} | {threads_win} |")

    # Throughput QPS
    qps_delta = delta_str(rust_bench['qps'], go_bench['qps'], higher_is_better=True)
    qps_win = "🦀 Rust" if rust_bench['qps'] >= go_bench['qps'] else "🐹 Go"
    md.append(f"| **Throughput (QPS)** | {go_bench['qps']:.1f} req/s | {rust_bench['qps']:.1f} req/s | {qps_delta} | {qps_win} |")

    # Median Latency
    p50_delta = delta_str(rust_bench['median_ms'], go_bench['median_ms'], higher_is_better=False)
    p50_win = "🦀 Rust" if rust_bench['median_ms'] <= go_bench['median_ms'] else "🐹 Go"
    md.append(f"| **Latency (Median / P50)** | {go_bench['median_ms']:.2f} ms | {rust_bench['median_ms']:.2f} ms | {p50_delta} | {p50_win} |")

    # P95 Latency
    p95_delta = delta_str(rust_bench['p95_ms'], go_bench['p95_ms'], higher_is_better=False)
    p95_win = "🦀 Rust" if rust_bench['p95_ms'] <= go_bench['p95_ms'] else "🐹 Go"
    md.append(f"| **Latency (P95)** | {go_bench['p95_ms']:.2f} ms | {rust_bench['p95_ms']:.2f} ms | {p95_delta} | {p95_win} |")

    # P99 Latency
    p99_delta = delta_str(rust_bench['p99_ms'], go_bench['p99_ms'], higher_is_better=False)
    p99_win = "🦀 Rust" if rust_bench['p99_ms'] <= go_bench['p99_ms'] else "🐹 Go"
    md.append(f"| **Latency (P99)** | {go_bench['p99_ms']:.2f} ms | {rust_bench['p99_ms']:.2f} ms | {p99_delta} | {p99_win} |")

    md_output = "\n".join(md)
    print("\n" + md_output)

    if args.markdown_file:
        with open(args.markdown_file, "w") as f:
            f.write(md_output + "\n")
        print(f"\n[✓] Markdown summary written to {args.markdown_file}")

if __name__ == "__main__":
    main()
