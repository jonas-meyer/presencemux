# PresenceMux

PresenceMux is a meeting setup that does not depend on the host.

A Raspberry Pi 5 connects the camera, the microphone and the light on the desk
to the computer that a KVM switch selects. Each host sees one USB device
with a camera, a microphone and speakers. The Pi does all audio and video
processing and all privacy control. Nothing is installed on the hosts.

## Hardware

- Raspberry Pi 5
- Fujifilm X100VI as the camera
- Shure SM7dB microphone and Native Instruments Komplete Audio 2
- Elgato Key Light
- Elgato Stream Deck Neo for the controls

## Layout

```text
crates/presencemux-core   controller state machine, without I/O
bins/presencemuxd         daemon that runs the controller on the Pi
bins/presencemuxctl       command-line client for the daemon
```

## License

MIT. See [LICENSE](LICENSE).
