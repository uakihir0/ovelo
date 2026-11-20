# ovelo

Android app compatibility runtime for macOS on Apple Silicon.

Launch unmodified Android APKs from the Mac Dock — without booting an Android
guest OS and without a host JVM — through a compatibility layer written in
Rust that connects app code to the Android APIs.

See [`docs/macos-android-compat-runtime-design.md`](docs/macos-android-compat-runtime-design.md)
for the basic design (v0.1) and [`docs/compat.md`](docs/compat.md) for
the real-APK compatibility list.

## Usage

```sh
ovelo inspect  <apk> [--json]   # manifest/features/ABIs/dex inventory
ovelo install  <apk> [splits...]# verify signature, commit to the store
ovelo run      <package>        # launch the installed app in a window
ovelo diagnose <apk> [--json]   # compatibility report (+ ledger entry)
ovelo gate     <binary|.app>    # sign/entitlements/notarization check
```

`install` verifies the APK Signature Scheme v2/v3 block and stores the
package under `~/.ovelo/store/`. `run` merges every `classes*.dex` into
one VM, runs `androidx.startup` initializers and `Application.onCreate`,
then drives the launcher Activity's lifecycle on a supervised worker —
a guest crash is reported under `data/diagnostics/` and the worker
restarts once rather than taking down the host.

## Scope and limits

This is a compatibility layer, not an emulator. What works today:

- Real APKs install and launch; guest dex code runs through an
  interpreter (plus Cranelift AOT for int-typed methods).
- Guest `Activity`/`View`/`TextView`/`Button` calls drive a real host
  window; taps reach `onClick` listeners.
- `SharedPreferences`, `Bundle`, `SQLite`, `ContentResolver`, intents
  and `startActivity` transitions, options menus, themes/styles and
  layout inflation are modelled.

What is explicitly out of scope: **GMS** (Google Mobile Services — push,
Play services APIs, Play Integrity), native `.so` code paths (JNI is a
skeleton), real GL/Vulkan rendering (object model only), and hardware
APIs. Apps built on those — Discord, YouTube, most banking and AAA
games — cannot run yet; see [docs/compat.md](docs/compat.md).

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
