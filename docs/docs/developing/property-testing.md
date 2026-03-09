---
sidebar_position: 4
---

# Property testing

Commit-Boost uses [`proptest`](https://docs.rs/proptest/latest/proptest/) for invariant-driven tests in `cb-common` and `cb-pbs`.

## Deterministic runs

- Fast PR profile:
  - `just test-prop-fast`
  - Uses `PROPTEST_CASES=64` and `PROPTEST_RNG_SEED=20260305`
- Heavy nightly profile:
  - `just test-prop-heavy`
  - Uses `PROPTEST_CASES=1024` and `PROPTEST_RNG_SEED=20260305`

You can override these locally:

```bash
PROPTEST_CASES=256 PROPTEST_RNG_SEED=12345 cargo test -p cb-common -p cb-pbs --all-features prop_
```

## Replaying failures

When a property test fails, `proptest` prints the minimized case and reproduction hints.

Use the same seed and test filter to replay quickly:

```bash
PROPTEST_CASES=64 PROPTEST_RNG_SEED=<seed-from-failure> cargo test -p cb-pbs --all-features <failing_test_name> -- --nocapture
```
