# lterm MAT `CODEX_HOME` propagation report

Date: 2026-06-17 KST
Status: fixed in the current worktree; observed on the installed/released `lterm 1.0.25` behavior before this branch change. The package version has not been bumped by this fix.

## Summary

When `lterm omx --madmax` was launched from inside a MAT Codex profile session, the initial `lterm` client process saw MAT's isolated `CODEX_HOME`, but the `omx` / `codex` process spawned by the long-running lterm daemon did not.

Result: Codex used the default `~/.codex` home instead of the MAT session home, so account identity and quota appeared to belong to the default Codex account rather than the selected `ddig` profile.

## Observed released behavior

Command path:

```sh
mat session start codex ddig
lterm omx --madmax
```

Observed process chain:

```text
mat session start codex ddig
  /bin/zsh
    lterm omx --madmax
      lterm daemon
        omx --madmax
          codex ...
```

Observed environment on the affected run:

```text
PID 87590  lterm omx --madmax
  CODEX_HOME=/Users/jinhongan/.multi-account-tool/sessions/codex-ddig-f47cfcf1/CODEX_HOME

PID 87591  node .../omx --madmax
  CODEX_HOME missing

PID 87675  node /opt/homebrew/bin/codex ...
  CODEX_HOME missing

PID 87676  native codex binary ...
  CODEX_HOME missing
```

Non-secret identity fingerprints from local files showed MAT had created/copied the `ddig` profile home correctly:

```text
live ~/.codex:                  fp=e35abf9f65fc
mat profile codex/default:      fp=e35abf9f65fc
mat profile codex/ddiG:         fp=9da6a066ca89
current session ddig f47cfcf1:  fp=9da6a066ca89
```

So the issue was not missing MAT profile data; it was lterm's client→daemon session environment propagation.

## Verified root cause

The daemon spawns PTY children from request data, not from the short-lived caller shell. Before this fix, `src/client.rs::new_session` forwarded selected terminal/color/cmux variables, but did not copy `CODEX_HOME` from the launching client into `Request::New.env`.

The server-side path already applied sanitized request env to the child and did not reject ordinary `CODEX_HOME` values, so the missing link was client-side request assembly rather than daemon spawning or MAT profile setup.

## Fix applied

Implemented a narrow client-side allowlist:

- `src/client.rs`
  - added `inherit_client_session_home_env(&mut env)`;
  - allowlists only `CODEX_HOME`;
  - preserves explicit caller-supplied `env["CODEX_HOME"]`;
  - ignores missing or empty client `CODEX_HOME`;
  - makes no protocol shape change.
- `src/server.rs`
  - added sanitizer regression coverage proving `CODEX_HOME` remains an allowed ordinary child env key.
- `tests/cli_smoke.rs`
  - added a prestarted-daemon regression where the daemon is started with `CODEX_HOME` removed, then a second client creates a session with a sentinel `CODEX_HOME` and the child prints it;
  - added a reported-path regression using a fake hermetic `omx` executable so `lterm omx --raw --no-status -- --probe` proves the agent launcher receives the sentinel through the daemon hop.
- `README.md` / `README.ko.md`
  - documented that lterm forwards only this narrow Codex profile-home signal and does not broadly forward the caller environment.

This fix intentionally does **not** copy the whole client environment and does **not** add a broad CLI-home variable family.

## Verification evidence

Targeted checks:

```sh
cargo fmt -- --check
cargo test --bin lterm codex_home -- --nocapture
cargo test --bin lterm child_env_rejects_private_multiplexer_keys_but_allows_cmux_context -- --nocapture
cargo test --test cli_smoke codex_home -- --nocapture
```

Result: pass.

Full gate:

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test -- --test-threads=1
cargo build --release --locked
cargo audit
git diff --check
```

Result: pass. Log: `/tmp/lterm-codex-home-full-verify-serial-1781701448.log`.

Note: an earlier unqualified parallel `cargo test` run hit existing cmux-oriented smoke flakiness. The repository handoff already recommends serial testing for daemon/process coverage; the serial full suite above passed.

## Expected behavior after this fix

Launching from a MAT session:

```sh
mat session start codex ddig
lterm omx --madmax
```

should preserve the MAT session Codex home into the final agent process:

```text
CODEX_HOME=/Users/jinhongan/.multi-account-tool/sessions/<session-id>/CODEX_HOME
```

The final `omx` and `codex` process tree should show `CODEX_HOME` present unless an explicit lterm session env deliberately overrides it.

## Reproduction / inspection commands

From a shell:

```sh
mat session start codex ddig
```

Inside the MAT subshell:

```sh
echo "$CODEX_HOME"
lterm omx --madmax
```

From another terminal:

```sh
ps auxww | egrep -i '([m]at|[l]term|[o]mx|[c]odex)'

for p in <lterm_pid> <omx_pid> <codex_node_pid> <codex_native_pid>; do
  echo "--- PID $p"
  ps eww -p "$p" -o command= | tr ' ' '\n' | egrep '^(CODEX_HOME|HOME|LTERM|OMX|CODEX)='
done
```

Expected on the fixed branch: `CODEX_HOME` appears on the spawned `omx` / `codex` child environment, not only on the `lterm` client.

## Workaround for unfixed installed builds

Until using a build that contains this fix, avoid launching Codex through `lterm` when relying on MAT session isolation. Run OMX directly inside the MAT subshell:

```sh
mat session start codex ddig
omx --madmax
```

or use MAT's direct command-scoped runner for direct Codex invocation:

```sh
mat session run codex ddig -- [codex args...]
```

## Notes

This report intentionally avoids printing credential values. It uses only account fingerprints, environment variable presence/absence, code-path evidence, and test results.
