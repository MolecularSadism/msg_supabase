# Benchmark baselines

Criterion baseline `base`, captured 2026-08-26.

| | |
|---|---|
| Commit | `4823e483b6ba` |
| Branch | `claude/delta-sync` |
| Toolchain | rustc 1.98.0 (88d9e12ae 2026-08-18) |
| Host | Linux x86_64 container (shared/virtualised) |

## Results

Mean with 95% confidence interval.

| Benchmark | Mean | 95% CI |
|---|---:|---|
| `take_batch/3x1000_pending` | 1.652 µs | 1.635 µs – 1.672 µs |
| `take_batch/steady_state_none` | 71.87 ns | 71.58 ns – 72.18 ns |

## Reproducing

```sh
cargo bench -- --save-baseline base   # capture
cargo bench -- --baseline base        # compare against it
```

These were taken in a shared virtualised container, so absolute figures carry
more run-to-run noise than a dedicated machine. Comparisons made with
`--baseline base` on the same host are meaningful; comparing these absolute
numbers against a different machine is not.
