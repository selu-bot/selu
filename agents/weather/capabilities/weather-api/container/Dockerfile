# PAP Capability Container: Weather API
#
# Multi-stage build:
#   1. Generate Python protobuf/gRPC stubs from capability.proto
#   2. Produce a minimal runtime image with a non-root user
#
# Security:
#   - Runs as UID 1000 (matches PAP runner's user: "1000:1000")
#   - No shell in final image would be ideal, but Python needs it
#   - Minimal dependencies

FROM python:3.12-slim AS builder

WORKDIR /build

COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Generate protobuf + gRPC stubs
COPY capability.proto .
RUN python -m grpc_tools.protoc \
    --proto_path=. \
    --python_out=. \
    --grpc_python_out=. \
    capability.proto

# ── Runtime stage ──────────────────────────────────────────────────────────────

FROM python:3.12-slim

# Install only runtime dependencies (no build tools)
COPY requirements.txt /tmp/requirements.txt
RUN pip install --no-cache-dir -r /tmp/requirements.txt && rm /tmp/requirements.txt

# Create non-root user matching PAP's expected UID/GID
RUN groupadd -g 1000 pap && useradd -u 1000 -g 1000 -m pap

WORKDIR /app

# Copy application code
COPY server.py .

# Copy generated protobuf stubs from builder
COPY --from=builder /build/capability_pb2.py .
COPY --from=builder /build/capability_pb2_grpc.py .

# Switch to non-root user
USER 1000:1000

EXPOSE 50051

CMD ["python", "server.py"]
