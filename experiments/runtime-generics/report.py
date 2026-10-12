#!/usr/bin/env python3
"""Produce a standalone, interactive report from the recorded experiment outputs."""
import argparse
import html
import json
from pathlib import Path
import statistics

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("directory", type=Path)
args = parser.parse_args()
root = args.directory
results = root / "results"
if (root / "final-results").exists():
    results = root / "final-results"
rows = json.loads((results / "measurements.json").read_text())
semantic = json.loads((root / "semantic/summary.json").read_text())
protocol = json.loads((results / "protocol.json").read_text())
medians = []
for case in dict.fromkeys(r["case"] for r in rows if r["group"] == "timing"):
    for mode in dict.fromkeys(r["mode"] for r in rows):
        samples = [r for r in rows if r["group"] == "timing" and r["case"] == case and r["mode"] == mode]
        if not samples:
            continue
        costs = {k: statistics.median(r["costs"][k] for r in samples) for k in samples[0]["costs"]}
        costs["execute_and_promote_ns"] = statistics.median(
            sum(r["costs"][k] for k in ("execute_ns", "specialization_ns", "verify_ns", "jit_ns")) for r in samples)
        medians.append(dict(case=case, mode=mode, costs=costs,
                            calls=samples[0]["rounds"] * samples[0]["active"],
                            wall_ns=statistics.median(r["wall_ns"] for r in samples),
                            rss_kib=statistics.median(r["max_rss_kib"] for r in samples)))
(root / "summary.json").write_text(json.dumps(medians, indent=2) + "\n")
payload = json.dumps(medians).replace("<", "\\u003c")
page = """<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1"><title>MIRVM generic execution prototype</title>
<style>body{font:16px/1.6 system-ui;max-width:1120px;margin:40px auto;padding:0 24px;background:#fafbf9;color:#152c2a}h1{line-height:1.15}h2{margin-top:2em}select,input{font:inherit;padding:6px;margin:5px}table{border-collapse:collapse;width:100%;font-variant-numeric:tabular-nums}td,th{padding:8px 12px;border-bottom:1px solid #cad8d4;text-align:right}td:first-child,th:first-child{text-align:left}.bar{height:17px;background:#338b72;border-radius:3px;min-width:1px}.note{background:#eef2df;padding:16px;border-left:4px solid #899e4c}code{font-size:.9em}a{color:#17654f}.controls{display:flex;flex-wrap:wrap;gap:16px}small{color:#42615b}</style>
<h1>Shared generic execution<br>and hot specialization</h1>
<p>A bounded real-MIR exporter plus an execution experiment using MIRVM's existing interpreter,
verifier and JIT. The production execution path is unchanged.</p>
<p class="note"><b>Decision:</b> proceed with staged integration. Shared code reduces synthetic
IR preparation, and repeated loops can repay specialization. Tiny calls and native witness work
lose under the same promotion policy. These results do not establish a whole-program startup gain.</p>
<h2>Measured execution and preparation</h2>
<div class="controls"><label>Workload <select id="case"></select></label><label>Metric
<select id="metric"><option value="execute_and_promote_ns">Execution + all promotion</option>
<option value="execute_ns">Execution only</option><option value="bodies_ns">Initial body construction</option>
<option value="first_result_ns">First result, excluding audit</option>
<option value="initial_ir_bytes">Initial serialized IR bytes</option></select></label></div>
<p id="description"></p><table><thead><tr><th>Mode</th><th>Median</th><th style="width:30%">Relative scale</th><th>JIT ms</th><th>Peak RSS MiB</th></tr></thead><tbody id="rows"></tbody></table>
<p><small>Three alternating release repetitions. Execution + promotion is the median of each
sample's sum, not a sum of medians. Timed execution includes the raw export activation boundary.
Native rows are meaningful only for execution, because the harness also builds an unused Engine.
Whole-process timings and all individual costs remain in the raw JSON.</small></p>
<h2>How many later calls would repay compilation?</h2>
<p>This is a sensitivity calculation using measured shared-interpreter and eager-JIT execution
per call, not an implemented prediction policy. It assumes future calls have the same workload.</p>
<label>Later calls <input id="calls" type="number" min="0" max="1000000" value="256"></label>
<p id="payback"></p>
<h2>Actual generic MIR handoff</h2><p>EXPORT</p>
<p>One generic instruction stream serves the five layouts/const lengths below. Each execution mode
runs in a fresh process after the exporting compiler has finished. The const-array executor passes
zero as its runtime length. The native result and owned descriptor must agree.</p>
<table><thead><tr><th>Size</th><th>Alignment</th><th>Const length</th></tr></thead><tbody>LAYOUTS</tbody></table>
<p>An invalid instantiation in an unentered branch is rejected before artifact publication.
Unsupported MIR is also rejected. Production integration still needs eager fallback.</p>
<h2>Coverage and limits</h2><ul>
<li>Thirty synthetic differential runs: mixed layouts, zero-sized/aligned values, const lengths,
native Clone/Drop cleanup and reentry, empty input, and a single long invocation.</li>
<li>Six real-MIR execution differentials, three exports, and two rejection checks.</li>
<li>Ninety timing samples. Every sample checks return values and copies, or Clone/Drop counts.</li>
<li>Clone/Drop uses native C-unwind witnesses; guest panic ownership and general trait resolution
are not established. The real exporter admits an opaque-pointee copy loop only.</li>
<li>Promotion is serial, between calls. The single long invocation promotes zero instances despite
crossing the work threshold; this demonstrates the missing OSR path.</li>
<li>Sparse preparation uses 4,096 nominal entries with equal descriptors. Existing canonical
fragments deduplicate eager code; transient pointer immediates in research adapters are not a
production storage design. No proportional disk saving is claimed.</li>
<li>The runtime hooks are confined to an isolated source copy. Product storage, concurrent
publication, stable native function pointers, fork and trace domains remain integration gates.</li></ul>
<h2>Reproducible evidence</h2><p><a href="RESULTS/measurements.json">Raw timings</a> ·
<a href="RESULTS/protocol.json">Protocol and binary hash</a> ·
<a href="semantic/summary.json">MIR/semantic checks</a> ·
<a href="semantic/export-0.json">Owned template and descriptors</a> ·
<a href="evidence.tar.gz">Raw evidence archive</a></p>
<p><small>Binary SHA-256: HASH</small></p>
<script>const data=PAYLOAD;
const descriptions={"sparse-preparation":"4,096 synthetic entries; one used once. Equal descriptors isolate instruction construction; they do not represent 4,096 independent Rust layouts.","short-calls":"64 entries, one active; 2,048 one-element calls. Work threshold: 64 elements.","copy-loop":"64 entries, one active; 256 calls × 256 elements. Work threshold: 1,024 elements.","clone-loop":"64 entries, one active; 64 calls × 32 native Clone/Drop witnesses. Work threshold: 128 elements.","dense-cold":"64 entries all active; two calls × 32 elements per entry. Work threshold: 1,024 elements."};
const c=document.querySelector('#case'),m=document.querySelector('#metric');
for(const name of [...new Set(data.map(x=>x.case))]){const o=document.createElement('option');o.value=name;o.textContent=name;c.append(o)}
c.value='copy-loop';
function render(){const selected=data.filter(x=>x.case===c.value);const bytes=m.value.endsWith('bytes');const max=Math.max(...selected.map(x=>x.costs[m.value]));document.querySelector('#description').textContent=descriptions[c.value];
document.querySelector('#rows').innerHTML=selected.map(x=>`<tr><td>${x.mode}</td><td>${(x.costs[m.value]/(bytes?1:1e6)).toFixed(bytes?0:3)} ${bytes?'B':'ms'}</td><td><div class="bar" style="width:${100*x.costs[m.value]/max}%"></div></td><td>${(x.costs.jit_ns/1e6).toFixed(3)}</td><td>${(x.rss_kib/1024).toFixed(1)}</td></tr>`).join('');
const shared=selected.find(x=>x.mode==='shared'),hot=selected.find(x=>x.mode==='eager-jit'),tiered=selected.find(x=>x.mode==='tiered');
const saved=(shared.costs.execute_ns-hot.costs.execute_ns)/shared.calls;const price=hot.costs.jit_ns+ tiered.costs.specialization_ns+tiered.costs.verify_ns;
const calls=Math.max(0,Number(document.querySelector('#calls').value)||0);
document.querySelector('#payback').textContent=saved>0?`Measured saving per call: ${(saved/1e3).toFixed(2)} µs. Transition cost estimate: ${(price/1e6).toFixed(3)} ms. Break-even: ${Math.ceil(price/saved)} later calls; at ${calls} calls, net ${( (calls*saved-price)/1e6).toFixed(3)} ms saved. A negative value means compilation costs more than it saves.`:'This sample has no positive execution saving from which to infer payback.';}
c.onchange=render;m.onchange=render;document.querySelector('#calls').oninput=render;render();</script></html>"""
export = {k: statistics.median(r[k] for r in semantic["exports"]) / 1000
          for k in ("export_ns", "descriptors_ns", "validation_ns")}
page = page.replace("EXPORT", f"Median template export: {export['export_ns']:.1f} µs; five descriptors: {export['descriptors_ns']:.1f} µs; preceding mono validation/collection: {export['validation_ns']:.1f} µs. Export can reuse warmed semantic queries; these are not independent cold-query costs.")
page = page.replace("LAYOUTS", "".join(f"<tr><td>{size}</td><td>{align}</td><td>{length}</td></tr>" for size, align, length in semantic["layouts"]))
page = page.replace("RESULTS", results.name).replace("HASH", html.escape(protocol["binary_sha256"])).replace("PAYLOAD", payload)
(root / "index.html").write_text(page)
print(root / "index.html")
