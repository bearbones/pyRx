from dataclasses import dataclass


@dataclass
class RequestContext:
    request_id: str
    user_id: int


def validate_context(ctx: RequestContext) -> bool:
    if not ctx.request_id:
        raise ValueError("missing request_id")
    return ctx.user_id > 0
