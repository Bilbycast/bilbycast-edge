# Test-pattern channel-ident voice clips

The synthetic **test-pattern** input (`type: "test_pattern"`) can announce each
audio channel's number so an operator can tell channels apart by ear. Set
**Channel Content → Per-channel number ident** (`audio_content: "channel_ident"`)
in the input's manager UI / config.

Each channel `N` (1-based) plays the announcement for digit `N`, looped with a
gap, all channels phase-aligned to a common period. Spoken-digit prompts for
1–8 are **compiled into the binary** (see
[`../assets/testgen_voice/`](../assets/testgen_voice/)), so the feature works
out of the box with **no setup**. The per-channel source is resolved in order:

1. **Operator override** — a `<N>.wav` in the runtime voice directory (below).
2. **Built-in voice** — the prompt compiled into the binary.
3. **`N` counted beeps** — only if a digit has no clip at all (e.g. a channel
   number > 8, or a decode failure).

This is gated by `audio_enabled`, and is **overridden by the A/V-sync marker**
(`av_sync_marker: true`): a sync test needs the pip on every channel, so the
ident is suppressed while it's on.

## Overriding the built-in voice (optional)

To use your own recordings (a different voice, language, or house style),
drop one WAV per channel number into the runtime voice directory — this
overrides the built-in prompt for that digit, **without rebuilding**:

```
<voice_dir>/1.wav   # spoken "one"
<voice_dir>/2.wav   # spoken "two"
...
<voice_dir>/8.wav   # spoken "eight"   (only need as many as your max channel count)
```

`<voice_dir>` resolves in this order:

1. `BILBYCAST_TESTGEN_VOICE_DIR` if set and non-empty.
2. Otherwise `<media_dir>/testgen_voice/` — where `<media_dir>` is the edge's
   media-library directory (`BILBYCAST_MEDIA_DIR` → XDG data dir →
   `$HOME/.bilbycast/media/` → `./media/`).

Clips are read at input start, so add/replace them and re-create (or restart)
the test-pattern input to pick up changes. An unreadable override silently
falls back to the built-in voice (then beeps); a `tracing::warn` records why.

### Accepted clip format

The built-in decoder is intentionally tiny. Clips are decoded to 48 kHz mono and
peak-normalised to the input's configured **Tone Level (dBFS)**:

| Property      | Accepted                                                 |
|---------------|----------------------------------------------------------|
| Container     | RIFF / WAVE (`.wav`)                                      |
| Sample format | 16-bit PCM, 24-bit PCM, or 32-bit IEEE float             |
| Channels      | Any (multi-channel is downmixed to mono by averaging)    |
| Sample rate   | Any (linear-resampled to 48 kHz)                         |
| Length        | Keep it short — ~0.5–1.5 s per digit is ideal            |

Anything else (e.g. compressed WAV, ADPCM) is rejected and that channel uses the
beep fallback. Keep clips short: the loop period is the longest clip plus a
0.6 s gap, rounded up to whole seconds (minimum 2 s), shared by all channels.
