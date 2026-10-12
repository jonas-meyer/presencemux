#!/bin/sh
# Makes the default slates of the cam worker: a 1920x1080 JPEG from each SVG in
# bins/presencemux-cam/assets/slates. Needs GStreamer with rsvg and jpeg.
set -eu
dir="$(dirname "$0")/../bins/presencemux-cam/assets/slates"
for svg in "$dir"/*.svg; do
    jpg="${svg%.svg}.jpg"
    gst-launch-1.0 -q filesrc location="$svg" ! rsvgdec ! videoconvert \
        ! videoscale ! video/x-raw,width=1920,height=1080,format=I420 \
        ! jpegenc quality=85 ! filesink location="$jpg"
    echo "$jpg: $(wc -c < "$jpg") bytes"
done
