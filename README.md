# afunix-agent

`afunix-agent` exposes a Windows SSH agent backend through a native Windows
AF_UNIX socket. The initial backend is the Pageant-compatible interface exposed
by GnuPG `gpg-agent` with `enable-putty-support`.

## Requirements

- Windows 10 version 1803 or later
- GnuPG `gpg-agent` configured with `enable-putty-support`
- Rust 1.98.1 or later when building from source

## Installation

Install the latest stable release with WinGet:

```powershell
winget install --exact --id Nanamiiiii.afunix-agent
```

Alternatively, download the Windows x64 archive from
[GitHub Releases](https://github.com/Nanamiiiii/afunix-agent/releases), extract it,
and place `afunix-agent.exe` on `PATH`.

## Build

```powershell
cargo build --release
```

## Usage

Start the background daemon:

```powershell
afunix-agent start
```

The background daemon does not retain the caller's redirected input/output
pipes, so capturing the output of `start` does not wait for the daemon to stop.

Run it in the foreground:

```powershell
afunix-agent serve
```

Use a custom socket path:

```powershell
afunix-agent start --socket "$HOME\.ssh\wezterm-agent.sock"
```

Inspect and control the daemon:

```powershell
afunix-agent status
afunix-agent status --json
afunix-agent socket
afunix-agent restart
afunix-agent stop
```

Set the socket for the current PowerShell session:

```powershell
$env:SSH_AUTH_SOCK = afunix-agent socket
```

The default socket is `%USERPROFILE%\.ssh\ssh-agent.sock`.

## Release process

The `.github/workflows/release.yml` workflow runs for tags named `vX.Y.Z` or
`vX.Y.Z-prerelease`. The tag version must exactly match the package version in
`Cargo.toml`.

Before publishing the first stable release, add a repository Actions secret named
`WINGET_CREATE_GITHUB_TOKEN`. It must contain a GitHub personal access token
(classic) with the `public_repo` scope; fine-grained tokens are not supported by
`wingetcreate`. The token is used only to create a pull request in
[`microsoft/winget-pkgs`](https://github.com/microsoft/winget-pkgs).

To publish a release:

1. Update the version in `Cargo.toml` and refresh `Cargo.lock` with `cargo check`.
2. Commit the version change.
3. Create and push the matching tag, for example `v0.1.0`.

The workflow checks formatting, tests, and Clippy before building the Windows x64
binary. It publishes the executable, a ZIP archive, and SHA-256 checksum files to
a GitHub Release. Stable versions also generate and submit the WinGet manifests;
prerelease versions skip WinGet submission.

## GnuPG configuration

Add the following settings to `gpg-agent.conf` and restart `gpg-agent`:

```text
enable-ssh-support
enable-putty-support
```

The backend discovers Pageant for every SSH agent request, so restarting
`gpg-agent` does not require restarting `afunix-agent`. Pageant transactions use
an 8 KiB shared-memory mapping and a 30-second Windows message timeout. The
proxy rejects SSH agent payloads larger than 256 KiB before allocation; the
Pageant backend additionally rejects wire frames larger than its 8 KiB mapping.

## Lifecycle

The background `start` command launches `serve` and polls the control pipe until
the daemon reports its status. `restart` reads the active configuration, stops
the daemon, waits for singleton release, and starts the same configuration.

A sidecar file ending in `.afunix-agent-owned` records the Windows file identity
of the managed AF_UNIX endpoint. It allows recovery after an unclean shutdown
without deleting an unrelated socket or regular file. Both files are removed
after a graceful shutdown.

## Security

The daemon listens only on a native Windows AF_UNIX socket. The socket,
ownership marker, control named pipe, and single-instance mutex use
current-user-only Windows ACLs. Control clients verify that the named-pipe
server runs as the current user. SSH agent request and response payloads are
never logged.
