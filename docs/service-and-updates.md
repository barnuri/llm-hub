# Service and updates

## Install

Pick one way:

- **Install script**, macOS and Linux only:

  ```sh
  curl -fsSL https://raw.githubusercontent.com/barnuri/llm-hub/main/scripts/install.sh | sh
  ```

- **Release binary.** Download it from [releases](https://github.com/barnuri/llm-hub/releases).
  On Windows, download `llm-hub-windows-x86_64.exe` and rename it to `llm-hub.exe`.
- **From source.** Run `cargo build --release` in a clone. The binary is at
  `target/release/llm-hub`, or `target\release\llm-hub.exe` on Windows. To build on Windows,
  install the Visual Studio C++ build tools first. A binary that runs from the checkout updates
  from source. See [Updates](#updates).

## Service

```sh
cd ~/llm-hub   # the directory with .env
llm-hub service install
llm-hub service status
llm-hub service uninstall
```

`service install` registers an always-on service:

- **macOS:** a launchd LaunchAgent (`com.barnuri.llm-hub`). Install also adds the binary to the
  Application Firewall allow list, and sudo can ask for your password. Without that entry, macOS
  drops LAN connections to unsigned background binaries on every port.
- **Linux:** a systemd user unit (`llm-hub`). Run `loginctl enable-linger $USER` to start it at
  boot without a login session.
- **Windows:** a Task Scheduler logon task (`llm-hub`). It needs no admin rights. It runs
  `%LOCALAPPDATA%\llm-hub\llm-hub-service.cmd`, which changes to the install directory and starts
  the hub again whenever it exits. The hub runs in a console window. Closing the window stops it
  until the next logon or `schtasks /Run /TN llm-hub`.

The service definition stores:

- The directory you ran `service install` from. The service reads that directory's `.env`.
- `LLM_HUB_PORT=8888` and `LLM_HUB_BIND=0.0.0.0`. These win over `.env`, so a service always
  listens on `0.0.0.0:8888`.
- `LLM_HUB_SERVICE=1`, so restarts and updates leave the restart to the service manager.

Logs go to `logs/llm-hub.log` in that directory. Managed processes log to `logs/<profile>.log`.

Run `service install` again after an update to pick up changes to the service definition. It is
safe to run on an installed service.

### Service PATH

launchd and systemd start the service with a minimal `PATH` that does not include Homebrew. Any
program the hub starts must be reachable from that `PATH`:

- For `gh`, set `LLM_HUB_<NAME>_GH_PATH` to an absolute path.
- For a managed `COMMAND`, start it with `/usr/bin/env PATH=/opt/homebrew/bin:/usr/bin:/bin`.
  This covers programs that the command itself runs, such as `npx` or `ffmpeg`.

## Docker

```sh
docker build -t llm-hub .
docker run --rm -p 8410:8410 --env-file .env -e LLM_HUB_BIND=0.0.0.0 llm-hub
```

## Updates

Auto-update is on by default. The hub checks GitHub releases on startup and once a day, downloads
the binary for your OS and architecture, and swaps it in one step. Set
`LLM_HUB_AUTO_UPDATE=false` to only log a notice when a newer version exists.

```sh
llm-hub update   # update once
```

### Source installs

The updater resolves symlinks first. A binary counts as a source install when its real path is
inside a git checkout of llm-hub. A `PATH` link that points into the checkout counts too.

- A source install updates with `git pull --ff-only` and `cargo build --release`, not a download.
- Auto-update skips the pull while the checkout has uncommitted changes.
- A manual `llm-hub update` still runs the pull. If the pull cannot apply, git reports the
  conflict and the update fails.
- A copy from `cargo install --path .` lives outside the checkout, so it updates from release
  binaries.

### Windows

Windows cannot overwrite a running `.exe`. The updater renames it to `llm-hub.old.exe` first and
removes that file on the next update.
