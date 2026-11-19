# ovelo

Android app compatibility runtime for macOS on Apple Silicon.

Launch unmodified Android APKs from the Mac Dock — without booting an Android
guest OS and without a host JVM — through a compatibility layer written in
Rust that connects app code to the Android APIs.

See [`docs/macos-android-compat-runtime-design.md`](docs/macos-android-compat-runtime-design.md)
for the basic design (v0.1).

## Workspace layout

| Crate | Role | Design chapter |
| --- | --- | --- |
| `ovelo-error` | Shared error taxonomy (`PKG_SIGNATURE`, `DEX_UNSUPPORTED`, `ABI_SYMBOL`, `GPU_FEATURE`, `SERVICE_MISSING`, `POLICY_DENIED`, `HOST_CRASH`) | 14 |
| `ovelo-apk` | APK/ZIP inspection, binary `AndroidManifest.xml`, `resources.arsc`, APK Signature Scheme v2/v3, install records | 03 |
| `ovelo-dex` | DEX format parsing and structural verification (DEX 035–039) | 04 |
| `ovelo-dexrt` | DEX interpreter: object heap, frames, exceptions, statics, monitors | 04 |
| `ovelo-elf` | ARM64 ELF loading model, relocations, Bionic-compatible shims | 05 |
| `ovelo-jni` | JNIEnv bridge between DEX runtime and native libraries | 05 |
| `ovelo-ui` | Text layout (Minikin-subset) and CPU raster surface (Skia-subset) | 06 |
| `ovelo-framework` | Android API surface: `Activity`, `Context`, `Resources`, `Looper`/`Handler`, `View`, `TextView`, `Button`, `Canvas` | 06, 08 |
| `ovelo-host` | macOS host: windowing, Metal presentation, input/IME translation, `.app` launchers | 02, 08 |
| `ovelo-cli` | `inspect` / `install` / `run` / `diagnose` CLI and compatibility ledger | 12, 14 |

## Build

```sh
cargo build --workspace
cargo test --workspace
```

Requires a stable Rust toolchain. `ovelo-host` builds only on macOS
(`aarch64-apple-darwin`); other crates are platform-independent.
