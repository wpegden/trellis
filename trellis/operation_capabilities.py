"""Static producer capabilities checked before an agent operation dispatches.

This is deliberately a declaration plus one refusal check, not a schema or
prompt generator. Kernel request contracts remain the source of prompt data;
``declared_repo_writable_paths`` remains the source of sandbox mount authority.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

from trellis.sandbox import declared_repo_writable_paths


@dataclass(frozen=True)
class OperationCapability:
    producer_role: str
    request_kind: str
    work_kind: str
    prompt_fragment: str
    response_fields: tuple[str, ...]
    writable_artifact_paths: tuple[str, ...]
    required_artifacts: tuple[str, ...]
    checker_consumer: str
    next_transition: str


OPERATION_CAPABILITIES: Mapping[tuple[str, str], OperationCapability] = {
    ("worker", "standard"): OperationCapability(
        producer_role="worker",
        request_kind="worker",
        work_kind="standard",
        prompt_fragment="worker/common/30_request.md",
        response_fields=("worker_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:worker_result",),
        checker_consumer="kernel worker acceptance pipeline",
        next_transition="phase verifier or reviewer",
    ),
    ("paper", "standard"): OperationCapability(
        producer_role="reviewer",
        request_kind="paper",
        work_kind="standard",
        prompt_fragment="shared/90_artifact_delivery.md",
        response_fields=("paper_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:paper_result",),
        checker_consumer="kernel paper normalization",
        next_transition="Review",
    ),
    ("corr", "standard"): OperationCapability(
        producer_role="reviewer",
        request_kind="corr",
        work_kind="standard",
        prompt_fragment="shared/90_artifact_delivery.md",
        response_fields=("corr_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:corr_result",),
        checker_consumer="kernel correspondence normalization",
        next_transition="Review",
    ),
    ("sound", "standard"): OperationCapability(
        producer_role="reviewer",
        request_kind="sound",
        work_kind="standard",
        prompt_fragment="shared/90_artifact_delivery.md",
        response_fields=("sound_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:sound_result",),
        checker_consumer="kernel soundness normalization",
        next_transition="Review",
    ),
    ("review", "standard"): OperationCapability(
        producer_role="reviewer",
        request_kind="review",
        work_kind="standard",
        prompt_fragment="review/common/10_request.md",
        response_fields=("review_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:review_result",),
        checker_consumer="kernel reviewer legality checker",
        next_transition="commissioned operation or next phase",
    ),
    ("audit", "standard"): OperationCapability(
        producer_role="reviewer",
        request_kind="audit",
        work_kind="standard",
        prompt_fragment="audit/00_intro.md",
        response_fields=("audit_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:audit_result",),
        checker_consumer="kernel cleanup-audit checker",
        next_transition="Review",
    ),
    ("stuck_math_audit", "standard"): OperationCapability(
        producer_role="stuck_math_audit",
        request_kind="stuck_math_audit",
        work_kind="standard",
        prompt_fragment="stuck_math_audit/common/02_request_context.md",
        response_fields=("stuck_math_audit_result",),
        writable_artifact_paths=(),
        required_artifacts=("response:stuck_math_audit_result",),
        checker_consumer="kernel stuck-math-audit checker",
        next_transition="Review or HumanGate",
    ),
}


def validate_commissioned_operation(
    request: Mapping[str, object],
    *,
    repo: Path,
) -> OperationCapability:
    """Return the declared capability or refuse before producer dispatch."""
    kind = str(request.get("kind", "") or "").strip().lower()
    work_kind = str(request.get("work_kind", "standard") or "standard").strip().lower()
    capability = OPERATION_CAPABILITIES.get((kind, work_kind))
    if capability is None:
        raise ValueError(
            "REFUSED commissioned operation "
            f"kind={kind or '<missing>'} work_kind={work_kind or '<missing>'}: "
            "no producer capability declaration"
        )

    response_capabilities = set(capability.response_fields)
    required_paths = [
        required.partition(":")[2]
        for required in capability.required_artifacts
        if required.partition(":")[0] == "path"
    ]
    mount_side_artifacts = [*required_paths, *capability.writable_artifact_paths]
    writable_mounts = (
        declared_repo_writable_paths(repo, role=capability.producer_role)
        if mount_side_artifacts
        else []
    )
    for required in capability.required_artifacts:
        carrier, _, name = required.partition(":")
        if carrier == "response":
            if name not in response_capabilities:
                raise ValueError(
                    "REFUSED commissioned operation "
                    f"kind={kind} work_kind={work_kind}: required response artifact "
                    f"{name!r} is outside producer response capabilities"
                )
            continue
        if carrier == "path":
            artifact = (repo / name).resolve()
            if not any(
                artifact == mount or artifact.is_relative_to(mount)
                for mount in writable_mounts
            ):
                raise ValueError(
                    "REFUSED commissioned operation "
                    f"kind={kind} work_kind={work_kind}: required artifact path "
                    f"{name!r} is outside the {capability.producer_role} sandbox"
                )
            continue
        raise ValueError(
            "REFUSED commissioned operation "
            f"kind={kind} work_kind={work_kind}: unknown required artifact carrier {required!r}"
        )

    for relative in capability.writable_artifact_paths:
        artifact = (repo / relative).resolve()
        if not any(
            artifact == mount or artifact.is_relative_to(mount)
            for mount in writable_mounts
        ):
            raise ValueError(
                "REFUSED commissioned operation "
                f"kind={kind} work_kind={work_kind}: declared writable artifact "
                f"{relative!r} is outside the {capability.producer_role} sandbox"
            )
    return capability
