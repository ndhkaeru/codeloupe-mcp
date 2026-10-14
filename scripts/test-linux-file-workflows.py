#!/usr/bin/env python3
"""Execute the Linux-only ignored file workflow tests with enforced syscall failures."""

import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
IMAGE = "rust:1.98.1"
CROSS_DEVICE = "cross_device_move_when_shared_memory_is_available"
CHMOD = "existing_file_writers_succeed_when_chmod_is_denied"
EXCLUSIVE = "unix_exclusive_create_fallback_is_reported_by_all_new_file_writers"
EXPECTED = {CROSS_DEVICE, CHMOD, EXCLUSIVE}


def run(command, **kwargs):
    return subprocess.run(command, text=True, check=True, **kwargs)


def mount(source, target, readonly=False):
    value = "type=bind,source=" + str(source) + ",target=" + target
    return ["--mount", value + (",readonly" if readonly else "")]


def main():
    target = ROOT / "target" / "linux-file-workflows"
    cargo_cache = ROOT / "target" / "linux-file-workflows-cargo"
    target.mkdir(parents=True, exist_ok=True)
    cargo_cache.mkdir(parents=True, exist_ok=True)
    common = ["docker", "run", "--rm", "--init"]
    common += mount(ROOT, "/workspace", True) + mount(target, "/target")
    common += ["--workdir", "/workspace"]

    print("Build locked Linux integration tests in " + IMAGE, flush=True)
    command = common + mount(cargo_cache, "/cargo-cache")
    command += ["--env", "CARGO_HOME=/cargo-cache", "--env", "CARGO_TARGET_DIR=/target"]
    command += [IMAGE, "cargo", "test", "--release", "--locked", "--test",
                "test_file_workflows", "--no-run", "--message-format=json"]
    result = run(command, stdout=subprocess.PIPE)
    artifacts = []
    for line in result.stdout.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (item.get("reason") == "compiler-artifact"
                and item.get("target", {}).get("name") == "test_file_workflows"
                and item.get("profile", {}).get("test")
                and item.get("executable")):
            artifacts.append(item["executable"])
    if len(artifacts) != 1 or not artifacts[0].startswith("/target/release/deps/"):
        raise RuntimeError("Expected exactly one Linux integration-test executable")
    binary = artifacts[0]

    runtime = common + ["--read-only", "--network", "none",
                        "--tmpfs", "/tmp:rw,size=256m", "--shm-size", "64m",
                        "--env", "CODELOUPE_MCP_INDEX_MODE=off"]
    listed = run(runtime + [IMAGE, binary, "--ignored", "--list"], stdout=subprocess.PIPE)
    names = {line[:-6] for line in listed.stdout.splitlines() if line.endswith(": test")}
    if names != EXPECTED:
        raise RuntimeError("Ignored test coverage changed: " + repr(names))

    def execute(name, profile=None, flags=(), negative=False):
        command = list(runtime)
        if profile:
            profile_path = ROOT / "tests" / "fixtures" / "seccomp" / profile
            command += ["--security-opt", "seccomp=" + str(profile_path)]
        for flag in flags:
            command += ["--env", flag + "=1"]
        command += [IMAGE, binary, "--ignored", "--exact", name,
                    "--test-threads=1", "--nocapture"]
        label = ("negative control: " if negative else "enforced test: ") + name
        if profile:
            label += " [" + profile + "]"
        print("\n" + label, flush=True)
        result = subprocess.run(command, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT)
        print(result.stdout, flush=True)
        if "running 1 test" not in result.stdout:
            raise RuntimeError("Test filter did not execute exactly one test")
        if negative:
            if result.returncode == 0 or "test result: FAILED. 0 passed; 1 failed;" not in result.stdout:
                raise RuntimeError("Unrestricted syscall control did not fail as expected")
        elif result.returncode != 0 or "test result: ok. 1 passed; 0 failed;" not in result.stdout:
            raise RuntimeError("Enforced filesystem test failed")

    # Environment markers alone must not produce a false pass.
    execute(CHMOD, flags=("CODELOUPE_TEST_DENY_CHMOD",), negative=True)
    execute(EXCLUSIVE, flags=("CODELOUPE_TEST_EXCLUSIVE_CREATE",), negative=True)
    execute(CROSS_DEVICE)
    execute(CHMOD, "chmod-denied.json", ("CODELOUPE_TEST_DENY_CHMOD",))
    execute(EXCLUSIVE, "exclusive-create.json", ("CODELOUPE_TEST_EXCLUSIVE_CREATE",))
    combined = ("CODELOUPE_TEST_DENY_CHMOD", "CODELOUPE_TEST_EXCLUSIVE_CREATE")
    execute(CHMOD, "permissions-fallback.json", combined)
    execute(EXCLUSIVE, "permissions-fallback.json", combined)
    print("\nAll three ignored tests executed; combined permission/fallback cases and two negative controls passed.",
          flush=True)


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
