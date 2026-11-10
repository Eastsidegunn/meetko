# Capture provider interface 0.1

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

A capture provider is a separate process (sidecar) that records two audio channels onto
**one meeting timeline** and writes files plus a manifest. The core never captures audio itself.
Reference implementation: `capture/macos/` (`meetko-capture-mac`). A Windows (WASAPI) provider
must implement the same contract; see the last section.

Time rules follow `rfc-0001-contract.md` (시간, 화자): all times are integer milliseconds from the
recording start, one timeline per meeting. Channel 0 = mic = the user ("나"); channel 1 = loopback =
system audio = the remote side ("상대"). **The channel identifies the speaker**, so sample alignment
between the channels is the most important property of a provider.

## 1. CLI

```
<provider> devices
<provider> version
<provider> record --out <dir> [--mic-device <id>]
```

- `devices` prints one JSON object and exits 0: `{"devices":[{"id":"...","name":"...","default":true}]}`
  (input devices only; `id` is what `--mic-device` accepts). This is user-facing selection output, the
  only place device names appear on stdout.
- `version` prints `{"provider":{"id":"...","version":"..."},"contracts":["0.1"]}` and exits 0. The core
  picks a provider by checking `contracts`.
- `record` writes `mic.wav`, `loopback.wav`, `capture.json` into `<dir>`. Without `--mic-device` it uses
  the system default input (on Windows: the default capture endpoint of role `eConsole`).
- `--out` must not exist or must be an empty directory. Otherwise: `error` `bad_out_dir`, exit 2, nothing
  is written (no manifest). The core rejects symlinked channel files and manifests whose `file` is not a
  plain name inside the directory.
- Exit codes: 0 normal stop; 2 failed to start (an `error` event was emitted first; if an output dir was
  created a manifest with zero or few frames and `end_reason: "error"` is still written); 3 failed to
  finalize or fatal runtime failure; 64 usage.
- stdout carries **only** NDJSON events (one JSON object per line, flushed per line). Logs go to stderr.

### Stopping
The provider stops on any of: the exact line `stop` on stdin; EOF on stdin (parent died); SIGINT, SIGTERM or
SIGHUP (Windows: Ctrl-C/Ctrl-Break/console close as far as the OS allows); or an internal fatal condition.
Only the exact line `stop` (surrounding whitespace ignored) and EOF act; every other stdin line is ignored
and **the set of commands is not extensible** in 0.1. SIGPIPE is ignored.
A Windows child started with a hidden window has no console for signals: the stdin `stop` line is the primary
mechanism there; a named event (`Local\meetko-capture-stop-<pid>`) is an acceptable additional one (unverified).

Stop sequence: read `stopHost` (section 5.2) at the moment the stop is requested; stop the capture API
(watchdog: if that call does not return in 5 s, continue anyway with `end_reason: "watchdog"`); keep every
buffer already in flight whose timestamp is <= `stopHost` and discard later ones; zero-pad to the common
length; close the WAVs; compute sha256; write the manifest; emit `stopped`; exit. A stop that arrives while
startup is still in progress waits for startup to finish or fail, then finalizes: **whatever happens, a
manifest is written** (possibly with zero frames) unless `--out` was refused.

### Crash safety
A provisional `capture.json` (`"complete": false`) is written **at start**, before the first checkpoint. Every 5 s
the provider patches both WAV headers to the frames committed so far, fsyncs the files, and only then atomically
writes a new provisional manifest (temp file in the same directory, fsync, rename). The checkpoint I/O runs off the
sample-delivery thread on a snapshot of the state; the header patch uses `pwrite` so it never races with appends.
Ordering under power loss is best effort (the OS may reorder across files).

Provisional-file semantics, for a consumer recovering after SIGKILL or power loss:
- `complete` is false; `end_reason`, `recording_sha256` are absent and channel `sha256` is empty.
- `frames` is the count at the last checkpoint and may differ between channels; the two WAV files may differ too.
- A WAV may hold bytes **past** the size in its header (appended after the last checkpoint). Truncate each file to its
  header size, then either truncate both to the shorter channel or zero-pad the shorter one (record it as a `dropout`/`no_data`
  gap yourself), then recompute hashes. Gaps in a provisional manifest are valid for the checkpointed prefix only.

## 2. Events (stdout, NDJSON)

Every event has `"event"`. Consumers must ignore unknown fields and unknown events.
No event carries a device name (see privacy, section 4).

| event | fields | when |
|---|---|---|
| `started` | `sample_rate`, `started_at` (RFC 3339), `mic_device`, `loopback_device` (role strings), extras | capture is running; zero already fixed. Exactly once, first event. |
| `level` | `channel`, `rms` (0..1 of full scale, last second), `frames`, `at_ms` | about once per second per channel; `rms` 0 when idle |
| `gap` | `channel`, `start_ms`, `end_ms`, `reason`, `kind` | a discontinuity was recorded (same record as in the manifest) |
| `device_changed` | `channel`, `change` (`connected`, `disconnected`, `default_input_changed`), `device` (role), `at_ms` | emitted when the change is *detected*; `at_ms` is the detection time on the timeline (the actual change may be earlier) |
| `format_changed` | `channel`, `sample_rate`, `channels`, `previous_sample_rate`, `previous_channels`, `at_ms` | the input rate or channel count of a channel changed |
| `buffer_dropped` | `channel`, `reason`, `count` (running total) | a buffer was unusable (invalid timestamp, unsupported format, conversion failure); emitted for the 1st and every 100th |
| `stream_restarted` | `at_ms`, `interruption_ms` | after a restart, once audio flows again |
| `error` | `code`, `message`, `recoverable`? | see below |
| `stopped` | `manifest` | last event |

Error codes: `permission_screen_capture`, `permission_microphone` (message says exactly which OS setting to
change), `mic_device_not_found`, `bad_out_dir`, `capture_start_failed`, `stream_stopped` (recoverable, provider
restarts), `stream_lost` (gave up, stops), `write_failed` (fatal), `finalize_failed`, `bad_arguments`.
Permission failures never crash the process: emit the `error`, finalize, exit 2.

## 3. Output files

`mic.wav`, `loopback.wav`: RIFF/WAVE, **mono, 16 000 Hz, PCM16 little endian**, 44-byte header, no extra
chunks. Downmix by averaging channels and resample with a proper low-pass resampler (not decimation).
Both files have **exactly the same number of frames**.

## 4. Manifest `capture.json`

Schema: `capture/macos/capture.schema.json` (JSON Schema 2020-12). Strict: unknown top-level fields are
rejected, except an optional provider extension object named `x_<provider>` (e.g. `x_meetko_capture_mac`);
providers put non-contract extras only there.

```json
{
  "contract": "0.1", "kind": "capture",
  "provider": {"id": "meetko-capture-mac", "version": "0.1.0"},
  "started_at": "2026-10-27T09:18:22.781Z",
  "sample_rate": 16000, "duration_ms": 7530,
  "complete": true, "end_reason": "user", "recording_sha256": "<hex>",
  "channels": [
    {"index": 0, "role": "mic", "file": "mic.wav", "frames": 120494, "sha256": "<hex>",
     "device": "default_input", "first_buffer_offset_ms": 182, "dropped_buffers": 0,
     "gaps": [{"start_ms": 0, "end_ms": 182, "reason": "late_start", "kind": "fill"}]},
    {"index": 1, "role": "loopback", "file": "loopback.wav", "frames": 120494, "sha256": "<hex>",
     "device": "system_output", "first_buffer_offset_ms": 96, "dropped_buffers": 0, "gaps": []}
  ],
  "x_meetko_capture_mac": {"restarts": 0}
}
```

- `started_at`: the wall-clock time (RFC 3339, UTC, millisecond precision) at timeline zero; informational, never
  used for alignment.
- `duration_ms = floor(frames * 1000 / 16000)`.
- `complete`: false for a provisional manifest, or when finalization had problems (then `warnings` lists
  e.g. `frames_mismatch`, `pad_failed_channel_N`; a mismatch is never silent).
- `end_reason`: `user` (stop line), `eof`, `signal`, `stream_lost`, `write_failed`, `watchdog` (stop call hung),
  `error` (failed start).
- Write order at finalization: pad -> close WAVs -> channel `sha256` (of the finalized WAV bytes) -> manifest.
- `recording_sha256` = sha256 over the text lines `"<index>:<channel sha256>\n"` in index order (index 0 then 1),
  computed by the provider.
- `device`: a role string (`default_input`, `system_output`) or an opaque stable label `dev-<12 hex>` (hash of
  a user-chosen device id). **Device names and ids never appear in the manifest or on stdout**; they may
  appear in stderr logs.
- `first_buffer_offset_ms`: where the first delivered buffer of the channel fell relative to zero (before
  any trimming); absent if no buffer ever arrived. `dropped_buffers`: buffers discarded as unusable.

### Gap records: `reason` (closed enum) and `kind`

| reason | kind | meaning |
|---|---|---|
| `late_start` | fill | zeros between zero and the first buffer, **only if <= 1000 ms** |
| `end_pad` | fill | zeros appended at the end up to the common length, **only if <= 1000 ms** |
| `idle` | fill | the provider *knows* the source was idle (e.g. Windows `SILENT` flag) |
| `no_data` | no_data | no packets arrived and the provider cannot tell idle from loss (macOS loopback); also the label for a long `late_start`/`end_pad` or a never-delivering loopback channel |
| `dropout` | loss | the source should have delivered but did not (or too late); also the label for a long `late_start`/`end_pad` or a never-delivering mic |
| `discontinuity` | loss | timestamps jumped backwards by more than 200 ms; overlapping audio was removed (point marker) |
| `write_failed` | loss | disk write failed; the tail of **every** channel from that point is lost |
| `conversion_failed` | loss | buffers dropped because they could not be converted/used |
| `overlap_trimmed` | trim | clock drift or small overlap; audio removed (point marker) |
| `restart` | loss | capture stream was restarted; the span is the actual interruption window |

Rules:
- Millisecond values are **floor** of `frames*1000/16000`, like `duration_ms`. Every gap satisfies
  `0 <= start_ms < end_ms <= duration_ms`; zero-length (sub-millisecond) fills are not listed. A zero-fill gap spans
  exactly the zeros inserted (to ms resolution).
- **Long padding is not fill.** A `late_start` or `end_pad` longer than 1000 ms, or any pad on a channel that never
  delivered a buffer, is recorded as the channel's hole reason (`dropout` for mic, `no_data` for loopback). Such a
  channel carries `"no_buffers": true` in the manifest; if the files are fine `complete` stays true.
- **Markers.** `overlap_trimmed` and `discontinuity` gaps do not cover audio that exists in the file (the file has no
  samples for the removed audio). They are point markers: `end_ms == start_ms + 1` and `removed_ms` (>= 1) is the
  amount of audio removed at `start_ms`. Consecutive trims at the same place (within 40 ms) are merged by summing
  `removed_ms`. Consumers treat the 1 ms span as "timestamps after this point may be early by up to `removed_ms`
  relative to the true clock"; it is not a silent region and not coverage.
- **Restart window.** `restartStart` = when the provider detects the stream died (or, for a stop-then-reopen, when
  the old stream is told to stop); `restartEnd` = timestamp of the first buffer delivered by the NEW stream. The hole
  between them is labelled `restart` on both channels, whatever buffers of the old stream were still in flight (those
  are ignored). Holes before the window or after it keep their own reasons. `stream_restarted` is emitted when that
  first new-stream buffer arrives.
- Gaps of a channel are ordered by `start_ms`, ties broken by `reason` (alphabetically).

**The core derives meeting-data `coverage` from kinds: `loss` and `trim` spans are NOT coverage; `fill` and `no_data`
spans are** (silence that is plausibly real). Consumers tolerate no other reasons.

### Checklist of rules the schema cannot express (the core must check)
1. Both channels have equal `frames`; `duration_ms == floor(frames*1000/16000)`; each WAV is mono 16 kHz PCM16 whose
   data size equals `frames*2` bytes.
2. Every gap: `0 <= start_ms < end_ms <= duration_ms`; gaps of a channel sorted by (`start_ms`, `reason`);
   zero-fill gaps (`fill`, `no_data`, `dropout`, `restart`, `conversion_failed`, `write_failed`) do not overlap each other.
3. Markers (`overlap_trimmed`, `discontinuity`): `end_ms == start_ms + 1` and `removed_ms >= 1`; they are never coverage and
   not zero-fill; other reasons carry no `removed_ms`. `late_start`/`end_pad` longer than 1000 ms must not appear.
4. Each channel `sha256` equals the sha256 of its file; `recording_sha256` equals the hash of `"<index>:<sha>\n"` lines.
5. `file` names are plain names, not symlinks, inside the manifest directory.
6. If `warnings` is present then `complete` is false (the schema enforces this); `end_reason == "write_failed"` implies a
   `write_failed` warning/gap and `complete: false`.
7. Provisional manifest (`complete: false`): channel `sha256` may be empty, `recording_sha256`/`end_reason` absent, `frames`
   may differ between channels, and the WAV files may hold bytes beyond the header size. See "Provisional-file semantics".
8. `first_buffer_offset_ms >= 0` (a first buffer that started before zero is reported as 0 after trimming).

## 5. Alignment guarantees (MUST)

1. **One zero.** Timeline zero is the host-clock reading taken **once, immediately before the first capture start call
   of either channel, after permission checks and device setup** (so a permission prompt cannot inflate `late_start`). Both channels are placed relative to it. Audio from before zero is discarded.
2. **Normative stop and length.** `stopHost` is the clock reading taken when the stop is requested. Total
   length = `max(frames written for channel 0, frames written for channel 1, round((stopHost - zero) * 16000))`.
   `end_pad` fills each channel up to that length. A buffer with timestamp > `stopHost` is not accepted; a
   buffer already in flight with timestamp <= `stopHost` is written.
3. **Place by timestamp, not by arrival.** A buffer is positioned at `round((buffer_timestamp - zero) * 16000)`
   using the OS timestamp on a clock common to both channels. Counting arrived samples silently shifts a
   channel after any dropout.
4. **Timestamp semantics.** A buffer timestamp means the capture time of its *first sample* on the shared
   clock. The provider compensates device latency when the OS reports it. The target is an inter-channel
   error of at most 20 ms. The provider records `first_buffer_offset_ms` per channel. This target is
   **unverified for macOS and Windows** until the hardware click test below is run.
5. **Gaps are zero-filled and recorded.** A discontinuity larger than the tolerance (20 ms) is filled with
   zeros up to the next timestamp, and recorded with real duration. Smaller deviations are jitter and are
   absorbed without shifting (a sample is never more than the tolerance away from its true position).
6. **Clock drift is bounded.** The placement rule trims or pads whenever the error exceeds the tolerance
   (`overlap_trimmed` / hole), so error stays under the tolerance for arbitrarily long recordings. A backward
   jump larger than 200 ms is a `discontinuity` (loss); the trim is kept and the real amount recorded.
7. **Equal length** (rule 2) and **never a silent shift**: anything that can move one channel relative to the
   other appears in `gaps`. Unusable buffers are counted in `dropped_buffers`, announced by `buffer_dropped`,
   and the resulting hole is a `conversion_failed` loss. Invalid or non-finite timestamps must never crash:
   drop and count. A write failure is fatal (the timeline must not advance past data that was not written):
   emit `error write_failed`, stop, `end_reason: "write_failed"`.
8. **Restarts.** If a stream restarts mid-recording the zero does not change; the interruption window (section 4) is
   recorded as a `restart` gap with its actual duration; `stream_restarted` is emitted only on the first buffer of the NEW
   stream. Buffers or errors from a stream that is not the current one are ignored; a stream that dies before it was
   assigned is still the current one (assign before starting). Restart and stop are serialized (a stop wins; no restart
   starts after a stop). The retry policy (the reference retries 5 times, 1 s apart, counter reset after 60 s of healthy
   audio) is a provider choice, not part of the contract. A stop-then-reopen (e.g. after a default-device change) uses the
   same window: it starts when the old client is stopped and ends at the first buffer of the new client.
9. The core adds no per-channel offset: the manifest *is* the alignment statement.

### Hardware click test (unverified for macOS and Windows)
Play a loud click through the speakers/headphones while holding a mic next to the speaker (or use a loopback
cable so the mic hears the same click). Record 10 s with one click at about 5 s. The click's first-sample
position in `mic.wav` and `loopback.wav` should differ only by physical acoustic delay (a few ms/m), i.e. well
under 20 ms. Repeat after 10 minutes of recording (drift), with Bluetooth output, and after a default-device change.

## 6. Device changes and dropouts

Padding and failures are labelled by size and cause, never as speech coverage by accident:
- A `late_start`/`end_pad` over 1000 ms, and any pad on a channel that never delivered, is a hole (`dropout` mic /
  `no_data` loopback), with `no_buffers: true` on a channel that never delivered (manifest stays `complete: true`).
- After a write failure ingestion stops on both channels; **every** channel's tail up to the common length is a
  `write_failed` loss gap and the manifest has `complete: false`, `end_reason: "write_failed"`, warning `write_failed`.
- A restart is a `restart` window (section 4), not a generic dropout.

- Mic unplugged / default input changed: emit `device_changed`; the channel continues on whatever the OS
  provides, or goes silent; the silent span becomes a `dropout` gap when data resumes (or `end_pad`).
  The recording is not stopped.
- Loopback source disappears (output device changed): same treatment (`no_data` on macOS).
- OS stream error: emit `error` (`stream_stopped`, recoverable), release the old stream, retry (5 times, 1 s
  apart, counter reset after 60 s healthy), then `stream_lost` and a graceful stop with `end_reason: "stream_lost"`.
- Format changes (rate or channel count) mid-stream (e.g. Bluetooth hands-free profile): `format_changed`,
  the resampler is recreated; timestamps keep the timeline (unverified on real hardware).

## 7. macOS specifics (`meetko-capture-mac`)

- Requires macOS 15+ (the microphone output of ScreenCaptureKit). Build: `swift build -c release`; tests:
  `capture/macos/run-tests.sh` (Swift Testing from Command Line Tools, no Xcode).
- **One `SCStream` delivers both channels**: system audio (`capturesAudio`, `excludesCurrentProcessAudio`) as
  `.audio`, microphone (`captureMicrophone`, optional `microphoneCaptureDeviceID`) as `.microphone`. Chosen over
  AVAudioEngine because both outputs come from one capture session with `CMSampleBuffer` presentation timestamps
  on the same host clock; AVAudioEngine would add a second pipeline whose `AVAudioTime.hostTime` must be
  reconciled with SCK timestamps. That SCK's mic PTS is as accurate as the system-audio PTS is **unverified**
  (no click test run).
- A 2x2, 1 fps video stream is attached (SCK needs a display filter); frames are ignored.
- Reference clock: `CMClockGetHostTimeClock()`. Zero is read once before permission-gated capture start; the first
  buffers arrive 100-400 ms later and are recorded as `late_start`.
- Loopback holes are labelled `no_data` (SCK delivers nothing while nothing plays; idle cannot be told from loss).
- Downmix by averaging, then `AVAudioConverter` (Float32 mono -> Int16 16 kHz, `primeMethod = .none`, state kept
  across buffers). **Known limitation:** the converter has a measured constant delay of about 0.3-1.8 ms per input
  rate, not compensated (well under the 20 ms target).
- TCC: *Microphone* and *Screen & System Audio Recording* are granted to the **host app** (terminal or the app
  that spawns the sidecar), not the binary; changing them requires restarting the host. Fix text is in the error message.
- Structure: `MeetkoCaptureCore` (`ChannelTimeline`, `CapturePipeline`, `WavWriter`, manifest) has no SCK
  dependency and carries the unit tests; `Recorder` is the thin SCK adapter.
- Observed (this machine, macOS 26): 7.5 s recording with `afplay Glass.aiff`: both files 119661 frames; loopback RMS 0.021
  in the first 2.5 s; `late_start` 130 ms (loopback) / 223 ms (mic); manifest validates and passes the checklist. SIGKILL
  at 7 s left valid WAVs (5.2 s each, unequal by 380 samples) and a provisional manifest.
- Buffers whose audio buffer list is shorter than the sample count are dropped and counted (never zero-filled invisibly);
  NaN/Inf samples become 0.
- Not verified: Bluetooth, aggregate devices, sleep/wake, multiple displays, SCK behaviour when the default
  output changes, stream restart on real errors, long recordings (drift is covered only by a synthetic test),
  DRM audio, the TCC error paths.

## 8. Implementing on Windows (WASAPI loopback)

Written without access to Windows; everything here is **unverified** and must be checked against the Microsoft
docs and a real machine. Implement sections 1-6 unchanged, and reuse the timeline logic.

- **Loopback capture.** `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)`, `Activate` an
  `IAudioClient`, `GetMixFormat`, `Initialize` in shared mode with `AUDCLNT_STREAMFLAGS_LOOPBACK`, then
  `GetService(IAudioCaptureClient)`. Event-driven loopback (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`) is supported on
  Windows 10+, but keep a timer fallback (wake about every 10 ms) because an idle endpoint may stop signalling.
- **Microphone.** Second `IAudioClient` on the default **capture** endpoint for role `eConsole` (not
  `eCommunications`: the communications role may be a different device, such as a headset the user did not pick
  for general input, and may trigger ducking of other audio); `--mic-device <id>` (an `IMMDevice::GetId` string)
  overrides it. Shared mode only.
- **Format.** The shared-mode mix format is usually float32, 44.1/48 kHz, 2+ channels. Average the channels, resample to
  16 kHz, write PCM16. Prefer the Media Foundation resampler (`CLSID_CResamplerMediaObject`, `IMFTransform`) over
  `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`: it gives explicit quality and state control (preference
  based on documentation reading; unverified).
- **Timestamps.** `GetBuffer(&data, &frames, &flags, &devicePosition, &qpcPosition)` returns `qpcPosition` in 100 ns
  units. Use QPC as the single reference clock for both clients. Zero = one `QueryPerformanceCounter` reading immediately before
  the first `IAudioClient::Start` of either client, after devices are opened and formats negotiated (convert with `QueryPerformanceFrequency`). Place every packet at
  `round((qpc_seconds - zero) * 16000)`; do not count frames. Latency: add `IAudioClient::GetStreamLatency` where it
  is meaningful (unverified), and run the click test (section 5).
- **Flags.**
  - `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`: packets were lost or glitched. Rely on QPC placement; record a `dropout`
    (loss) if the placement shows a hole; if the flag is set but no hole is visible, record nothing extra but log it.
  - `AUDCLNT_BUFFERFLAGS_SILENT`: treat the data as zeros (do not read it) and place it by timestamp. Labelling such a
    span `idle` (fill) is optional but RECOMMENDED; otherwise it is simply data (zeros) or, if packets stop entirely,
    `no_data`. **Still call `ReleaseBuffer(frames)` with the frame count.**
  - `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR`: the QPC value is unreliable; place the packet at the expected position (end of
    the previous one) and record no gap, unless the next valid timestamp disagrees with the running position by more than the
    tolerance, in which case the normal hole/trim rules apply.
- **No packets while nothing plays.** Loopback on a render endpoint delivers little or nothing when no application
  renders. Do not use arrival to decide time: when the next timestamp is later than the written position by more than
  the tolerance, zero-fill by the clock, and at stop pad to the common length. Windows can distinguish idle (`SILENT`
  packets, reason `idle`) from a hole with no packets at all (`no_data`, since idle vs loss is again unknown) .
- **Default device changes.** Register an `IMMNotificationClient` (`OnDefaultDeviceChanged`, `OnDeviceStateChanged`,
  `OnDeviceRemoved`). Emit `device_changed`, stop the old client, open a client on the new endpoint, keep the same zero;
  the interval is a `restart` window (from the moment the old client stops until the first packet of the new one). `AUDCLNT_E_DEVICE_INVALIDATED` from `GetBuffer`/`GetCurrentPadding` means the same.
- **Exclusive mode.** Loopback works on shared-mode render streams. If another application owns the endpoint in exclusive
  mode, loopback may fail or go silent; a mic opened exclusively elsewhere makes `Initialize` fail with
  `AUDCLNT_E_DEVICE_IN_USE`. Report `capture_start_failed` (or a runtime `error`) with an explicit message. Never request
  exclusive mode.
- **Process loopback (`ActivateAudioInterfaceAsync` with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10
  2004+) is REQUIRED if meetko ever plays audio itself** (prompts, previews, a sound on start), to keep that audio out of
  channel 1. Exclude the provider's and meetko's process trees. Not needed while meetko is silent.
- **Permissions.** On Windows 11 (and recent Windows 10) Settings > Privacy & security > Microphone has three switches:
  "Microphone access", "Let apps access your microphone", and "Let desktop apps access your microphone"; a Win32 sidecar
  needs the master switch and the desktop-apps switch on. A blocked mic surfaces as an access-denied HRESULT or silent
  (zero) data (unverified): map it to `permission_microphone` with that path in the message. Loopback needs no permission
  (unverified).
- **Conformance.** Port the tests in `capture/macos/Tests` (late start, dropout, jitter, 0.1 % fast clock, shared zero,
  write failure, in-flight buffers at stop, stop before start, backward jump, invalid timestamp, format change, checkpoint
  recovery, equal frames). Validate the manifest against `capture.schema.json`, apply the checklist in section 4, and run
  the click test.
