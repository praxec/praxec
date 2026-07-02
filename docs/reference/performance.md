# Performance

Benchmarks measure the cost of core operations — store writes and
audit emission — to track overhead and catch regressions.

## Machine spec

- CPU: AMD Ryzen AI 9 365 w/ Radeon 880M (8 cores / 16 threads)
- OS: Linux 6.6 (WSL2)
- Build: release profile (`cargo bench`)
- Criterion settings: 1s warmup, 3s measurement, 30 samples
- Date: 2026-05-13

Numbers from other hardware will differ — expect ~2× swing on
mobile/cloud CPUs. Treat them as ratios, not absolutes.

## Store create

| Backend | Lower 95% CI | Mean | Upper 95% CI |
|---------|--------------|------|--------------|
| in-memory | 525 ns | 547 ns | 567 ns |
| SQLite (in-memory) | 91.4 µs | 95.1 µs | 101 µs |

The in-memory store is a `RwLock<HashMap>`; the dominant cost is
hash + clone. SQLite is ~170× slower because every create flushes
through `prepare → execute → commit`. An in-memory SQLite is the
floor; on-disk SQLite adds fsync cost (≈100–500 µs typical, varies
by hardware).

## Audit emission

| Sink | Lower 95% CI | Mean | Upper 95% CI |
|------|--------------|------|--------------|
| null | 177 ns | 180 ns | 184 ns |
| memory | 22.0 µs | 34.7 µs | 48.4 µs |

The null sink measures the async dispatch overhead alone. The
memory sink wraps a `Mutex<Vec<AuditEvent>>` and clones the event
on each push; the wide range reflects allocator behavior under
load. For production, prefer the file sink with a buffered writer.

## Interpretation

- **In-memory baseline:** a submit (`praxec.command`) against the
  in-memory store + null audit sink touches ~700 ns of overhead beyond
  the executor itself. That's the irreducible floor.
- **SQLite baseline:** add ~95 µs per submit for the optimistic-lock
  write. For SLA-sensitive paths, the store is the dominant cost.
- **Audit cost:** the null sink is essentially free. The memory sink
  is 100× costlier due to allocation. File and stdout sinks should be
  in the same ballpark as memory once writes are buffered.

For typical workflows (3–5 transitions, 1–2 guards each), end-to-end
gateway overhead per submit (`praxec.command`) is comfortably under
1 ms on this hardware. The store write dominates.

## Running benchmarks

```bash
cargo bench --bench gateway_overhead
```

For quick local runs (lower statistical confidence, ~15 s total):

```bash
cargo bench --bench gateway_overhead -- --warm-up-time 1 --measurement-time 3 --sample-size 30
```

HTML reports land in `target/criterion/`.
