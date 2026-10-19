from example_pkg.core import RequestContext, validate_context


def handle_request(req_id: str, uid: int) -> dict[str, str]:
    ctx = RequestContext(request_id=req_id, user_id=uid)
    try:
        ok = validate_context(ctx)
    except ValueError:
        return {"status": "invalid"}
    return {"status": "ok" if ok else "denied"}
