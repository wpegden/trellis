"""Extract a scratch crate using a run's supplied tools and configuration."""
from pathlib import Path
import shutil

from trellis.modeling.observations import fingerprint, observe, read_json, write_json
from scripts.extract_pv_model import copy_source_inputs


def probe_source(root: Path, source: Path, output: Path, *, through: str = "lean", timeout: float | None = None) -> dict:
    before = fingerprint(source)
    workspace = output / "probe"
    copy_source_inputs(source, workspace / "work/crate")
    if fingerprint(source) != before:
        raise ValueError("Probe source changed while its snapshot was copied")
    shutil.copytree(workspace / "work/crate", workspace / "upstream")
    config = read_json(root / "config.json")
    write_json(workspace / "config.json", config)
    if "paper_input" in config:
        from trellis.paper_implementation import preparation_paper_digest, evidence_path
        preparation_paper_digest(root)
        for name in config["paper_input"]["paper_files"]:
            destination = workspace / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(evidence_path(root, name), destination)
        preparation_paper_digest(workspace)
    shutil.copy2(root / "work/extraction.json", workspace / "work/extraction.json")
    if (root / "tool-identities.json").exists():
        shutil.copy2(root / "tool-identities.json", workspace / "tool-identities.json")
    return observe(workspace, output / "observation", through=through, timeout=timeout,
                   history_root=output, cache_root=root / ".trellis/scratch/check-cache", read_caches=[root / ".trellis/modeling/cache"])
