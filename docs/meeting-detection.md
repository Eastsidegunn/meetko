# Meeting detection

<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->

Settings → 회의 알림. On by default on macOS, both switches (native meeting apps and browsers); the user can turn
either off in settings, and that choice is kept. When on, the app asks "회의가 시작된 것 같아요. 녹음할까요?" once a known
meeting app has been using the microphone for about 6 seconds in a row, and, while recording, asks "녹음을 정지할까요?" once a
native meeting app has stopped using it for 30 seconds. It never starts or stops a recording by itself.

## Signal

Every 3 seconds (only while the setting is on, macOS only) the app reads CoreAudio's process objects
(`kAudioHardwarePropertyProcessObjectList`, macOS 14+): for each process, whether it is reading audio input
(`kAudioProcessPropertyIsRunningInput`) and its bundle id. That is all it looks at: no window titles, browser tabs,
URLs or screen contents, no Accessibility or Screen Recording permission, nothing recorded. meetko's own process is
excluded. Implemented in Rust in the app crate (`app/src-tauri/src/meeting_watch.rs`) through the C API, with no new
dependency; the capture sidecar's interface is unchanged. Other platforms build without it and the setting reads
"이 기기에서는 아직 쓸 수 없어요".

On by default because a meeting that starts without anyone pressing 녹음 is the most common
way to lose one, and the check is narrow. It only reads which process uses the microphone, nothing is recorded or sent,
and the app only asks; it never starts a recording by itself. The settings text says exactly this next to the switches,
so turning it off is one click away.

The setting lives in `<data dir>/app-settings.json` under the key `meeting_prompt` (`enabled`, `browsers`); other
keys in that file are kept. A missing file, or a file without that key, means on; a saved `false` stays off. A file
that exists but cannot be read (or a key that is not the two switches) means off, so a damaged file never turns the
watch on by guessing; saving any setting replaces it.
Other platforms are always off whatever the file says.

## Apps

A bundle id matches exactly or as a prefix followed by `.` (helper processes count as their app).

| App | Bundle ids |
|---|---|
| Zoom | `us.zoom.xos` |
| Microsoft Teams (new, classic) | `com.microsoft.teams2`, `com.microsoft.teams` |
| Slack (huddles) | `com.tinyspeck.slackmacgap` |
| Webex | `Cisco-Systems.Spark`, `com.webex.meetingmanager`, `com.cisco.webexmeetingsapp` |
| FaceTime | `com.apple.FaceTime`, `com.apple.avconferenced` |
| Discord | `com.hnc.Discord` |
| WhatsApp | `net.whatsapp.WhatsApp`, `desktop.WhatsApp` |
| Jitsi Meet (desktop) | `org.jitsi.jitsi-meet` |

Browsers (web meetings such as Google Meet), only with the second switch on: Chrome, Edge, Arc, Brave, Firefox,
Vivaldi, Opera, Safari (including `com.apple.WebKit.GPU`, which other WebKit apps share). A browser using the
microphone may be something other than a meeting, so the prompt says "웹 회의일 수 있어요", and a browser meeting's
end is not guessed.

## Rules

- Each app has its own meeting: its mic use with no gap longer than 30 s. A browser that keeps the mic does not
  stand in for a Zoom meeting that starts later, and a Zoom recording still gets its stop question when Zoom lets
  go while a browser holds on. Each meeting is offered at most once, one question at a time. Two apps that end
  together get a stop question each, one after the other.
- 이번엔 안 함: silent for the rest of that meeting (and of the other apps on the mic at that moment), and no
  question at all for 10 minutes. A meeting that is still on when the 10 minutes are over is asked about then.
- Escape: silent for the rest of that meeting only, no cooldown; another app's meeting can be asked about next.
- Every answer carries the number of the question it answers; an answer to a question that has since changed
  is ignored.
- A recording covers a meeting only after it has run for 5 seconds, and 녹음 시작 is sent only once the capture
  program reports that it started. A start that fails (permissions, say) leaves the question open.
- Recording (from the prompt or by hand) covers the meeting: after the stop it is not offered again.
- Turning the feature off forgets everything, cooldown included; turning browsers off takes a browser question
  away at once.
- The clock counts time asleep (`CLOCK_MONOTONIC` on macOS), so a sleep longer than 30 s ends every meeting and
  runs down the cooldown. A failed CoreAudio query skips that poll.
- In the background the Dock icon bounces once. There is no system notification (that would need a notification
  plugin and permission).

Unverified on real meetings: which process each app captures from (in particular FaceTime and Safari), and whether
an app releases the microphone while muted (which would cause an early "녹음을 정지할까요?").
