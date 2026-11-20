# APK Compatibility List

Status of real-world APKs under `ovelo run`, updated as the runtime
gains surface. Verified entries are exercised on every compat pass;
the rest are assessments from `ovelo inspect`/`diagnose` plus known
dependency requirements.

Legend:

- ✅ **works** — launches, renders, and handles input end-to-end
- 🟡 **partial** — installs and runs guest code; crashes or stubs out
  before/during first frame
- ❌ **blocked** — fails at inspect/install/launch
- ⛔ **out of scope** — requires GMS, native code paths, or other
  explicitly unsupported surface

## Verified (run against the store)

| App | Package | Status | Current blocker / notes |
|-----|---------|--------|-------------------------|
| Tiny Weather Forecast Germany | `de.kaffeemitkoffein.tinyweatherforecastgermany` | 🟡 | Launcher `MainActivity` finishes on first launch and `startActivity` correctly launches `WelcomeActivity`; crashes in `onResume` on framework gaps |
| Fossify Notes | `org.fossify.notes` | 🟡 | `SplashActivity.onCreate` → appcompat drawable path (VectorDrawableCompat) |
| F-Droid | `org.fdroid.fdroid` | 🟡 | Hilt DI in `App.onCreate` → `SettingsManager` → preference flow init |
| NewPipe | `org.schabi.newpipe` | 🟡 | `MainActivity` instantiates; WorkManager init runs; `onResume` menu handling gaps |

## Famous apps — assessment

These need surfaces the runtime does not provide yet. Ordered roughly
by how much new work they imply.

| App | Status | What's missing |
|-----|--------|----------------|
| Signal | ❌ | WebRTC, SQLCipher (native), GMS-optional push |
| Telegram (FOSS) | ❌ | tgnet native lib, JNI hot path |
| Discord | ⛔ | GMS push + FCM, WebRTC voice, native codecs |
| WhatsApp | ⛔ | GMS, native crypto/media stack |
| YouTube | ⛔ | GMS, ExoPlayer native codecs |
| Google Maps | ⛔ | GMS (by definition) |
| Spotify | ⛔ | GMS + native audio engine |
| Twitter/X | ⛔ | GMS + dynamic feature delivery |
| Netflix | ⛔ | Widevine DRM — cannot be legally substituted |
| Banking apps (typical) | ⛔ | SafetyNet/Play Integrity — cannot pass |

## Game engines

| Engine | Status | What's missing |
|--------|--------|----------------|
| libGDX | ❌ | GLES surface wiring (ANativeWindow → EGL) exists as object model; no live GL context yet |
| Godot | ❌ | Same + native engine `.so` needs real JNI |
| Unity | ❌ | Same + native engine `.so` needs real JNI |

## Methodology

`ovelo install` verifies the v2/v3 signature and commits the APK to the
store; `ovelo run` builds a Vm over all `classes*.dex`, registers
compat shims, runs `androidx.startup` initializers, `Application.onCreate`,
then the launcher Activity's lifecycle on a supervised worker. Each
`diagnose` records an entry in `~/.ovelo/store/ledger/`.
