# Contributing to snapshot-to-s3

Thank you for your interest in contributing to snapshot-to-s3! This document provides guidelines and information for contributors.

## Table of Contents

- [Development Setup](#development-setup)
- [Code Style](#code-style)
- [Testing](#testing)
- [Submitting Changes](#submitting-changes)
- [Versioning and Releases](#versioning-and-releases)

## Development Setup

### Prerequisites

- Rust 1.70 or later
- Cargo (comes with Rust)

### Building the Project

```bash
# Clone the repository
git clone https://github.com/SilverBut/snapshot-to-s3.git
cd snapshot-to-s3

# Build the project
cargo build

# Run tests
cargo test

# Run the application
cargo run -- --help
```

## Code Style

This project follows standard Rust conventions and uses automated tools to enforce them:

### Formatting

We use `rustfmt` to maintain consistent code formatting:

```bash
# Check formatting
cargo fmt --all -- --check

# Apply formatting
cargo fmt --all
```

All code must be formatted before submitting a pull request.

### Linting

We use `clippy` for linting and catching common mistakes:

```bash
# Run clippy
cargo clippy --all-targets --all-features -- -D warnings
```

All clippy warnings must be resolved before merging.

## Testing

### Running Tests

```bash
# Run all tests
cargo test

# Run tests with verbose output
cargo test --verbose

# Run a specific test
cargo test test_name
```

### Writing Tests

- Unit tests should be placed in the same file as the code they test, in a `tests` module
- Integration tests should be placed in the `tests/` directory
- All new features should include appropriate tests

## Submitting Changes

1. **Fork the repository** and create a new branch from `main`

2. **Make your changes**:
   - Write clear, concise commit messages
   - Follow the code style guidelines
   - Add tests for new functionality
   - Update documentation as needed

3. **Test your changes**:
   ```bash
   cargo test
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   ```

4. **Submit a pull request**:
   - Provide a clear description of the changes
   - Reference any related issues
   - Ensure all CI checks pass

## Versioning and Releases

This project follows [Semantic Versioning](https://semver.org/) (SemVer):

- **MAJOR version** (X.0.0): Incompatible API changes
- **MINOR version** (0.X.0): New functionality in a backward-compatible manner
- **PATCH version** (0.0.X): Backward-compatible bug fixes

### Tag Format Rules

Tags must follow these format rules to trigger automated releases:

#### Stable Releases

Stable releases use the format: `vMAJOR.MINOR.PATCH`

Examples:
- `v1.0.0` - First major release
- `v1.2.3` - Minor and patch updates
- `v2.0.0` - Breaking changes

#### Pre-releases

Pre-release tags include a suffix and are marked as pre-releases in GitHub:

- **Alpha releases**: `vMAJOR.MINOR.PATCH-alpha.N`
  - Example: `v1.0.0-alpha.1`, `v1.0.0-alpha.2`
  - Early development, unstable features

- **Beta releases**: `vMAJOR.MINOR.PATCH-beta.N`
  - Example: `v1.0.0-beta.1`, `v1.0.0-beta.2`
  - Feature complete, testing phase

- **Release candidates**: `vMAJOR.MINOR.PATCH-rc.N`
  - Example: `v1.0.0-rc.1`, `v1.0.0-rc.2`
  - Final testing before stable release

- **Pre-releases**: `vMAJOR.MINOR.PATCH-pre.N`
  - Example: `v1.0.0-pre.1`
  - General pre-release versions

Where `N` is a sequential number starting from 1.

### Creating a Release

Only maintainers can create releases:

1. **Update version** in `Cargo.toml`

2. **Update CHANGELOG.md** (if it exists) with release notes

3. **Commit the changes**:
   ```bash
   git commit -am "Bump version to X.Y.Z"
   git push
   ```

4. **Create and push a tag**:
   ```bash
   # For stable release
   git tag vX.Y.Z
   
   # For pre-release
   git tag vX.Y.Z-alpha.1
   
   # Push the tag
   git push origin vX.Y.Z
   ```

5. The GitHub Actions workflow will automatically:
   - Build a static x86_64 Linux binary
   - Create a GitHub Release
   - Mark it as pre-release or stable based on the tag format
   - Upload the binary and SHA256 checksum

### Release Assets

Each release includes:
- `snapshot-to-s3-linux-x86_64.tar.gz` - Statically linked binary for Linux x86_64
- `snapshot-to-s3-linux-x86_64.tar.gz.sha256` - SHA256 checksum for verification

To verify a downloaded release:
```bash
sha256sum -c snapshot-to-s3-linux-x86_64.tar.gz.sha256
```

## Questions?

If you have questions or need help, please:
- Open an issue on GitHub
- Check existing issues and pull requests

Thank you for contributing to snapshot-to-s3!
