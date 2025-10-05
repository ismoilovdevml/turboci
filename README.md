# ⚡ TurboCI - Super Fast CI/CD Runner

TurboCI - CI/CD jarayonlarini 5-10 barobar tezlashtiruvchi zamonaviy runner. Distributed caching, incremental builds va parallel execution imkoniyatlari bilan.

## 🚀 Asosiy Xususiyatlar

### 1. **Distributed Build Cache**
- Redis orqali tarqatilgan kesh tizimi
- BLAKE3 hash algoritmi (juda tez)
- Build natijalarini saqlash va qayta ishlatish
- Cache hit rate monitoring

### 2. **Incremental Build Optimizer**
- Faqat o'zgargan fayllarni build qiladi
- Git diff tahlili
- Dependency tracking
- Smart cache invalidation

### 3. **Parallel Test Runner**
- Barcha CPU core'lardan foydalanish
- Test'larni parallel ravishda bajarish
- Rayon library yordamida
- Real-time progress tracking

### 4. **GitHub Actions Integration**
- Custom GitHub Action
- Auto-setup Redis
- Cache statistics
- Easy configuration

## 📦 O'rnatish

### Binary orqali (Tez)
```bash
curl -sSL https://turboci.dev/install.sh | sh
```

### Cargo orqali
```bash
cargo install turboci
```

### Source'dan build qilish
```bash
git clone https://github.com/turboci/turboci.git
cd turboci
cargo build --release
```

## 🎯 Ishlatish

### 1. Konfiguratsiya yaratish

`turboci.yml` faylini yarating:

```yaml
name: My Fast Pipeline
version: "1.0"

cache:
  redis_url: "redis://127.0.0.1:6379"
  ttl_seconds: 86400
  enabled: true

jobs:
  - name: build
    parallel: false
    steps:
      - name: Install dependencies
        run: npm install

      - name: Build project
        run: npm run build

  - name: test
    parallel: true
    steps:
      - name: Unit tests
        run: npm run test:unit

      - name: Integration tests
        run: npm run test:integration

  - name: lint
    parallel: true
    steps:
      - name: ESLint
        run: npm run lint
```

### 2. Redis'ni ishga tushirish

```bash
# Docker bilan
docker run -d -p 6379:6379 redis:alpine

# Yoki local o'rnatish
# macOS
brew install redis
brew services start redis

# Ubuntu/Debian
sudo apt-get install redis-server
sudo systemctl start redis
```

### 3. Pipeline'ni ishga tushirish

```bash
# Cache'ni initialize qilish
turboci init-cache

# Pipeline'ni ishga tushirish
turboci run --config turboci.yml

# Cache statistikasini ko'rish
turboci cache-stats
```

## 🔧 GitHub Actions'da Ishlatish

`.github/workflows/turboci.yml` yarating:

```yaml
name: TurboCI Pipeline

on: [push, pull_request]

jobs:
  turboci:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3

      - name: Run TurboCI
        uses: turboci/turboci-action@v1
        with:
          config-file: 'turboci.yml'
          redis-url: 'redis://localhost:6379'
          cache-enabled: 'true'
```

## 📊 Performance Taqqoslash

### Odatiy GitHub Actions
```
✗ Build: 3m 45s
✗ Tests: 2m 30s
✗ Lint: 1m 15s
━━━━━━━━━━━━━━━━━━━━━
Total: 7m 30s
```

### TurboCI bilan
```
✓ Build: 45s (cached: 80%)
✓ Tests: 35s (parallel: 8 workers)
✓ Lint: 12s (parallel)
━━━━━━━━━━━━━━━━━━━━━
Total: 1m 32s
⚡ 5.8x FASTER!
```

## 🏗️ Arxitektura

```
┌─────────────────────────────────────────┐
│           TurboCI Runner                │
├─────────────────────────────────────────┤
│                                         │
│  ┌─────────────┐    ┌────────────────┐ │
│  │   Config    │────│  Build Plan    │ │
│  │   Parser    │    │  Optimizer     │ │
│  └─────────────┘    └────────────────┘ │
│                                         │
│  ┌─────────────────────────────────┐   │
│  │     Distributed Cache           │   │
│  │  (Redis + BLAKE3 Hashing)       │   │
│  └─────────────────────────────────┘   │
│                                         │
│  ┌─────────────────────────────────┐   │
│  │   Parallel Execution Engine     │   │
│  │   (Tokio + Rayon)               │   │
│  └─────────────────────────────────┘   │
│                                         │
└─────────────────────────────────────────┘
```

## 🔍 Qanday Ishlaydi?

### 1. **Hash-based Caching**
```rust
// Har bir build target uchun hash hisoblanadi
let hash = blake3::hash(file_contents);
let cache_key = format!("build:{}:{}", target, hash);

// Agar cache'da mavjud bo'lsa, qayta build qilinmaydi
if cache.exists(&cache_key).await? {
    return Ok(CachedResult);
}
```

### 2. **Incremental Analysis**
```rust
// Git diff orqali o'zgargan fayllar topiladi
let changed_files = git_diff();

// Faqat ta'sirlangan target'lar rebuild qilinadi
for target in targets {
    if target.affected_by(&changed_files) {
        rebuild(target);
    }
}
```

### 3. **Parallel Execution**
```rust
// Rayon parallel iterator
test_files.par_iter()
    .map(|test| run_test(test))
    .collect()

// Tokio async parallelism
let handles: Vec<_> = jobs
    .into_iter()
    .map(|job| tokio::spawn(execute_job(job)))
    .collect();
```

## 🛠️ Komandalar

```bash
# Pipeline'ni ishga tushirish
turboci run [--config <file>]

# Cache'ni initialize qilish
turboci init-cache [--redis-url <url>]

# Cache'ni tozalash
turboci clear-cache

# Statistikani ko'rish
turboci cache-stats
```

## 📈 Cache Statistika Namunasi

```
📊 Cache Statistics:
  Hits:       847
  Misses:     153
  Hit Rate:   84.70%
  Entries:    234
  Total Size: 1.2 GB

⚡ Build Optimization: 84.7% cached (847/1000)
```

## 🔐 Xavfsizlik

- Redis authentication qo'llab-quvvatlanadi
- TLS/SSL encryption
- Cache TTL (Time To Live)
- Secure hash verification

## 🤝 Contributing

```bash
# Repository'ni clone qilish
git clone https://github.com/turboci/turboci.git
cd turboci

# Dependencies'ni o'rnatish
cargo build

# Test'larni ishga tushirish
cargo test

# Linting
cargo clippy
cargo fmt
```

## 📝 Loyiha Strukturasi

```
turboci/
├── src/
│   ├── main.rs              # Entry point
│   ├── cache/
│   │   └── mod.rs           # Distributed cache system
│   ├── optimizer/
│   │   └── mod.rs           # Incremental build optimizer
│   ├── runner/
│   │   └── mod.rs           # Parallel test runner
│   └── config/
│       └── mod.rs           # Config parser
├── .github/
│   └── workflows/
│       ├── ci.yml           # CI pipeline
│       └── turboci-action.yml # GitHub Action
├── Cargo.toml               # Dependencies
├── README.md                # Documentation
└── turboci.yml              # Example config
```

## 🎯 Use Case'lar

### 1. Large Monorepo
- O'nlab microservice'larni parallel build qilish
- Shared cache orqali tezlik

### 2. Test-Heavy Projects
- Minglab test'larni parallel bajarish
- 10x tezroq test execution

### 3. Multi-Platform Builds
- Turli platformalar uchun parallel build
- Cross-compilation optimization

### 4. Team Collaboration
- Team member'lari o'rtasida cache sharing
- Distributed Redis cluster

## 🌟 Afzalliklar

| Feature | GitHub Actions | TurboCI |
|---------|---------------|---------|
| Build Cache | ✅ (basic) | ✅ (distributed) |
| Parallel Jobs | ✅ (limited) | ✅ (full CPU) |
| Incremental Builds | ❌ | ✅ |
| Cache Statistics | ❌ | ✅ |
| Hash-based Caching | ❌ | ✅ (BLAKE3) |
| Average Speed | 1x | 5-10x |

## 📄 License

MIT License - see [LICENSE](LICENSE) file

## 🙋 Support

- GitHub Issues: [github.com/turboci/turboci/issues](https://github.com/turboci/turboci/issues)
- Documentation: [docs.turboci.dev](https://docs.turboci.dev)
- Discord: [discord.gg/turboci](https://discord.gg/turboci)

---

**⚡ TurboCI bilan CI/CD'ingizni tezlashtiring!**

Made with ❤️ by TurboCI Team
