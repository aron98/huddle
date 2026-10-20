# Audio echo handling

Why you sometimes hear yourself, what huddle does about it, and what to do
when it is not enough.

## Echo is a client-side problem

An echo happens when one participant's speakers play another participant's
voice and their microphone picks it back up. The only place that can cancel
it is the browser capturing the microphone, because that is the only place
where both signals — what the mic hears and what the speakers are playing —
exist together.

The SFU deliberately cannot help: it forwards WebRTC media end-to-end
encrypted (DTLS-SRTP) and never decodes a single audio sample. It routes
packets; it does not (and should not) listen to them. Any echo suppression
therefore happens strictly in the capturing browser, and huddle's echo
handling lives entirely in the web client.

## What the client does

Every microphone acquisition — the initial prejoin capture, every device
switch, and every audio-settings change — requests the track with explicit
constraints:

- `echoCancellation: true`
- `noiseSuppression: true`
- `autoGainControl: true`

Echo cancellation and noise suppression are on by default and can be
toggled by the user (prejoin and in-call audio settings). A toggle takes
effect immediately: the client stops the old capture, re-acquires the
microphone with the new constraints, and — in a call — swaps the new track
onto the existing sender without renegotiating, while preserving the mute
state and the selected device. Choices persist in the browser's
`localStorage` and apply to future meetings.

Two details worth knowing:

- The old capture is stopped **before** the new one opens. Browsers keep
  per-device audio-processing state (AEC, noise suppression) while a
  capture of that device is open, so a second concurrent `getUserMedia`
  can silently inherit the old settings instead of the requested ones.
  Stopping first guarantees the new constraints are actually applied.
- Because the old mic is stopped first, a failed re-acquisition (device
  unplugged, permission revoked) leaves the call without a microphone
  until the next toggle attempt. The failure is surfaced as a status
  message in the call; the session itself stays up.

The in-call swap uses `RTCRtpSender.replaceTrack` — same media kind, no
signaling round trip, no interruption of the session.

## Headphones remain the reliable fix

Browser AEC is good but not perfect: it struggles with loud speakers, long
echo tails, and multiple talkers. If participants still hear echo, the most
reliable fix is a headset (or headphones) for whoever causes it — this
removes the acoustic coupling entirely, no DSP required.

## Regression coverage

`test/e2e/e2e-audio.mjs` asserts end to end, in a real browser against a
real server, that the acquired track reports the expected constraints via
`getSettings()` — in the prejoin page, after toggles, across device
switches, page reloads (persistence), and after an in-call settings change —
and that the in-call track swap keeps media flowing.
