#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///
"""kperf-compare — A/B compare VM *instructions retired* between two baml-cli builds.

Quantifies the static overhead of the event/tracing system: pack the SAME workload
with each build, run each under the in-VM kperf probe, and diff the instruction
counts. Run with tracing OFF (the default) to isolate the always-present cost.
Sweep several workloads at once to see how the saving tracks call density.

How it works
------------
* `bex_vm`'s kperf probe is always compiled in; it's gated only by `BAML_KPERF=1`
  at runtime and needs PMC access (=> sudo) on Apple Silicon. No special build.
* The probe prints a summary to STDERR at process exit, e.g.
      [kperf] exec calls=123  VM ops=0
      [kperf]   cycles=2.500e9  instructions=3.100e9  IPC=1.240
  Instructions retired is near-deterministic (unlike cycles), so it's the metric
  we compare. NOTE: it's printed as `{:.3}e9`, i.e. ~1e6 resolution — pick heavy
  workloads so the delta clears that floor (rows under it are flagged).

Typical use (Apple Silicon, under sudo)
---------------------------------------
    # build both CLIs first, as your normal user (NOT under sudo). To also get the
    # exact "VM ops" counter (the un-confounded work metric), add the kperf feature
    # at the WORKSPACE ROOT — bex_vm is transitive, so `-p` rejects the feature:
    #   (canary wt)    cargo build --release --features bex_vm/kperf
    #   (trim-events)  cargo build --release --features bex_vm/kperf
    # (without the feature you still get instructions retired, just no VM-ops table)
    sudo uv run scripts/kperf_compare.py \
        --a ../canary-bench/baml_language/target/release/baml-cli --a-label canary \
        --b target/release/baml-cli                               --b-label trim-events \
        --workload-dir tools/speedtest/workloads/compute \
        --runs 7

`--a` is the baseline (events present), `--b` the contender (events stripped); the
script reports how many instructions `--b` saves vs `--a`, per workload and overall.
Pass `--workload FILE` (repeatable) and/or `--workload-dir DIR` (globs *.md/*.baml).
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path
from string import Template

INSTR_RE = re.compile(r"instructions=([0-9.]+e[0-9]+)")
CYCLES_RE = re.compile(r"cycles=([0-9.]+e[0-9]+)")
CALLS_RE = re.compile(r"exec calls=(\d+)")
OPS_RE = re.compile(r"VM ops=(\d+)")
RES_FLOOR = 1e6  # kperf prints {:.3}e9 => ~1e6 print resolution


class _DDTemplate(Template):
    # Workload .md files template with $$var (single $ is literal).
    delimiter = "$$"


class WorkloadError(Exception):
    """A single workload could not be prepared/packed — skip it, don't abort."""


def eprint(*a):
    print(*a, file=sys.stderr)


def die(msg: str, code: int = 1):
    eprint(f"ERROR: {msg}")
    sys.exit(code)


def extract_baml(workload: Path) -> str:
    """Return BAML source from a `.baml` file or the `## BAML` section of a `.md`."""
    text = workload.read_text()
    if workload.suffix == ".baml":
        return text
    # Minimal mirror of tools/speedtest loader: optional `## eval-setup` python
    # block defines vars, then `$$var` substitution into the `## BAML` block.
    sections: dict[str, str] = {}
    for m in re.finditer(r"^##\s+([\w-]+)\s*\n```\w*\n(.*?)```", text, re.M | re.S):
        sections[m.group(1).lower()] = m.group(2)
    baml = sections.get("baml")
    if baml is None:
        raise WorkloadError(f"no `## BAML` section in {workload.name}")
    setup = sections.get("eval-setup")
    if setup:
        ns: dict = {}
        try:
            exec(setup, {"__builtins__": __builtins__}, ns)  # trusted repo file
        except Exception as e:  # noqa: BLE001
            raise WorkloadError(f"eval-setup failed in {workload.name}: {e}") from e
        baml = _DDTemplate(baml).safe_substitute(ns)
    return baml


def pack(cli: Path, baml_src: Path, out: Path, entry: str, env: dict) -> None:
    cmd = [str(cli), "pack", entry, "--file", str(baml_src), "-o", str(out)]
    r = subprocess.run(cmd, capture_output=True, text=True, env=env)
    if r.returncode != 0:
        raise WorkloadError(
            f"pack failed ({cli.name}): {r.stderr.strip() or r.stdout.strip()}"
        )
    if not out.exists():
        raise WorkloadError(f"pack produced no binary at {out}")
    out.chmod(0o755)


def run_once(binary: Path, extra_args: list[str], env: dict) -> dict:
    """Run the packed binary once with kperf enabled; parse the stderr summary."""
    r = subprocess.run(
        [str(binary), *extra_args], capture_output=True, text=True, env=env
    )
    se = r.stderr
    if r.returncode != 0:
        raise WorkloadError(f"run failed (exit {r.returncode}): {se.strip()}")
    if "need sudo" in se or "kpc_force_all_ctrs_set failed" in se:
        die("kperf could not access PMCs — run this whole script under `sudo`.")
    m = INSTR_RE.search(se)
    if not m:
        die(
            "no kperf summary found. Confirm: Apple Silicon, under sudo, and a normal\n"
            "release build (bex_vm's kperf probe is always compiled in).\n"
            f"  stderr was:\n{se.strip()}"
        )
    out = {"instructions": float(m.group(1))}
    if (c := CYCLES_RE.search(se)):
        out["cycles"] = float(c.group(1))
    if (cl := CALLS_RE.search(se)):
        out["exec_calls"] = int(cl.group(1))
    if (op := OPS_RE.search(se)):
        out["vm_ops"] = int(op.group(1))
    return out


def measure(binary: Path, runs: int, warmup: int, extra: list[str], env: dict) -> dict:
    for _ in range(warmup):
        run_once(binary, extra, env)
    samples = [run_once(binary, extra, env) for _ in range(runs)]
    instrs = [s["instructions"] for s in samples]
    cycs = [s["cycles"] for s in samples if "cycles" in s] or [0.0]
    return {
        "instructions": instrs,
        "median": statistics.median(instrs),
        "stdev": statistics.pstdev(instrs) if len(instrs) > 1 else 0.0,
        "cyc_median": statistics.median(cycs),
        "cyc_stdev": statistics.pstdev(cycs) if len(cycs) > 1 else 0.0,
        "exec_calls": samples[0].get("exec_calls"),
        "vm_ops": samples[0].get("vm_ops"),
    }


def fmt_e9(x: float) -> str:
    return f"{x / 1e9:.4f}e9"


def collect_workloads(args) -> list[Path]:
    files: list[Path] = []
    for w in args.workload or []:
        p = Path(w).resolve()
        if not p.is_file():
            die(f"workload not found: {p}")
        files.append(p)
    if args.workload_dir:
        d = Path(args.workload_dir).resolve()
        if not d.is_dir():
            die(f"--workload-dir is not a directory: {d}")
        for ext in ("*.md", "*.baml"):
            files.extend(sorted(d.rglob(ext)))
    seen: set[Path] = set()
    out: list[Path] = []
    for f in files:
        if f not in seen:
            seen.add(f)
            out.append(f)
    return out


def main() -> None:
    p = argparse.ArgumentParser(
        description="Compare VM instructions retired between two baml builds (kperf).",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="Run under sudo on Apple Silicon. See the module docstring for examples.",
    )
    p.add_argument("--a", required=True, help="baseline: baml-cli (or packed bin with --prepacked)")
    p.add_argument("--b", required=True, help="contender: baml-cli (or packed bin with --prepacked)")
    p.add_argument("--a-label", default="A")
    p.add_argument("--b-label", default="B")
    p.add_argument("--workload", action="append", help="workload .md/.baml (repeatable)")
    p.add_argument("--workload-dir", help="directory to sweep (globs *.md and *.baml)")
    p.add_argument("--entry", default="main", help="entry function to pack (default: main)")
    p.add_argument("--runs", type=int, default=7, help="measured runs per build (default: 7)")
    p.add_argument("--warmup", type=int, default=1, help="discarded warmup runs (default: 1)")
    p.add_argument("--prepacked", action="store_true",
                   help="treat --a/--b as already-packed binaries for ONE workload; skip packing")
    p.add_argument("--args", default="", help="extra args passed to the packed binary")
    p.add_argument("--keep", action="store_true", help="keep temp packed binaries")
    p.add_argument("--verbose", action="store_true", help="print per-run instruction counts")
    args = p.parse_args()

    a_path, b_path = Path(args.a).resolve(), Path(args.b).resolve()
    for label, pth in ((args.a_label, a_path), (args.b_label, b_path)):
        if not pth.is_file():
            die(f"{label}: not a file: {pth}")

    extra = args.args.split() if args.args else []
    # pack step must NOT see BAML_KPERF (packing also runs the VM); runs must.
    pack_env = {k: v for k, v in os.environ.items() if k != "BAML_KPERF"}
    run_env = {**os.environ, "BAML_KPERF": "1"}

    if args.prepacked:
        workloads = [None]  # single synthetic job
        if args.workload or args.workload_dir:
            eprint("note: --prepacked ignores --workload/--workload-dir")
    else:
        workloads = collect_workloads(args)
        if not workloads:
            die("no workloads given — use --workload FILE and/or --workload-dir DIR")

    tmp = Path(tempfile.mkdtemp(prefix="kperf-cmp-"))
    rows: list[dict] = []
    skipped: list[tuple[str, str]] = []
    try:
        for i, wl in enumerate(workloads):
            name = "(pre-packed)" if wl is None else wl.stem
            try:
                if args.prepacked:
                    a_bin, b_bin = a_path, b_path
                else:
                    baml_src = tmp / f"wl{i}.baml"
                    baml_src.write_text(extract_baml(wl))
                    a_bin, b_bin = tmp / f"a{i}.bin", tmp / f"b{i}.bin"
                    pack(a_path, baml_src, a_bin, args.entry, pack_env)
                    pack(b_path, baml_src, b_bin, args.entry, pack_env)
                eprint(f"[{i+1}/{len(workloads)}] {name}: measuring "
                       f"{args.runs}+{args.warmup} runs x2 ...")
                a = measure(a_bin, args.runs, args.warmup, extra, run_env)
                b = measure(b_bin, args.runs, args.warmup, extra, run_env)
            except WorkloadError as e:
                eprint(f"  skipped {name}: {e}")
                skipped.append((name, str(e)))
                continue
            if args.verbose:
                eprint(f"  {args.a_label}: " + ", ".join(fmt_e9(v) for v in a["instructions"]))
                eprint(f"  {args.b_label}: " + ", ".join(fmt_e9(v) for v in b["instructions"]))
            rows.append({"name": name, "a": a, "b": b,
                         "saved": a["median"] - b["median"]})
    finally:
        if args.keep:
            eprint(f"kept temp dir: {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)

    if not rows:
        die("no workloads produced a measurement")

    # ── report ──────────────────────────────────────────────────────────────
    al, bl = args.a_label, args.b_label
    wcol = max(20, min(34, max(len(r["name"]) for r in rows) + 1))
    ncol, scol = 14, 9
    width = wcol + ncol * 3 + scol

    def emit_table(metric: str, val, std, with_floor: bool):
        """Print one comparison table. `val(measure)->float`, `std(measure)->float`.
        Returns (tot_a, tot_b, floor_names, var_names)."""
        print()
        print(f"metric: {metric};  {bl} vs baseline {al}")
        print("-" * width)
        print(f"{'workload':<{wcol}}{al[:ncol-1]:>{ncol}}{bl[:ncol-1]:>{ncol}}"
              f"{'saved':>{ncol}}{'Δ%':>{scol}}")
        print("-" * width)
        floor, var = [], []
        for r in rows:
            a, b = r["a"], r["b"]
            va, vb = val(a), val(b)
            saved = va - vb
            pct = (saved / va * 100) if va else 0.0
            flag = ""
            if with_floor and abs(saved) < 2 * RES_FLOOR:
                flag += "*"
                floor.append(r["name"])
            if max(std(a), std(b)) > 0.01 * max(va, vb, 1):
                flag += "~"
                var.append(r["name"])
            nm = r["name"] if len(r["name"]) < wcol else r["name"][:wcol - 2] + "…"
            print(f"{nm:<{wcol}}{fmt_e9(va):>{ncol}}{fmt_e9(vb):>{ncol}}"
                  f"{fmt_e9(saved):>{ncol}}{pct:>{scol-1}.2f}%{flag}")
        print("-" * width)
        ta, tb = sum(val(r["a"]) for r in rows), sum(val(r["b"]) for r in rows)
        tpct = ((ta - tb) / ta * 100) if ta else 0.0
        print(f"{'TOTAL (' + str(len(rows)) + ' workloads)':<{wcol}}"
              f"{fmt_e9(ta):>{ncol}}{fmt_e9(tb):>{ncol}}"
              f"{fmt_e9(ta - tb):>{ncol}}{tpct:>{scol-1}.2f}%")
        pcts = [((val(r["a"]) - val(r["b"])) / val(r["a"]) * 100) if val(r["a"]) else 0.0
                for r in rows]
        print(f"per-workload Δ%: min {min(pcts):+.2f}  median {statistics.median(pcts):+.2f}  "
              f"max {max(pcts):+.2f}")
        return ta, tb, floor, var

    ti_a, ti_b, notes_floor, notes_var = emit_table(
        f"VM instructions retired (median of {args.runs} runs)",
        lambda m: m["median"], lambda m: m["stdev"], True)
    tc_a, tc_b, _, notes_cvar = emit_table(
        f"CPU cycles (median of {args.runs} runs)",
        lambda m: m["cyc_median"], lambda m: m["cyc_stdev"], False)

    has_ops = bool(rows[0]["a"].get("vm_ops"))
    to_a = to_b = 0.0
    if has_ops:
        to_a, to_b, _, _ = emit_table(
            "VM ops dispatched (exact, deterministic)",
            lambda m: float(m["vm_ops"] or 0), lambda m: 0.0, False)

    # ── per-op / IPC summary (the stall lens) ────────────────────────────────
    di = ((ti_a - ti_b) / ti_a * 100) if ti_a else 0.0   # + => bl fewer instrs
    dc = ((tc_a - tc_b) / tc_a * 100) if tc_a else 0.0    # + => bl fewer cycles
    ipc_a = ti_a / tc_a if tc_a else 0.0
    ipc_b = ti_b / tc_b if tc_b else 0.0
    print()
    print("per-op / IPC (volume-weighted totals" +
          (f", VM ops={fmt_e9(to_a)})" if has_ops else ")") + ":")
    if has_ops and to_a:
        print(f"  {'':<{wcol-2}}{'instr/op':>12}{'cyc/op':>12}{'IPC':>10}")
        print(f"  {al:<{wcol-2}}{ti_a/to_a:>12.2f}{tc_a/to_a:>12.2f}{ipc_a:>10.3f}")
        print(f"  {bl:<{wcol-2}}{ti_b/to_b:>12.2f}{tc_b/to_b:>12.2f}{ipc_b:>10.3f}")
    else:
        print(f"  {al}: IPC {ipc_a:.3f}   {bl}: IPC {ipc_b:.3f}")
    print(f"  Δ instructions {di:+.2f}%   Δ cycles {dc:+.2f}%   (positive = {bl} better)")

    print()
    if args.runs < 3:
        print("⚠  cycles are non-deterministic — run with --runs 5+ before trusting the cycle/")
        print("   IPC verdict below (instructions are deterministic, so those are fine at 1 run).")
    if abs(dc - di) < 0.5:
        print(f"→ cycles track instructions (IPC {ipc_a:.3f} vs {ipc_b:.3f}): the delta is the")
        print("  retired instructions themselves (codegen), not new stalls.")
    elif dc < di:
        print(f"→ {bl} loses MORE on cycles than instructions (IPC {ipc_a:.3f}→{ipc_b:.3f}): the")
        print("  cost is STALLS — branch mispredicts / I-cache from the reshuffled dispatch")
        print("  layout — not extra work. Confirm with Instruments 'CPU Counters'")
        print("  (BRANCH_MISPRED_NONSPEC, L1I) on the two packed binaries.")
    elif di < -0.5 and dc > 0.5:
        # sign flip: more instructions, yet FEWER cycles → genuinely faster.
        print(f"→ {bl} retires MORE instructions yet runs in FEWER cycles "
              f"(IPC {ipc_a:.3f}→{ipc_b:.3f}): instruction count is MISLEADING — on cycles (the")
        print(f"  real speed metric) {bl} is ~{dc:.1f}% FASTER. The leaner dispatch loop schedules")
        print("  with more ILP, so the extra instructions sit off the critical path.")
    else:
        print(f"→ {bl} gains more on cycles than instructions (IPC {ipc_a:.3f}→{ipc_b:.3f}): the")
        print("  instruction delta overstates the cost — extra insns are ~free on this wide core.")

    if has_ops and to_a == to_b:
        print(f"  (VM ops identical ⇒ all of the above is instructions/op + cyc/op, i.e. pure")
        print(f"   codegen — zero difference in VM work.)")
    elif not has_ops:
        print("  (build both with `--features bex_vm/kperf` for the VM-ops / per-op columns.)")

    if notes_floor:
        print(f"\n*  Δ within kperf's ~1e6 print resolution → treat as noise: "
              f"{', '.join(notes_floor)}")
    if notes_var or notes_cvar:
        bad = sorted(set(notes_var) | set(notes_cvar))
        print(f"~  per-run variance >1% (expected for cycles; bump --runs): {', '.join(bad)}")
    if skipped:
        print(f"\nskipped {len(skipped)} workload(s): "
              f"{', '.join(n for n, _ in skipped)}")


if __name__ == "__main__":
    main()
