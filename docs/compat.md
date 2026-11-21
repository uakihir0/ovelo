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
| Fossify File Manager | `org.fossify.filemanager` | 🟡 | **Renders real data**: lifecycle completes, RecyclerView rows bind actual filenames from the seeded external root (Pictures/Download/Documents/readme.md). Row layout positions are still rough |
| Fossify Notes | `org.fossify.notes` | 🟡 | Full lifecycle + window render; toolbar and search menu draw; note list rows bind real SQLite data (Burmese text renders with fallback fonts) |
| Fossify Gallery | `org.fossify.gallery` | 🟡 | Lifecycle completes end-to-end (Splash → MainActivity, no crashes); window opens; RecyclerView waits for its adapter (media-query data path pending) |
| Fossify Calendar | `org.fossify.calendar` | 🟡 | Launches and renders the search/toolbar UI; month-view adapter attach path pending |
| Fossify Messages | `org.fossify.messages` | 🟡 | Launches and renders the search/toolbar UI; conversation-list adapter attach path pending |
| Bouncy (pinball, libGDX) | `com.dozingcatsoftware.bouncy` | 🟡 | Full lifecycle; menu screen renders with real resource strings ("Cambiar tablero"/"INICIAR"); GL game surface not wired yet |
| Tiny Weather Forecast Germany | `de.kaffeemitkoffein.tinyweatherforecastgermany` | 🟡 | `WelcomeActivity` renders to the window (first real APK first frame) |
| NewPipe | `org.schabi.newpipe` | 🟡 | `MainActivity` full lifecycle; WorkManager runs real SQLite queries; error screen renders (no network layer yet) |
| F-Droid | `org.fdroid.fdroid` | 🟡 | Multi-dex + Hilt init runs without crashing; UI is Compose — no render path yet |

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
| libGDX | 🟡 | View-layer menu renders (Bouncy verified); GLES surface wiring (ANativeWindow → EGL) exists as object model, no live GL context yet |
| Godot | ❌ | Same + native engine `.so` needs real JNI |
| Unity | ❌ | Same + native engine `.so` needs real JNI |

## Methodology

`ovelo install` verifies the v2/v3 signature and commits the APK to the
store; `ovelo run` builds a Vm over all `classes*.dex`, registers
compat shims, runs `androidx.startup` initializers, `Application.onCreate`,
then the launcher Activity's lifecycle on a supervised worker. Each
`diagnose` records an entry in `~/.ovelo/store/ledger/`.
