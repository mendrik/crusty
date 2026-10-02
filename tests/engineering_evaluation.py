#!/usr/bin/env python3
"""Independent engineering oracles; agent invocation and condition setup are host-owned.

prepare writes a baseline fixture. grade copies a candidate into a fresh directory,
adds held-out oracles, and records compiler/runtime outcomes without modifying it.
Conditions and model costs are declarations, never inferred quality measurements.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import tempfile


CASES = {
    "validated-port": {
        "prompt": "Replace the panicking port parser with a recoverable validated type. Accept trimmed decimal ports 1..65535. Invalid values must return errors. Expose get(), keep the representation private, and prevent direct unchecked construction. Preserve a small idiomatic public API.",
        "baseline": "pub struct Port(pub u16);\npub fn parse_port(s: &str) -> Port { Port(s.parse().unwrap()) }\n",
        "reference": "#[derive(Debug, PartialEq, Eq)]\npub enum PortError { Invalid, OutOfRange }\n#[derive(Debug)]\npub struct Port(u16);\nimpl Port { pub fn get(&self) -> u16 { self.0 } }\npub fn parse_port(s: &str) -> Result<Port, PortError> { let n: u16 = s.trim().parse().map_err(|_| PortError::Invalid)?; if n == 0 { Err(PortError::OutOfRange) } else { Ok(Port(n)) } }\n",
        "oracle": "use engineering_fixture::parse_port;\n#[test] fn ports() { for (s,n) in [(\" 80 \",80),(\"65535\",65535)] { assert_eq!(parse_port(s).unwrap().get(),n); } for s in [\"\",\"0\",\"65536\",\"-1\",\"abc\"] { assert!(parse_port(s).is_err(), \"{s}\"); } }\n",
        "negative": "fn main() { let _ = engineering_fixture::Port(0); }\n",
        "features": {},
        "profiles": [[]],
    },
    "owned-worker-shutdown": {
        "prompt": "Repair Worker ownership and shutdown. Worker::new(capacity) must use a bounded queue; try_submit(u8) reports backpressure or closure without blocking. Dropping an idle or used Worker must disconnect input before joining the worker, drain accepted jobs, and terminate. Keep lifecycle ownership explicit.",
        "baseline": "use std::{sync::mpsc,thread};\npub struct Worker { sender: mpsc::Sender<u8>, handle: Option<thread::JoinHandle<()>> }\nimpl Worker { pub fn new(_: usize) -> Self { let (sender, receiver) = mpsc::channel(); let handle = thread::spawn(move || { for _ in receiver {} }); Self { sender, handle: Some(handle) } } pub fn try_submit(&self, value: u8) -> Result<(), mpsc::SendError<u8>> { self.sender.send(value) } }\nimpl Drop for Worker { fn drop(&mut self) { if let Some(handle) = self.handle.take() { let _ = handle.join(); } } }\n",
        "reference": "use std::{sync::mpsc,thread};\npub struct Worker { sender: Option<mpsc::SyncSender<u8>>, handle: Option<thread::JoinHandle<()>> }\nimpl Worker { pub fn new(capacity: usize) -> Self { let (sender, receiver) = mpsc::sync_channel(capacity); let handle = thread::spawn(move || { for _ in receiver {} }); Self { sender: Some(sender), handle: Some(handle) } } pub fn try_submit(&self, value: u8) -> Result<(), mpsc::TrySendError<u8>> { self.sender.as_ref().ok_or(mpsc::TrySendError::Disconnected(value))?.try_send(value) } }\nimpl Drop for Worker { fn drop(&mut self) { drop(self.sender.take()); if let Some(handle) = self.handle.take() { let _ = handle.join(); } } }\n",
        "oracle": "use engineering_fixture::Worker;\nuse std::{sync::mpsc,thread,time::Duration};\n#[test] fn bounded_and_owned() { let zero = Worker::new(0); let result: Result<(), mpsc::TrySendError<u8>> = zero.try_submit(1); assert!(result.is_ok() || matches!(result,Err(mpsc::TrySendError::Full(1)))); drop(zero); for used in [false,true] { let (tx,rx) = mpsc::channel(); thread::spawn(move || { let w = Worker::new(2); if used { let _ = w.try_submit(3); } drop(w); tx.send(()).unwrap(); }); assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), \"drop did not terminate\"); } }\n",
        "features": {},
        "profiles": [[]],
    },
    "supported-feature-matrix": {
        "prompt": "Make this library support its declared profiles: default std, no default features with alloc, and no default features without an allocator. join(parts) concatenates strings when alloc exists. Do not force std or alloc into the allocation-free build. Preserve feature declarations.",
        "baseline": "#![cfg_attr(not(feature = \"std\"), no_std)]\npub fn join(parts: &[&str]) -> String { parts.concat() }\n",
        "reference": "#![cfg_attr(not(feature = \"std\"), no_std)]\n#[cfg(feature = \"alloc\")] extern crate alloc;\n#[cfg(feature = \"alloc\")] pub fn join(parts: &[&str]) -> alloc::string::String { parts.concat() }\n",
        "oracle": "#[cfg(feature = \"alloc\")] #[test] fn joins() { assert_eq!(engineering_fixture::join(&[\"a\",\"b\"]),\"ab\"); assert_eq!(engineering_fixture::join(&[]),\"\"); }\n",
        "features": {"default": ["std"], "std": ["alloc"], "alloc": []},
        "profiles": [[], ["--no-default-features", "--features", "alloc"], ["--no-default-features"]],
    },
}
CONDITIONS = ("bare", "skills", "crusty", "crusty-and-skills")


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def prepare(case_id, destination, condition, model):
    case = CASES[case_id]
    destination.mkdir(parents=True, exist_ok=False)
    features = "\n".join(f"{key} = {json.dumps(value)}" for key, value in case["features"].items())
    write(destination / "Cargo.toml", '[package]\nname = "engineering-fixture"\nversion = "0.1.0"\nedition = "2024"\n\n[features]\n' + features + "\n")
    write(destination / "src/lib.rs", case["baseline"])
    write(destination / "TASK.md", case["prompt"] + "\n")
    write(destination / "evaluation.json", json.dumps({"case": case_id, "condition": condition, "model": model, "condition_authority": "Declared by the invoking host; harness does not invoke an LLM or attach skills."}, indent=2))
    result = run(["cargo", "generate-lockfile", "--offline"], destination)
    if not result["passed"]:
        raise RuntimeError(result)


def run(command, directory):
    process = subprocess.Popen(command, cwd=directory, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=60)
        return {"command": command, "passed": process.returncode == 0, "exit_code": process.returncode, "stdout": stdout[-32000:], "stderr": stderr[-32000:]}
    except subprocess.TimeoutExpired as error:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        return {"command": command, "passed": False, "timeout": True, "error": str(error)}


def candidate_files(source):
    ignored = {"target", ".git", ".rust-repo-intelligence", "evaluation-results.json"}
    files = []
    for directory, dirs, names in os.walk(source, followlinks=False):
        dirs[:] = [name for name in dirs if name not in ignored]
        for name in dirs:
            if (Path(directory) / name).is_symlink():
                raise ValueError(f"unsupported candidate directory: {Path(directory) / name}")
        for name in names:
            path = Path(directory) / name
            if name in ignored:
                continue
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"unsupported candidate input: {path}")
            files.append(path.relative_to(source))
    if len(files) > 1000 or sum((source / path).stat().st_size for path in files) > 8_000_000:
        raise ValueError("candidate exceeds evaluation input budget")
    return sorted(files)


def fingerprint(source, files):
    digest = hashlib.sha256()
    for path in files:
        data = (source / path).read_bytes()
        digest.update(str(path).encode() + b"\0" + len(data).to_bytes(8, "little") + data)
    return digest.hexdigest()


def grade(source, tokens=None, seconds=None):
    metadata = json.loads((source / "evaluation.json").read_text())
    case = CASES[metadata["case"]]
    files = candidate_files(source)
    before = fingerprint(source, files)
    checks = []
    with tempfile.TemporaryDirectory(prefix="crusty-engineering-grade-") as scratch:
        directory = Path(scratch)
        for path in files:
            (directory / path).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source / path, directory / path)
        # Candidate oracles and Cargo configuration cannot weaken held-out checks.
        shutil.rmtree(directory / ".cargo", ignore_errors=True)
        shutil.rmtree(directory / "tests", ignore_errors=True)
        shutil.rmtree(directory / "examples", ignore_errors=True)
        (directory / "build.rs").unlink(missing_ok=True)
        manifest = '[package]\nname = "engineering-fixture"\nversion = "0.1.0"\nedition = "2024"\n\n[features]\n'
        manifest += "\n".join(f"{key} = {json.dumps(value)}" for key, value in case["features"].items())
        write(directory / "Cargo.toml", manifest + "\n")
        write(directory / "tests/oracle.rs", case["oracle"])
        checks.append(run(["cargo", "generate-lockfile", "--offline"], directory))
        for profile in case["profiles"]:
            for command in [["test", "--test", "oracle"], ["clippy", "--lib"]]:
                args = ["cargo", *command, "--locked", "--offline", *profile]
                if command[0] == "clippy":
                    args += ["--", "-D", "warnings"]
                checks.append(run(args, directory))
        if "negative" in case:
            write(directory / "examples/invariant_bypass.rs", case["negative"])
            negative = run(["cargo", "check", "--locked", "--offline", "--example", "invariant_bypass"], directory)
            negative["passed"] = not negative["passed"] and "private" in negative.get("stderr", "")
            negative["oracle"] = "Unchecked construction must fail because the representation is private."
            checks.append(negative)
    unchanged = files == candidate_files(source) and before == fingerprint(source, files)
    return {**metadata, "candidate_digest": before, "candidate_unchanged": unchanged, "passed": unchanged and all(check["passed"] for check in checks), "checks": checks, "reported_cost": {"tokens": tokens, "seconds": seconds, "authority": "Host-reported; unavailable values remain null."}, "limits": "Three focused dependency-free repositories; passing these oracles is not proof of universally good code. Agent outcomes require actual host runs for each declared condition."}


def self_test():
    with tempfile.TemporaryDirectory(prefix="crusty-engineering-oracles-") as scratch:
        for case_id, case in CASES.items():
            source = Path(scratch) / case_id
            prepare(case_id, source, "bare", "oracle self-test; no LLM invoked")
            baseline = grade(source)
            assert not baseline["passed"], (case_id, baseline)
            write(source / "src/lib.rs", case["reference"])
            reference = grade(source)
            assert reference["passed"], (case_id, reference)
            print(f"{case_id}: baseline fails; reference passes; candidate retained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    subcommands.add_parser("cases")
    subcommands.add_parser("self-test")
    setup = subcommands.add_parser("prepare")
    setup.add_argument("case", choices=CASES)
    setup.add_argument("destination", type=Path)
    setup.add_argument("--condition", choices=CONDITIONS, required=True)
    setup.add_argument("--model", required=True)
    check = subcommands.add_parser("grade")
    check.add_argument("source", type=Path)
    check.add_argument("--output", type=Path, required=True)
    check.add_argument("--tokens", type=int)
    check.add_argument("--seconds", type=float)
    args = parser.parse_args()
    if args.command == "cases":
        print(json.dumps({key: {"prompt": value["prompt"], "profiles": value["profiles"]} for key, value in CASES.items()}, indent=2))
    elif args.command == "self-test":
        self_test()
    elif args.command == "prepare":
        prepare(args.case, args.destination, args.condition, args.model)
    else:
        if (args.tokens is not None and args.tokens < 0) or (args.seconds is not None and not 0 <= args.seconds < float("inf")):
            parser.error("reported costs must be finite and nonnegative")
        write(args.output, json.dumps(grade(args.source.resolve(), args.tokens, args.seconds), indent=2) + "\n")


if __name__ == "__main__":
    main()
