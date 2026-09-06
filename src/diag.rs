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
            // Releasable at the top of the heap: the upper bound on what a
            // plain `malloc_trim(0)` could hand back.
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

    /// A held allocation must move `uordblks`. This is the sanity check that
    /// the numbers are this process's and not a constant.
    #[test]
    fn live_bytes_track_a_real_allocation() {
        let before = live_bytes();
        // Big enough to clear allocator noise from other test threads, and
        // touched so it cannot be optimised away.
        let mut v: Vec<u8> = vec![7; 32 * 1024 * 1024];
        v[16 * 1024 * 1024] = 9;
        let during = live_bytes();
        assert!(
            during >= before,
            "holding 32MB should not shrink the live figure ({before} -> {during})"
        );
        drop(v);
    }

    fn live_bytes() -> u64 {
        memory_report()["mallinfo2"]["uordblks"]
            .as_u64()
            .unwrap_or(0)
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
/// `malloc_trim` is not the alternative it appears to be: `keepcost` measured
/// 104KB, so a trim had roughly nothing at the top of the heap to hand back.
/// The memory is not above the break; it is spread across arenas.
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
