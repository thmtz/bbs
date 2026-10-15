#!/usr/bin/env python3
"""Install this checkout's BBS hooks and skill using only the Python standard library."""

import argparse
import copy
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile


REPO = Path(__file__).resolve().parents[1]


def load_config(path):
    if not path.exists():
        if path.is_symlink():
            raise ValueError(f"{path}: dangling configuration symlink")
        return {}
    config = json.loads(path.read_text())
    if not isinstance(config, dict):
        raise ValueError(f"{path}: configuration must be a JSON object")
    hooks = config.get("hooks", {})
    if not isinstance(hooks, dict):
        raise ValueError(f"{path}: hooks must be an object")
    for event, groups in hooks.items():
        if not isinstance(groups, list):
            raise ValueError(f"{path}: hooks.{event} must be an array")
        for group in groups:
            if not isinstance(group, dict) or not isinstance(group.get("hooks"), list):
                raise ValueError(f"{path}: hooks.{event} entries need a hooks array")
            if not all(isinstance(handler, dict) for handler in group["hooks"]):
                raise ValueError(f"{path}: hooks.{event} handlers must be objects")
    return config


def merge_hooks(config, template):
    """Keep existing handlers and ordering, including customized BBS handlers."""
    merged = copy.deepcopy(config)
    hooks = merged.setdefault("hooks", {})
    for event, wanted_groups in template["hooks"].items():
        groups = hooks.setdefault(event, [])
        for wanted in wanted_groups:
            missing = []
            for handler in wanted["hooks"]:
                installed = any(
                    group.get("matcher", "") == wanted.get("matcher", "")
                    and existing.get("type") == handler["type"]
                    and existing.get("command") == handler["command"]
                    for group in groups
                    for existing in group["hooks"]
                )
                if not installed:
                    missing.append(copy.deepcopy(handler))
            if missing:
                groups.append({**copy.deepcopy(wanted), "hooks": missing})
    # Remove unmodified BBS handlers from events this version no longer uses.
    ours = [
        handler
        for groups in template["hooks"].values()
        for group in groups
        for handler in group["hooks"]
    ]
    for event in [event for event in hooks if event not in template["hooks"]]:
        groups = []
        for group in hooks[event]:
            kept = [handler for handler in group["hooks"] if handler not in ours]
            # Drop a group only when it held nothing but BBS handlers.
            if kept or not group["hooks"]:
                groups.append({**group, "hooks": kept})
        if groups:
            hooks[event] = groups
        else:
            del hooks[event]
    return merged


def write_config(path, config):
    # Follow config symlinks so dotfiles-managed settings stay linked.
    target = path.resolve()
    target.parent.mkdir(parents=True, exist_ok=True)
    mode = target.stat().st_mode & 0o777 if target.exists() else 0o600
    backup = target.with_name(target.name + ".bak-bbs")
    if target.exists() and not (backup.exists() or backup.is_symlink()):
        shutil.copy2(target, backup)
    fd, temporary = tempfile.mkstemp(prefix=target.name + ".", dir=target.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            stream.write(json.dumps(config, indent=2) + "\n")
        os.chmod(temporary, mode)
        os.replace(temporary, target)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--claude-dir", type=Path,
        default=Path(os.environ.get("CLAUDE_CONFIG_DIR", Path.home() / ".claude")),
        help="Claude config directory (default: CLAUDE_CONFIG_DIR or ~/.claude)",
    )
    parser.add_argument(
        "--codex-dir", type=Path,
        default=Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")),
        help="Codex config directory (default: CODEX_HOME or ~/.codex)",
    )
    parser.add_argument(
        "--skills-dir", type=Path, default=Path.home() / ".agents" / "skills",
        help="Codex user skill directory (default: ~/.agents/skills)",
    )
    parser.add_argument("--dry-run", action="store_true", help="show changes without writing")
    args = parser.parse_args()

    # Validate all inputs and skill destinations before changing anything.
    configs = []
    for path, harness in (
        (args.claude_dir.expanduser() / "settings.json", "claude-code"),
        (args.codex_dir.expanduser() / "hooks.json", "codex"),
    ):
        current = load_config(path)
        template = load_config(REPO / "integrations" / harness / "hooks.json")
        configs.append((path, current, merge_hooks(current, template)))

    source = REPO / "integrations" / "skills" / "bbs"
    links = []
    for parent in (args.skills_dir.expanduser(), args.claude_dir.expanduser() / "skills"):
        # Resolve the parent, not the link itself; the skill directory may be shared.
        link = parent.resolve() / "bbs"
        if link in links or link.resolve() == source:
            continue
        if link.exists() and not link.is_symlink():
            raise ValueError(f"{link}: move the existing skill aside before linking this repo")
        links.append(link)

    for path, current, merged in configs:
        changed = current != merged
        print(f"{'Update' if changed else 'Unchanged'} {path}")
        if changed and not args.dry_run:
            write_config(path, merged)
    for link in links:
        print(f"Link {link} -> {source}")
        if not args.dry_run:
            link.parent.mkdir(parents=True, exist_ok=True)
            if link.is_symlink():
                link.unlink()
            link.symlink_to(source, target_is_directory=True)
    print("BBS must be on PATH for both harnesses. Review new Codex hooks with /hooks.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError) as error:
        print(f"setup: {error}", file=sys.stderr)
        sys.exit(1)
