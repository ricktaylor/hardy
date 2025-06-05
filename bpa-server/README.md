# Hardy BPA

The Hardy project Bundle Processing Agent

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

### Configuration file

The `hardy-bpa-server` router uses a configuration file in TOML format to specify its behavior. The configuration file allows you to customize various aspects of the router, such as network settings, routing policies, and storage options.

Here is a minimal example of a `hardy-bpa-server` configuration file:

```toml
# The administrative endpoint - You *MUST* change this
administrative_endpoints = "ipn:977000.1.0"

# The local address:port to listen for gRPC requests
grpc_address="[::1]:50051"

# SQLite metadata storage engine specific options
[sqlite]
# Location of the metadata database
db_dir="<fully qualified directory path>"

# Local disk bundle storage engine specific options
[localdisk]
# Root directory of the stored files
store_dir="<fully qualified directory path>"

# static routes options
[static_routes]
# Filepath of static routes file
routes_file = "./static_routes"
```

By default a configuration file named `hardy-bpa-server.toml` is read from:

- `/etc/opt/` on Linux
- `/etc/` on other UNIX's
- `<Executable directory>/` on Windows

To use an alternate configuration file, you can specify the path to the file using the `--config` command line argument. For example:

```
hardy-bpa-server --config /path/to/config.toml
```

The router will then read the configuration file and apply the specified settings during its operation.

Please note that the specific configuration options and their meanings may vary depending on the version of `hardy-bpa-server` you are using. It is recommended to consult the [example configuration file](./example_config.toml) in the project for more detailed information on the available configuration options.

### Command line arguments

You can provide the following arguments on the command line:

- `--config <file>`: Specifies the path to the configuration file for `hardy-bpa-server`.
- `--help`: Displays the help message for `hardy-bpa-server`, showing all available command line options.

Example usage:

```
hardy-bpa-server --config /path/to/config.toml --log-level debug
```
