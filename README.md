# Hardy

A performant, compliant, and extensible BPv7 DTN solution for the Cloud.

## Table of Contents

- [Components](#components)
- [Contributing](#contributing)
- [License](#license)

## Components

The Hardy project is composed of a number of modular components, written in reliable, accessible, asynchronous Rust.  

Every component is designed to be executed independently, and intercommunicate using gRPC APIs, making the solution ideal for hosting in a containerized Cloud environment.

The Hardy project provides a set of components and utility libraries, namely:

1. `cbor`: A Rust library for working with CBOR, providing encoding and decoding of generic types via traits.

1. `bpv7`: A Rust library for working with BPv7 bundles in a generic manner.

1. `proto`: The protobuf v3 specifications of the various gRPC APIs used across the project.

1. [`bpa`](./bpa/README.md): The `hardy-bpa` modular BPv7 Bundle Processing Agent and router.

1. `bpa-api`: A Rust library defining the `hardy-bpa` plugin APIs 

1. `localdisk-storage`: A Rust library implementing a 'bundle storage engine' plugin that uses the local filesystem.

1. `sqlite-storage`: A Rust library implementing a 'metadata storage engine' plugin that uses a local SQLite database.

1. `tcpcl`: A Rust library implementing a TCP-CLv4 (RFC9174) convergence layer adaptor.

## Contributing

We welcome contributions to the Hardy project! If you would like to contribute, please follow these guidelines:

1. Fork the repository and create a new branch for your contribution.
1. Make your changes and ensure that the code follows the project's coding style and conventions.
1. Write tests to cover your changes and ensure that all existing tests pass.
1. Submit a pull request with a clear description of your changes and the problem they solve.

By contributing to Hardy, you agree to license your contributions under the project's license.

Thank you for your interest in contributing to the project!

## License

Hardy is licensed under the [Apache 2.0 License](./LICENSE).
