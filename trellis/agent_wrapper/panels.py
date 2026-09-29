"""Panel execution helpers built on top of the single-request wrapper."""

from __future__ import annotations

import concurrent.futures
from typing import List, Optional

from .executor import LanePortResolver, execute_agent_request
from .protocol import (
    PanelExecutionResponse,
    PanelRequest,
    SingleAgentResponse,
)


def execute_panel_raw(
    request: PanelRequest,
    *,
    port_resolver: Optional[LanePortResolver] = None,
) -> PanelExecutionResponse:
    if not request.members:
        return PanelExecutionResponse(
            request_id=request.request_id,
            cycle=request.cycle,
            kind=request.kind,
            member_responses=[],
        )

    members: List[SingleAgentResponse] = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(request.members)) as pool:
        futures = [
            pool.submit(execute_agent_request, member, port_resolver=port_resolver)
            for member in request.members
        ]
        for future in concurrent.futures.as_completed(futures):
            members.append(future.result())

    members.sort(key=lambda item: item.request_id)
    return PanelExecutionResponse(
        request_id=request.request_id,
        cycle=request.cycle,
        kind=request.kind,
        member_responses=members,
    )
