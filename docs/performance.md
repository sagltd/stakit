# stakit-model — Performance

Benchmarked with [divan](https://docs.rs/divan) (`cargo bench -p stakit-model`,
release profile: `opt-level=3`, thin LTO, `codegen-units=1`). Construction is
excluded from timing via `Bencher::with_inputs(...).bench_refs(...)`.

## Model under test

```rust
#[derive(Model)]
struct User {
    #[garde(length(min = 3, max = 20))] name: String,
    #[garde(email)]                     email: String,
    #[garde(range(min = 18, max = 120))] age: u8,
    #[garde(url)]                       website: String,
}
```

## Results

| Benchmark          | fastest | median  | mean¹   | what it measures |
|--------------------|---------|---------|---------|------------------|
| `validate_valid`   | 833 ns  | 875 ns  | ~4 µs¹  | all rules run to completion (real email + URL parse) |
| `validate_invalid` | 500 ns  | 583 ns  | 693 ns  | error-aggregation path (rules fail fast) |
| `generate_ts`      | 93 ns   | 99 ns   | 106 ns  | full `export interface` string build |

¹ The `validate_valid` *mean* is skewed by a one-off outlier (first iteration
pays `url`/`regex` lazy-static init). **Median (875 ns) is representative**; the
mean is not.

## Analysis

- **Validation is sub-microsecond.** A 4-field model with length, email, range,
  and URL rules validates in ~0.9 µs on the happy path — ample for request-time
  use in an API layer.
- **Valid > invalid.** The valid path is *slower* than the invalid one: a real
  email + a real `https://…` URL are fully parsed, whereas the invalid inputs
  (`"not-an-email"`, `"nope"`) are rejected early. URL parsing dominates the
  valid path.
- **Zero-overhead dispatch.** `#[derive(Model)]` emits a hand-written
  `impl garde::Validate` that calls garde's rule functions directly — no
  reflection, no boxing, no per-field allocation beyond what a failing rule's
  message needs. The error path only allocates when something actually fails.
- **TS generation is cheap** (~99 ns) and allocation-bound (building the
  interface string); it is a build/codegen-time concern, not a hot path.

## Reproduce

```bash
cargo bench -p stakit-model
```
