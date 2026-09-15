# AGENTS.md

## Project Overview

`afunix-agent` is a Windows-only Rust daemon that exposes a Windows SSH agent backend through a native AF_UNIX socket. The current backend talks to the Pageant-compatible interface provided by GnuPG's `gpg-agent`.

The crate uses Rust 2024 and requires Rust 1.85 or later. Runtime and Windows integration behavior must remain compatible with Windows 10 version 1803 or later.

## Repository Layout

- `src/main.rs`: platform-gated executable entry point.
- `src/cli.rs`: command-line parsing and command dispatch.
- `src/daemon.rs`: daemon setup and orchestration.
- `src/proxy.rs`: SSH agent connection handling and backend forwarding.
- `src/framing.rs`: length-prefixed SSH agent frame parsing and validation.
- `src/backend/`: backend abstraction and Pageant implementation.
- `src/control.rs`: daemon control named-pipe server and client behavior.
- `src/control_protocol.rs`: newline-delimited JSON control protocol.
- `src/lifecycle.rs`: shutdown and active-connection state.
- `src/model.rs`: shared configuration and status types.
- `src/unix_socket.rs`: Windows AF_UNIX socket management.
- `src/platform/windows.rs`: Windows API wrappers, ACLs, process handling, named pipes, and ownership checks.

Unit tests are colocated with the modules they cover under `#[cfg(test)]`.

## Development Commands

Run these commands from the repository root:

```text
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

Run `cargo fmt --all` after editing Rust code.

Most platform-independent tests can run on other operating systems, but Windows-specific code and integration behavior must be validated on Windows. When changing daemon lifecycle, sockets, named pipes, ACLs, or Pageant communication, also exercise the relevant CLI flow on Windows:

```text
cargo run -- serve
cargo run -- status
cargo run -- socket
cargo run -- stop
```

A Pageant integration test requires GnuPG `gpg-agent` configured with `enable-putty-support`.

## Coding Guidelines

- Keep changes focused and consistent with the existing module boundaries.
- Prefer standard library facilities and existing dependencies. Add a dependency only when it clearly reduces risk or complexity.
- Preserve Rust 1.85 compatibility; do not use APIs stabilized in later Rust versions.
- Use `thiserror` for structured, reusable error types and `anyhow` with context at application or integration boundaries, following the existing patterns.
- Keep protocol parsing bounded and reject malformed or oversized input before allocating or copying data.
- Use RAII wrappers such as `OwnedHandle` and `Drop` implementations for Windows resources.
- Keep `unsafe` blocks narrow. Validate pointers, lengths, ownership, and Windows API return values at every FFI boundary.
- Add comments only for non-obvious invariants, safety constraints, or platform behavior.
- Add or update tests for normal behavior, boundary conditions, malformed input, and regression cases.

## Security and Protocol Invariants

Treat all socket, pipe, shared-memory, and backend data as untrusted.

- Never log SSH agent request or response payloads, key material, or other secrets.
- SSH agent frames use a four-byte big-endian payload length. Preserve full wire frames when passing data to a backend.
- Reject SSH agent payloads larger than 256 KiB before allocation.
- Pageant shared memory is 8 KiB. Validate both requests and responses against that bound before reading or writing.
- The control protocol is newline-delimited JSON with a 16 KiB line limit. Preserve framing, size checks, and CRLF compatibility.
- Keep the AF_UNIX socket, ownership marker, control named pipe, and singleton mutex restricted to the current Windows user.
- Control clients must continue verifying that the named-pipe server belongs to the current user.
- Do not weaken socket ownership-marker checks or delete a socket/file unless ownership has been established. These checks prevent removal of unrelated files after an unclean shutdown.
- Preserve graceful shutdown ordering: acknowledge and flush control responses before requesting shutdown, and allow active connection guards to unwind correctly.

Any intentional change to these limits or guarantees must update the implementation, tests, and `README.md` together.

## Testing Expectations

- Put focused unit tests beside the implementation unless a true integration test is needed.
- Prefer deterministic tests that do not depend on a running daemon or Pageant instance.
- Use small fake backends and in-memory readers/writers for protocol and proxy tests.
- For concurrency changes, test lifecycle transitions and resource accounting explicitly.
- For Windows API changes, test pure name/path/security-descriptor helpers separately where possible, then validate the actual OS interaction on Windows.

Before finishing, run the narrowest relevant tests first, followed by formatting, the full test suite, Clippy, and a release build when the environment supports them.

## Documentation

Update `README.md` when changing:

- CLI commands, flags, defaults, or output.
- Windows or GnuPG requirements.
- Socket paths, lifecycle behavior, protocol limits, or cleanup behavior.
- Security properties or user-visible error recovery steps.
