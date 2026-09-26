"""Small, independently authored media answers; never derive answers from freemkv."""

import argparse
import hashlib
import json
from pathlib import Path
import struct
import sys
import tempfile

from media_checks import ValidationError, probe, run


FIXTURES = Path(__file__).with_name("media-fixtures")


def digest(data):
    return hashlib.sha256(data).hexdigest()


def decoded(path, index, kind):
    command = ["ffmpeg", "-nostdin", "-v", "error", "-xerror", "-threads", "1"]
    if kind == "audio":
        command += ["-c:a", "ac3_fixed"]
    command += ["-i", str(path), "-map", f"0:{index}", "-threads", "1"]
    if kind == "video":
        command += ["-pix_fmt", "yuv420p", "-fps_mode", "passthrough", "-f", "rawvideo", "-"]
    else:
        command += ["-c:a", "pcm_s16le", "-f", "s16le", "-"]
    result = run(command)
    if result.stderr.strip() or not result.stdout:
        raise ValidationError("decoder diagnostics or empty decoded content")
    return result.stdout


def stream_answers(path):
    data = probe(path, frames=True)
    answers = []
    for stream in data["streams"]:
        index, kind = stream["index"], stream["codec_type"]
        if kind not in ("audio", "video"):
            continue
        frames = [f for f in data.get("frames", []) if f.get("stream_index") == index]
        if not frames:
            raise ValidationError(f"no decoded frames for stream {index}")
        content = decoded(path, index, kind)
        answers.append({"kind": kind, "codec": stream["codec_name"],
                        "language": stream.get("tags", {}).get("language", ""),
                        "format": {k: stream[k] for k in ("width", "height", "channels", "sample_rate") if k in stream},
                        "sha256": digest(content), "bytes": len(content),
                        "pts_us": [round(float(f["pts_time"]) * 1_000_000) for f in frames],
                        "frames": len(frames),
                        "samples": sum(int(f.get("nb_samples", 0)) for f in frames)})
    return answers


def pgs_answers(path):
    metadata = probe(path)
    subtitles = []
    for stream in metadata["streams"]:
        if stream["codec_type"] != "subtitle":
            continue
        result = run(["ffprobe", "-v", "error", "-select_streams", str(stream["index"]),
                      "-show_frames", "-of", "json", str(path)])
        if result.stderr.strip():
            raise ValidationError(result.stderr.decode(errors="replace"))
        frames = json.loads(result.stdout).get("frames", [])
        subtitles.append({"language": stream.get("tags", {}).get("language"),
                          "forced": stream.get("disposition", {}).get("forced", 0),
                          "events": [[round(float(f["pts_time"]) * 1_000_000), f["num_rects"]]
                                     for f in frames]})
    return subtitles


def matches_streams(actual, expected):
    if len(actual) != len(expected):
        return False
    for got, want in zip(actual, expected):
        if set(got) != set(want):
            return False
        for key in want:
            if key == "pts_us":
                # The input MKV quantizes timestamps to milliseconds.
                if len(got[key]) != len(want[key]) or any(
                    abs(a - b) > 1000 for a, b in zip(got[key], want[key])
                ):
                    return False
            elif got[key] != want[key]:
                return False
    return True


def segment(kind, payload):
    return bytes([kind]) + struct.pack(">H", len(payload)) + payload


def sup(events, forced):
    data = bytearray()
    for number, (seconds, visible) in enumerate(events):
        pcs = bytes.fromhex("0780043810") + struct.pack(">H", number)
        pcs += bytes([0x80 if visible else 0, 0, 0, int(visible)])
        if visible:
            pcs += bytes([0, 0, 0, 0x40 if forced else 0, 0, 0, 0, 0])
        pieces = [(0x16, pcs), (0x17, bytes.fromhex("01000000000000020002"))]
        if visible:
            pieces += [(0x14, bytes([0, 0, 1, 235, 128, 128, 255])),
                       (0x15, bytes.fromhex("000000c000000c000200020101000001010000"))]
        pieces.append((0x80, b""))
        for kind, payload in pieces:
            data += b"PG" + struct.pack(">II", round(seconds * 90000), 0) + segment(kind, payload)
    return data


def generate():
    FIXTURES.mkdir(exist_ok=True)
    manifest = {"schema": 1, "provenance": "Synthetic pixels and PCM authored here; FFmpeg encodes inputs. "
                "Video hashes and subtitle events come from source values. AC-3 answers use the "
                "fixed-point reference decoder on committed input, never freemkv output.", "cases": {}}
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        video = [bytes((x + n * 7) % 220 + 16 for x in range(32 * 32))
                 + bytes([96 + n]) * 256 + bytes([160 - n]) * 256 for n in range(30)]
        (root / "video.yuv").write_bytes(b"".join(video))
        for track in range(2):
            (root / f"a{track}.pcm").write_bytes(b"".join(
                struct.pack("<h", ((i * (track + 3)) % 200 - 100) * 100) for i in range(57600)))
        for name, indices in [("cfr", list(range(30))), ("vfr", [0, 1, 2, 5, 10, 15, 20, 25, 29])]:
            output = FIXTURES / f"{name}.mkv"
            command = ["ffmpeg", "-nostdin", "-y", "-v", "error", "-f", "rawvideo",
                       "-pix_fmt", "yuv420p", "-s", "32x32", "-r", "25", "-i", str(root / "video.yuv")]
            for track in range(2):
                command += ["-f", "s16le", "-ar", "48000", "-ac", "1", "-i", str(root / f"a{track}.pcm")]
            command += ["-map", "0:v", "-map", "1:a", "-map", "2:a", "-c:v", "libx264",
                        "-qp", "0", "-preset", "ultrafast", "-threads", "1", "-c:a", "ac3",
                        "-b:a", "192k", "-af", "asetpts=PTS+0.1/TB", "-metadata:s:a:0", "language=eng",
                        "-metadata:s:a:1", "language=spa"]
            if name == "vfr":
                command += ["-vf", "select=" + "+".join(f"eq(n\\,{i})" for i in indices), "-fps_mode", "vfr"]
            run(command + [str(output)])
            answers = stream_answers(output)
            if answers[0]["sha256"] != digest(b"".join(video[i] for i in indices)):
                raise ValidationError("fixture encoding changed authored pixels")
            if answers[0]["pts_us"] != [i * 40000 for i in indices]:
                raise ValidationError("fixture encoding changed authored video timing")
            manifest["cases"][name] = {"input_sha256": digest(output.read_bytes()), "streams": answers}
        forced = [(0, 1), (1, 1), (3, 0), (3600, 1), (3603, 0)]
        ordinary = [(0.5, 1), (2, 0)]
        (root / "forced.sup").write_bytes(sup(forced, True))
        (root / "ordinary.sup").write_bytes(sup(ordinary, False))
        output = FIXTURES / "pgs.mkv"
        run(["ffmpeg", "-nostdin", "-y", "-v", "error", "-copyts", "-i", str(FIXTURES / "cfr.mkv"),
             "-i", str(root / "forced.sup"), "-i", str(root / "ordinary.sup"),
             "-map", "0:v", "-map", "1:s", "-map", "2:s", "-c", "copy",
             "-metadata:s:s:0", "language=eng", "-disposition:s:0", "forced",
             "-metadata:s:s:1", "language=spa", "-disposition:s:1", "0", str(output)])
        expected = [{"language": language, "forced": force,
                     "events": [[round(t * 1_000_000), visible] for t, visible in events]}
                    for language, force, events in [("eng", 1, forced), ("spa", 0, ordinary)]]
        if pgs_answers(output) != expected:
            raise ValidationError("fixture encoding changed authored subtitle events")
        manifest["cases"]["pgs"] = {"input_sha256": digest(output.read_bytes()), "subtitles": expected, "streams": stream_answers(output)}
    (FIXTURES / "answers.json").write_text(json.dumps(manifest, indent=2) + "\n")


def validate(binary, artifacts):
    manifest = json.loads((FIXTURES / "answers.json").read_text())
    if manifest.get("schema") != 1 or set(manifest.get("cases", {})) != {"cfr", "vfr", "pgs"}:
        raise ValidationError("missing required known-answer cases")
    artifacts.mkdir(parents=True, exist_ok=True)
    report = {}
    for name, expected in manifest["cases"].items():
        source = FIXTURES / f"{name}.mkv"
        if digest(source.read_bytes()) != expected["input_sha256"]:
            raise ValidationError(f"{name}: input fixture checksum mismatch")
        for attempt in range(2):
            output = artifacts / f"{name}-{attempt}.mkv"
            run([str(binary), f"mkv://{source.resolve()}", f"mkv://{output.resolve()}"],
                log=artifacts / f"{name}-{attempt}.log")
            actual = {"streams": stream_answers(output)}
            agrees = matches_streams(actual["streams"], expected["streams"])
            if name == "pgs":
                actual["subtitles"] = pgs_answers(output)
                agrees = agrees and actual["subtitles"] == expected["subtitles"]
            if not agrees:
                (artifacts / f"{name}-{attempt}.actual.json").write_text(json.dumps(actual, indent=2))
                raise ValidationError(f"{name} remux {attempt}: differs from known answer")
            report[f"{name}-{attempt}"] = actual
            source = output
    (artifacts / "answers.json").write_text(json.dumps(report, sort_keys=True, indent=2) + "\n")
    print(f"PASS: {len(report)} known-answer remuxes (content, frame/sample counts, timing, languages, PGS lifecycle)")


def compare_reports(directory):
    expected_keys = {f"{name}-{n}" for name in ("cfr", "vfr", "pgs") for n in range(2)}
    reports = []
    for platform in ("ubuntu-latest", "macos-latest", "windows-latest"):
        report = json.loads((directory / f"media-{platform}" / "answers.json").read_text())
        if set(report) != expected_keys:
            raise ValidationError(f"{platform}: missing or unexpected cases")
        reports.append(report)
    if any(report != reports[0] for report in reports[1:]):
        raise ValidationError("decoded content, metadata or timing differs across platforms")
    print("PASS: all six known-answer results identical across Linux, macOS and Windows")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--generate", action="store_true")
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--artifacts", type=Path, default=Path("media-artifacts"))
    args = parser.parse_args()
    try:
        if args.compare:
            compare_reports(args.compare)
        elif args.generate:
            generate()
        elif args.binary:
            validate(args.binary.resolve(), args.artifacts)
        else:
            parser.error("provide --binary or explicitly regenerate fixtures with --generate")
    except (ValidationError, OSError, ValueError, KeyError) as exc:
        print(f"media KAT failed: {exc}", file=sys.stderr)
        sys.exit(1)
