# Layout policy `auto`

Compose reads the policy as data: `policies/<name>.json`, shipped in the image under `RECORDER_POLICY_DIR`, its name and
version stamped into the manifest. Every number the layout uses lives in the file, not in code. A new policy is a new
file; a changed number is a re-run of compose on old recordings.

`scene.rs` turns the event log, the manifest and the policy into a timeline of layouts, one per change point; it is a
pure function with no GStreamer in it and is unit-tested on its own. `compose.rs` applies the geometry to a GStreamer
`compositor` through control sources keyed on the recording clock, so the pipeline runs as fast as the CPU allows and
stays deterministic.

## Rules

- **Scene state** at any `t`: the set of joined peers, each with camera on/off, muted, held, hand raised; at most one
  active share (the most recent `share.started` wins; an earlier share is shown as a normal tile); the current speaker
  and the ordered list of the last speakers.
- **No share**: a grid of every joined peer, up to `maxGridTiles` (9). Beyond that, the tiles are the nine most recent
  speakers, and the remaining names are listed in a strip at the bottom (`nameStripHeightPx`). Equal-size tiles at
  `aspectRatio`, centred, with `gapPx` between them.
- **Share**: the share fills the stage (`shareStageWidthRatio`, 0.78 of the width); a vertical filmstrip on the right
  holds `filmstripTiles` (4): the sharer first, then the most recent speakers. When the share stops, back to the grid.
- **Orientation and shape**: a phone's camera is turned as its `recorder.orientation` events say, changing mid-recording
  when the phone turns. Every video tile takes the picture's real shape (read from the file), fitted and centred in its
  slot, so a portrait phone gets a portrait tile with the label and corners on the picture, never stretched.
- **Tile content**: video if the camera is on and frames exist at `t`; otherwise the avatar slate: a disc in the peer's
  signature colour (a hash of the peer id) with the initials, on a dark gradient. Name label bottom-left in a
  translucent pill (`labelHeightPx`, `labelFontPx`), with one icon per status after the name (screen while sharing,
  pause while on hold, crossed mic while muted, a hand while raised) from Material Icons when
  `RECORDER_COMPOSE_ICON_FONT` exists, otherwise drawn as shapes. `tileCornerRadiusPx` rounded corners. A
  `speakerBorderPx` border in the signature colour while the peer is the current speaker.
- **Transitions**: tile add, remove and reorder animate over `transitionMs` (300). Speaker changes reorder only after
  `speakerHoldMs` (3000) so the grid does not flicker.
- **Audio**: all mic tracks mixed to one stereo AAC track, in the composite and as `audio.m4a`. Per-peer audio is the
  per-track WebM already stored.
- **Missing data**: a `recorder.gap` shows the last good frame frozen for up to `freezeMaxMs` (2000), then the slate. A
  track whose file is missing is treated as camera off for its whole span and listed in `composite.missing`. The
  composite ends a second after the last media, not when the capture was declared over.
- **Title card**: the video opens with a title card, full frame for 1 s and gone by 1.5 s, over the first moments (the
  audio runs underneath): the title (from `capture.start`, else "Meeting recording"), the start time in UTC, who pressed
  record, how many took part (everyone who joined at any point), and `RECORDER_COMPOSE_BRAND` top right when set. Same
  dark gradient as the slates.
- **Thumbnail**: the same title card, not a frame.

## Fonts

Labels and slates are drawn in Rust with `ab_glyph`, so the runtime image needs no GStreamer text plugin. The label font
is `RECORDER_FONT` (DejaVu Sans in the image); the icon font is `RECORDER_COMPOSE_ICON_FONT` and is optional.

## Output

`RECORDER_COMPOSE_WIDTH` × `RECORDER_COMPOSE_HEIGHT` at `RECORDER_COMPOSE_FPS`, H.264 (x264) at
`RECORDER_COMPOSE_VIDEO_KBPS` with AAC at `RECORDER_COMPOSE_AUDIO_KBPS`, fragmented MP4 remuxed to a normal MP4 at the
end. Output caps are fully pinned so a camera joining mid-recording cannot renegotiate the encoder (which once produced
an unreadable file). If the pipeline's position passes the recording's length by 15 s, EOS is forced: a stream whose RTP
timestamps jumped by hours must not run the render away.
