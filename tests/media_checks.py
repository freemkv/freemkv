"""Strict external media checks shared by synthetic and real-disc validation."""

import argparse
import json
import math
from pathlib import Path
import statistics
import subprocess
import sys


class ValidationError(Exception):
    pass


def run(command, log=None):
    result = subprocess.run(command, capture_output=True)
    if log is not None:
        Path(log).write_bytes(result.stdout + result.stderr)
    if result.returncode:
        raise ValidationError(
            f"{command[0]} exited {result.returncode}: "
            + result.stderr.decode(errors="replace")[-4000:]
        )
    return result


def number(value):
    try:
        value = float(value)
    except (ValueError, TypeError) as exc:
        raise ValidationError(f"invalid timestamp: {value!r}") from exc
    if not math.isfinite(value):
        raise ValidationError("non-finite timestamp")
    return value


def probe(path, packets=False, frames=False):
    command = ["ffprobe", "-v", "error"]
    entries = ("stream=index,codec_type,codec_name,channels,sample_rate,width,height:"
               "stream_tags=language:stream_disposition=forced:format=duration")
    if packets:
        entries += ":packet=stream_index,pts_time,duration_time"
    if frames:
        command += ["-flags2", "+skip_manual"]
        entries += ":frame=stream_index,pts_time,best_effort_timestamp_time,nb_samples"
    result = run(command + ["-show_entries", entries, "-of", "json", str(path)])
    if result.stderr.strip():
        raise ValidationError(result.stderr.decode(errors="replace"))
    try:
        data = json.loads(result.stdout)
    except (ValueError, UnicodeError) as exc:
        raise ValidationError("invalid probe JSON") from exc
    if not isinstance(data, dict) or not data.get("streams"):
        raise ValidationError("probe returned no streams")
    return data


def timeline(data, mode="strict"):
    streams = data.get("streams", [])
    packets = data.get("packets", [])
    if not streams or not packets:
        raise ValidationError("missing streams or packets")
    duration = number(data.get("format", {}).get("duration"))
    if duration <= 0:
        raise ValidationError("non-positive duration")
    by = {s["index"]: [] for s in streams}
    kinds = {s["index"]: s.get("codec_type") for s in streams}
    audio_duration = {i: 0.0 for i in by}
    for packet in packets:
        index = packet.get("stream_index")
        if index not in by:
            raise ValidationError("packet references an unknown stream")
        if packet.get("pts_time") not in (None, "N/A"):
            by[index].append(number(packet["pts_time"]))
        elif kinds[index] in ("audio", "video"):
            raise ValidationError(f"stream {index}: missing packet timestamp")
        if kinds[index] == "audio":
            length = number(packet.get("duration_time"))
            if length <= 0:
                raise ValidationError(f"stream {index}: missing audio packet duration")
            audio_duration[index] += length
    report = []
    for stream in streams:
        index, kind = stream["index"], stream.get("codec_type")
        timestamps = by[index]
        continuous = kind in ("audio", "video")
        if continuous and len(timestamps) < 2:
            raise ValidationError(f"stream {index}: insufficient timestamped packets")
        if not timestamps:
            continue
        first, last = min(timestamps), max(timestamps)
        if first < -1:
            raise ValidationError(f"stream {index}: excessive negative start time")
        if kind == "audio" and audio_duration[index] - (last - first) > 0.25:
            raise ValidationError(f"stream {index}: audio duration exceeds timestamp span")
        if any(b - a < -0.5 for a, b in zip(timestamps, timestamps[1:])):
            raise ValidationError(f"stream {index}: backward timestamp jump")
        if last > duration + 1:
            raise ValidationError(f"stream {index}: packets beyond declared duration")
        if continuous:
            ordered = sorted(timestamps)
            gaps = [b - a for a, b in zip(ordered, ordered[1:])]
            positive = [g for g in gaps if g > 0]
            if not positive:
                raise ValidationError(f"stream {index}: collapsed timeline")
            median = statistics.median(positive)
            squashed = sum(median - g for g in gaps if g < median / 10)
            if kind == "audio" and squashed > 0.25:
                raise ValidationError(f"stream {index}: {squashed:.3f}s compressed audio")
            if mode == "strict" and abs(last - first - duration) >= 1:
                raise ValidationError(f"stream {index}: span differs from duration")
        report.append({"stream": index, "kind": kind, "first": first, "last": last,
                       "packets": len(timestamps)})
    if not any(s.get("codec_type") in ("audio", "video") for s in streams):
        raise ValidationError("no continuous media streams")
    return report


def decode(path, offsets):
    streams = [s for s in probe(path)["streams"] if s.get("codec_type") in ("audio", "video")]
    if not streams:
        raise ValidationError("no audio/video streams to decode")
    for offset in offsets:
        command = ["ffmpeg", "-nostdin", "-v", "error", "-xerror"]
        if offset:
            command += ["-ss", str(offset)]
        command += ["-i", str(path), "-t", "20", "-map", "0:v?", "-map", "0:a?",
                    "-f", "framehash", "-"]
        result = run(command)
        if result.stderr.strip():
            raise ValidationError(result.stderr.decode(errors="replace"))
        rows = [line for line in result.stdout.splitlines()
                if line.strip() and not line.startswith(b"#")]
        try:
            present = {int(row.split(b",")[0]) for row in rows}
        except ValueError as exc:
            raise ValidationError("invalid decoder framehash output") from exc
        if present != set(range(len(streams))):
            raise ValidationError(f"decoder produced no frames for some streams at {offset}s")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("check", choices=["timeline", "decode"])
    parser.add_argument("media", type=Path)
    parser.add_argument("--mode", choices=["strict", "cadence"], default="strict")
    parser.add_argument("--offset", action="append", type=float)
    args = parser.parse_args()
    try:
        if args.check == "timeline":
            print(json.dumps(timeline(probe(args.media, packets=True), args.mode)))
        else:
            decode(args.media, args.offset or [0])
    except (ValidationError, OSError, ValueError, KeyError, TypeError) as exc:
        print(f"media validation failed: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
