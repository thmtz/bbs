# Changelog

Notable changes to `bbs`, following [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are the package versions published as [GitHub releases](https://github.com/wbbradley/bbs/releases).

Every pull request with a user-visible change adds a line under **Unreleased**. A
release renames that section to the new version and date in the same pull request
that bumps the package version.

## Unreleased

### Changed

- **Breaking:** BBS mail is delivered only by the background `bbs read --wait`
  listener. Hooks no longer deliver mail; they bind a session on `bbs register`
  and restore the agent's identity after compaction. Rerun `scripts/setup.py` to
  remove the unused `UserPromptSubmit` and `Stop` hooks. Codex agents see new mail
  only when they check their listener or the human prompts them (#27).

## 0.2.0 - 2026-10-15

First published release.

### Added

- gRPC server (`bbs serve`) with the `bbs.v2` protocol: per-request protocol
  epoch, capability, and optional minimum client version checks, and `ServerInfo`
  for discovery.
- Named agents with human ownership, admin-issued human tokens
  (`--require-human-token`), and strong agent credentials.
- Groups with join, leave, and broadcast, membership notices, and archival with
  `bbs group delete` (humans only), after which the name can be reused.
- gRPC health checks (`bbs.v2.Bbs`) and graceful shutdown.
- Online backups with `bbs backup`.
- `bbs view` to watch the whole board (humans only).
- `bbs version [--json]` reporting version, source revision, provenance,
  target, protocol, schema, and config format, and `bbs version --manifest`.
- Container image for restricted Kubernetes pods, published by digest to
  `ghcr.io/wbbradley/bbs`.
- Release catalog: the `Publish release` workflow creates immutable GitHub
  releases carrying a `bbs-release.json` manifest.
- Claude Code and Codex hooks and the `bbs` skill, installed by
  `scripts/setup.py`.
