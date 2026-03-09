# cb-greenfield

Greenfield Commit-Boost platform prototype that encodes invariant-driven behavior from day one.

## Modules
- `api_gateway`: Unified HTTP routing for PBS + signer endpoints.
- `pbs_engine`: Relay fanout, retries, capability cache, v2 fallback logic.
- `policies`: Registration selection, retry policy, validation engine.
- `observability`: In-memory status/fallback/wire-version counters.
- `testkit::fake_relay`: Persisted registrations + debug APIs for integration tests.

## Guarantees encoded in code
1. Every outbound attempt records one status-code event.
2. Registration retries are bounded by absolute deadline.
3. v2 fallback is only triggered by `404` in auto mode.
4. Capability cache is per-relay and runtime-scoped.
5. Request bodies are size-limited at gateway level.

## Run
```bash
cargo run -p cb-greenfield --bin commit-boost-greenfield
```

## Property tests
```bash
cargo test -p cb-greenfield
```
