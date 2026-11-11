# Known issues

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

## Not verified yet

- **Engines are not bundled.** A release build has no live-caption or diarization engine next to it, so it cannot make
  live captions (nor the automatic first draft; whisper still works if set up); the settings say so. Today the app is used from a source build (debug), which finds the
  engines under `engines/nemotron-stream/target/debug/` ([model-setup.md](model-setup.md)).
- **Model download.** The 107 MB diarization file was downloaded once by hand through Hugging Face and matched its pin;
  the 742 MB live-caption download, and a recording or diarization run with a downloaded model, have not been checked
  end to end.
- **Meeting prompt.** Not tried on real meetings: which process each app uses for the microphone (FaceTime and Safari
  in particular), and whether an app lets go of the microphone while muted, which would bring an early
  "녹음을 정지할까요?" ([meeting-detection.md](meeting-detection.md)).
- **Speaker separation while recording.** Checked only by feeding the files of one 69 s test meeting at recording pace
  through the core with both real engines (engines/nemotron-stream README, Diarization), not in a real recording through
  the app. The caption engine and the two diarization sessions share the GPU; on long meetings and slower machines it
  is not known how often a session falls behind (then it is dropped and the draft separates the speakers after the
  recording, as before) or whether the shared GPU slows the captions down. The labels shown while recording are
  provisional: with the 10 s rule (a speaker with less talk time is merged into the others) a channel shows no numbers
  until a second voice has spoken for 10 s, and numbers can still change before the recording ends. The stored
  speakers come from the final turns and may be numbered differently from what the record screen last showed.
- **Automatic re-transcription after a recording.** When a recording stops, the live draft is made and then, in the
  same job slot, the recording is transcribed again by the most accurate ready batch engine (setting `batch_engine`:
  `auto` by default = Qwen3-ASR, else Whisper; an explicit choice that is not ready falls back to the other one;
  `auto_retranscribe`, on by default, turns the step off). Checked with fake engines only (app `api/tests.rs`,
  `pipeline.rs`), not yet in a real recording through the app. Known limits:
  - It starts only from the stop of a recording, and only when the job slot is free then. If another meeting's job
    runs at that moment, neither the live draft nor the re-transcription starts on its own; the meeting screen offers
    "초안 만들기" and "다시 받아쓰기" as buttons. A plain "초안 만들기" click does not chain the re-transcription.
  - A recording without live captions gets no automatic step (there is no live draft to follow); it is transcribed
    by hand as before. The CLI has no live captions, so it has no automatic step either.
  - Speakers: a channel whose live session separated the speakers (stored on the live version only after the
    fed-audio sha256 checks of `live::store_live`) reuses those turns; the producer is the live session's own, with
    `params.live: true` and `params.reused_from: <live version id>`. A channel without live speakers is separated
    again by the diarization engine, even when the live draft already separated it after the recording (so such a
    channel is separated twice per recording).
  - Results made on the live draft go stale when the new draft becomes the head; no plugin runs again on its own.
  - Qwen3-ASR takes about 2.5–3 min per 10 min of meeting on the machine measured; slower machines and very long
    meetings are not measured. The job holds the one transcription slot all that time, so another meeting's
    transcription waits (or can cancel it).
  - With `auto` as the default, the app checks the Qwen3-ASR models (and its sandbox) at every status read; hashes
    are cached, but the first check after a model download hashes about 3.5 GB.
- **Windows and Linux.** Windows is built and tested in CI but has no capture program and no meeting prompt,
  and has not been used for real. Linux is neither built nor tested. Plugins have no sandbox on either.

## Echo: speaker playback leaks into the mic

Seen in the first real end-to-end capture. Remote speech played through the speakers reached the microphone, so every
remote word was transcribed twice: once on the mic channel as "나" and once on the loopback channel as "상대". In the
draft the turns interleave word by word between the two speakers.

Nothing in the core assumes the two channels have disjoint words. Words, turns and speaker spans are per channel, and
compose orders them by (start, channel, position), so duplicates are kept as the engine reported them. The channel still
decides the speaker, which is why the mic copy is labelled "나". Not solved yet.

Directions, none implemented:
- **Headset guidance.** Tell the user to use headphones; this removes the leak at the source.
- **An `echo` processing step.** An engine with role `edit` that drops mic words matching a loopback word within about
  300 ms (same text, close start time). The step must record its provenance in `version.producers` (role `edit`, its
  parameters and the number of words dropped) and produce a new version; the original draft stays untouched.
- **OS echo cancellation.** macOS voice processing IO, Windows communications AEC at capture time. Unverified: neither
  has been tried, and either may change the loopback timing the capture alignment guarantees rely on.
