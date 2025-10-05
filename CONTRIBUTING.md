# Contributing to TurboCI

Thank you for contributing to TurboCI! 🎉

## Development Setup

### Prerequisites

- Rust 1.70+
- Redis server
- Git

### Setup

```bash
# Clone the repository
git clone https://github.com/turboci/turboci.git
cd turboci

# Build dependencies
cargo build

# Run tests
cargo test

# Start Redis (using Docker)
docker run -d -p 6379:6379 redis:alpine
```

## Code Style

- Rust formatting: `cargo fmt`
- Linting: `cargo clippy`
- Documentation for each function
- Minimum 80% test coverage

## Pull Request Process

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add amazing feature'`)
4. Push the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## Testing

```bash
# Run all tests
cargo test

# Integration tests
cargo test --test integration

# Specific test
cargo test test_cache_manager
```

## Architecture

- `src/cache/` - Distributed caching system
- `src/optimizer/` - Build optimization logic
- `src/runner/` - Parallel execution engine
- `src/config/` - Configuration management

## Reporting Bugs

When you find a bug, open an issue with:

- Bug description
- Reproduction steps
- Expected vs actual behavior
- Environment (OS, Rust version, etc.)

## Feature Requests

To suggest a new feature:

1. Open an issue with "Feature Request" template
2. Explain the use case
3. Provide implementation details (if available)

## Code of Conduct

- Be respectful
- Provide constructive feedback
- Create an inclusive environment

## License

MIT License - see [LICENSE](LICENSE) file

---

Questions? Join our Discord: [discord.gg/turboci](https://discord.gg/turboci)
