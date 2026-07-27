# gauntlet

An adversarial test harness for the SVG thumbnail provider DLL.

Where `Testing/unload-harness` targets one specific known bug (the dllhost.exe crash from
unloading the DLL while a thread still cached live Direct2D-on-WARP state), the
gauntlet tries to break the provider in every *other* way: malformed input,
hostile callers, concurrency, and resource retention.

Like the unload harness, it deliberately does **not** link against the
`win_svg_thumbs` crate. It loads the built DLL with `LoadLibraryW` and goes in
through `DllGetClassObject` / `IClassFactory` / `IInitializeWithStream` /
`IThumbnailProvider`, exactly as Explorer's thumbnail surrogate does, so the
tests exercise the shipping binary rather than a statically linked copy of the
sources.

## Running it

```powershell
cargo build --release -p win_svg_thumbs   # the DLL under test
cargo build --release -p gauntlet

# everything
./target/release/gauntlet.exe run

# one suite
./target/release/gauntlet.exe run --suite adversarial

# list the suites
./target/release/gauntlet.exe list
```

Useful flags:

| Flag | Meaning |
|---|---|
| `--dll PATH` | DLL to test (defaults to the provider matching the harness's own architecture, under `target/release/`) |
| `--suite NAME` | Run only this suite; repeatable |
| `--seed N` | Fix the RNG seed to replay a previous run exactly |
| `--scale N` | Multiply the workload; `1` is the per-commit default |
| `--corpus DIR` | Real-world SVG corpus for the `breadth` suite |
| `--only CASE` | With `exec`, run a single named case in isolation |

## How failures are reported

`run` is a supervisor: each suite runs in its own child process under a
watchdog. That is what makes the gauntlet safe to run on every push.

* A suite that **faults** takes down only itself; the rest of the run continues.
* A suite that **deadlocks** is killed at a known timeout and reported as a hang
  rather than consuming the whole CI job.
* Every child writes a heartbeat naming the case it is about to attempt, so a
  crash or hang is attributed to **one specific input** instead of "somewhere in
  the adversarial suite". The supervisor prints a ready-to-paste replay command.
* Suites that feed bombs and oversized inputs cap their own committed memory with
  a job object, so a decompression bomb fails the test instead of the machine.

Exit codes: `0` pass, `1` a check failed, `10` the suite could not run
(inconclusive, treated as failure so a silently skipped suite cannot look green),
`101` a panic in the harness, `124` watchdog kill, and a large negative value for
an unhandled Windows exception.

## Suites

| Suite | What it attacks |
|---|---|
| `api-misuse` | COM contract abuse: null pointers into raw vtable entries, wrong CLSIDs, aggregation, double `Initialize`, `GetThumbnail` before `Initialize`, out-of-order release, `LockServer` imbalance, thumbnail sizes from `0` to `u32::MAX` |
| `stream-faults` | Hostile `IStream`s: `Stat` that fails or lies (zero, `u64::MAX`, over/under the size cap), one-byte and short reads, mid-stream failure, zero-bytes-forever, byte counts larger than the buffer, `S_FALSE` partial reads |
| `render` | Correctness: size sweep 1–4096, byte-for-byte determinism, no cross-render contamination, alpha un-premultiplication, CSS precedence and `!important` handling, viewBox synthesis and scaling, fallback detection |
| `adversarial` | ~200 generated malformed SVGs: truncation at many offsets, BOM/UTF-16/invalid UTF-8/embedded NULs, 50k-deep nesting, huge attributes, CSS brace-matcher torture around the parser's 256 work-stack cap, numeric extremes (`NaN`, `Infinity`, `1e308`), recursive `<use>`, and XXE (external DTDs and entities, UNC paths, remote hrefs) against a live loopback listener that must never be contacted |
| `svgz` | Compressed input: truncated headers and bodies, bad CRC and ISIZE, corrupt DEFLATE, concatenated members, trailing garbage, and decompression bombs that expand to hundreds of megabytes |
| `size-limits` | Inputs one byte either side of the documented 101 MiB cap |
| `lifecycle` | Randomized, reproducible COM lifecycle sequences across STA and MTA threads, plus cross-thread object handoff; asserts `DllCanUnloadNow` never says `S_OK` while objects are live |
| `concurrency` | Many threads rendering at once, verifying nobody receives another thread's image; threads exiting while others render |
| `churn` | Measures the deliberate per-thread D2D cache leak. Stable pool (no thread exits) must be flat; thread churn reports retained bytes per completed rendering thread against a documented budget |
| `breadth` | A pinned, cached slice of `microsoft/fluentui-system-icons` (MIT, downloaded for testing only, never vendored), each file also round-tripped through gzip |

## Architectures

The gauntlet loads the provider with `LoadLibraryW`, so **the harness and the DLL
must be the same architecture** — a 64-bit process cannot load the 32-bit
provider. CI therefore builds `gauntlet.exe` once per target alongside the DLL,
and `--dll` defaults to the provider matching the harness's own architecture.

| Lane | Binaries | Host | Why it matters |
|---|---|---|---|
| `x64` | x64 | `windows-latest` | Primary. Runs the full per-suite matrix. |
| `x86 (WOW64)` | x86 | `windows-latest` | The x64 MSI installs this too — 32-bit applications hosting the shell's thumbnail path load the 32-bit provider. It is also the only configuration where `usize` is 32 bits, so the overflow guards around pitch and buffer-size arithmetic in the bitmap copy are load-bearing here and nowhere else. |
| `arm64 (native)` | arm64 | `windows-11-arm` | Native ARM64 on real ARM hardware, rather than cross-compiled and never run. |
| `x64 on ARM (emulated)` | x64 | `windows-11-arm` | A real end-user configuration: a 64-bit x86 shell host on an ARM PC, going through Windows' x64-on-ARM emulation. |

The x64 lane splits by suite so a failure points at one job. The other three run
every suite in a single job — the question they answer is "does this build behave
the same everywhere", not "which suite failed" — and the supervisor still
isolates each suite in its own watchdogged child process, so one crashing suite
leaves the rest of the lane's results intact.

## Measured baseline: what a rendering thread retains

`src/lib.rs` deliberately never destroys a thread's cached Direct2D-on-WARP
chain, because destroying it would run under the loader lock — the cause of the
original dllhost.exe crash. The code comment justifies the leak with "threads
that render thumbnails ... in practice live until process exit, so nothing
meaningful accumulates."

The `churn` suite measures that instead of assuming it. On a `windows-latest`
runner (WARP, x64):

| Completed one-shot rendering threads | Private bytes retained | Per thread |
|---|---|---|
| 1 | 1.07 MiB | 1.07 MiB |
| 9 | 12.07 MiB | 1.34 MiB |
| 73 | 106.68 MiB | 1.46 MiB |
| 329 | 482.97 MiB | 1.47 MiB |

Growth is almost exactly linear (slope ratio 1.00 between the 73- and 329-thread
samples) at roughly **1.47 MiB and 24 kernel handles per exited rendering
thread**, with GDI objects and loaded modules both completely flat.

So the design holds — the cost is bounded and predictable, not a runaway leak —
but it is not free, and it now has a number attached. A host that recycled
rendering threads aggressively rather than pooling them would pay about 1.5 GiB
per thousand threads. The suite fails if the per-thread figure exceeds 12 MiB or
if growth turns superlinear, so a regression in either direction is caught.

## There is no allowlist

Any failed check fails the build. There is deliberately no mechanism for
marking a finding as "known" and letting the run stay green.

A suite that goes green while a real defect exists is worse than no suite at
all: it teaches you that green means nothing, so nobody reads the log, so the
next genuine regression sails through. If a finding is not worth going red for,
the honest fix is to delete the check — not to hide its result.

A run does not stop at the first failure. Every suite executes every check,
every suite runs regardless of what the previous one did, and the results are
accumulated; the non-zero exit happens at the end. So a red run gives you the
complete picture in one go, not just the first thing that broke.

## Open findings (these currently fail the build)

Three defects, all demonstrated by named checks and none of them fixed:

| Check | Defect |
|---|---|
| `stream-faults/over_reported_read_leaks_thread_graphics_cache` | `Initialize` trusts the byte count an `IStream` reports, so an over-reporting stream panics the read loop; `ffi_guard`'s recovery then abandons the thread's D2D device chain (~28 handles per occurrence) |
| `api-misuse/unbalanced_unlock_corrupts_reference_count` | `LockServer(FALSE)` decrements `DLL_REFERENCES` without a floor, so an unmatched unlock lets `DllCanUnloadNow` return `S_OK` while providers are live |
| `render/css_important_{uppercase,mixedcase}_is_stripped` | `!important` is detected case-insensitively but stripped case-sensitively, so `!IMPORTANT` survives into the document and Direct2D drops the declaration |

**See [FINDINGS.md](FINDINGS.md)** for the full write-up: exact line references,
how each was found, measured costs, suggested fixes, and the constraints a fix
must not break (in particular, the `ManuallyDrop` TLS cache and the module pin
are the v1.11.0 crash fix and must stay).
