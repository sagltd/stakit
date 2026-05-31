# stakit-model — Performance

Benchmarked with [divan](https://docs.rs/divan) (`cargo bench -p stakit-model`,
release: `opt-level=3`, thin LTO, `codegen-units=1`). Construction is excluded
from timing via `Bencher::with_inputs(...).bench_refs(...)`.

## Model under test

```rust
#[derive(Model)]
struct User {
    #[validate(min_len = 3, max_len = 20)] name: String,
    #[validate(email)]                      email: String,
    #[validate(min = 18, max = 120)]        age: u8,
    bio: Option<String>,
}
```

## New (inlined) vs legacy (garde) — same struct, same machine

| Path     | legacy `garde` derive | **stakit `#[derive(Model)]`** | speedup |
|----------|----------------------:|------------------------------:|--------:|
| valid    | 791 ns (~1.26 M/s)    | **16.2 ns (~62 M/s)**         | **~49×** |
| invalid  | 875 ns (~1.14 M/s)    | **207 ns (~4.8 M/s)**         | ~4×     |
| `generate_ts` | —                | 99 ns                         | —       |

(Legacy numbers from the earlier garde-derive POC; new numbers are the shipped
inlined validator wired through the `Model`/`Validate` trait.)

## Why it's fast

- **Direct inlined branches.** The derive emits one direct call per rule to an
  `#[inline]` `validate::*` function — no per-rule trait dispatch, no `Report`
  builder, no `Path` machinery. With LTO these inline into straight-line code.
- **Allocation-free happy path.** On success no error is constructed, the
  backing `Vec` never allocates, and `validate()` returns a pointer-thin
  `Result`. Verified: 16 ns with zero heap traffic.
- **`Vec` over `SmallVec` — measured, not assumed.** A `SmallVec` inline buffer
  *bloats the success `Result`* (it's memcpy'd on every return): inline-8 = 29 ns,
  inline-4 = 22 ns, `Vec` = **16 ns**. `SmallVec` only avoids a heap alloc on the
  *error* path, which is exceptional. The hot path wins with `Vec`.

## Error / nested paths

The error path (invalid input) allocates one `Vec` plus a `String` per failing
field, and builds dotted/indexed paths (`tags[0].name`, `rows[0][home].n`) as
errors bubble up through cascading container `Validate` impls. At ~207 ns for a
3-error struct it is still ~4× faster than garde and far off the hot path.

## Validations per second

~**62 million/sec per core** on the happy path for this 4-field model; stateless,
so it scales ~linearly across cores.

## Reproduce

```bash
cargo bench -p stakit-model
```
