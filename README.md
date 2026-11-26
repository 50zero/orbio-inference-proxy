# orbio-inference-proxy

TEE sidecar for Orbio's private inference box. It sits next to SGLang inside a Phala Cloud GPU CVM and speaks the subset of NEAR's public cloud API that the Orbio gateway already verifies.

Fork of [nearai/inference-proxy](https://github.com/nearai/inference-proxy) at commit **`37c3f6d3a4e0c0423354acc8f20caea5da9aa1b6`** (full history is in this repository). See [NOTICE.md](NOTICE.md). This fork does not add a license.

Deployment target:

- Phala Cloud GPU TEE, 4x NVIDIA B300 (Blackwell), dstack OS v0.6.5, Intel TDX
- SGLang at `http://sglang:30000` (OpenAI-compatible, no API key on the internal network). DeepSeek V4.1 parsers: `--reasoning-parser deepseek-v41` and `--tool-call-parser deepseekv41`
- Model id `orbio/deepseek-v4.1-flash-uncensored`
- Public URL `https://gpu.orbiocompute.com` — dstack-ingress terminates TLS inside the CVM, Caddy allowlists, this proxy listens on `:8000`
- Prompts and completions are not logged and are not written to disk

## What changed from upstream

- `GPU_ARCH` (default `BLACKWELL`, also `HOPPER`) replaces the hard-coded NRAS arch. `nvidia_payload` is `{nonce, evidence_list, arch}` with the client nonce and one evidence entry per GPU (`GPU_COUNT`, default 4).
- GPU nonce self-check no longer fails open on an unknown SPDM header. A GET_MEASUREMENTS request (SPDM 1.0–1.4, including Hopper `0x11` and later Blackwell versions) must carry the nonce at offset 4. Any other layout is accepted only when the nonce bytes (or their hex) occur in the evidence; otherwise the proxy retries and then fails closed.
- `GET /v1/attestation/report` accepts the NEAR query (`model`, `provider`, `signing_algo`, `include_tls_fingerprint`, `nonce`, `signing_address`), with or without `Authorization` and `x-no-aliasing`. The JSON keeps the flat report and `all_attestations`, and adds `model_attestations`. With `include_tls_fingerprint=false`, TDX `report_data` is `ed25519_pubkey (32) || nonce (32)`. A signing address that is not this instance is HTTP 503 and the body contains `Signing address not found`. A different `model` is 404.
- `POST /v1/chat/completions` still does NEAR E2EE v1–v3. For v2 (`X-Encryption-Version: 2`, `X-Encrypt-All-Fields: true`) fields are `hex(ephemeral X25519 pk || 24-byte nonce || XChaCha20-Poly1305 ciphertext+tag)` after Ed25519→X25519, ephemeral ECDH, and HKDF-SHA256 with no salt and info `ed25519_encryption`. `X-Model-Pub-Key`, when present, must be this instance's key or the proxy returns 421 before generating. Buffered JSON responses carry `inference-id` (UUIDv4 when usage reporting did not set one). Plaintext requests, streaming, and tool calls still work.
- `GET /v1/signature/{id}?signing_algo=ed25519` adds `signature_kind: "provider_tee"`. `text` is `<model>:<sha256 hex of the exact request bytes>:<sha256 hex of the exact response bytes>`. The proxy does not compress or re-encode that response.
- `GET /v1/model/list` (no auth) returns the one model in NEAR's catalog shape. `GET /v1/models` and `GET /healthz` are unchanged.
- Signing keys still come from dstack KMS at `<MODEL_NAME>/ed25519-signing-key` and `<MODEL_NAME>/ecdsa-signing-key`, so they survive reboot for the same app id.

## Environment

| Variable | Default | Role |
| --- | --- | --- |
| `MODEL_NAME` | required | Served model id and KMS key path prefix |
| `TOKEN` | required | Bearer token for chat and signature. Attestation and `/v1/model/list` do not use it |
| `VLLM_BASE_URL` | `http://localhost:8000` | SGLang/vLLM origin. Set `http://sglang:30000` |
| `GPU_ARCH` | `BLACKWELL` | NRAS `arch`. `HOPPER` or `BLACKWELL` |
| `GPU_COUNT` | `4` | Required evidence entries. `0` disables the check |
| `CONTEXT_LENGTH` | `262144` | `metadata.contextLength` |
| `MAX_OUTPUT_LENGTH` | `8192` | `metadata.maxOutputLength` |
| `INPUT_COST_PER_TOKEN_AMOUNT` | `0` | Nano-dollars when `COST_SCALE=9` |
| `OUTPUT_COST_PER_TOKEN_AMOUNT` | `0` | Same |
| `COST_PER_IMAGE_AMOUNT` | `0` | NEAR requires `costPerImage` on each model |
| `COST_SCALE` | `9` | NEAR fixed scale (nano-dollars) |
| `COST_CURRENCY` | `USD` | NEAR `DecimalPrice.currency` |
| `VLLM_PROXY_MAX_REQUEST_SIZE` | `10485760` | Max chat/completions body bytes |
| `VLLM_PROXY_STRICT_REQUEST_MODEL` | off | Set `1` so a body `model` other than `MODEL_NAME` is rejected |
| `LISTEN_PORT` | `8000` | Listen port |

NEAR's public `GET /v1/model/list` ([docs](https://docs.near.ai/api-reference/models/list-models-with-pricing)) uses `DecimalPrice`:

```json
{"amount": 150, "scale": 9, "currency": "USD"}
```

`amount` is an integer at `scale` decimal places. Scale 9 means nano-dollars (`150` = `$0.000000150` per token). This proxy uses that object for `inputCostPerToken`, `outputCostPerToken`, and `costPerImage`. The list response also echoes `limit` (default 100), `offset` (default 0), and `total`.

Upstream still reads many other `VLLM_PROXY_*` variables. Leave them unset on this box.

## Endpoints

All under `/v1` except health.

- `GET /v1/attestation/report?model=&provider=&signing_algo=ed25519&include_tls_fingerprint=false&nonce=<64 lowercase hex>[&signing_address=<64 hex>]`
- `POST /v1/chat/completions` — optional E2EE headers `X-Signing-Algo`, `X-Client-Pub-Key`, `X-Model-Pub-Key`, `X-Encryption-Version`, `X-Encrypt-All-Fields`
- `GET /v1/signature/{id}?signing_algo=ed25519` — `Authorization: Bearer <TOKEN>`
- `GET /v1/model/list?limit=&offset=` — no auth
- `GET /v1/models` — proxied to SGLang, no auth
- `GET /healthz` — backend readiness

`provider` is accepted and ignored. `x-no-aliasing: true` is accepted; a `model` that is not `MODEL_NAME` is rejected rather than rewritten.

## Compose (dstack)

Pin the image by digest after the first GHCR publish (`sha-<shortsha>` and `latest` are moving tags). Replace `<digest>` with the digest printed in the image workflow summary.

```yaml
services:
  sglang:
    # Pin the SGLang image by digest in the real compose. DeepSeek V4.1
    # parsers are deepseek-v41 / deepseekv41. Served id matches MODEL_NAME.
    command:
      - python3
      - -m
      - sglang.launch_server
      - --model-path
      - orbio/deepseek-v4.1-flash-uncensored
      - --served-model-name
      - orbio/deepseek-v4.1-flash-uncensored
      - --host
      - 0.0.0.0
      - --port
      - "30000"
      - --reasoning-parser
      - deepseek-v41
      - --tool-call-parser
      - deepseekv41
  inference-proxy:
    image: ghcr.io/50zero/orbio-inference-proxy@sha256:<digest>
    user: "65532:65532"
    environment:
      MODEL_NAME: orbio/deepseek-v4.1-flash-uncensored
      VLLM_BASE_URL: http://sglang:30000
      TOKEN: ${TOKEN}
      GPU_ARCH: BLACKWELL
      GPU_COUNT: "4"
      CONTEXT_LENGTH: "262144"
      MAX_OUTPUT_LENGTH: "8192"
      INPUT_COST_PER_TOKEN_AMOUNT: "0"
      OUTPUT_COST_PER_TOKEN_AMOUNT: "0"
      COST_SCALE: "9"
      COST_CURRENCY: USD
      VLLM_PROXY_MAX_REQUEST_SIZE: "10485760"
      VLLM_PROXY_STRICT_REQUEST_MODEL: "1"
      NVIDIA_VISIBLE_DEVICES: all
      NVIDIA_DRIVER_CAPABILITIES: compute,utility
      LISTEN_PORT: "8000"
    volumes:
      - /var/run/dstack.sock:/var/run/dstack.sock
    deploy:
      resources:
        reservations:
          devices:
            - driver: nvidia
              count: all
              capabilities: [gpu]
```

No `privileged: true`. The NVIDIA container runtime mounts the driver and `/dev/nvidia*` for `count: all`. The process uid is 65532, so those nodes and `/var/run/dstack.sock` must be readable by that user (dstack guests usually mode the socket `0666`). If evidence collection fails with `EPERM` on NVML, fix the device-node mode or group rather than enabling privileged. That has not been checked on a B300 host; see below.

## curl

```bash
# Catalog (no auth)
curl -sS 'https://gpu.orbiocompute.com/v1/model/list?limit=10&offset=0'

# Attestation. Save signing_public_key from model_attestations[0].
NONCE="$(openssl rand -hex 32)"
curl -sS -G 'https://gpu.orbiocompute.com/v1/attestation/report' \
  --data-urlencode 'model=orbio/deepseek-v4.1-flash-uncensored' \
  --data-urlencode 'provider=orbio' \
  --data-urlencode 'signing_algo=ed25519' \
  --data-urlencode 'include_tls_fingerprint=false' \
  --data-urlencode "nonce=${NONCE}" \
  -H 'x-no-aliasing: true'

# Plaintext chat (zero data retention; body is not logged)
curl -sS -D - 'https://gpu.orbiocompute.com/v1/chat/completions' \
  -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' \
  -d '{"model":"orbio/deepseek-v4.1-flash-uncensored","stream":false,"messages":[{"role":"user","content":"ping"}]}'

# E2EE v2. Encrypt each field as hex(epk || nonce || ct) to the attested
# model key before sending. X-Model-Pub-Key is that key (64 lowercase hex).
curl -sS -D - 'https://gpu.orbiocompute.com/v1/chat/completions' \
  -H "Authorization: Bearer ${TOKEN}" \
  -H 'Content-Type: application/json' \
  -H 'X-Signing-Algo: ed25519' \
  -H "X-Client-Pub-Key: ${CLIENT_ED25519_PUB}" \
  -H "X-Model-Pub-Key: ${MODEL_ED25519_PUB}" \
  -H 'X-Encryption-Version: 2' \
  -H 'X-Encrypt-All-Fields: true' \
  -d @encrypted-chat.json

# Signature over the exact bytes of that request and the response body.
curl -sS -H "Authorization: Bearer ${TOKEN}" \
  "https://gpu.orbiocompute.com/v1/signature/${INFERENCE_RESPONSE_ID}?signing_algo=ed25519"
```

`inference-id` is the response header (a UUIDv4 on this deployment). The signature path uses the JSON `id`.

## Image

`.github/workflows/image.yml` builds `linux/amd64` on push to `main` and on tags, pushes `ghcr.io/50zero/orbio-inference-proxy:sha-<shortsha>` and `:latest` with `GITHUB_TOKEN` (`packages: write`), and prints the digest in the job summary. The Dockerfile is multi-stage, bases are pinned by digest, `cargo build --release --locked` is used, and the process runs as uid 65532.

Upstream's Docker Hub publish and the bit-reproducible verifier workflow are manual (`workflow_dispatch`) on this fork.

## Verify on real B300 hardware

Unit tests mock dstack and NVML. These still need a 4x B300 CVM on dstack OS 0.6.5:

1. Live `nvidia_payload` is accepted unchanged by NVIDIA NRAS `POST /v3/attest/gpu` with `arch: BLACKWELL`, and the list contains four GPU evidence entries.
2. The driver evidence either starts with an SPDM GET_MEASUREMENTS request whose nonce is at offset 4, or embeds the raw nonce (or its hex) somewhere the fail-closed scan can see. A layout that hashes the nonce and does not embed it will be rejected here until the parser is updated from a real capture.
3. NVML evidence collection works as uid 65532 without `privileged`, with `NVIDIA_VISIBLE_DEVICES=all` and `count: all`.
4. `/var/run/dstack.sock` is readable by uid 65532. `GetKey` for `<MODEL_NAME>/ed25519-signing-key` returns the same key after reboot for the same app id. `/Info` still deserializes with the vendored dstack SDK 0.1.3 types (`info.compose_hash`, `info.tcb_info.app_compose`). Unknown fields are ignored; a renamed required field fails quote generation.
5. The TDX quote's `report_data` (not only the JSON echo) is the 32-byte Ed25519 public key concatenated with the 32-byte client nonce when `include_tls_fingerprint=false`. Intel DCAP accepts the quote for this Phala TDX platform.
6. A real request's prompt does not appear in container logs or on the serial console. The in-repo test only covers this process's tracing subscriber.
