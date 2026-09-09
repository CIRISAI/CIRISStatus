//! ciris-status — the ciris.ai public health/status surface, now a **ciris-server
//! fabric node + a `StatusAdapter`** (mirrors CIRISAgent's adapter model).
//!
//! The whole node — the shared persist `Engine`, the Reticulum edge,
//! `consent:replication` peering, the read API, NodeCode, ownership, the safety
//! foundation, and NAT-traversal — is `ciris_server::serve_with_adapter`. The
//! status page is a `StatusAdapter` folded onto that SAME shared core: its routers
//! (`/health`, `/v1/status`, `/api/v1/status`, `/api/v1/status/history`,
//! `/api/v1/scoring`, the live SSE/WS sockets) merge onto the node's read-API
//! listener, and its background lifecycle probes the external services → emits
//! signed `observation:reachability:v1` (Flow B) + rebuilds the public roster from this
//! node's OWN corpus (Flow A) → updates the cache + uptime history + live push.
//!
//! **Zero env** (Server 0.5 zero-env model): boot takes only `--home <path>` and
//! `--key-id <name>` on the CLI. The node's identity/listen/peering resolve from
//! that home + the node's own `config:*` CEG; the StatusAdapter's own config
//! (probe targets, poll cadence, CORS) is `config:*` CEG read via `graph_config`,
//! and the uptime-history DB path is derived from the node `data_dir`. There are
//! no `STATUS_*`/`CIRIS_*` env vars.

mod adapter;
mod aggregate;
mod capability;
mod ceg;
mod ci;
mod config;
mod diag;
mod history;
mod model;
mod probe;
mod retention;
mod roster;

use std::path::PathBuf;
use std::sync::Arc;

/// The data root default. Matches `ciris_server`'s `DEFAULT_CIRIS_HOME`
/// (`/var/lib/ciris`); the docker-compose deploy passes `--home /data` to point
/// at the mounted volume.
const DEFAULT_HOME: &str = "/var/lib/ciris";
/// The federation key label default for the status node.
const DEFAULT_KEY_ID: &str = "ciris-status";

/// The floored node runtime, not `#[tokio::main]`.
///
/// A default runtime sizes to core count, so on the 2-vCPU canonical this binary
/// got TWO workers — a two-slot budget for the accept loop, replication, the
/// scorer and every request at once. One blocking task and HTTP becomes
/// unschedulable: the socket stays LISTEN, `Recv-Q` climbs as the kernel completes
/// handshakes, and userspace never calls `accept()`. That is CIRISServer#501,
/// measured on that host with one worker pegged at 99.9% and five threads idle.
///
/// ciris-status runs on that same box, alongside six other containers, and calls
/// `serve_with_adapter` — so it inherits every fix INSIDE the server and none of
/// the floor, which lives in the runtime the binary builds for itself.
/// `node_runtime::build` is the shared one: floor of 4, never a cap, and it
/// honours a deliberate `TOKIO_WORKER_THREADS`.
fn main() -> anyhow::Result<()> {
    // BEFORE the runtime exists: lowering the arena cap does not reclaim arenas
    // already created, so this only works while this thread is the only one
    // allocating. See `diag::cap_malloc_arenas` for the 205-minute A/B that
    // sized it (-847MB committed, live memory identical).
    let arena_cap = diag::cap_malloc_arenas();
    let runtime = ciris_server::node_runtime::build("ciris-status")?;
    runtime.block_on(async_main(arena_cap))
}

async fn async_main(arena_cap: diag::ArenaCap) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    // Applied before the runtime existed; reported now that a subscriber does.
    arena_cap.log();

    let mut args = std::env::args().skip(1);
    let first = args.next();

    // Console subcommand: `ciris-status config set/get <key> <json>` — mirrors
    // ciris-server's `config` arm so a HEADLESS status node (console-only, no app/
    // owner session) can set boot-structural knobs like `net.bootstrap_peers`.
    // ciris-status ships its OWN binary (not ciris-server's), so it needs this arm
    // too; the logic re-uses ciris-server's `run_config_set/get` (node-signed write).
    if first.as_deref() == Some("config") {
        return match args.next().as_deref() {
            Some("set") => run_config_set_cli(args).await,
            Some("get") => run_config_get_cli(args).await,
            other => Err(anyhow::anyhow!(
                "usage: ciris-status config set <key> <json-value> [--home <path>] [--key-id <name>]\n\
                 \x20      ciris-status config get <key> [--home <path>] [--key-id <name>] (got {:?})",
                other
            )),
        };
    }

    // Default: serve the node + StatusAdapter. Reconstruct the arg iterator
    // (`first` was consumed by the subcommand peek).
    let (home, key_id, diagnostics_window) = parse_args(first.into_iter().chain(args))?;

    // THE OPENER. 0.3.69 gated `/api/v1/debug/memory` on
    // `ciris_server::diag::enabled()` (CIRISStatus#73) — correct switch, and it
    // could never be flipped here: `diag::enable()` is called from
    // ciris-server's OWN binary entry point, which this binary does not run. We
    // call `serve_with_adapter` as a library, so the atomic stayed false
    // forever and the route was not "gated", it was gone, with no way for an
    // operator to get it back. A gate whose only opener lives in a `main` you
    // do not execute is a removal wearing a switch's clothes.
    //
    // Both doors, matching the sibling: `--diagnostics` (its flag) and
    // `CIRIS_DIAGNOSTICS=1` (its env, read through its own parser so the truthy
    // set cannot drift from theirs). Before `serve_with_adapter`, because
    // `routers()` asks `enabled()` while building.
    let diagnostics_mins = diagnostics_window
        .or_else(|| ciris_server::diag::env_requests().then_some(diag::DEFAULT_WINDOW_MINS));
    if let Some(diagnostics_mins) = diagnostics_mins {
        let source = if diagnostics_window.is_some() {
            "ciris-status --diagnostics"
        } else {
            "ciris-status CIRIS_DIAGNOSTICS"
        };
        ciris_server::diag::enable(source);
        // And it closes itself. The route is not loopback-bound here, so while
        // it is open the edge proxy is the only thing between allocator
        // internals and the internet — which makes "remember to turn it off" a
        // security control, and it has already failed once: a reading that
        // finished at 16:37 left the endpoint answering until 20:31.
        diag::open_window(diagnostics_mins);
        tracing::warn!(
            source,
            window_mins = diagnostics_mins,
            "diagnostics OPEN — /api/v1/debug/memory answers until the window \
             expires, then 404s; it is NOT loopback-gated here"
        );
    }

    // Zero-env node config: derived entirely from `--home`/`--key-id` + config:*.
    let cfg = ciris_server::ServerConfig::from_home(home, key_id)?;
    // The status page, as an adapter folded onto the node's shared core. It
    // resolves its own config:* at runtime from the AdapterContext; here it just
    // primes the HTTP client + live channel (no env, no corpus read yet).
    let adapter = Arc::new(adapter::StatusAdapter::new()?);

    tracing::info!(
        data_dir = %cfg.data_dir.display(),
        "ciris-status starting as a ciris-server node + StatusAdapter (zero-env)"
    );
    ciris_server::serve_with_adapter(cfg, adapter).await
}

/// `ciris-status config set <key> <json-value> …` — write a node-signed `config:*`
/// object from the console (headless path). Re-uses ciris-server's `run_config_set`.
async fn run_config_set_cli(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut home: Option<String> = None;
    let mut key_id = DEFAULT_KEY_ID.to_string();
    let mut reason = "console-cli".to_string();
    let mut key: Option<String> = None;
    let mut value_raw: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--home" => home = Some(args.next().context("--home needs a path")?),
            "--key-id" => key_id = args.next().context("--key-id needs a name")?,
            "--reason" => reason = args.next().context("--reason needs a value")?,
            other if other.starts_with("--") => {
                return Err(anyhow::anyhow!("unknown config-set arg: {other}"))
            }
            positional => {
                if key.is_none() {
                    key = Some(positional.to_string());
                } else if value_raw.is_none() {
                    value_raw = Some(positional.to_string());
                } else {
                    return Err(anyhow::anyhow!(
                        "unexpected extra config-set arg: {positional}"
                    ));
                }
            }
        }
    }
    let key = key.context("config set requires <key> (e.g. net.bootstrap_peers)")?;
    let value_raw =
        value_raw.context("config set requires <json-value> (e.g. '[\"108.61.242.236:4242\"]')")?;
    let value = parse_config_value(&value_raw);
    let home = home.unwrap_or_else(|| DEFAULT_HOME.to_string());
    let cfg = ciris_server::ServerConfig::from_home(PathBuf::from(home), key_id)?;
    let entry = ciris_server::run_config_set(cfg, &key, value, &reason).await?;
    println!(
        "✅ config set {} (version {}, authored by {})",
        entry.key, entry.version, entry.updated_by
    );
    println!("{}", serde_json::to_string_pretty(&entry.value)?);
    Ok(())
}

/// `ciris-status config get <key> …` — read the latest-wins `config:*` value.
async fn run_config_get_cli(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut home: Option<String> = None;
    let mut key_id = DEFAULT_KEY_ID.to_string();
    let mut key: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--home" => home = Some(args.next().context("--home needs a path")?),
            "--key-id" => key_id = args.next().context("--key-id needs a name")?,
            other if other.starts_with("--") => {
                return Err(anyhow::anyhow!("unknown config-get arg: {other}"))
            }
            positional => {
                if key.is_none() {
                    key = Some(positional.to_string());
                } else {
                    return Err(anyhow::anyhow!(
                        "unexpected extra config-get arg: {positional}"
                    ));
                }
            }
        }
    }
    let key = key.context("config get requires <key> (e.g. net.bootstrap_peers)")?;
    let home = home.unwrap_or_else(|| DEFAULT_HOME.to_string());
    let cfg = ciris_server::ServerConfig::from_home(PathBuf::from(home), key_id)?;
    match ciris_server::run_config_get(cfg, &key).await? {
        Some(entry) => println!("{}", serde_json::to_string_pretty(&entry.value)?),
        None => eprintln!("(no config for key {key:?})"),
    }
    Ok(())
}

/// Parse a `config set` value: JSON-first (so `'["a:1"]'`→List, `true`→Bool,
/// `7`→I64), with a bare-string fallback. Mirrors ciris-server's helper.
fn parse_config_value(raw: &str) -> ciris_server::ConfigValue {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => serde_json::from_value::<ciris_server::ConfigValue>(v)
            .unwrap_or_else(|_| ciris_server::ConfigValue::Str(raw.to_string())),
        Err(_) => ciris_server::ConfigValue::Str(raw.to_string()),
    }
}

/// Parse `--home <path>` / `--key-id <name>` (both optional; `--flag=value` also
/// accepted). Unknown args are an error — fail loud, never silently ignore a
/// misspelled flag on the boot path. Mirrors ciris-server's `parse_serve_flags`.
/// Returns `(home, key_id, diagnostics_window_mins)`. `None` for the window
/// means diagnostics stay closed.
fn parse_args(
    args: impl Iterator<Item = String>,
) -> anyhow::Result<(PathBuf, String, Option<u64>)> {
    let mut home: Option<String> = None;
    let mut key_id: Option<String> = None;
    let mut diagnostics: Option<u64> = None;

    let mut it = args;
    while let Some(arg) = it.next() {
        let (name, eq_value) = match arg.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut take = |arg_name: &str| -> anyhow::Result<String> {
            match eq_value.clone() {
                Some(v) => Ok(v),
                None => it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("{arg_name} needs a value")),
            }
        };
        match name.as_str() {
            "--home" => home = Some(take("--home")?),
            "--key-id" => key_id = Some(take("--key-id")?),
            // Mirrors ciris-server's own flag, plus an optional window:
            // `--diagnostics` opens for the default, `--diagnostics=30` for
            // thirty minutes. The value is optional via `=` only, so a bare
            // `--diagnostics` never swallows the argument after it.
            "--diagnostics" => {
                diagnostics = Some(match eq_value.as_deref() {
                    None => diag::DEFAULT_WINDOW_MINS,
                    Some(v) => v.trim().parse::<u64>().map_err(|_| {
                        anyhow::anyhow!(
                            "--diagnostics takes minutes, e.g. --diagnostics=30 (got {v:?})"
                        )
                    })?,
                });
            }
            other => {
                return Err(anyhow::anyhow!(
                    "unknown arg: {other} (usage: ciris-status [--home <path>] [--key-id <name>] \
                     [--diagnostics])"
                ))
            }
        }
    }

    Ok((
        PathBuf::from(home.unwrap_or_else(|| DEFAULT_HOME.to_string())),
        key_id.unwrap_or_else(|| DEFAULT_KEY_ID.to_string()),
        diagnostics,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> anyhow::Result<(PathBuf, String, Option<u64>)> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_when_no_flags() {
        let (home, key_id, diagnostics) = parse(&[]).unwrap();
        assert_eq!(home, PathBuf::from(DEFAULT_HOME));
        assert_eq!(key_id, DEFAULT_KEY_ID);
        assert!(
            diagnostics.is_none(),
            "diagnostics stay OFF unless asked for"
        );
    }

    /// CIRISStatus#73 gated the memory route on the server's switch; 0.3.69
    /// shipped that gate with no way to open it from THIS binary, because
    /// `diag::enable()` is called from ciris-server's own `main`, which we do
    /// not run. The flag is one of the two openers — without it the route is
    /// not gated, it is gone.
    #[test]
    fn diagnostics_flag_is_accepted_and_off_by_default() {
        let (_, _, w) = parse(&["--diagnostics"]).unwrap();
        assert_eq!(w, Some(diag::DEFAULT_WINDOW_MINS));

        // Bare form takes no value, so it must not swallow the next argument.
        let (home, key_id, w) =
            parse(&["--diagnostics", "--home", "/data", "--key-id", "node-b"]).unwrap();
        assert_eq!(w, Some(diag::DEFAULT_WINDOW_MINS));
        assert_eq!(home, PathBuf::from("/data"));
        assert_eq!(key_id, "node-b");
    }

    /// The window is the point: an operator who wants a short exposure gets to
    /// ask for one, and a typo does not silently become "open for the default
    /// two hours".
    #[test]
    fn the_diagnostics_window_is_settable_and_a_bad_value_is_refused() {
        let (_, _, w) = parse(&["--diagnostics=30"]).unwrap();
        assert_eq!(w, Some(30));
        assert!(parse(&["--diagnostics=soon"]).is_err());
        assert!(parse(&["--diagnostics="]).is_err());
    }

    #[test]
    fn space_and_eq_forms_parse() {
        let (home, key_id, _) = parse(&["--home", "/data", "--key-id", "ciris-status"]).unwrap();
        assert_eq!(home, PathBuf::from("/data"));
        assert_eq!(key_id, "ciris-status");

        let (home, key_id, _) = parse(&["--home=/data", "--key-id=node-b"]).unwrap();
        assert_eq!(home, PathBuf::from("/data"));
        assert_eq!(key_id, "node-b");
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["--home"]).is_err()); // missing value
    }
}
