#!/usr/bin/env python3
"""Regression tests for release/dependency helper shell scripts.

These tests use tiny throwaway repositories and PATH stubs so they can verify
release-gate orchestration without compiling Rust or contacting crates.io.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parent.parent
REAL_PYTHON = sys.executable


def cargo_package_version(cargo_toml: str) -> str:
    in_package = False
    for raw_line in cargo_toml.splitlines():
        line = raw_line.strip()
        if line == "[package]":
            in_package = True
            continue
        if line.startswith("[") and line.endswith("]"):
            in_package = False
            continue
        if in_package and line.startswith("version"):
            key, _, value = line.partition("=")
            if key.strip() == "version":
                return value.strip().strip('"')
    raise AssertionError("Cargo.toml package.version is missing")


def write_executable(path: Path, text: str) -> None:
    path.write_text(textwrap.dedent(text).lstrip(), encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def run_checked(args: list[str], cwd: Path, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def file_hash(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class HelperScriptTests(unittest.TestCase):
    def make_stub_dir(self, log_path: Path, *, include_node: bool = True, include_cargo_audit: bool = True) -> Path:
        stub_dir = log_path.parent / "bin"
        stub_dir.mkdir(exist_ok=True)
        write_executable(
            stub_dir / "cargo",
            r"""
            #!/bin/bash
            set -euo pipefail
            printf 'cargo' >> "$LTERM_HELPER_TEST_LOG"
            printf '\t%s' "$@" >> "$LTERM_HELPER_TEST_LOG"
            printf '\n' >> "$LTERM_HELPER_TEST_LOG"
            if [[ "${1:-}" == "build" ]]; then
              target="${CARGO_TARGET_DIR:-target}"
              mkdir -p "$target/debug" "$target/release"
              : > "$target/debug/lterm"
              : > "$target/release/lterm"
            fi
            if [[ "${1:-}" == "update" && "${LTERM_DEP_STUB_MUTATE:-0}" == "1" ]]; then
              manifest=""
              while [[ $# -gt 0 ]]; do
                case "$1" in
                  --manifest-path) manifest="$2"; shift 2 ;;
                  *) shift ;;
                esac
              done
              if [[ -n "$manifest" ]]; then
                printf '\n# stub mutation\n' >> "$(dirname "$manifest")/Cargo.lock"
              fi
            fi
            exit 0
            """,
        )
        write_executable(
            stub_dir / "python3",
            f"""
            #!/bin/bash
            set -euo pipefail
            case "${{1:-}}" in
              scripts/*|*/scripts/*)
                printf 'python3' >> "$LTERM_HELPER_TEST_LOG"
                printf '\\t%s' "$@" >> "$LTERM_HELPER_TEST_LOG"
                printf '\\n' >> "$LTERM_HELPER_TEST_LOG"
                exit 0
                ;;
            esac
            exec {REAL_PYTHON!r} "$@"
            """,
        )
        if include_node:
            write_executable(
                stub_dir / "node",
                f"""
                #!/bin/bash
                set -euo pipefail
                printf 'node' >> "$LTERM_HELPER_TEST_LOG"
                printf '\\t%s' "$@" >> "$LTERM_HELPER_TEST_LOG"
                printf '\\n' >> "$LTERM_HELPER_TEST_LOG"
                if [[ "${{1:-}}" == "-e" ]]; then
                  exec {REAL_PYTHON!r} - "$3" <<'PY'
import json
import sys
print(json.load(open(sys.argv[1], encoding="utf-8"))["version"])
PY
                fi
                echo "unexpected node invocation: $*" >&2
                exit 2
                """,
            )
        if include_cargo_audit:
            write_executable(
                stub_dir / "cargo-audit",
                """
                #!/bin/bash
                exit 0
                """,
            )
        return stub_dir

    def make_release_fixture(self, root: Path, version: str = "1.0.1") -> Path:
        fixture = root / "repo"
        (fixture / "scripts").mkdir(parents=True)
        (fixture / "docs").mkdir()
        (fixture / "npm" / "platforms" / "lterm-test-platform").mkdir(parents=True)
        shutil.copy(REPO_ROOT / "scripts" / "release-preflight.sh", fixture / "scripts" / "release-preflight.sh")
        (fixture / "Cargo.toml").write_text(
            f'[package]\nname = "light-terminal"\nversion = "{version}"\n', encoding="utf-8"
        )
        (fixture / "package.json").write_text(json.dumps({"version": version}), encoding="utf-8")
        (fixture / "npm" / "platforms" / "lterm-test-platform" / "package.json").write_text(
            json.dumps({"version": version}), encoding="utf-8"
        )
        (fixture / "docs" / "contract-manifest.json").write_text(
            json.dumps({"release": f"lterm-{version}"}), encoding="utf-8"
        )
        return fixture

    def run_release_preflight(
        self,
        fixture: Path,
        args: list[str],
        *,
        include_node: bool = True,
        include_cargo_audit: bool = True,
    ) -> tuple[subprocess.CompletedProcess[str], str]:
        log_path = fixture.parent / "commands.log"
        stub_dir = self.make_stub_dir(log_path, include_node=include_node, include_cargo_audit=include_cargo_audit)
        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{stub_dir}{os.pathsep}{os.defpath}",
                "LTERM_HELPER_TEST_LOG": str(log_path),
                "CARGO_TARGET_DIR": str(fixture / "target"),
            }
        )
        proc = run_checked(["/bin/bash", "scripts/release-preflight.sh", *args], fixture, env)
        return proc, log_path.read_text(encoding="utf-8") if log_path.exists() else ""

    def test_release_preflight_contract_only_command_plan(self) -> None:
        with tempfile.TemporaryDirectory(prefix="lterm-release-preflight-test-") as tmp:
            fixture = self.make_release_fixture(Path(tmp))
            proc, log = self.run_release_preflight(fixture, ["--contract-only"], include_cargo_audit=False)
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            self.assertIn("cargo\tbuild\t--locked\n", log)
            self.assertNotIn("cargo\tfmt", log)
            self.assertNotIn("cargo\tclippy", log)
            self.assertNotIn("cargo\ttest\t--locked\t--test\tupgrade_matrix", log)
            self.assertNotIn("cargo\taudit", log)
            self.assertIn("python3\tscripts/validate_contract_manifest.py\tdocs/contract-manifest.json", log)
            self.assertIn("python3\tscripts/check_contract_drift.py\t--self-test", log)

    def test_release_preflight_full_mode_command_plan_and_audit_modes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="lterm-release-preflight-test-") as tmp:
            fixture = self.make_release_fixture(Path(tmp))
            proc, log = self.run_release_preflight(fixture, [])
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            for expected in [
                "cargo\tfmt\t--\t--check",
                "cargo\tclippy\t--locked\t--all-targets\t--\t-D\twarnings",
                "cargo\ttest\t--locked\t--\t--test-threads=1",
                "cargo\tbuild\t--release\t--locked",
                "cargo\ttest\t--locked\t--test\tupgrade_matrix\t--\t--include-ignored\t--nocapture\t--test-threads=1",
                "cargo\taudit",
            ]:
                self.assertIn(expected, log)

        with tempfile.TemporaryDirectory(prefix="lterm-release-preflight-test-") as tmp:
            fixture = self.make_release_fixture(Path(tmp))
            proc, log = self.run_release_preflight(fixture, ["--skip-audit"])
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            self.assertNotIn("cargo\taudit", log)

        with tempfile.TemporaryDirectory(prefix="lterm-release-preflight-test-") as tmp:
            fixture = self.make_release_fixture(Path(tmp))
            proc, _ = self.run_release_preflight(fixture, ["--require-audit"], include_cargo_audit=False)
            self.assertEqual(proc.returncode, 66, proc.stderr + proc.stdout)
            self.assertIn("cargo-audit required but not found", proc.stderr)

    def test_release_preflight_version_mismatches_exit_65(self) -> None:
        mismatch_cases = [
            ("package.json", lambda fixture: (fixture / "package.json").write_text(json.dumps({"version": "9.9.9"}))),
            (
                "npm/platforms",
                lambda fixture: (fixture / "npm" / "platforms" / "lterm-test-platform" / "package.json").write_text(
                    json.dumps({"version": "9.9.9"}), encoding="utf-8"
                ),
            ),
            (
                "contract-manifest",
                lambda fixture: (fixture / "docs" / "contract-manifest.json").write_text(
                    json.dumps({"release": "lterm-9.9.9"}), encoding="utf-8"
                ),
            ),
        ]
        for label, mutate in mismatch_cases:
            with self.subTest(label=label), tempfile.TemporaryDirectory(
                prefix="lterm-release-preflight-test-"
            ) as tmp:
                fixture = self.make_release_fixture(Path(tmp))
                mutate(fixture)
                proc, log = self.run_release_preflight(fixture, ["--contract-only"])
                self.assertEqual(proc.returncode, 65, proc.stderr + proc.stdout)
                self.assertNotIn("cargo\tbuild", log)

        with self.subTest(label="package.json without node"), tempfile.TemporaryDirectory(
            prefix="lterm-release-preflight-test-"
        ) as tmp:
            fixture = self.make_release_fixture(Path(tmp))
            (fixture / "package.json").write_text(json.dumps({"version": "9.9.9"}), encoding="utf-8")
            proc, log = self.run_release_preflight(fixture, ["--contract-only"], include_node=False)
            self.assertEqual(proc.returncode, 65, proc.stderr + proc.stdout)
            self.assertIn("Cargo.toml version", proc.stderr)
            self.assertNotIn("cargo\tbuild", log)

    def make_dependency_fixture(self, root: Path) -> Path:
        fixture = root / "repo"
        (fixture / "scripts").mkdir(parents=True)
        shutil.copy(
            REPO_ROOT / "scripts" / "dependency-minor-dry-run.sh",
            fixture / "scripts" / "dependency-minor-dry-run.sh",
        )
        (fixture / "Cargo.toml").write_text(
            '[package]\nname = "fixture"\nversion = "0.1.0"\n', encoding="utf-8"
        )
        (fixture / "Cargo.lock").write_text("version = 4\n", encoding="utf-8")
        subprocess.run(["git", "init", "-q"], cwd=fixture, check=True)
        subprocess.run(["git", "add", "."], cwd=fixture, check=True)
        subprocess.run(
            ["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "fixture"],
            cwd=fixture,
            check=True,
        )
        return fixture

    def run_dependency_dry_run(
        self, fixture: Path, args: list[str], *, mutate: bool
    ) -> tuple[subprocess.CompletedProcess[str], str]:
        log_path = fixture.parent / "commands.log"
        stub_dir = self.make_stub_dir(log_path, include_node=False, include_cargo_audit=False)
        env = os.environ.copy()
        env.update(
            {
                "PATH": f"{stub_dir}{os.pathsep}{os.environ['PATH']}",
                "LTERM_HELPER_TEST_LOG": str(log_path),
                "LTERM_DEP_STUB_MUTATE": "1" if mutate else "0",
            }
        )
        proc = run_checked(["/bin/bash", "scripts/dependency-minor-dry-run.sh", *args], fixture, env)
        return proc, log_path.read_text(encoding="utf-8") if log_path.exists() else ""

    def test_dependency_dry_run_isolated_and_forwards_packages(self) -> None:
        with tempfile.TemporaryDirectory(prefix="lterm-dep-dry-run-test-") as tmp:
            fixture = self.make_dependency_fixture(Path(tmp))
            before = file_hash(fixture / "Cargo.lock")
            proc, log = self.run_dependency_dry_run(
                fixture, ["--package", "serde_json", "-p", "tempfile"], mutate=True
            )
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            self.assertEqual(before, file_hash(fixture / "Cargo.lock"))
            self.assertIn("cargo\tupdate\t--manifest-path", log)
            self.assertIn("\t--package\tserde_json\t--package\ttempfile", log)
            self.assertIn("--- ", proc.stdout)
            self.assertIn("+++ ", proc.stdout)
            self.assertIn("# stub mutation", proc.stdout)

    def test_dependency_dry_run_no_change_and_option_errors(self) -> None:
        with tempfile.TemporaryDirectory(prefix="lterm-dep-dry-run-test-") as tmp:
            fixture = self.make_dependency_fixture(Path(tmp))
            proc, _ = self.run_dependency_dry_run(fixture, [], mutate=False)
            self.assertEqual(proc.returncode, 0, proc.stderr + proc.stdout)
            self.assertIn("No Cargo.lock changes from dry-run update.", proc.stdout)

            missing, _ = self.run_dependency_dry_run(fixture, ["--package"], mutate=False)
            self.assertEqual(missing.returncode, 64)
            self.assertIn("--package requires a value", missing.stderr)

            unknown, _ = self.run_dependency_dry_run(fixture, ["--unknown"], mutate=False)
            self.assertEqual(unknown.returncode, 64)
            self.assertIn("unknown option: --unknown", unknown.stderr)

    def test_package_manifest_keeps_documented_assets(self) -> None:
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))
        files = package.get("files")
        self.assertIsInstance(files, list)
        self.assertIn("docs/assets/lterm-demo.svg", files)
        for rel_path in files:
            path = REPO_ROOT / rel_path
            self.assertTrue(path.exists(), f"package files entry is missing: {rel_path}")

    def test_homebrew_formula_version_matches_package_manifests(self) -> None:
        version = cargo_package_version((REPO_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
        package = json.loads((REPO_ROOT / "package.json").read_text(encoding="utf-8"))
        self.assertEqual(package["version"], version)

        formula = (REPO_ROOT / "packaging" / "homebrew" / "lterm.rb").read_text(encoding="utf-8")
        self.assertIn(f"/refs/tags/v{version}.tar.gz", formula)
        self.assertIn(f'assert_match "lterm {version}"', formula)


if __name__ == "__main__":
    unittest.main(verbosity=2)
