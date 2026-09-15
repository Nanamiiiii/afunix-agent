# afunix-agent

`afunix-agent` exposes a Windows SSH agent backend through a native Windows
AF_UNIX socket. The initial backend is the Pageant-compatible interface exposed
by GnuPG `gpg-agent` with `enable-putty-support`.

## Requirements

- Windows 10 version 1803 or later
- GnuPG `gpg-agent` configured with `enable-putty-support`
- Rust 1.85 or later when building from source

## Build

```powershell
cargo build --release
```

## Usage

Start the background daemon:

```powershell
afunix-agent start
```

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
