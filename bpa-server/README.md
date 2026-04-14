# Hardy BPA

The Hardy project Bundle Processing Agent.

Part of the [Hardy](https://github.com/ricktaylor/hardy) DTN Bundle Protocol implementation.

## Table of Contents

- [Installation](#installation)
- [Usage](#usage)

## Installation

To install using Cargo, follow these steps:

1. Make sure you have Rust and Cargo installed on your system. If not, you can install them by following the instructions at [https://www.rust-lang.org/tools/install](https://www.rust-lang.org/tools/install).

2. Open a terminal and navigate to the directory where the project is located.

3. Run the following command to build and install the project:

   ```
   cargo install --path .
   ```

   This will compile the project and install the executable in your system's binary directory.

4. Once the installation is complete, you can use the project by running the command:

   ```
   hardy-bpa-server
   ```

   This will start the BPv7 DTN router for the Cloud.

## Usage

### Configuration

Configuration is read from YAML, TOML, or JSON files and environment
variables (`HARDY_BPA_SERVER_` prefix, `__` for nesting). See the
[User Documentation](https://ricktaylor.github.io/hardy/getting-started/quick-start/)
for the full configuration reference, or the
[example configuration file](./example_config.toml) for a starting point.

```bash
hardy-bpa-server --config /path/to/config.yaml
```

## Container Image

```bash
docker pull ghcr.io/ricktaylor/hardy/hardy-bpa-server:latest
```

## Documentation

- [Design](docs/design.md)
- [Static Routes Design](docs/static_routes_design.md)
- [Test Coverage](docs/test_coverage_report.md)
- [Example Configuration](example_config.toml)
- [User Documentation](https://ricktaylor.github.io/hardy/getting-started/quick-start/)

## Licence

Apache 2.0 — see [LICENSE](../LICENSE)
