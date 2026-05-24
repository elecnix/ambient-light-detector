use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn, Level};

#[cfg(target_os = "linux")]
use rscam::{Camera, Config};

/// Ambient light detector — adjusts screen brightness from webcam luminance.
///
/// Opens the webcam directly, measures average brightness, and smoothly adjusts
/// the screen backlight. Camera stays streaming (LED stays on).
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

    /// EMA alpha for small ambient changes (tiny fluctuations are smoothed away)
    #[arg(long, default_value = "0.10")]
    alpha: f32,

    /// Time in seconds for brightness to traverse the full range
    #[arg(long, default_value = "2.0")]
    ramp_seconds: f32,

    /// Interval between frame samples (ambient light measurement)
    #[arg(long, default_value = "200ms")]
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
        anyhow::bail!("no /sys/class/backlight directory");
    }
    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
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
        anyhow::bail!("backlight device '{wanted}' not found");
    }
    anyhow::bail!("no usable backlight device found")
}

fn read_sysfs_u32(path: &PathBuf, file: &str) -> Result<u32> {
    let p = path.join(file);
    let s =
        std::fs::read_to_string(&p).with_context(|| format!("cannot read {}", p.display()))?;
    s.trim()
        .parse::<u32>()
        .with_context(|| format!("invalid number in {}", p.display()))
}

fn write_brightness(backlight: &PathBuf, value: u32) -> Result<()> {
    let p = backlight.join("brightness");
    let v = value.to_string();
    if std::fs::write(&p, &v).is_ok() {
        return Ok(());
    }
    let mut child = std::process::Command::new("sudo")
        .args(["tee", p.to_str().unwrap_or("?")])
        .stdin(std::process::Stdio::piped())
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
        anyhow::bail!("cannot write brightness")
    }
}

// ---------------------------------------------------------------------------
// Webcam capture
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
struct StreamingCamera {
    cam: Camera,
}

#[cfg(target_os = "linux")]
impl StreamingCamera {
    fn new(device: &str) -> Result<Self> {
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
                cam.start(&Config {
                    interval: (1, 30),
                    resolution: (320, 240),
                    format: b"YUYV",
                    ..Default::default()
                })?;
            }
        }
        info!("camera streaming started (LED on)");
        Ok(Self { cam })
    }

    fn capture_frame(&mut self) -> Result<Vec<u8>> {
        Ok(self.cam.capture()?.to_vec())
    }
}

#[cfg(target_os = "linux")]
fn luminance_from_frame(data: &[u8]) -> f32 {
    // Try JPEG decode first, then YUYV
    if let Ok(img) = image::load_from_memory(data) {
        let mut sum: f64 = 0.0;
        let mut count: u64 = 0;
        for pixel in img.to_rgb8().chunks_exact(3).step_by(4) {
            sum += 0.2126 * pixel[0] as f64 + 0.7152 * pixel[1] as f64 + 0.0722 * pixel[2] as f64;
            count += 1;
        }
        return (sum / count as f64) as f32;
    }
    // YUYV fallback: Y0 U Y1 V — just average the Y values
    let mut sum: f64 = 0.0;
    let mut count: u64 = 0;
    for chunk in data.chunks_exact(4) {
        sum += chunk[0] as f64 + chunk[2] as f64;
        count += 2;
    }
    (sum / count as f64) as f32
}

// ---------------------------------------------------------------------------
// Adaptive EMA: fast response to big changes, smooth for small ones
// ---------------------------------------------------------------------------

/// Returns an effective alpha that scales with the magnitude of the change.
/// - Small drift (|diff| < 0.03):  use the baseline alpha (slow, stable)
/// - Medium shift (|diff| 0.03–0.15): ramp up alpha for quicker tracking
/// - Large jump   (|diff| > 0.15):  snap toward the new value fast
fn adaptive_alpha(base_alpha: f32, diff: f32) -> f32 {
    if diff < 0.03 {
        base_alpha // tiny noise — smooth it away
    } else if diff < 0.15 {
        (base_alpha + (diff - 0.03) / 0.12 * (0.5 - base_alpha)).clamp(base_alpha, 0.5)
    } else {
        (0.5 + (diff - 0.15) * 2.0).min(0.9) // large jump: cap at 0.9 to avoid overshoot
    }
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
    tracing_subscriber::fmt().with_max_level(log_level).init();

    let backlight = find_backlight(args.backlight.as_deref())?;
    let max_brightness = read_sysfs_u32(&backlight, "max_brightness")?;
    let current_brightness = read_sysfs_u32(&backlight, "brightness")?;
    let min_brightness = (max_brightness as f32 * args.min_fraction).round() as u32;

    let tick: Duration = args.tick_interval.into();
    let full_range = (max_brightness - min_brightness) as f32;
    let steps_for_full_range = (args.ramp_seconds / tick.as_secs_f32()).max(1.0);
    let ramp_per_tick = ((full_range / steps_for_full_range).round() as u32).max(1);

    info!(
        "backlight: {} range {}–{}, current {}",
        backlight.display(),
        min_brightness,
        max_brightness,
        current_brightness
    );
    info!(
        "sample {:?}, tick {:?}, ramp {}/tick (full range in {:.1}s)",
        args.sample_interval,
        tick,
        ramp_per_tick,
        args.ramp_seconds,
    );

    let current_brightness_arc = Arc::new(AtomicU32::new(current_brightness));
    let target_brightness_arc = Arc::new(AtomicU32::new(current_brightness));

    // --- Brightness tick task: writes every `tick` ms, smoothly ramping ---
    let current_clone = current_brightness_arc.clone();
    let target_clone = target_brightness_arc.clone();
    let bl_clone = backlight.clone();
    let tick_handle = tokio::spawn(async move {
        loop {
            let target = target_clone.load(Ordering::SeqCst);
            let current = current_clone.load(Ordering::SeqCst);
            if current != target {
                // Proportional moving average: bigger gap = faster movement
                let diff = (target as i32 - current as i32).abs() as f32;
                let step = (diff * 0.15).clamp(1.0, ramp_per_tick as f32).round() as u32;
                let next = if current < target {
                    (current + step).min(target)
                } else {
                    current.saturating_sub(step).max(target)
                };
                // Write with timeout to prevent blocking the watchdog
                let bl_path = bl_clone.clone();
                let result = tokio::time::timeout(
                    Duration::from_millis(100),
                    async move { write_brightness(&bl_path, next) }
                ).await;
                if result.as_ref().map(|r| r.is_ok()).unwrap_or(false) {
                    current_clone.store(next, Ordering::SeqCst);
                } else if let Ok(actual) = read_sysfs_u32(&bl_clone, "brightness") {
                    // Sync with reality on write failure
                    current_clone.store(actual, Ordering::SeqCst);
                }
            }
            tokio::time::sleep(tick).await;
        }
    });

    // --- Camera sampling task: captures every second, computes target ---
    let sample_interval: Duration = args.sample_interval.into();
    let base_alpha = args.alpha;

    let current_ref = current_brightness_arc.clone();
    let target_ref = target_brightness_arc.clone();

    let sample_handle = tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let mut camera = match StreamingCamera::new(&args.device) {
            Ok(c) => c,
            Err(e) => {
                error!("camera init failed: {e:#}");
                return;
            }
        };

        // Seed smoothed value from current brightness so we don't jump on startup
        let mut smoothed_val: f32 =
            current_ref.load(Ordering::SeqCst) as f32 / max_brightness as f32;

        loop {
            #[cfg(target_os = "linux")]
            {
                match camera.capture_frame() {
                    Ok(frame) => {
                        let ambient = luminance_from_frame(&frame) / 255.0;

                        // Adaptive EMA: big change → fast alpha, small drift → slow alpha
                        let diff = (ambient - smoothed_val).abs();
                        let alpha = adaptive_alpha(base_alpha, diff);
                        smoothed_val = alpha * ambient + (1.0 - alpha) * smoothed_val;

                        let target_f = min_brightness as f32
                            + smoothed_val * (max_brightness - min_brightness) as f32;
                        let target = target_f.round() as u32;
                        let target = target.clamp(min_brightness, max_brightness);

                        // Hysteresis: only update target if it changed by more than 1
                        let old_target = target_ref.load(Ordering::SeqCst);
                        if (target as i32 - old_target as i32).abs() > 1 {
                            target_ref.store(target, Ordering::SeqCst);
                        }

                        info!(
                            "luma={:.2} alpha={:.2} smoothed={:.3} target={} current={}",
                            ambient,
                            alpha,
                            smoothed_val,
                            target,
                            current_ref.load(Ordering::SeqCst),
                        );
                    }
                    Err(e) => warn!("capture failed: {e:#}"),
                }
            }

            tokio::time::sleep(sample_interval).await;
        }
    });

    tokio::select! {
        _ = tick_handle => {}
        _ = sample_handle => {}
    }
    Ok(())
}