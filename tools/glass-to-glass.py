# /// script
# requires-python = ">=3.11"
# dependencies = ["opencv-python>=4.8", "numpy"]
# ///
"""Measures the glass-to-glass delay of a camera on macOS.

The script shows the clock of the Mac as a full-screen QR code and updates it
on each screen refresh. Point the camera at the screen. The script reads the
camera, decodes the QR code in each frame and logs the difference between the
arrival time of the frame and the time in the QR code.

The delay includes the display and the capture stack of the Mac. Measure the
camera through the Pi and directly on the Mac with the same settings. The
difference of the medians is the delay of the Pi.

Usage:
    uv run tools/glass-to-glass.py --list
    uv run tools/glass-to-glass.py --camera 1 --frames 600 --csv pi.csv

Press q or Esc in the QR window to stop early.
"""

import argparse
import csv
import statistics
import threading
import time

import cv2
import numpy as np

WINDOW = "glass-to-glass"


def now_ms() -> float:
    return time.perf_counter() * 1000.0


def list_cameras() -> None:
    for index in range(6):
        capture = cv2.VideoCapture(index, cv2.CAP_AVFOUNDATION)
        if capture.isOpened():
            width = capture.get(cv2.CAP_PROP_FRAME_WIDTH)
            height = capture.get(cv2.CAP_PROP_FRAME_HEIGHT)
            fps = capture.get(cv2.CAP_PROP_FPS)
            print(f"camera {index}: {width:.0f}x{height:.0f} at {fps:.2f} fps")
        capture.release()
    print("macOS does not give names here. Check the order in "
          "`system_profiler SPCameraDataType`, or try each index with --preview.")


class Reader(threading.Thread):
    """Reads camera frames, timestamps them on arrival and decodes the QR code."""

    def __init__(self, index: int, width: int, height: int, fps: int, preview: bool,
                 native: bool):
        super().__init__(daemon=True)
        self.capture = cv2.VideoCapture(index, cv2.CAP_AVFOUNDATION)
        if not native:
            self.capture.set(cv2.CAP_PROP_FRAME_WIDTH, width)
            self.capture.set(cv2.CAP_PROP_FRAME_HEIGHT, height)
            self.capture.set(cv2.CAP_PROP_FPS, fps)
        self.format = (f"{self.capture.get(cv2.CAP_PROP_FRAME_WIDTH):.0f}x"
                       f"{self.capture.get(cv2.CAP_PROP_FRAME_HEIGHT):.0f} at "
                       f"{self.capture.get(cv2.CAP_PROP_FPS):.2f} fps")
        self.detector = cv2.QRCodeDetector()
        self.preview = preview
        self.delays: list[float] = []
        self.frames = 0
        self.undecoded = 0
        self.last_preview = None
        self.stop = threading.Event()

    def run(self) -> None:
        while not self.stop.is_set():
            ok, frame = self.capture.read()
            arrived = now_ms()
            if not ok:
                continue
            self.frames += 1
            gray = cv2.cvtColor(cv2.resize(frame, (960, 540)), cv2.COLOR_BGR2GRAY)
            text, _, _ = self.detector.detectAndDecode(gray)
            if self.preview:
                self.last_preview = cv2.resize(frame, (480, 270))
            try:
                shown = float(text)
            except ValueError:
                self.undecoded += 1
                continue
            self.delays.append(arrived - shown)

    def close(self) -> None:
        self.stop.set()
        self.join(timeout=2)
        self.capture.release()


def qr_image(text: str, size: int) -> np.ndarray:
    matrix = cv2.QRCodeEncoder.create().encode(text)
    image = cv2.resize(matrix, (size, size), interpolation=cv2.INTER_NEAREST)
    return image


def summary(delays: list[float]) -> str:
    ordered = sorted(delays)
    p10 = ordered[len(ordered) // 10]
    p90 = ordered[len(ordered) * 9 // 10]
    return (f"n={len(ordered)} median={statistics.median(ordered):.1f} ms "
            f"p10={p10:.1f} p90={p90:.1f} min={ordered[0]:.1f} max={ordered[-1]:.1f}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list", action="store_true", help="list the camera indexes")
    parser.add_argument("--camera", type=int, default=0, help="camera index")
    parser.add_argument("--frames", type=int, default=600, help="decoded frames to measure")
    parser.add_argument("--width", type=int, default=1920)
    parser.add_argument("--height", type=int, default=1080)
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument("--native", action="store_true",
                        help="keep the format of the camera, without --width, --height and --fps")
    parser.add_argument("--preview", action="store_true",
                        help="show the camera image in a small window")
    parser.add_argument("--csv", help="write each delay in ms to this file")
    args = parser.parse_args()

    if args.list:
        list_cameras()
        return

    reader = Reader(args.camera, args.width, args.height, args.fps, args.preview,
                    args.native)
    if not reader.capture.isOpened():
        raise SystemExit(f"cannot open camera {args.camera}")

    cv2.namedWindow(WINDOW, cv2.WINDOW_NORMAL)
    cv2.setWindowProperty(WINDOW, cv2.WND_PROP_FULLSCREEN, cv2.WINDOW_FULLSCREEN)
    reader.start()

    size = 900
    canvas = np.full((size + 100, size + 100), 255, dtype=np.uint8)
    reported = 0
    try:
        while len(reader.delays) < args.frames:
            canvas[50:50 + size, 50:50 + size] = qr_image(f"{now_ms():.1f}", size)
            cv2.imshow(WINDOW, canvas)
            if args.preview and reader.last_preview is not None:
                cv2.imshow("camera", reader.last_preview)
            key = cv2.waitKey(1) & 0xFF
            if key in (ord("q"), 27):
                break
            if len(reader.delays) >= reported + 60:
                reported = len(reader.delays)
                print(summary(reader.delays[-60:]), flush=True)
    finally:
        reader.close()
        cv2.destroyAllWindows()

    if not reader.delays:
        raise SystemExit(f"no QR code decoded in {reader.frames} frames. "
                         "Check the camera index, the focus and the framing.")
    print(f"format {reader.format}")
    print(f"frames {reader.frames}, undecoded {reader.undecoded}")
    print("total:", summary(reader.delays))
    if args.csv:
        with open(args.csv, "w", newline="") as file:
            writer = csv.writer(file)
            writer.writerow(["delay_ms"])
            writer.writerows([[f"{delay:.2f}"] for delay in reader.delays])
        print(f"wrote {args.csv}")


if __name__ == "__main__":
    main()
