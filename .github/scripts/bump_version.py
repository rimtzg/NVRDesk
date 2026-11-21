"""Semantic version bump helper for PRs targeting main."""

import pathlib
import re
import sys


def parse_semver(raw_version: str) -> tuple[int, int, int]:
    """Parses major.minor.patch from a version string."""
    match = re.match(r"^(\d+)\.(\d+)\.(\d+)", raw_version.strip())
    if not match:
        raise ValueError(f"Unsupported semver string: {raw_version!r}")
    return tuple(int(group) for group in match.groups())


def extract_cargo_version(cargo_text: str) -> str:
    """Extracts the workspace package version from Cargo.toml text."""
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo_text, flags=re.MULTILINE)
    if not match:
        raise ValueError("Could not find version field in Cargo.toml")
    return match.group(1)


def compute_next_version(base_ver: str, head_ver: str, labels_csv: str) -> str | None:
    """Returns next version if head_ver <= base_ver, else None."""
    base_tuple = parse_semver(base_ver)
    head_tuple = parse_semver(head_ver)
    if head_tuple > base_tuple:
        return None

    labels = {label.strip().lower() for label in labels_csv.split(",") if label.strip()}
    major, minor, patch = base_tuple
    if "bump:major" in labels:
        return f"{major + 1}.0.0"
    if "bump:minor" in labels:
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def update_manifests(new_version: str) -> None:
    """Updates Cargo.toml, Cargo.lock workspace crates, and macOS Info.plist bundle version."""
    cargo_path = pathlib.Path("Cargo.toml")
    cargo_text = cargo_path.read_text(encoding="utf-8")
    updated_cargo = re.sub(
        r'(\[workspace\.package\]\s*\nversion\s*=\s*)"[^"]+"',
        rf'\1"{new_version}"',
        cargo_text,
        count=1,
    )
    cargo_path.write_text(updated_cargo, encoding="utf-8")

    lock_path = pathlib.Path("Cargo.lock")
    if lock_path.exists():
        lock_text = lock_path.read_text(encoding="utf-8")
        updated_lock = re.sub(
            r'(name = "nvrdesk-[^"]+"\s*\nversion = )"[^"]+"',
            rf'\1"{new_version}"',
            lock_text,
        )
        lock_path.write_text(updated_lock, encoding="utf-8")

    plist_path = pathlib.Path("packaging/macos/Info.plist")
    if plist_path.exists():
        plist_text = plist_path.read_text(encoding="utf-8")
        for key in ("CFBundleVersion", "CFBundleShortVersionString"):
            plist_text = re.sub(
                rf"(<key>{key}</key>\s*<string>)[^<]+(</string>)",
                rf"\g<1>{new_version}\2",
                plist_text,
            )
        plist_path.write_text(plist_text, encoding="utf-8")


def main() -> int:
    """CLI entrypoint expecting: <base_cargo_toml_path> <labels_csv>."""
    if len(sys.argv) < 3:
        sys.stderr.write("Usage: bump_version.py <base_cargo_toml_path> <labels_csv>\n")
        return 1

    try:
        base_text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
        head_text = pathlib.Path("Cargo.toml").read_text(encoding="utf-8")
        base_ver = extract_cargo_version(base_text)
        head_ver = extract_cargo_version(head_text)
        next_ver = compute_next_version(base_ver, head_ver, sys.argv[2])
        if not next_ver:
            sys.stderr.write(
                f"Head version ({head_ver}) is already ahead of main ({base_ver}); skipping.\n"
            )
            return 0
        update_manifests(next_ver)
        sys.stdout.write(next_ver)
        return 0
    except Exception as exc:
        sys.stderr.write(f"Version bump failed: {exc}\n")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
