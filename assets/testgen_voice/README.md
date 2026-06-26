# Built-in test-pattern voice prompts

`1.wav` … `8.wav` are the spoken-digit prompts compiled into the edge binary
(via `include_bytes!` in `src/engine/input_test_pattern.rs`) and used by the
test-pattern input's **channel-ident** audio mode — each audio channel `N`
announces its number using `N.wav`.

- Format: 16-bit PCM mono, 44.1 kHz (decoded + resampled to 48 kHz at runtime).
- They ship inside the signed binary, so the feature works with zero setup.
- Operators can override any digit at runtime by dropping their own `<n>.wav`
  into the voice directory — see [`../../docs/testgen-voice.md`](../../docs/testgen-voice.md).

## Licensing / provenance

These prompts are distributed as part of bilbycast-edge under the same licence as
the binary (AGPL-3.0-or-later for the default build). **Before publishing a
release, confirm these recordings are ours to redistribute under that licence**
— if they were produced by a third-party or AI text-to-speech service, check
that service's output-licensing terms permit redistribution, and add the
required attribution to the binary's `NOTICE` if so.
