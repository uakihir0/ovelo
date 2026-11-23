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
| Fossify Gallery | `org.fossify.gallery` | 🟡 | **Renders real data**: grid cell shows the actual "Pictures" folder with item count and a real decoded PNG thumbnail (Glide fetch→decode→transform→draw chain works) |
| Fossify Music Player | `org.fossify.musicplayer` | 🟡 | **Renders real data**: MediaStore scan → Room DB (tracks/albums/artists tables populated) → tab bar + track list bound with real `Lfu4` rows ("All 3" tab) |
| Fossify Voice Recorder | `org.fossify.voicerecorder` | 🟡 | Recorder page renders; SAF walk (`DocumentFile`/`DocumentsContract`) lists real recordings — list rows materialize on the Player tab |
| Fossify Calendar | `org.fossify.calendar` | 🟡 | **Renders real data**: month grid draws 7-column day cells with real locale strings, today marker, month name |
| Fossify Clock | `org.fossify.clock` | 🟢 | **Renders real data**: clock page shows live time + date, tab bar (Clock/Alarm/Stopwatch/Timer); alarm list RecyclerViews are legitimately empty |
| Fossify Messages | `org.fossify.messages` | 🟡 | Lifecycle completes to correct end state: "No stored conversations" empty state + FAB render (DB is empty) |
| Fossify Paint | `org.fossify.paint` | 🟡 | Full lifecycle; toolbar + canvas render, brush strokes register (stroke position offset from touch point) |
| Fossify Contacts | `org.fossify.contacts` | 🟡 | SplashActivity → MainActivity transition works; tab bar (Contacts/Favorites/Groups) + contact grid render — needs `invoke-static` superclass-chain resolution |
| Fossify Documents | `org.fossify.documents` | 🟡 | Lifecycle completes; UI is Compose (`ViewTreeLifecycleOwner`) — no render path yet |
| Fossify Home (launcher) | `org.fossify.home` | 🟢 | Renders correct end state: "No items found." empty grid on a clean install |
| BMI Calculator | `com.github.characterdog.bmicalculator` | 🟡 | Lifecycle completes and the form renders (title/Weight/kg labels); v1-JAR-signed APK accepted by new `verify_v1` chain; layout rough |
| Bouncy (pinball, libGDX) | `com.dozingcatsoftware.bouncy` | 🟡 | **Playable**: menu → table select → live pinball table with bumpers rendering; box2d `.so` physics runs via JNI. TLS self-heal in the segv handler patches kernel-rewritten TPIDR_EL0 per access (Darwin rewrites it at signal/exception boundaries). Text labels and ball rendering still rough |
| Open Sudoku | `org.moire.opensudoku` | 🟢 | **Renders real data**: full 9×9 board drawn with given digits, pencil-mark notes and cell coloring; custom `View.onDraw` path works end to end |
| Tiny Weather Forecast Germany | `de.kaffeemitkoffein.tinyweatherforecastgermany` | 🟡 | `WelcomeActivity` renders to the window (first real APK first frame) |
| NewPipe | `org.schabi.newpipe` | 🟢 | **Renders real data**: kiosk "Live" tab lists actual Innertube-fetched videos with titles/channels and real image thumbnails (host-bridged Coil3 `enqueue` → HTTP → WebP decode → `setImageDrawable`). Feed tab empty is expected without subscriptions |
| F-Droid | `org.fdroid.fdroid` | 🟡 | Multi-dex + Hilt init runs without crashing; UI is Compose — no render path yet |
| 2048 (AndStatus) | `org.andstatus.game2048` | 🟡 | Lifecycle completes (KorGE engine); GLSurfaceView opens but GLES2 pipeline is not implemented — blank window |
| Snakes (drugi) | `red.drugi.snakes` | 🟡 | Lifecycle completes (Godot engine); host `Lifecycle` throwable on onCreate, blank GL window |
| Privacy Friendly Sudoku | `org.secuso.privacyfriendlysudoku` | 🟡 | SplashActivity → TutorialActivity transition runs fully in dex; Welcome screen renders real text (layout rough — overlapping strings). Unblocked by dex-enum `compareTo` fix (LifecycleRegistry sync) |
| Aegis Authenticator | `com.beemdevelopment.aegis` | 🟡 | MainActivity → IntroActivity transition runs; intro pager chrome renders (dots + button) but page text/images missing; two non-fatal inflater NPEs in MainActivity.onCreate (EntryListView/SearchView not found) |
| Markor | `net.gsantner.markor` | 🟡 | Full lifecycle clean (MainActivity → IntroActivity); window opens but no content views materialize (inflater gap, same class as aegis) |
| Tasks.org | `org.tasks` | 🟡 | Hilt DI graph init fails: `dexrt.type` inside `SingletonCImpl.httpClientFactory` provider chain (KeyStoreEncryptionProvider); window opens blank |

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
| KorGE | 🟡 | Lifecycle completes (AndStatus 2048); needs GLES2 shaders/context |
| Godot | 🟡 | Lifecycle completes (drugi snakes); needs GLES2 + engine `.so` JNI |
| Unity | ❌ | Same + native engine `.so` needs real JNI |

## Methodology

`ovelo install` verifies the v2/v3 signature and commits the APK to the
store; `ovelo run` builds a Vm over all `classes*.dex`, registers
compat shims, runs `androidx.startup` initializers, `Application.onCreate`,
then the launcher Activity's lifecycle on a supervised worker. Each
`diagnose` records an entry in `~/.ovelo/store/ledger/`.
