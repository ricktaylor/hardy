# Crate README Style Guide

This guide defines the style and content expectations for per-crate `README.md` files in Hardy.

## Purpose

Each crate README is the **entry point for someone encountering the crate for the first time**. It answers: "what is this, why does it exist, and how do I use it?" READMEs serve two audiences:

- **crates.io / docs.rs readers** — evaluating whether to use the crate
- **Internal developers** — understanding a crate's role in the Hardy workspace

## What Belongs in READMEs

- **One-line description** — what the crate does
- **Role in Hardy** — how it fits into the larger system
- **Key features / capabilities**
- **Quick start / usage example** (for binaries and public API crates)
- **Configuration reference** (for server binaries)
- **Links** to design docs, test coverage reports, API documentation, and the relevant section of the [user documentation](https://ricktaylor.github.io/hardy/). Test plans are referenced from the coverage report — do not link them separately from the README

## What Does NOT Belong in READMEs

- **Exhaustive API reference** — that belongs in rustdoc
- **Design rationale** — that belongs in the design doc
- **Test coverage details** — that belongs in the coverage report
- **TODO lists or known issues** — those belong in docs/TODO.md

## Document Structure

### Library Crates

```markdown
# <crate-name>

<One-line description of what this crate does.>

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Overview

<2-3 sentences explaining the crate's role in the system, what standard(s)
it implements, and its relationship to other Hardy crates.>

## Features

- Feature 1
- Feature 2
- Feature flag: `feature-name` — what it enables

## Usage

```rust
// Minimal example showing the primary API
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [API Documentation](https://docs.rs/<crate-name>)
- [User Documentation](https://ricktaylor.github.io/hardy/<relevant-section>/)

## Licence

Apache 2.0 — see [LICENCE](../LICENCE)
```

### Server / Binary Crates

```markdown
# <binary-name>

<One-line description of what this binary does.>

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Quick Start

<3-5 steps to get running, including minimal config and run command.>

## Configuration

<Table of configuration options with defaults and descriptions.>

Configuration is read from TOML files and environment variables
(`<PREFIX>_` prefix).

| Option | Default | Description |
|--------|---------|-------------|
| ... | ... | ... |

## Container Image

```bash
docker pull ghcr.io/ricktaylor/hardy/<image-name>:latest
```

## Documentation

- [Design](docs/design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [User Documentation](https://ricktaylor.github.io/hardy/<relevant-section>/)

## Licence

Apache 2.0 — see [LICENCE](../LICENCE)
```

### Small / Internal Library Crates

For crates under ~200 lines with a single responsibility:

```markdown
# <crate-name>

<One-line description.>

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.
<1-2 sentences on what it does and which crate(s) use it.>

## Licence

Apache 2.0 — see [LICENCE](../LICENCE)
```

## Writing Style

- **Be concise** — READMEs should be scannable. If a section exceeds a screenful, it probably belongs in a separate doc
- **Lead with the most useful information** — what it does, not how it's built
- **Use concrete examples** — a 5-line code snippet is worth a paragraph of description
- **Link, don't duplicate** — point to docs.rs for API details, design docs for rationale, user docs for operations

## Naming Conventions

- **Title**: use the crate name as-is (e.g., `# hardy-bpv7`, not `# BPv7 Library`)
- **Links**: use relative paths within the repo (e.g., `docs/design.md`, not absolute URLs)
- **Images**: avoid unless they add significant clarity; prefer text descriptions

## Questions to Ask When Writing

1. Could someone understand what this crate does from the first two sentences?
2. Is there enough information to start using it without reading the source?
3. Am I duplicating content that lives in another document?
4. Would this be useful on crates.io / docs.rs?
