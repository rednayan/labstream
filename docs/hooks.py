"""MkDocs hook that mirrors root-level reference documents into docs/_ref/
before each build so the repository keeps one canonical copy of each file."""

from pathlib import Path
import shutil

ROOT_FILES = {
    "SPEC.md": "spec.md",
    "CHANGELOG.md": "changelog.md",
    "CONTRIBUTING.md": "contributing.md",
    "crates/labstream/docs/design.md": "design.md",
}


def on_pre_build(config, **kwargs):
    docs_dir = Path(config["docs_dir"])
    repo_root = docs_dir.parent
    ref_dir = docs_dir / "_ref"
    ref_dir.mkdir(parents=True, exist_ok=True)
    for src_rel, dst_name in ROOT_FILES.items():
        src = repo_root / src_rel
        if src.exists():
            shutil.copy2(src, ref_dir / dst_name)
