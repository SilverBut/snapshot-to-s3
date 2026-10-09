# Tests

Most tests need nothing but Cargo. Tests that need ZFS, a local S3 service or the
official Tink runtime are opt-in; see [DEVELOPMENT.md](../DEVELOPMENT.md).

## Layout

| Path | What | How to run |
|---|---|---|
| `src/**` `#[cfg(test)]` modules | Unit tests next to the code | `cargo test --lib` |
| `src/testing.rs` | In-memory `MemoryStore` and `FakeZfs` with fault injection; built for unit tests and, through the `test-support` feature, for integration tests. Release builds never include them | — |
| `workflow.rs` | Backup and restore workflows against the fakes: failures, unknown outcomes, tampering, multi-object streams | `cargo test --test workflow` |
| `http_store/` | `HttpStore` against in-process fake HTTP servers. `support/` holds the server, the SigV4 verifier and the environment guard | `cargo test --test http_store` |
| `zfs_backend/` | `SystemZfs` against the fake `zfs`/`zpool` scripts in `fixtures/`, installed by `support.rs` | `cargo test --test zfs_backend` |
| `crypto_stream.rs` | Streaming AEAD framing, tampering and Tink vectors | `cargo test --test crypto_stream` |
| `rate_limit.rs` | Ciphertext rate limiting | `cargo test --test rate_limit` |
| `live_http.rs` | `HttpStore` against a real S3 endpoint (ignored by default) | see DEVELOPMENT.md |
| `fixtures/` | Static inputs; provenance in [fixtures/README.md](fixtures/README.md) | — |
| `e2e/` | ZFS + S3 end-to-end scenarios on a labeled development pool | `tests/e2e/run.sh` |
| `provision/` | Prepares CI VMs and the cloud Copilot environment, then runs E2E | CI only |
| `tooling/` | S3 capability probe, Tink interoperability check, and the Python unit tests for the tooling and `scripts/` | `python3 -m unittest discover -s tests/tooling -p 'test_*.py'` |

Release and CI controllers live in [`scripts/`](../scripts), not here.

## End-to-end scenarios

`e2e/run.sh` sources `e2e/lib/*.sh` and then each `e2e/cases/NN_name.sh` in
numeric order. All cases share one ZFS namespace and S3 prefix; a case declares the
cases it builds on with a `# requires:` line.

```bash
tests/e2e/run.sh --list                # case names
tests/e2e/run.sh --case continue       # this case and the cases it requires
E2E_TRACE=1 tests/e2e/run.sh           # log every command
```

To add a case, create the next `cases/NN_name.sh`, declare its `# requires:` and use
the helpers in `lib/` (`cli`, `expect_failure`, `assert_same_guid`, `mount_restored` and so on).
Cases may only touch datasets inside the namespace that `run.sh` created.

## Conventions

* Never weaken an existing assertion to make a change pass. If the behavior
  changes deliberately, change the expected value and explain why in the commit.
* Bound every buffer and prefer generated streams to large fixtures.
* Python must pass `ruff check .` and `ruff format --check .`. Shell scripts must
  pass `shellcheck -x`.
