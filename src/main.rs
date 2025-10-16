use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::process::Command;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const IFACE_NAME: &str = "Wi-Fi"; // set exact adapter name (netsh interface show interface)
const SAMPLE_WINDOW_MS: u64 = 500; // window duration for RMS

fn set_iface(enabled: bool) {
    let admin = if enabled { "ENABLED" } else { "DISABLED" };
    let cmd = format!("netsh interface set interface \"{}\" admin={}", IFACE_NAME, admin);
    // run via cmd /C so quoting works
    let _ = Command::new("cmd")
        .args(&["/C", &cmd])
        .spawn()
        .and_then(|mut child| child.wait());
}

// --- calibration ---
fn calibrate(rx: &mpsc::Receiver<f32>, samples_per_window: usize) -> f32 {
    // collect ~3s of samples to compute ambient dB
    let mut buf = Vec::new();
    let target_samples = samples_per_window * 6; // 6 windows = ~3s if window=500ms
    while buf.len() < target_samples {
        if let Ok(s) = rx.recv_timeout(Duration::from_millis(200)) { buf.push(s); }
    }
    let sum_sq: f64 = buf.iter().map(|&s| (s as f64)*(s as f64)).sum();
    let rms = ((sum_sq / buf.len() as f64).sqrt()) as f32;
    rms_to_db(rms)
}

fn rms_to_db(rms: f32) -> f32 {
    if rms <= 1e-12 { return -999.0; }
    20.0 * rms.log10()
}

fn main() -> Result<(), anyhow::Error> {
    // small helper to print and ensure interface restored on exit
    ctrlc::set_handler(|| {
        println!("\nExiting — re-enabling interface.");
        set_iface(true);
        std::process::exit(0);
    }).ok();

    // CPAL setup
    let host = cpal::default_host();
    let device = host.default_input_device().expect("No input device available");
    let config = device.default_input_config().expect("No default input config");
    println!("Using input device: {}", device.name()?);
    println!("Input config: {:?}", config);

    // channel: callback will send f32 samples to aggregator
    let (tx, rx) = mpsc::channel::<f32>();
    let samples_per_window = (config.sample_rate().0 as u64 * SAMPLE_WINDOW_MS / 1000) as usize;

    // build input stream depending on sample format
    let tx_arc = Arc::new(Mutex::new(tx));
    match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let stream = device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    if let Ok(tx) = tx_arc.lock() {
                        for &s in data { let _ = tx.send(s); }
                    }
                },
                move |err| eprintln!("Stream error: {}", err)
            )?;
            stream.play()?;
            run_loop(rx, samples_per_window)?;
        }
        cpal::SampleFormat::I16 => {
            let stream = device.build_input_stream(
                &config.into(),
                move |data: &[i16], _| {
                    if let Ok(tx) = tx_arc.lock() {
                        for &s in data { let _ = tx.send(s as f32 / 32768.0); }
                    }
                },
                move |err| eprintln!("Stream error: {}", err)
            )?;
            stream.play()?;
            run_loop(rx, samples_per_window)?;
        }
        cpal::SampleFormat::U16 => {
            let stream = device.build_input_stream(
                &config.into(),
                move |data: &[u16], _| {
                    if let Ok(tx) = tx_arc.lock() {
                        for &s in data {
                            // convert unsigned 0..65535 to -1.0..1.0
                            let f = (s as f32 / 65535.0) * 2.0 - 1.0;
                            let _ = tx.send(f);
                        }
                    }
                },
                move |err| eprintln!("Stream error: {}", err)
            )?;
            stream.play()?;
            run_loop(rx, samples_per_window)?;
        }
    }

    Ok(())
}

fn run_loop(rx: mpsc::Receiver<f32>, samples_per_window: usize) -> Result<(), anyhow::Error> {
    let mut buffer = Vec::with_capacity(samples_per_window);
    let mut last_state: Option<String> = None;
    let mut last_sample_time = Instant::now();
    let mut iface_disabled = false;

    let ambient_db = calibrate(&rx, samples_per_window);
    let min_db = ambient_db + 15.0; // soft threshold
    let max_db = ambient_db + 45.0; // cut threshold
    println!("Ambient {:.1} dBFS -> min {:.1}, max {:.1}", ambient_db, min_db, max_db);

    loop {
        let start = Instant::now();
        // collect window
        while buffer.len() < samples_per_window {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(s) => {
                    buffer.push(s);
                    last_sample_time = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if start.elapsed() > Duration::from_millis(SAMPLE_WINDOW_MS + 200) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        // ---- inactivity watchdog ----
        if last_sample_time.elapsed() > Duration::from_secs(3) {
            if iface_disabled {
                set_iface(true);
                println!("No audio for 3s — restoring interface.");
                iface_disabled = false;
            }
            buffer.clear();
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        // compute RMS
        let rms = if buffer.is_empty() {
            0.0
        } else {
            let sum_sq: f64 = buffer.iter().map(|&s| (s as f64) * (s as f64)).sum();
            ((sum_sq / (buffer.len() as f64)).sqrt()) as f32
        };
        buffer.clear();

        let db = rms_to_db(rms);
        println!("Current volume: dB={:.1}", db);

        let pct = if db <= min_db {
            100
        } else if db >= max_db {
            0
        } else {
            let v = 1.0 - (db - min_db) / (max_db - min_db);
            (100.0 * v).round() as i32
        };

        let state = if pct == 0 {
            set_iface(false);
            iface_disabled = true;
            "CUT".to_string()
        } else {
            set_iface(true);
            iface_disabled = false;
            format!("OK {}%", pct)
        };

        if Some(state.clone()) != last_state {
            println!("dB={:.1} -> {}", db, state);
            last_state = Some(state);
        }

        thread::sleep(Duration::from_millis(50));
    }
}