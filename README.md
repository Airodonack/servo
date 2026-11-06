> One change of the Servo fork: `review/26`, 26 of 29 in the stack.
> Below it: [`review/25`](https://github.com/Airodonack/servo/tree/review/25). Above it: [`review/27`](https://github.com/Airodonack/servo/tree/review/27).
> The change itself: [commit 41bcc4f2c5c63b34d5ee5575f11026023aa34eee](https://github.com/Airodonack/servo/commit/41bcc4f2c5c63b34d5ee5575f11026023aa34eee). This README sits in a commit on top of it and is not part of the pull request.
> The whole stack: [overview README](https://github.com/Airodonack/servo/blob/vodscribe/main/README.md).

# Embedders can choose the address the WebDriver server listens on

`webdriver_server::start_server` bound 0.0.0.0 unconditionally. An
embedder that only drives itself from the same machine still exposed a
server that runs script in its pages to every network the machine is on.

[`start_server`](https://github.com/Airodonack/servo/blob/41bcc4f2c5c63b34d5ee5575f11026023aa34eee/components/webdriver_server/lib.rs#L137-L165)
now takes an `IpAddr` before the port and binds
`SocketAddr::new(address, port)`. servoshell, the only caller in the tree,
passes `Ipv4Addr::UNSPECIFIED`
([running_app_state.rs](https://github.com/Airodonack/servo/blob/41bcc4f2c5c63b34d5ee5575f11026023aa34eee/ports/servoshell/running_app_state.rs#L270-L276)),
so its behaviour is unchanged.

## Options considered

| Option | Why not chosen |
|---|---|
| **An address argument** (this change) | Chosen: the embedder decides, servoshell keeps its behaviour. |
| Always bind 127.0.0.1 | Changes servoshell; remote WebDriver setups that rely on 0.0.0.0 would break. |
| A preference | Prefs are page-facing configuration; this is a choice of the embedding code. |

## What it costs

An API change for embedders: code that calls `start_server` passes an
address.

## How it was checked

From the commit message (2026-11-01): an embedding app built on the crate
listened on 127.0.0.1 only. servoshell's call site is the one-line change
shown, and `mach check` builds servoshell with it.

The restack's gate (2026-11-05) ran on this change before the day's fixups
to lower refs were folded in (the same patch): `mach fmt` changed nothing,
`test-tidy` passed, and `mach check` exited 0 with no warnings. At the
stack's tip the same checks and `mach clippy` pass.

## Questions a reviewer may ask

- **Does servoshell change?** No; it passes 0.0.0.0 as before.
- **Why not a preference?** Prefs are page-facing configuration; which
  interface a server binds is a choice of the embedding code.
