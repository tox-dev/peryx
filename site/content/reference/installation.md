+++
title = "Installation"
description = "The install channels, the platforms each covers, and how each one updates."
weight = 0
+++

Every channel ships the same single static binary; pick by how you manage tools.

| Channel                    | Command                                                                                                        | Updates with              |
| -------------------------- | -------------------------------------------------------------------------------------------------------------- | ------------------------- |
| Installer script (Unix)    | `curl -LsSf https://github.com/tox-dev/velodex/releases/latest/download/velodex-installer.sh \| sh`            | `velodex self update`     |
| Installer script (Windows) | `powershell -c "irm https://github.com/tox-dev/velodex/releases/latest/download/velodex-installer.ps1 \| iex"` | `velodex self update`     |
| PyPI wheel                 | `uv tool install velodex` or `pip install velodex`                                                             | `uv tool upgrade` / `pip` |
| From source                | `cargo build --release` in a checkout                                                                          | `git pull` and rebuild    |

## Platforms

GitHub releases carry binaries for macOS (Apple Silicon and Intel), Linux glibc (x86_64 and aarch64), and Windows x64,
each with a sha256 checksum. PyPI wheels additionally cover musl Linux (Alpine) on both architectures and Windows arm64;
the wheel embeds the same binary as a console script, so no Python ABI is involved and one wheel per platform serves
every interpreter.

## Self-update

`velodex self update` replaces the binary with the newest GitHub release. It works only for copies placed by the
installer scripts: those write an install receipt the updater reads. A pip- or cargo-installed velodex has no receipt
and is refused with a pointer back to its own package manager, so two tools never fight over one file.
