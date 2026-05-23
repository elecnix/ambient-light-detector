# ambient-light-detector

A lightweight daemon that keeps your screen brightness in sync with ambient light, using your webcam as the sensor.

**How it works:**
1. Grabs a JPEG frame from an [aeyes](https://github.com/elecnix/aeyes) daemon every second
2. Measures the average luminance (0–255) of the frame
3. Smooths the reading with an exponential moving average (EMA)
4. Maps the smoothed value to a brightness range (5%–100% of max)
5. **Ramps the brightness at 10 Hz** so changes feel silky-smooth — never a sudden jump

## Quick start

```bash
# 1. Make sure aeyes is running
aeyes start

# 2. Run the daemon (needs write access to backlight — see below)
sudo ./target/debug/ambient-light-detector

# Or, with a custom aeyes URL:
ambient-light-detector --aeyes-url http://192.168.1.50:43210

# Or, with more verbose logging:
ambient-light-detector -v
```

## Options

```
Options:
      --aeyes-url <URL>          aeyes daemon URL [default: http://127.0.0.1:43210]
      --camera <ID>              camera ID [default: default]
      --backlight <NAME>         backlight device name (auto-detected if omitted)
      --min-fraction <FRAC>      minimum brightness as fraction of max [default: 0.05]
      --alpha <ALPHA>            EMA smoothing factor [default: 0.08]
      --ramp-rate <RATE>         brightness ramp speed as fraction/sec of max [default: 0.40]
      --sample-interval <DUR>    time between frame samples [default: 1s]
      --tick-interval <DUR>       time between brightness interpolation ticks [default: 100ms]
  -v, --verbose                  enable debug logging
  -h, --help                     show help
  -V, --version                  show version
```

## Brightness permissions

The daemon needs write access to `/sys/class/backlight/*/brightness`. There are two approaches:

### Option A: udev rule (recommended)

```bash
# /etc/udev/rules.d/90-backlight.rules
SUBSYSTEM=="backlight", ACTION=="add", \
  RUN+="/bin/chgrp video /sys/class/backlight/%k/brightness", \
  RUN+="/bin/chmod g+w /sys/class/backlight/%k/brightness"
```

Then add your user to the `video` group:
```bash
sudo usermod -aG video $USER
# Log out and back in for the group change to take effect
```

### Option B: Run with sudo

```bash
sudo ambient-light-detector
```

The daemon will try direct file write first, then automatically fall back to `sudo tee` if needed.

## Smooth brightness transitions

The key design goal is that brightness changes **never jump**. Instead:

- A new target brightness is computed from the webcam every **1 second**
- A **background tick at 10 Hz** (configurable via `--tick-interval`) moves the current brightness toward the target in small increments
- At the default ramp rate of **0.40** (40% of max brightness per second), a full swing from min to max takes about **2.5 seconds**
- Combined with EMA smoothing on the light reading itself (default `--alpha 0.08`), this gives a buttery-smooth transition that is easy on the eyes

## License

MIT