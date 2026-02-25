"""
PAP Capability Container: HomeKit API

Implements the pap.capability.v1.Capability gRPC service.
Proxies requests to the HomeKit Bridge macOS app's REST API
running on the Docker host at port 18089.

All outbound HTTP traffic goes through the orchestrator's egress proxy,
which enforces the allowlist declared in manifest.yaml.
"""

import json
import logging
import os
import signal
import sys
from concurrent import futures
from typing import Optional

import grpc
import requests

import capability_pb2
import capability_pb2_grpc

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
log = logging.getLogger("homekit-api")

GRPC_PORT = 50051

# The HomeKit Bridge REST API runs on the Docker host.
# Traffic goes through the orchestrator's egress proxy, which runs on the
# host itself, so we target 127.0.0.1 (the proxy connects from the host).
BRIDGE_BASE_URL = os.environ.get(
    "HOMEKIT_BRIDGE_URL", "http://127.0.0.1:18089"
)


def _get_session() -> requests.Session:
    """
    Build an HTTP session that respects the egress proxy env vars
    injected by the orchestrator (HTTP_PROXY / HTTPS_PROXY).
    """
    session = requests.Session()
    http_proxy = os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy")
    https_proxy = os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy")
    if http_proxy or https_proxy:
        session.proxies = {
            "http": http_proxy,
            "https": https_proxy,
        }
        log.info("Using egress proxy: http=%s https=%s", http_proxy, https_proxy)
    return session


HTTP = _get_session()


def _bridge_get(path: str) -> dict:
    """GET request to the HomeKit Bridge."""
    url = f"{BRIDGE_BASE_URL}{path}"
    log.info("GET %s", url)
    resp = HTTP.get(url, timeout=10)
    resp.raise_for_status()
    return resp.json()


def _bridge_post(path: str, body: Optional[dict] = None) -> dict:
    """POST request to the HomeKit Bridge."""
    url = f"{BRIDGE_BASE_URL}{path}"
    log.info("POST %s body=%s", url, json.dumps(body) if body else "{}")
    resp = HTTP.post(url, json=body or {}, timeout=10)
    resp.raise_for_status()
    return resp.json()


# ── Tool implementations ──────────────────────────────────────────────────────


def tool_list_homes(args: dict) -> dict:
    """List all HomeKit homes."""
    return _bridge_get("/homes")


def tool_list_accessories(args: dict) -> dict:
    """List all accessories in a home."""
    home_id = args.get("home_id")
    if not home_id:
        raise ValueError("'home_id' is required")
    return _bridge_get(f"/homes/{home_id}/accessories")


def tool_list_rooms(args: dict) -> dict:
    """List all rooms in a home."""
    home_id = args.get("home_id")
    if not home_id:
        raise ValueError("'home_id' is required")
    return _bridge_get(f"/homes/{home_id}/rooms")


def tool_set_characteristic(args: dict) -> dict:
    """Set a characteristic value on an accessory."""
    home_id = args.get("home_id")
    accessory_id = args.get("accessory_id")
    characteristic = args.get("characteristic")
    value = args.get("value")

    if not home_id:
        raise ValueError("'home_id' is required")
    if not accessory_id:
        raise ValueError("'accessory_id' is required")
    if not characteristic:
        raise ValueError("'characteristic' is required")
    if value is None:
        raise ValueError("'value' is required")

    return _bridge_post(
        f"/homes/{home_id}/accessories/{accessory_id}/set",
        {"characteristic": characteristic, "value": value},
    )


def tool_list_scenes(args: dict) -> dict:
    """List all scenes in a home."""
    home_id = args.get("home_id")
    if not home_id:
        raise ValueError("'home_id' is required")
    return _bridge_get(f"/homes/{home_id}/scenes")


def tool_execute_scene(args: dict) -> dict:
    """Execute a scene."""
    home_id = args.get("home_id")
    scene_id = args.get("scene_id")

    if not home_id:
        raise ValueError("'home_id' is required")
    if not scene_id:
        raise ValueError("'scene_id' is required")

    return _bridge_post(f"/homes/{home_id}/scenes/{scene_id}/execute")


# Tool dispatch table
TOOLS = {
    "list_homes": tool_list_homes,
    "list_accessories": tool_list_accessories,
    "list_rooms": tool_list_rooms,
    "set_characteristic": tool_set_characteristic,
    "list_scenes": tool_list_scenes,
    "execute_scene": tool_execute_scene,
}


# ── gRPC service ──────────────────────────────────────────────────────────────


class CapabilityServicer(capability_pb2_grpc.CapabilityServicer):
    """Implements the pap.capability.v1.Capability gRPC service."""

    def Healthcheck(self, request, context):
        # Optionally verify the bridge is reachable
        try:
            resp = HTTP.get(f"{BRIDGE_BASE_URL}/health", timeout=5)
            if resp.status_code == 200:
                return capability_pb2.HealthResponse(
                    ready=True, message="homekit-api ready, bridge reachable"
                )
        except Exception as e:
            log.warning("Bridge health check failed: %s", e)

        # Still report ready -- the bridge might start later
        return capability_pb2.HealthResponse(
            ready=True,
            message="homekit-api ready, bridge not yet reachable",
        )

    def Invoke(self, request, context):
        tool = request.tool_name
        log.info("Invoke tool=%s", tool)

        handler = TOOLS.get(tool)
        if not handler:
            return capability_pb2.InvokeResponse(
                error=f"Unknown tool: '{tool}'"
            )

        try:
            args = json.loads(request.args_json) if request.args_json else {}
            result = handler(args)
            return capability_pb2.InvokeResponse(
                result_json=json.dumps(result).encode("utf-8")
            )
        except requests.exceptions.ConnectionError:
            return capability_pb2.InvokeResponse(
                error="Cannot reach the HomeKit Bridge. "
                      "Is the HomeKitBridge app running on the host?"
            )
        except requests.exceptions.HTTPError as e:
            return capability_pb2.InvokeResponse(
                error=f"HomeKit Bridge returned an error: {e.response.text}"
            )
        except Exception as e:
            log.exception("Error during %s invocation", tool)
            return capability_pb2.InvokeResponse(error=str(e))

    def StreamInvoke(self, request, context):
        # Tool-class capabilities don't use streaming; return a single chunk.
        resp = self.Invoke(request, context)
        if resp.error:
            yield capability_pb2.InvokeChunk(error=resp.error, done=True)
        else:
            yield capability_pb2.InvokeChunk(data=resp.result_json, done=True)


def serve():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=4))
    capability_pb2_grpc.add_CapabilityServicer_to_server(
        CapabilityServicer(), server
    )
    server.add_insecure_port(f"0.0.0.0:{GRPC_PORT}")
    server.start()
    log.info("HomeKit API capability listening on port %d", GRPC_PORT)

    # Graceful shutdown on SIGTERM (Docker stop)
    def _shutdown(signum, frame):
        log.info("Shutting down...")
        server.stop(grace=5)
        sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)
    server.wait_for_termination()


if __name__ == "__main__":
    serve()
