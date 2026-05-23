use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn, Level};

#[cfg(target_os = "linux")]
use rscam::{Camera, Config};

/// Ambient light detector — adjusts screen brightness from webcam luminance.
///
/// Opens the webcam directly (no aeyes needed), measures average brightness,
/// and smoothly adjusts the screen backlight so that:
///   • dark room  → low brightness (easier on the eyes)
///   • bright room → high brightness (keeps the screen readable)
///
/// Brightness is written to sysfs every few milliseconds so transitions
/// are butter-smooth — never a visible step or jump.
#[derive(Parser, Debug)]
#[command(name = "ambient-light-detector", version)]
struct Args {
    /// Video device (e.g. /dev/video0)
    #[arg(long, default_value = "/dev/video0")]
    device: String,

    /// Backlight device name under /sys/class/backlight/ (auto-detected if omitted)
    #[arg(long)]
    backlight: Option<String>,

    /// Minimum brightness as a fraction of max (0.0–0.5)
    #[arg(long, default_value = "0.05")]
    min_fraction: f32,

    /// EMA alpha for ambient-light smoothing (0.01 = very slow, 1.0 = no smoothing)
    #[arg(long, default_value = "0.08")]
    alpha: f32,

    /// Time in seconds for brightness to traverse the full range (lower = snappier)
    #[arg(long, default_value = "2.0")]
    ramp_seconds: f32,

    /// Interval between frame samples
    #[arg(long, default_value = "1s")]
    sample_interval: humantime::Duration,

    /// Interval between brightness writes (smooth interpolation tick)
    #[arg(long, default_value = "5ms")]
    tick_interval: humantime::Duration,

    /// Verbose logging
    #[arg(short, long)]
    verbose: bool,
}

// ---------------------------------------------------------------------------
// Backlight helpers
// ---------------------------------------------------------------------------

fn find_backlight(name: Option<&str>) -> Result<PathBuf> {
    let dir = PathBuf::from("/sys/class/backlight");
    if !dir.exists() {
        anyhow::bail!(
            "no /sys/class/backlight directory — is this a laptop/display with a backlight?"
        );
    }
    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if let Some(wanted) = name {
            if file_name != wanted {
                continue;
            }
        }
        if path.join("brightness").exists() && path.join("max_brightness").exists() {
            return Ok(path);
        }
    }
    if let Some(wanted) = name {
        anyhow::bail!("backlight device '{wanted}' not found in /sys/class/backlight");
    }
    anyhow::bail!("no usable backlight device found in /sys/class/backlight")
}

fn read_sysfs_u32(path: &PathBuf, file: &str) -> Result<u32> {
    let p = path.join(file);
    let s = std::fs::read_to_string(&p)
        .with_context(|| format!("cannot read {}", p.display()))?;
    s.trim()
        .parse::<u32>()
        .with_context(|| format!("invalid number in {}", p.display()))
}

/// Write a brightness value. Tries direct write first, falls back to sudo.
fn write_brightness(backlight: &PathBuf, value: u32) -> Result<()> {
    let p = backlight.join("brightness");
    let v = value.to_string();

    if std::fs::write(&p, &v).is_ok() {
        return Ok(());
    }

    let mut child = std::process::Command::new("sudo")
        .args(["tee", p.to_str().unwrap_or("?")])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to run sudo")?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = writeln!(stdin, "{v}");
    }

    let status = child.wait().context("failed to wait for sudo")?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("cannot write brightness — add udev rule or run as root")
    }
}

// ---------------------------------------------------------------------------
// Webcam capture (Linux V4L2)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn capture_frame(device: &str) -> Result<Vec<u8>> {
    let mut cam = Camera::new(device)?;
    let config = Config {
        interval: (1, 30),
        resolution: (320, 240),
        format: b"MJPG",
        ..Default::default()
    };
    match cam.start(&config) {
        Ok(()) => {}
        Err(_) => {
            let config_yuyv = Config {
                interval: (1, 30),
                resolution: (320, 240),
                format: b"YUYV",
                ..Default::default()
            };
            cam.start(&config_yuyv)?;
        }
    }
    let frame = cam.capture()?;
    Ok(frame.to_vec())
}

#[cfg(target_os = "linux")]
fn luminance_from_mjpeg(jpeg: &[u8]) -> Result<f32> {
    let img = image::load_from_memory(jpeg).context("failed to decode JPEG frame")?;
    luminance_from_image(&img)
}

#[cfg(target_os = "linux")]
fn luminance_from_yuyv(yuyv: &[u8], _width: u32, _height: u32) -> Result<f32> {
    if yuyv.is_empty() {
        anyhow::bail!("empty YUYV buffer");
    }
    let mut sum: f64 = 0.0;
    let mut count: u64 = 0;
    for chunk in yuyv.chunks_exact(4) {
        sum += chunk[0] as f64;
        count += 1;
        sum += chunk[2] as f64;
        count += 1;
    }
    if count == 0 {
        anyhow::bail!("no pixels");
    }
    Ok((sum / count as f64) as f32)
}

#[cfg(not(target_os = "linux"))]
fn capture_frame(_device: &str) -> Result<Vec<u8>> {
    anyhow::bail!("webcam capture is only supported on Linux (V4L2)")
}

fn luminance_from_image(img: &image::DynamicImage) -> Result<f32> {
    let mut sum: f64 = 0.0;
    let mut count: u64 = 0;
    let rgb = img.to_rgb8();
    for pixel in rgb.chunks_exact(3).step_by(4) {
        let r = pixel[0] as f64;
        let g = pixel[1] as f64;
        let b = pixel[2] as f64;
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        sum += y;
        count += 1;
    }
    if count == 0 {
        anyhow::bail!("empty image");
    }
    Ok((sum / count as f64) as f32)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let log_level = if args.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .init();

    // --- Discover backlight ------------------------------------------------
    let backlight = find_backlight(args.backlight.as_deref())?;
    let max_brightness = read_sysfs_u32(&backlight, "max_brightness")?;
    let current_brightness = read_sysfs_u32(&backlight, "brightness")?;
    let min_brightness = (max_brightness as f32 * args.min_fraction).round() as u32;

    // Derive step size: full range covered in ramp_seconds, ticking every tick_interval
    let tick: Duration = args.tick_interval.into();
    let full_range = (max_brightness - min_brightness) as f32;
    let steps_for_full_range = (args.ramp_seconds / tick.as_secs_f32()).max(1.0);
    let ramp_per_tick = (full_range / steps_for_full_range).round() as u32;
    let ramp_per_tick = ramp_per_tick.max(1);

    info!(
        "backlight: {} range {}–{}, current {}",
        backlight.display(), min_brightness, max_brightness, current_brightness,
    );
    info!(
        "sample {:?}, tick {:?}, ramp {}/tick (full sweep ≈{:.1}s)",
        args.sample_interval, tick, ramp_per_tick, args.ramp_seconds,
    );

    // --- Shared state -------------------------------------------------------
    struct Brightness {
        current: u32,
        target: u32,
    }
    let brightness = Arc::new(Mutex::new(Brightness {
        current: current_brightness,
        target: current_brightness,
    }));

    // --- Sample task (every 1 s) -------------------------------------------
    let sample_interval: Duration = args.sample_interval.into();
    let alpha = args.alpha;
    let device = args.device.clone();
    let b_clone = brightness.clone();
    let bl_clone = backlight.clone();

    let sample_handle = tokio::spawn(async move {
        let mut smoothed_luma: f32 = current_brightness as f32 / max_brightness as f32;
        let mut consecutive_errors: u32 = 0;

        loop {
            #[cfg(target_os = "linux")]
            {
                match capture_frame(&device) {
                    Ok(frame_data) => {
                        consecutive_errors = 0;
                        let luma = match luminance_from_mjpeg(&frame_data) {
                            Ok(l) => l,
                            Err(_) => luminance_from_yuyv(&frame_data, 320, 240)
                                .unwrap_or(smoothed_luma * 255.0),
                        };
                        let ambient = luma / 255.0;
                        smoothed_luma = alpha * ambient + (1.0 - alpha) * smoothed_luma;
                        let raw_target = min_brightness as f32
                            + smoothed_luma * (max_brightness - min_brightness) as f32;
                        let target = raw_target.round() as u32;
                        let target = target.clamp(min_brightness, max_brightness);

                        let b = b_clone.lock().unwrap();
                        info!(
                            "luma={:.0} ambient={:.3} smoothed={:.3} target={} current={}",
                            luma, ambient, smoothed_luma, target, b.current,
                        );
                        drop(b);
                        b_clone.lock().unwrap().target = target;
                    }
                    Err(e) => {
                        consecutive_errors += 1;
                        if consecutive_errors <= 3 || consecutive_errors % 30 == 0 {
                            warn!("webcam capture failed ({consecutive_errors}): {e:#}");
                        }
                    }
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                warn!("webcam capture not supported on this platform");
            }

            tokio::time::sleep(sample_interval).await;
        }
    });

    // --- Tick task (every few ms) ------------------------------------------
    let tick_handle = tokio::spawn(async move {
        let mut write_fail_log: u32 = 0;

        loop {
            let target;
            let mut current;
            {
                let b = brightness.lock().unwrap();
                target = b.target;
                current = b.current;
            }

            if current != target {
                if current < target {
                    current = (current + ramp_per_tick).min(target);
                } else {
                    current = current.saturating_sub(ramp_per_tick).max(target);
                }

                match write_brightness(&backlight, current) {
                    Ok(()) => {
                        write_fail_log = 0;
                        brightness.lock().unwrap().current = current;
                    }
                    Err(_) => {
                        write_fail_log += 1;
                        if write_fail_log == 1 || write_fail_log % 50 == 0 {
                            error!(
                                "brightness write failed ({write_fail_log}×) — \
                                 add udev rule or run as root"
                            );
                        }
                        if let Ok(v) = read_sysfs_u32(&backlight, "brightness") {
                            brightness.lock().unwrap().current = v;
                        }
                    }
                }
            }

            tokio::time::sleep(tick).await;
        }
    });

    tokio::select! {
        r = sample_handle => { error!("sample task ended: {r:?}"); r?; }
        r = tick_handle => { error!("tick task ended: {r:?}"); r?; }
    }
    Ok(())
}
