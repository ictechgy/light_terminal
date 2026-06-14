# Footprint benchmark baseline

`lterm` includes an opt-in, dependency-free benchmark harness for collecting a
local baseline against `tmux`:

```bash
python3 scripts/footprint_benchmark.py --quick \
  --json target/footprint-baseline.json \
  --markdown target/footprint-baseline.md

python3 scripts/footprint_benchmark.py \
  --iterations 5 \
  --sessions 8 \
  --workflow-iterations 3 \
  --lterm-bin target/release/lterm \
  --tmux-bin tmux \
  --json target/footprint-baseline.json \
  --markdown target/footprint-baseline.md
```

The JSON report is the source of truth. Markdown is derived from the same report
for quick release-note or PR summaries.

## What it measures

The harness uses isolated temporary state for each sample and records raw samples,
summary statistics, metadata, skip reasons, and cleanup evidence.

| Metric | `lterm` method | `tmux` method |
| --- | --- | --- |
| `daemon_ready_ms` | Spawn `lterm daemon` in isolated `LTERM_RUNTIME_DIR`/`LTERM_DATA_DIR`, then poll `lterm doctor --json` until `daemon_reachable=true`. | Start an isolated `tmux -L <name>` server and probe `display-message -p '#{pid}'`. If the server cannot expose a PID/readiness probe, the metric is skipped. |
| `first_session_ms` | Cold detached `lterm start -d` in isolated state. | Cold `tmux -L <name> new-session -d`. |
| `rss_kib` | `ps -o rss= -p <pid>` against the owned `lterm daemon` process. | `ps -o rss= -p <pid>` against the probed isolated tmux server PID. |
| `omx_style_workflow_ms` | Bounded `lterm tmux-compat` sequence: create controller, detached helper pane, list panes, send marker, capture, cleanup. | Equivalent isolated `tmux -L <name>` command sequence. |

The workflow never launches real AI tools (`omx`, `codex`, `claude`, `gemini`,
etc.) and does not use networked workloads. It is meant to approximate the local
tmux-compatible orchestration shape, not model a full agent session.

## Isolation and cleanup

- `lterm` samples set fresh `LTERM_RUNTIME_DIR` and `LTERM_DATA_DIR` values.
- `tmux` samples use unique `-L` server names.
- Cleanup is registered immediately after starting a daemon/server/session.
- Graceful cleanup (`lterm shutdown`, `tmux kill-server`, pane/session kills) runs
  before forceful process termination.
- Use `--keep-temp` to preserve temporary roots when diagnosing a failed run.

The harness must not touch a user's live `lterm` daemon or default `tmux` server.
If cleanup fails, the JSON report records that failure instead of hiding it.

## Interpreting results

Performance and RSS numbers are host-dependent. Compare reports only when their
metadata is comparable: machine, OS, Python version, binary paths, build profile,
iteration counts, session counts, and timeout settings. The benchmark is not a CI
pass/fail gate and should not be used to promise universal superiority numbers.

Expected skip cases:

- `tmux` is not installed: the `tmux` lane is `skipped`, while `lterm` can still run.
- PID/RSS probing is unavailable: latency metrics remain valid and RSS fields are
  `null` with an `rss_reason`.
- A tmux readiness probe is unsupported on a host: only that metric is skipped.

## JSON shape

Top-level keys are stable for automation:

```json
{
  "schema_version": 1,
  "generated_at_unix_secs": 1780000000,
  "config": { "iterations": 1, "sessions": 2, "workflow_iterations": 1 },
  "environment": { "platform": "...", "rss_command": "ps -o rss= -p <pid>" },
  "tools": { "lterm": { "available": true }, "tmux": { "available": false } },
  "results": {
    "lterm": {
      "status": "ok",
      "metrics": {
        "daemon_ready_ms": { "status": "ok", "samples": [12.3] },
        "first_session_ms": { "status": "ok", "samples": [15.6] },
        "rss_kib": { "idle_daemon": { "rss_kib": 1234 } },
        "omx_style_workflow_ms": { "status": "ok", "iterations": [] }
      }
    },
    "tmux": { "status": "skipped" }
  }
}
```

`daemon_ready_ms` and `first_session_ms` are intentionally separate; avoid rolling
them into a vague `startup_ms` metric.
