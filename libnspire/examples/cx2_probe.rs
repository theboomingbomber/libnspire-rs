// Hardware probe for a connected TI-Nspire CX II (VID 0x0451, PID 0xe022).
// Used to capture baseline behaviour and to validate stability changes.
//
// Usage:
//   cargo run --example cx2_probe            # info + list root
//   cargo run --example cx2_probe -- idle    # info, sleep 60s, info again
//   cargo run --example cx2_probe -- loop 30 # poll info every 1s for 30s

use std::convert::TryFrom;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rusb::{GlobalContext, Hotplug, HotplugBuilder, UsbContext};

// Global lock mirroring n-link's USB_LOCK, used by the `stress` mode.
static STRESS_LOCK: Mutex<()> = Mutex::new(());

struct NullMon;
impl Hotplug<GlobalContext> for NullMon {
    fn device_arrived(&mut self, _: rusb::Device<GlobalContext>) {}
    fn device_left(&mut self, _: rusb::Device<GlobalContext>) {}
}

const VID: u16 = 0x0451;
const PID_CX2: u16 = 0xe022;
const PID: u16 = 0xe012;

fn open() -> libnspire::Handle<rusb::GlobalContext> {
    let dev = rusb::open_device_with_vid_pid(VID, PID_CX2)
        .or_else(|| rusb::open_device_with_vid_pid(VID, PID))
        .expect("no TI-Nspire found (looked for 0xe022 then 0xe012)");
    libnspire::Handle::new(dev).expect("failed to init libnspire handle")
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "info".to_string());
    match mode.as_str() {
        "info" => {
            let handle = open();
            match handle.info() {
                Ok(info) => println!("INFO OK: {}", serde_json::to_string(&info).unwrap()),
                Err(e) => println!("INFO ERR: {:?}", e),
            }
            match handle.list_dir("/") {
                Ok(dir) => {
                    println!("LIST OK: {} entries", dir.iter().count());
                    for f in dir.iter() {
                        println!("  {}", f.name().to_string_lossy());
                    }
                }
                Err(e) => println!("LIST ERR: {:?}", e),
            }
        }
        "idle" => {
            let secs: u64 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(60);
            let handle = open();
            println!("before idle: {:?}", handle.info().map(|i| i.name));
            println!("sleeping {secs}s (leave calc untouched)...");
            std::thread::sleep(Duration::from_secs(secs));
            let start = Instant::now();
            let r = handle.info();
            println!(
                "after {secs}s idle ({}ms): {:?}",
                start.elapsed().as_millis(),
                r.map(|i| i.name).map_err(|e| format!("{e:?}"))
            );
        }
        "loop" => {
            let secs: u64 = std::env::args()
                .nth(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            let handle = open();
            let start = Instant::now();
            let mut n = 0u64;
            while start.elapsed() < Duration::from_secs(secs) {
                n += 1;
                match handle.info() {
                    Ok(_) => print!("."),
                    Err(e) => print!("[{n}:{e:?}]"),
                }
                use std::io::Write;
                std::io::stdout().flush().ok();
                std::thread::sleep(Duration::from_secs(1));
            }
            println!("\ndone: {n} polls over {secs}s");
        }
        "pull" => {
            // pull <calc-path> <size-bytes> [out-file]
            let path = std::env::args().nth(2).expect("usage: pull <calc-path> <size> [out]");
            let size: usize = std::env::args()
                .nth(3)
                .and_then(|s| s.parse().ok())
                .expect("need size in bytes (from `info`/ls)");
            let out = std::env::args().nth(4);
            let handle = open();
            let mut buf = vec![0u8; size];
            let start = Instant::now();
            let r = handle.read_file(&path, &mut buf, &mut |rem| {
                if rem % 65536 < 4096 {
                    print!("\r  {} / {} bytes", size - rem, size);
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            });
            println!();
            match r {
                Ok(n) => {
                    let cs = crc32(&buf[..n.min(buf.len())]);
                    println!(
                        "PULL OK: {n} bytes in {}ms crc32={cs:08x}",
                        start.elapsed().as_millis()
                    );
                    if let Some(out) = out {
                        std::fs::write(&out, &buf[..n.min(buf.len())]).unwrap();
                        println!("wrote {out}");
                    }
                }
                Err(e) => println!("PULL ERR after {}ms: {e:?}", start.elapsed().as_millis()),
            }
        }
        "push" => {
            // push <local-file> <calc-dest-dir>
            let local = std::env::args().nth(2).expect("usage: push <local-file> <calc-dest-dir>");
            let dest = std::env::args().nth(3).expect("usage: push <local-file> <calc-dest-dir>");
            let buf = std::fs::read(&local).expect("read local file");
            let name = std::path::Path::new(&local)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let full = format!("{}/{}", dest.trim_end_matches('/'), name);
            let handle = open();
            let start = Instant::now();
            let r = handle.write_file(&full, &buf, &mut |rem| {
                if rem % 65536 < 4096 {
                    print!("\r  {} / {} bytes", buf.len() - rem, buf.len());
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            });
            println!();
            match r {
                Ok(()) => println!(
                    "PUSH OK: {} bytes ({}) in {}ms crc32={:08x}",
                    buf.len(),
                    full,
                    start.elapsed().as_millis(),
                    crc32(&buf)
                ),
                Err(e) => println!("PUSH ERR after {}ms: {e:?}", start.elapsed().as_millis()),
            }
        }
        "stress" => {
            // Reproduce n-link's hotplug-pump-vs-sync-transfer conflict.
            //   cargo run --example cx2_probe -- stress         (no lock: should hang)
            //   cargo run --example cx2_probe -- stress lock     (serialized: should not hang)
            let use_lock = std::env::args().nth(2).as_deref() == Some("lock");
            println!("stress mode, use_lock={use_lock}");
            if rusb::has_hotplug() {
                let reg = HotplugBuilder::new()
                    .vendor_id(VID)
                    .register(GlobalContext::default(), Box::new(NullMon))
                    .unwrap();
                std::mem::forget(reg);
                std::thread::spawn(move || loop {
                    if use_lock {
                        {
                            let _g = STRESS_LOCK.lock().unwrap();
                            let _ = GlobalContext::default()
                                .handle_events(Some(Duration::from_millis(50)));
                        }
                        std::thread::sleep(Duration::from_millis(150));
                    } else {
                        let _ = GlobalContext::default().handle_events(None);
                    }
                });
            } else {
                println!("no hotplug support");
            }
            let handle = open();
            let start = Instant::now();
            let mut n = 0u32;
            while start.elapsed() < Duration::from_secs(60) {
                n += 1;
                let t = Instant::now();
                let r = if use_lock {
                    let _g = STRESS_LOCK.lock().unwrap();
                    handle.list_dir("/")
                } else {
                    handle.list_dir("/")
                };
                println!(
                    "op {n}: {} in {}ms",
                    r.map(|d| format!("{} entries", d.iter().count()))
                        .unwrap_or_else(|e| format!("{e:?}")),
                    t.elapsed().as_millis()
                );
                std::thread::sleep(Duration::from_millis(500));
            }
            println!("DONE: {n} ops over 60s with no hang");
        }
        "enum" => {
            // Replicate n-link's add_device() exactly to see where GUI
            // enumeration drops the device.
            for dev in rusb::devices().unwrap().iter() {
                let d = match dev.device_descriptor() {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                if d.vendor_id() != VID || !matches!(d.product_id(), PID_CX2 | PID) {
                    continue;
                }
                println!(
                    "found {:04x}:{:04x} bus {} addr {}",
                    d.vendor_id(),
                    d.product_id(),
                    dev.bus_number(),
                    dev.address()
                );
                match dev.open() {
                    Ok(h) => {
                        println!("  open: OK");
                        match h.read_languages(Duration::from_millis(100)) {
                            Ok(langs) => {
                                println!("  languages: {:?}", langs);
                                match langs.first() {
                                    Some(l) => match h.read_product_string(
                                        *l,
                                        &d,
                                        Duration::from_millis(100),
                                    ) {
                                        Ok(s) => println!("  product_string: {s:?}"),
                                        Err(e) => println!("  read_product_string ERR: {e:?}"),
                                    },
                                    None => println!("  NO LANGUAGES -> langs[0] would PANIC"),
                                }
                            }
                            Err(e) => println!("  read_languages ERR: {e:?}"),
                        }
                    }
                    Err(e) => println!("  open ERR: {e:?}"),
                }
            }
        }
        "shot" => {
            let out = std::env::args()
                .nth(2)
                .unwrap_or_else(|| "/tmp/cx2_shot.png".to_string());
            let handle = open();
            match handle.screenshot() {
                Ok(img) => {
                    println!("SCREENSHOT OK: {}x{} bpp={}", img.width, img.height, img.bpp);
                    match image::DynamicImage::try_from(img) {
                        Ok(di) => {
                            di.save(&out).unwrap();
                            println!("saved {out}");
                        }
                        Err(e) => println!("convert err: {e:?}"),
                    }
                }
                Err(e) => println!("SCREENSHOT ERR: {e:?}"),
            }
        }
        "mkdir" => {
            let path = std::env::args().nth(2).expect("usage: mkdir <calc-path>");
            let handle = open();
            println!("MKDIR {path}: {:?}", handle.create_dir(&path));
        }
        "rm" => {
            let path = std::env::args().nth(2).expect("usage: rm <calc-path>");
            let handle = open();
            println!("RM {path}: {:?}", handle.delete_file(&path));
        }
        "rmdir" => {
            let path = std::env::args().nth(2).expect("usage: rmdir <calc-path>");
            let handle = open();
            println!("RMDIR {path}: {:?}", handle.delete_dir(&path));
        }
        other => println!("unknown mode: {other}"),
    }
}

/// Tiny dependency-free CRC32 (IEEE) for integrity checks.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
