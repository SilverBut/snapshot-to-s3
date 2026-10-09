# Agent guide

Open only the documents your task needs. Each one is the single source for its topic.

## Always

* **ZFS pools are shared with the host.** Never create, import, export, destroy or relabel pools. Use an
  existing `ONLINE` pool whose pool property `user:isdev` is `yes`, and only inside your own new child
  dataset. Read [ZFS development pools](DEVELOPMENT.md#zfs-development-pools) before running any `zfs` or
  `zpool` command.
* Never run `tests/provision/ci_e2e.sh` or the cloud setup bootstrap. They own and destroy pools.
* Never weaken or delete a test to make a change pass.
* Release workflows are started manually (`workflow_dispatch`) only.

## When you need to

| Task | Read |
| --- | --- |
| Build, lint and test before a PR | [CONTRIBUTING: build and check](CONTRIBUTING.md#build-and-check) |
| Find the module that owns a feature | [design: architecture](docs/design.md#architecture) |
| Touch object layout, encryption, metadata or locking | [storage.md](docs/storage.md) |
| Change backup or restore steps | [workflow.md](docs/workflow.md) |
| Add or change tests, fakes or fixtures | [tests/README.md](tests/README.md), [engineering: writing tests](docs/engineering.md#writing-tests) |
| Refactor, or improve code and test quality | [engineering.md](docs/engineering.md) |
| Run the ZFS, S3 or Tink tests locally | [DEVELOPMENT: local E2E](DEVELOPMENT.md#local-end-to-end-tests) |
| Work in the cloud Copilot VM (`target/copilot-dev/env.sh` exists) | [DEVELOPMENT: cloud handoff](DEVELOPMENT.md#cloud-copilot-handoff) |
| Run heavy checks (E2E, mutation testing) or read CI | [engineering: validation](docs/engineering.md#validation-and-resources), [DEVELOPMENT: CI](DEVELOPMENT.md#ci) |
| Prepare or publish a release | [CONTRIBUTING: releases](CONTRIBUTING.md#releases) |
