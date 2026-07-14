# Probing streams over stdin: an integration guide

Since hdrprobe 0.7.0 (schema 2.3), `hdrprobe -` reads its input from stdin. This is built for
applications whose media has no filesystem path an external process could open: Kodi addons
(Kodi resolves `nfs://`, `smb://`, `bluray://`, and `udf://` URLs inside its own process),
media-server plugins, ranged HTTP fetches, or any pipeline that already holds the bytes.
Instead of staging a temp file, pipe the stream in and let hdrprobe take what it needs.

```sh
curl -sr 0-25165823 "$URL" | hdrprobe --json -
```

## The contract

**hdrprobe decides how much to read, not you.** It sniffs the container format from the first
bytes and reads only its own head budget: currently 24 MiB for MPEG-TS/M2TS (whose metadata
rides about one GOP into the stream) and 16 MiB for everything else, plus one byte to detect
whether the stream continues. Do not size the transfer yourself; stream blocks until hdrprobe
stops accepting them. The budgets may change in future releases and your code should not care.

**A broken pipe is the success signal.** When hdrprobe has read its budget it stops consuming
stdin and finishes the probe. Your next write fails with a broken-pipe error (Python:
`BrokenPipeError`, sometimes surfacing as `OSError` on Windows). Treat that as "hdrprobe has
everything it needs" and move on to reading the report.

**A stream that ends early is a complete probe.** If your stream reaches end-of-file within
the budget (you close stdin after the last block), hdrprobe has seen the entire input and the
report is identical to probing the same bytes as a file, except that `file` is `"-"`.

**Exit codes are unchanged.** `0` means a report was produced, `2` means the input could not
be parsed (not a recognized container, or a head too short to carry the format's metadata).
An empty or unparseable stdin never hangs and never fabricates a report.

**Constraints.** `-` may appear at most once per invocation. `--full` is rejected on stdin (a
pipe cannot be seeked or fully scanned). Metadata sidecar files (raw RPU `.bin`, DV CM XML,
HDR10+ JSON) are recognized by file extension and are therefore not detectable on stdin; pass
those as paths.

## Reading the report

Everything is standard [SCHEMA.md](SCHEMA.md) output. Three things are specific to stdin:

- `file` is the literal string `"-"`.
- `input_truncated: true` appears when the stream exceeded the head budget, meaning only a
  leading window was probed. It is absent (never `false`) for complete streams and file probes.
- When `input_truncated` is present, `size_bytes` is the number of bytes probed, not the size
  of the source, and hdrprobe withholds the fields a prefix cannot honestly state rather than
  reporting wrong numbers: a TS input's `duration_secs` (its duration is normally measured
  across the whole file) and every `bitrate` except MP4/MOV's exact `video_stream` rate. All
  declared header facts report normally: resolution, frame rate, color, HDR10 static metadata,
  HDR10+, the full Dolby Vision section, and MP4/MKV durations (those are stated in the
  header, not measured).

For HDR/DV identification, which is the point of a head probe, a truncated probe and a whole
file probe of the same title agree: profile, level, CM version, layer structure, FEL/MEL,
mastering display, and the L5/L6 metadata all ride the head of the stream.

## Kodi addon example (Python)

A drop-in pattern for probing the currently playing VFS URL. Works on any Kodi platform where
the addon can execute a bundled binary (for example CoreELEC with the aarch64 build).

```python
import json
import subprocess

import xbmcvfs

BLOCK = 1024 * 1024  # 1 MiB per VFS read


def probe_stream(hdrprobe_path: str, vfs_url: str) -> dict | None:
    """Pipe the head of a Kodi VFS stream into hdrprobe and parse its report.

    Returns the report dict, or None when the stream could not be probed.
    """
    proc = subprocess.Popen(
        [hdrprobe_path, "--json", "-"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    src = xbmcvfs.File(vfs_url)
    try:
        while True:
            block = src.readBytes(BLOCK)
            if not block:
                break  # end of stream: hdrprobe reports a complete probe
            try:
                proc.stdin.write(block)
            except (BrokenPipeError, OSError):
                break  # hdrprobe took what it needs: success, keep going
    finally:
        src.close()
        try:
            proc.stdin.close()
        except OSError:
            pass

    out, _ = proc.communicate()
    if not out:
        return None
    try:
        report = json.loads(out)
    except ValueError:
        return None
    return report if isinstance(report, dict) else None
```

Notes for Kodi specifically:

- **Resolve Blu-ray references to the playing `.m2ts` first.** A raw `.iso`, a `bluray://`
  reference, or a `.mpls` playlist is not a video stream; piping its head probes the disc
  filesystem or playlist bytes, not the feature. Map the reference to the stream file inside
  the disc (Kodi's `udf://` VFS can list `BDMV/STREAM/`) and pipe that. hdrprobe's own ISO
  handling (pass the `.iso` as a path) is only available when the image is a real local file.
- **No temp file is needed**, so nothing is written to flash and there is no fixed chunk size
  to tune or shared temp path to guard against concurrent probes.
- If the addon also scans the same head bytes itself (for example an audio bitstream scan),
  accumulate the blocks you pipe into a buffer as you go; you are already holding each block.

## What a head probe cannot give you

These require the end of the file and are unavailable from any prefix, no matter the size:

- The runtime of a TS/M2TS stream (measured from timestamps at both ends of the file).
- MKV bitrate statistics tags (written after the clusters).
- An MP4 whose index (`moov`) was written at the end of the file rather than the front; such
  a stream fails honestly with exit code 2.

If you need those, give hdrprobe a real file path; everything else about the two invocations
is identical.
