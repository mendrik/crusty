# Evaluate engineering outcomes

`tests/engineering_evaluation.py` creates three dependency-free Rust repositories with independent behavioral oracles: validated ports and private construction, bounded worker ownership/shutdown, and std/alloc/allocation-free supported profiles. The harness self-test proves each supplied broken baseline fails and each reference passes. It does not invoke an LLM and does not report agent improvements without actual agent runs.

```bash
python3 tests/engineering_evaluation.py cases
python3 tests/engineering_evaluation.py self-test
python3 tests/engineering_evaluation.py prepare validated-port /tmp/port-crusty \
  --condition crusty --model '<actual model/version>'
```

Have the host run the task in the generated `TASK.md` using the declared condition: `bare`, `skills`, `crusty`, or `crusty-and-skills`. For skill conditions record the exact versions of cleanup/Rust/domain skills. For Crusty conditions connect the tested executable to that fixture and use its current guidance and evidence tools. Keep model, settings, task and budgets comparable; perform repeated runs rather than selecting one success.

```bash
python3 tests/engineering_evaluation.py grade /tmp/port-crusty \
  --output /tmp/port-crusty-results.json --tokens 1200 --seconds 45
```

Grade copies candidate files into a fresh workspace, restores the declared Cargo/feature contract, injects immutable oracles, and runs actual tests and Clippy for each declared profile. A compile-fail oracle checks private port construction. Candidate tests/configuration/build scripts cannot weaken held-out checks. Candidate files remain untouched; the result records their digest, stability, commands, diagnostics and pass/fail. Costs are optional host-reported values, remaining null when unavailable. The illustrative cost flags above are examples, not observed measurements.

The harness assumes cooperative task execution and an installed local Rust toolchain; it is not an adversarial sandbox. Oracles are public, so production evaluation should reserve additional unseen cases and inspect solutions for overfitting. The initial corpus is intentionally focused, not proof of general superiority. Real outcomes, unsafe/concurrency stress, allocation benchmarks and larger project migrations should extend the corpus before claiming the skills can be retired.
