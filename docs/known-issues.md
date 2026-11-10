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
