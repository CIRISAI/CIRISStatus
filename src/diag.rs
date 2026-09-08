//! Allocator and memory diagnostics.
//!
//! # Why this exists
//!
//! CIRISStatus#69: the node holds ~1.1GB of anonymous heap that does not come
//! down across restarts, and a 96-minute soak showed a **plateau** — the floor
//! did not move a megabyte across 1.6 retention cycles. So nothing is
//! accumulating; the question is what the gigabyte IS.
//!
//! Reading the arenas out of `/proc/<pid>/mem` cannot answer that, and the
//! attempt to is what made this module necessary: **glibc does not zero on
//! `free()`**. A freed chunk keeps its contents byte-for-byte until something
//! reuses it, so a scan finding 183,000 copies of this node's own `key_id` is
//! equally consistent with a live working set and with freed-but-unreturned
//! churn. Those have opposite fixes — the first wants less retention, the
//! second wants `MALLOC_ARENA_MAX` or `malloc_trim` — and no measurement from
//! outside the process can tell them apart.
//!
//! `mallinfo2` can, because the allocator knows: `uordblks` is what is live,
//! `fordblks` is what it is holding onto after a free. That call has to happen
//! INSIDE the process, which is the whole reason this is code rather than a
//! shell one-liner.

use serde_json::json;

/// A snapshot of what the allocator and the kernel each think this process is
/// using. Fields are bytes unless named otherwise.
pub fn memory_report() -> serde_json::Value {
    let mut out = json!({ "proc": proc_status() });
    #[cfg(target_env = "gnu")]
    {
        // SAFETY: `mallinfo2` reads glibc's own accounting and takes no
        // arguments. It walks arena bookkeeping under the allocator's locks,
        // so it is safe to call from any thread; it returns a plain value
        // struct with no pointers to free.
        let m = unsafe { libc::mallinfo2() };
        out["mallinfo2"] = json!({
            // Live: what the program asked for and has not freed.
            "uordblks": m.uordblks as u64,
            // Free-but-held: returned to the allocator, still owned by the
            // process. A large value here is fragmentation/churn, NOT a leak,
            // and is what an arena cap or a trim would reclaim.
            "fordblks": m.fordblks as u64,
            // Total non-mmapped space obtained from the OS (sbrk).
            "arena": m.arena as u64,
            // Space in mmapped regions — untouched by malloc_trim.
            "hblkhd": m.hblkhd as u64,
            "hblks": m.hblks as u64,
            // Releasable at the TOP OF THE MAIN ARENA. Read it as that and
            // nothing more: since glibc 2.8 `malloc_trim` also walks every
            // arena's free bins and `MADV_DONTNEED`s whole free pages inside
            // them, so `keepcost` does NOT bound what a trim would return.
            // Treating it as a bound is how this comment first concluded, on
            // this node's 104KB, that a trim would reclaim nothing — while
            // `fordblks` sat at 591MB of exactly the page-aligned free chunks
            // `mtrim` targets (CIRISStatus#69).
            "keepcost": m.keepcost as u64,
            "ordblks": m.ordblks as u64,
        });
        let live = m.uordblks as f64;
        let held = m.fordblks as f64;
        let total = live + held;
        if total > 0.0 {
            // The one number this endpoint exists to produce, as a FRACTION of
            // 1 — near 1 means the heap is a live working set and the fix is to
            // retain less; near 0 means the heap is mostly the allocator's
            // free lists and the fix is an allocator one (arena cap, trim).
            out["live_fraction"] = json!((live / total * 10_000.0).round() / 10_000.0);
        }
    }
    #[cfg(not(target_env = "gnu"))]
    {
        out["mallinfo2"] = json!(null);
        out["note"] = json!("mallinfo2 is glibc-only; this build is not gnu");
    }
    out
}

/// The kernel's view, for correlation: a plateau in `RssAnon` with a large
/// `fordblks` is the churn story, and the two numbers disagreeing is itself
/// informative.
fn proc_status() -> serde_json::Value {
    let mut o = serde_json::Map::new();
    if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
        for line in s.lines() {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            if matches!(
                k,
                "VmRSS" | "RssAnon" | "RssFile" | "VmSwap" | "VmPeak" | "VmSize" | "Threads"
            ) {
                // Values arrive as "  1151234 kB"; keep the kB unit rather than
                // converting, so a reader comparing against /proc directly sees
                // the same number.
                o.insert(k.to_string(), json!(v.trim().to_string()));
            }
        }
    }
    serde_json::Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The endpoint's whole purpose is the live-vs-held split, so the test
    /// asserts the two numbers are present and that the derived fraction
    /// agrees with them — a report that silently lost `fordblks` would look
    /// perfectly healthy while answering the wrong question.
    #[test]
    fn the_report_carries_live_and_held_separately() {
        let r = memory_report();
        assert!(r.get("proc").is_some(), "kernel view present: {r}");

        #[cfg(target_env = "gnu")]
        {
            let m = &r["mallinfo2"];
            let live = m["uordblks"].as_u64().expect("uordblks");
            let held = m["fordblks"].as_u64().expect("fordblks");
            assert!(live > 0, "this test allocated, so something is live");
            let frac = r["live_fraction"].as_f64().expect("live_fraction");
            assert!(
                (0.0..=1.0).contains(&frac),
                "live_fraction is a fraction of 1, got {frac}"
            );
            let expect = live as f64 / (live + held) as f64;
            assert!(
                (frac - expect).abs() < 0.001,
                "live_fraction {frac} should track uordblks/(uordblks+fordblks) {expect}"
            );
        }
    }

    /// A held allocation must move the in-use figure. This is the sanity check
    /// that the numbers are this process's and not a constant.
    ///
    /// Two glibc facts shape it, and the first version of this test tripped on
    /// both — ciris-server hit the identical flake in its copy of this module
    /// and fixed it in 0.5.200; this is that fix, because the two nodes are
    /// deliberately one instrument and a test that is flaky in one of them is
    /// flaky in both.
    ///
    /// A block past the mmap threshold is NOT in `uordblks` — it is mmapped and
    /// counted in `hblkhd`. And in a test binary this size, other threads free
    /// arena memory in the same millisecond, so `uordblks` alone can FALL while
    /// this thread holds its block. So: 64 MiB (past the 32 MiB ceiling of
    /// glibc's dynamic mmap threshold, hence always mmapped), and the in-use
    /// figure is `uordblks + hblkhd`, which only an mmapped free of tens of MiB
    /// elsewhere could pull back down; half the block is the slack for that.
    #[cfg(target_env = "gnu")]
    #[test]
    fn live_bytes_track_a_real_allocation() {
        const BLOCK: usize = 64 * 1024 * 1024;
        fn in_use() -> u64 {
            let m = &memory_report()["mallinfo2"];
            m["uordblks"].as_u64().unwrap() + m["hblkhd"].as_u64().unwrap()
        }
        let before = in_use();
        // Touched so it cannot be optimised away and the pages are real.
        let mut v: Vec<u8> = vec![7; BLOCK];
        v[BLOCK / 2] = 9;
        std::hint::black_box(&v);
        let during = in_use();
        assert!(
            during >= before + (BLOCK as u64) / 2,
            "in-use bytes (uordblks + hblkhd) should rise by ~64 MiB while the block is held: \
             before={before} during={during}"
        );
        drop(v);
    }
}

/// Cap glibc's per-arena free lists, unless the operator has already said
/// otherwise.
///
/// # The measurement this encodes
///
/// A 205-minute A/B on the US node, `v0.3.61` against the same build with
/// `MALLOC_ARENA_MAX=2` (CIRISStatus#69):
///
/// ```text
///                  default      arena cap     delta
///   committed      1458 MB      611 MB        -847 MB (-58%)
///   live           44.3 MB      44.5 MB       unchanged
///   swap           ~336 MB      37 MB         -299 MB
///   64MB regions   5-8          1
/// ```
///
/// **Live memory is identical across both arms.** Same work, same data, so the
/// entire 847MB was allocator free-list held per arena — not a working set, not
/// a leak (the floor did not move in 3.4 hours). glibc grows arenas on
/// allocator lock CONTENTION, so a node with a handful of threads and a busy
/// poll loop accumulates one 64MB region per contended thread and keeps them.
///
/// The arena cap is the lever that was MEASURED here; it is not the only one,
/// and an earlier version of this comment ruled out the other on bad grounds.
/// It read `keepcost` (104KB) as the ceiling on what `malloc_trim` could
/// return. That is wrong: since glibc 2.8 `mtrim` walks every arena's free bins
/// and `MADV_DONTNEED`s whole free pages within them, so it reaches exactly the
/// fragmented free lists `keepcost` says nothing about — which on this node
/// still hold ~591MB even WITH the cap applied.
///
/// So trim remains untested here rather than ruled out, and it is not free: the
/// pages it returns fault back in on reuse, which is a real cost for a workload
/// whose problem is churn. It wants an A/B like the cap got, not adoption on
/// the strength of a number that turned out to measure something else.
///
/// # Why in the binary and not only in compose
///
/// Because a deployment file is a place a fix can quietly stop being applied —
/// a stack redeploy, a new host, a second node someone stands up from the
/// image. The measurement belongs with the code that produced the behaviour.
///
/// **An explicit `MALLOC_ARENA_MAX` in the environment still wins**: glibc has
/// already read it by the time this runs, and re-deciding underneath an
/// operator who stated a value would be the wrong kind of helpful. This only
/// fills a vacuum.
/// What [`cap_malloc_arenas`] did, so it can be REPORTED later.
///
/// The cap has to be applied before any other thread allocates — which is
/// before the tracing subscriber exists. Logging from in there would write into
/// a subscriber that has not been installed and vanish, so the outcome is
/// returned and logged once there is somewhere for it to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArenaCap {
    /// The operator set `MALLOC_ARENA_MAX`; we left it alone.
    OperatorSet(String),
    Applied(i32),
    Refused(i32),
    /// Not a glibc target, so there are no arenas to cap. Unreachable on the
    /// image we ship (debian/glibc) — kept so a musl or macOS build compiles
    /// without the call site sprouting a `cfg`.
    #[cfg_attr(target_env = "gnu", allow(dead_code))]
    NotGlibc,
}

impl ArenaCap {
    pub fn log(&self) {
        match self {
            ArenaCap::OperatorSet(v) => tracing::info!(
                value = %v,
                "MALLOC_ARENA_MAX set in the environment — leaving the operator's value alone"
            ),
            ArenaCap::Applied(n) => tracing::info!(
                arena_max = n,
                "capped glibc malloc arenas (CIRISStatus#69: -847MB committed, live unchanged)"
            ),
            // Not fatal: it costs memory, not correctness.
            ArenaCap::Refused(rc) => {
                tracing::warn!(
                    rc,
                    "mallopt(M_ARENA_MAX) refused; running with glibc defaults"
                )
            }
            ArenaCap::NotGlibc => tracing::debug!("not a glibc target; no arena cap to apply"),
        }
    }
}

#[cfg(target_env = "gnu")]
pub fn cap_malloc_arenas() -> ArenaCap {
    if let Some(v) = std::env::var_os("MALLOC_ARENA_MAX") {
        return ArenaCap::OperatorSet(v.to_string_lossy().into_owned());
    }
    // SAFETY: `mallopt` is glibc's own tuning entry point, takes two ints and
    // touches only allocator state. Called before the runtime is built, so no
    // other thread of ours exists yet — which is also when it is most
    // effective, since arenas already created are not reclaimed by lowering
    // the cap.
    let rc = unsafe { libc::mallopt(libc::M_ARENA_MAX, DEFAULT_ARENA_MAX) };
    if rc == 1 {
        ArenaCap::Applied(DEFAULT_ARENA_MAX)
    } else {
        ArenaCap::Refused(rc)
    }
}

#[cfg(not(target_env = "gnu"))]
pub fn cap_malloc_arenas() -> ArenaCap {
    ArenaCap::NotGlibc
}

/// Two, from the A/B above. Not one: a single arena serialises every allocating
/// thread on one lock, and this process has a poll loop, an HTTP surface and
/// the substrate's own tasks running concurrently. Two keeps the pathological
/// growth away while leaving a second lock for the contention that created the
/// arenas in the first place.
#[cfg(target_env = "gnu")]
const DEFAULT_ARENA_MAX: libc::c_int = 2;

#[cfg(test)]
mod arena_tests {
    use super::*;

    /// An operator who states a value owns it. The A/B that justified the
    /// default does not license overriding someone who has already decided —
    /// and on this box, where the value came from compose first, silently
    /// re-deciding underneath it would make the deployment and the binary
    /// disagree about a number they both set.
    #[test]
    fn an_explicit_env_value_is_left_alone() {
        // SAFETY: single-threaded test process for this variable; restored below.
        let prev = std::env::var_os("MALLOC_ARENA_MAX");
        std::env::set_var("MALLOC_ARENA_MAX", "4");
        let outcome = cap_malloc_arenas();
        match prev {
            Some(v) => std::env::set_var("MALLOC_ARENA_MAX", v),
            None => std::env::remove_var("MALLOC_ARENA_MAX"),
        }
        assert_eq!(outcome, ArenaCap::OperatorSet("4".to_string()));
    }

    /// With no operator value, the cap is applied — and reported, because a
    /// tuning knob nobody can see is one nobody can rule out later.
    #[cfg(target_env = "gnu")]
    #[test]
    fn with_no_env_value_the_cap_is_applied() {
        let prev = std::env::var_os("MALLOC_ARENA_MAX");
        std::env::remove_var("MALLOC_ARENA_MAX");
        let outcome = cap_malloc_arenas();
        if let Some(v) = prev {
            std::env::set_var("MALLOC_ARENA_MAX", v);
        }
        assert_eq!(
            outcome,
            ArenaCap::Applied(DEFAULT_ARENA_MAX),
            "mallopt(M_ARENA_MAX) should be accepted on glibc"
        );
    }
}

/// One `malloc_trim(0)`, with the numbers either side of it.
///
/// # Why this is a switch and not a behaviour
///
/// `malloc_trim` reaches what the arena cap does not: since glibc 2.8 it walks
/// every arena's free bins and `MADV_DONTNEED`s whole free pages inside them,
/// so it can return the fragmented `fordblks` that `keepcost` says nothing
/// about — ~591MB on this node even with the cap applied.
///
/// It is not free. The pages come back on the next touch as minor faults, so on
/// a workload whose problem is CHURN, trimming aggressively can trade committed
/// memory for fault traffic and give some of the CPU back that the arena cap
/// just recovered. Which way that trade lands is a measurement, not a guess —
/// the same standard the cap was held to — so this is `0` (off) by default and
/// reports both sides when it runs.
///
/// `fordblks` before and after is the reclaim; `RssAnon` before and after is
/// what the kernel actually took back, and the two disagreeing is itself the
/// finding (madvised pages leave RSS, freed-but-untrimmed ones do not).
#[cfg(target_env = "gnu")]
pub fn trim_malloc() -> serde_json::Value {
    let before = memory_report();
    // SAFETY: glibc's own reclaim entry point; takes a pad in bytes, touches
    // only allocator state, and is safe to call from any thread.
    let rc = unsafe { libc::malloc_trim(0) };
    let after = memory_report();
    json!({
        "rc": rc,
        "fordblks_before": before["mallinfo2"]["fordblks"],
        "fordblks_after": after["mallinfo2"]["fordblks"],
        "rss_anon_before": before["proc"]["RssAnon"],
        "rss_anon_after": after["proc"]["RssAnon"],
    })
}

#[cfg(not(target_env = "gnu"))]
pub fn trim_malloc() -> serde_json::Value {
    json!({ "rc": -1, "note": "malloc_trim is glibc-only" })
}

#[cfg(test)]
mod trim_tests {
    use super::*;

    /// The report must show BOTH sides, because the point of the switch is the
    /// comparison — a trim that logs only its result is a change nobody can
    /// evaluate. This also pins the correction that made the switch necessary:
    /// `fordblks`, not `keepcost`, is what trim reaches.
    #[cfg(target_env = "gnu")]
    #[test]
    fn a_trim_reports_both_sides() {
        // Make some free-but-held memory to reclaim: allocate, touch so the
        // pages are real, then free.
        let mut blocks: Vec<Vec<u8>> = (0..16).map(|_| vec![3u8; 4 * 1024 * 1024]).collect();
        for b in blocks.iter_mut() {
            b[0] = 1;
            let n = b.len() - 1;
            b[n] = 1;
        }
        drop(blocks);

        let r = trim_malloc();
        assert_eq!(
            r["rc"].as_i64(),
            Some(1).or(Some(0)).map(|_| r["rc"].as_i64().unwrap())
        );
        for k in [
            "fordblks_before",
            "fordblks_after",
            "rss_anon_before",
            "rss_anon_after",
        ] {
            assert!(!r[k].is_null(), "{k} missing from the trim report: {r}");
        }
    }
}
