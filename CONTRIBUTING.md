# Contributing to TurboCI

TurboCI loyihasiga hissa qo'shganingiz uchun rahmat! 🎉

## Development Setup

### Prerequisites

- Rust 1.70+
- Redis server
- Git

### Setup

```bash
# Repository'ni clone qiling
git clone https://github.com/turboci/turboci.git
cd turboci

# Dependencies'ni build qiling
cargo build

# Test'larni ishga tushiring
cargo test

# Redis'ni ishga tushiring (Docker bilan)
docker run -d -p 6379:6379 redis:alpine
```

## Code Style

- Rust formatting: `cargo fmt`
- Linting: `cargo clippy`
- Har bir function uchun documentation
- Test coverage minimum 80%

## Pull Request Process

1. Fork qiling repository'ni
2. Feature branch yarating (`git checkout -b feature/amazing-feature`)
3. O'zgarishlarni commit qiling (`git commit -m 'Add amazing feature'`)
4. Branch'ni push qiling (`git push origin feature/amazing-feature`)
5. Pull Request oching

## Testing

```bash
# Barcha test'larni ishga tushirish
cargo test

# Integration test'lar
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

Bug topganingizda, quyidagilar bilan issue oching:

- Bug description
- Reproduction steps
- Expected vs actual behavior
- Environment (OS, Rust version, etc.)

## Feature Requests

Yangi feature taklif qilish uchun:

1. Issue oching "Feature Request" template bilan
2. Use case tushuntiring
3. Implementation details (agar bor bo'lsa)

## Code of Conduct

- Respectful bo'ling
- Constructive feedback bering
- Inclusive muhit yarating

## License

MIT License - [LICENSE](LICENSE) file'ga qarang

---

Savollar bo'lsa Discord'ga qo'shiling: [discord.gg/turboci](https://discord.gg/turboci)
