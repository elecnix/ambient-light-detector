# ambient-light-detector

A software-based ambient light detector that uses your webcam to automatically adjust screen brightness.

## Why a Software-Based Solution?

This project exists because **many computers don't have hardware ambient light sensors (ALS)**.

- Most desktop computers lack ALS entirely
- All-in-One desktops (like HP 22-b009) often don't include ALS
- Laptops from certain manufacturers may have ALS that doesn't work under Linux
- The HP WMI ALS driver (`/sys/devices/platform/hp-wmi/als`) may return "Invalid argument" if the BIOS doesn't support the query

**Rationale**: When you need adaptive brightness but lack hardware ALS, the webcam serves as a viable substitute. Modern webcams can capture frames, and the average luminance of those frames correlates with ambient light levels.

## How It Works

1. **Frame Capture**: Opens the webcam directly via V4L2 and captures a 320x240 frame
2. **Luminance Calculation**: Computes average brightness using the luminance formula (Y = 0.2126R + 0.7152G + 0.0722B)
3. **Exponential Smoothing**: Applies EMA smoothing to reduce noise and jitter
4. **Brightness Mapping**: Maps smoothed luminance (0-1) to brightness range (min_fraction to 100%)
5. **Smooth Transitions**: Ramps brightness gradually at configurable intervals (10Hz default)

## Caveats and Drawbacks

### Accuracy Limitations
- **No calibrated lux readings**: Estimates are relative, not absolute light measurements
- **Camera-dependent**: Image processing varies between webcams
- **Indirect measurement**: Measures screen reflection more than true ambient light

### Hardware Dependencies
- **Webcam LED behavior**: The webcam LED stays on continuously while running (not flashing) because the camera streams continuously for more responsive light detection. This is intentional to keep the sensor active.
- **USB bandwidth**: Uses webcam continuously at ~30fps internally
- **Potential conflicts**: Can't be used simultaneously with other webcam applications

### Reliability Issues
- **Driver stuck states**: UVC driver can get stuck in buffer wait states (this happened after 6 days of continuous operation)
- **USB disconnects**: Webcam reconnection requires service restart
- **Permission issues**: Needs write access to `/sys/class/backlight/*/brightness`

### Privacy Considerations
- The camera is active whenever the daemon runs
- No image storage - frames are processed in-memory and discarded
- Consider covering webcam when not needed

## Quick Start

```bash
# 1. Build
cargo build --release

# 2. Run (may need root for brightness access)
sudo ./target/release/ambient-light-detector --min-fraction 0.0

# Or use systemd service (recommended)
sudo systemctl enable --now ambient-light-detector
```

## Options

```
--device <PATH>            Video device (default: /dev/video0)
--backlight <NAME>         Backlight device name (auto-detected if omitted)
--min-fraction <FRAC>      Minimum brightness as fraction (default: 0.05)
--alpha <ALPHA>            EMA smoothing factor (default: 0.08)
--ramp-seconds <SECONDS>   Time for full brightness sweep (default: 2.0)
--sample-interval <DUR>    Time between frame samples (default: 1s)
--tick-interval <DUR>       Tick interval for brightness updates (default: 5ms)
-v, --verbose              Enable debug logging
```

## Permissions

Add your user to the `video` group and use a udev rule to allow brightness writes:

```bash
# /etc/udev/rules.d/90-backlight.rules
SUBSYSTEM=="backlight", ACTION=="add", \
  RUN+="/bin/chgrp video /sys/class/backlight/%k/brightness", \
  RUN+="/bin/chmod g+w /sys/class/backlight/%k/brightness"
```

## License

MIT