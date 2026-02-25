"""
PAP Capability Container: Weather API

Implements the pap.capability.v1.Capability gRPC service.
Fetches weather data from the Open-Meteo API (free, no API key required).

All outbound HTTPS traffic goes through the orchestrator's egress proxy,
which enforces the allowlist declared in manifest.yaml.
"""

import json
import logging
import os
import signal
import sys
from concurrent import futures

import grpc
import requests

import capability_pb2
import capability_pb2_grpc

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(message)s",
)
log = logging.getLogger("weather-api")

GRPC_PORT = 50051

# Open-Meteo endpoints
GEOCODING_URL = "https://geocoding-api.open-meteo.com/v1/search"
FORECAST_URL = "https://api.open-meteo.com/v1/forecast"

# WMO Weather interpretation codes → human-readable descriptions
WMO_CODES = {
    0: "Clear sky",
    1: "Mainly clear",
    2: "Partly cloudy",
    3: "Overcast",
    45: "Fog",
    48: "Depositing rime fog",
    51: "Light drizzle",
    53: "Moderate drizzle",
    55: "Dense drizzle",
    56: "Light freezing drizzle",
    57: "Dense freezing drizzle",
    61: "Slight rain",
    63: "Moderate rain",
    65: "Heavy rain",
    66: "Light freezing rain",
    67: "Heavy freezing rain",
    71: "Slight snowfall",
    73: "Moderate snowfall",
    75: "Heavy snowfall",
    77: "Snow grains",
    80: "Slight rain showers",
    81: "Moderate rain showers",
    82: "Violent rain showers",
    85: "Slight snow showers",
    86: "Heavy snow showers",
    95: "Thunderstorm",
    96: "Thunderstorm with slight hail",
    99: "Thunderstorm with heavy hail",
}


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


def geocode(city: str) -> tuple[float, float, str]:
    """Geocode a city name → (lat, lon, display_name) via Open-Meteo."""
    resp = HTTP.get(
        GEOCODING_URL,
        params={"name": city, "count": 1, "language": "en", "format": "json"},
        timeout=10,
    )
    resp.raise_for_status()
    data = resp.json()

    results = data.get("results")
    if not results:
        raise ValueError(f"City not found: '{city}'")

    hit = results[0]
    display = hit.get("name", city)
    country = hit.get("country", "")
    admin = hit.get("admin1", "")
    parts = [p for p in [display, admin, country] if p]
    return hit["latitude"], hit["longitude"], ", ".join(parts)


def get_forecast(lat: float, lon: float) -> dict:
    """Fetch current weather + 7-day forecast from Open-Meteo."""
    resp = HTTP.get(
        FORECAST_URL,
        params={
            "latitude": lat,
            "longitude": lon,
            "current": "temperature_2m,relative_humidity_2m,apparent_temperature,"
                       "weather_code,wind_speed_10m,wind_direction_10m",
            "daily": "weather_code,temperature_2m_max,temperature_2m_min,"
                     "precipitation_probability_max,precipitation_sum,"
                     "sunrise,sunset",
            "timezone": "auto",
            "forecast_days": 7,
        },
        timeout=10,
    )
    resp.raise_for_status()
    return resp.json()


def format_forecast(raw: dict, location_name: str) -> dict:
    """Transform the Open-Meteo response into a clean JSON result."""
    current = raw.get("current", {})
    daily = raw.get("daily", {})
    tz = raw.get("timezone", "UTC")

    result = {
        "location": location_name,
        "timezone": tz,
        "current": {
            "temperature_c": current.get("temperature_2m"),
            "feels_like_c": current.get("apparent_temperature"),
            "humidity_pct": current.get("relative_humidity_2m"),
            "wind_speed_kmh": current.get("wind_speed_10m"),
            "wind_direction_deg": current.get("wind_direction_10m"),
            "condition": WMO_CODES.get(current.get("weather_code", -1), "Unknown"),
        },
        "daily_forecast": [],
    }

    dates = daily.get("time", [])
    for i, date in enumerate(dates):
        result["daily_forecast"].append({
            "date": date,
            "high_c": daily.get("temperature_2m_max", [None])[i],
            "low_c": daily.get("temperature_2m_min", [None])[i],
            "condition": WMO_CODES.get(
                (daily.get("weather_code") or [None])[i], "Unknown"
            ),
            "precipitation_probability_pct": (
                daily.get("precipitation_probability_max") or [None]
            )[i],
            "precipitation_mm": (daily.get("precipitation_sum") or [None])[i],
            "sunrise": (daily.get("sunrise") or [None])[i],
            "sunset": (daily.get("sunset") or [None])[i],
        })

    return result


class CapabilityServicer(capability_pb2_grpc.CapabilityServicer):
    """Implements the pap.capability.v1.Capability gRPC service."""

    def Healthcheck(self, request, context):
        return capability_pb2.HealthResponse(ready=True, message="weather-api ready")

    def Invoke(self, request, context):
        tool = request.tool_name
        log.info("Invoke tool=%s", tool)

        if tool != "get_forecast":
            return capability_pb2.InvokeResponse(
                error=f"Unknown tool: '{tool}'"
            )

        try:
            args = json.loads(request.args_json) if request.args_json else {}

            city = args.get("city")
            lat = args.get("latitude")
            lon = args.get("longitude")

            # Resolve location
            if city:
                lat, lon, location_name = geocode(city)
            elif lat is not None and lon is not None:
                location_name = f"{lat:.2f}°, {lon:.2f}°"
            else:
                return capability_pb2.InvokeResponse(
                    error="Either 'city' or both 'latitude' and 'longitude' must be provided."
                )

            raw = get_forecast(lat, lon)
            result = format_forecast(raw, location_name)

            return capability_pb2.InvokeResponse(
                result_json=json.dumps(result).encode("utf-8")
            )

        except Exception as e:
            log.exception("Error during get_forecast invocation")
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
    log.info("Weather API capability listening on port %d", GRPC_PORT)

    # Graceful shutdown on SIGTERM (Docker stop)
    def _shutdown(signum, frame):
        log.info("Shutting down...")
        server.stop(grace=5)
        sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)
    server.wait_for_termination()


if __name__ == "__main__":
    serve()
